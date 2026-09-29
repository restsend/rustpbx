use super::prelude::*;
use super::SipSession;
use crate::call::cookie::TransactionCookie;
use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallStatus};
use rsipstack::dialog::dialog_layer::DialogLayer;

impl SipSession {
    /// Resolve and execute the raw REFER delivered to this session.
    pub(super) async fn handle_received_refer(
        &mut self,
        dialog_id: DialogId,
        request: rsipstack::sip::Request,
        transaction: rsipstack::dialog::dialog::TransactionHandle,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        let Some(Dialog::Invite(dialog)) = self.server.dialog_layer.get_dialog(&dialog_id)
            .filter(|dialog| dialog.id() == dialog_id) else {
            transaction.reply(StatusCode::CallTransactionDoesNotExist).await?;
            return Ok(());
        };
        let targets: Vec<_> = request.headers.iter().filter_map(|header| match header {
            rsipstack::sip::Header::ReferTo(value) => Some(value.value().to_string()),
            rsipstack::sip::Header::Other(name, value) if name.eq_ignore_ascii_case("Refer-To") => Some(value.clone()),
            _ => None,
        }).collect();
        if targets.len() != 1 {
            transaction.reply(StatusCode::BadRequest).await?;
            return Ok(());
        }
        let refer_to = targets[0].trim().trim_start_matches('<').trim_end_matches('>').to_string();
        if !dialog.state().is_confirmed() || self.leg_id_for_dialog(&dialog_id.to_string()).is_none() {
            transaction.reply(StatusCode::CallTransactionDoesNotExist).await?;
            return Ok(());
        }
        // Preserve an already known agent identity only when this leg refers.
        // This does not classify ordinary dialed legs or query their Contact URIs.
        if let Some(leg_id) = self.leg_id_for_dialog(&dialog_id.to_string()) {
            let selected = self.resolve_transfer_leg();
            let mut agent_id = self.legs.get(&leg_id).and_then(|leg| leg.agent_id.clone());
            if agent_id.is_none() && leg_id == selected {
                agent_id = self.pinned_agent_id().or_else(|| self.session_ext_get("agent_id"));
            }
            if let Some(agent_id) = agent_id {
                if let Some(leg) = self.legs.get_mut(&leg_id) {
                    if leg.agent_id.is_none() { leg.agent_id = Some(agent_id); }
                }
            }
        }
        transaction.reply(StatusCode::Accepted).await?;
        info!(session_id = %self.id, %dialog_id, %refer_to, "Inbound REFER received by session");
        let t_refer = std::time::Instant::now();
        let (target_uri, replaces_header) = Self::parse_refer_to(&refer_to);
        let conference_target = self.server.conference_server.list_conferences_detail().await.into_iter()
            .any(|room| room.focus_uri.as_deref() == Some(target_uri.as_str()));
        if conference_target && replaces_header.is_some() {
            SipSession::notify_refer_bounded(&dialog, StatusCode::NotImplemented, "terminated;reason=noresource").await?;
            return Ok(());
        }
        // P1a: a locally registered target rings directly, bypassing route
        // rules (mirrors the inbound INVITE locator-first order).
        let locator_direct = !conference_target
            && Self::refer_target_locally_registered(&self.server, &target_uri).await;
        // R1: resolve app targets for BOTH refer flavors; an attended REFER
        // to an app runs the in-session hand-off like the blind path.
        let app_target = if !conference_target && !locator_direct {
            match Self::resolve_refer_app_target(
                &self.server, &self.context.session_id, &target_uri, &TransactionCookie::default(),
            ).await {
                Ok(target) => target,
                Err((code, reason)) if replaces_header.is_none() => {
                    warn!(session_id = %self.id, %reason, code, "Failed to resolve REFER target");
                    SipSession::notify_refer_bounded(&dialog, StatusCode::from(code), "terminated;reason=noresource").await?;
                    return Ok(());
                }
                // Attended: on lookup failure fall through to the room path.
                Err((code, reason)) => {
                    warn!(session_id = %self.id, %reason, code, "REFER app target lookup failed; falling back to room path");
                    None
                }
            }
        } else { None };
        info!(session_id = %self.id, elapsed_ms = t_refer.elapsed().as_millis() as u64,
            locator_direct, attended = replaces_header.is_some(),
            app_target = app_target.as_deref().unwrap_or("<direct-sip>"),
            "REFER stage: target resolved");
        if let Some(replaces) = replaces_header.as_deref() {
            // Bounded: the sender's contact may be stale; a doomed NOTIFY
            // must not starve the session loop.
            if let Err(error) = SipSession::notify_refer_bounded(&dialog, StatusCode::Trying, "active").await {
                warn!(session_id = %self.id, error = %error, "REFER NOTIFY (Trying) failed; continuing transfer");
            }
            if let Some(app_target) = app_target {
                return self.handle_inbound_refer(
                    dialog_id, app_target,
                    Self::refer_application_headers(request.headers.iter()), callee_state_rx,
                ).await;
            }
            return self.handle_refer_replaces(&dialog_id, replaces, &target_uri).await;
        }
        let in_session = replaces_header.is_none()
            && (conference_target || app_target.is_some() || self.server.proxy_config.load().inbound_refer_in_session);
        if in_session {
            return self.handle_inbound_refer(
                dialog_id, app_target.unwrap_or(target_uri),
                Self::refer_application_headers(request.headers.iter()), callee_state_rx,
            ).await;
        }
        let original_handle = self.server.active_call_registry.get_handle(&self.context.session_id)
            .ok_or_else(|| anyhow!("REFER session is no longer registered"))?;
        // The explicitly configured legacy blind-transfer path retains
        // its existing external execution. Blind in-session REFER is owned
        // entirely by SipSession after the event above.
        let dialog_layer = self.server.dialog_layer.clone();
        let refer_to_clone = refer_to.clone();
        let server = self.server.clone();
        let original_session_id = original_handle.session_id().to_string();
        crate::utils::spawn(async move {
            info!("Spawned inbound REFER background task");

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            info!("Sending NOTIFY 100 Trying for REFER");
            match Self::send_refer_notify(&dialog_layer, &dialog_id, 100, "Trying", &refer_to_clone)
                .await
            {
                Ok(_) => info!("Sent NOTIFY 100 Trying for REFER"),
                Err(e) => {
                    warn!(error = %e, "Failed to send NOTIFY 100 Trying");
                    return;
                }
            }

            let (target_uri, _) = Self::parse_refer_to(&refer_to_clone);

            let result = Self::execute_inbound_refer_transfer(
                &server, &original_handle, &original_session_id,
                &target_uri,
            ).await;

            let (notify_status, notify_reason) = match result {
                Ok(_) => (200, "OK"),
                Err((status, ref reason)) => {
                    warn!(status = status, reason = %reason, "Inbound REFER transfer failed");
                    (status, reason.as_str())
                }
            };

            if let Err(e) = Self::send_refer_notify(
                &dialog_layer,
                &dialog_id,
                notify_status,
                notify_reason,
                &refer_to_clone,
            )
            .await
            {
                warn!(error = %e, "Failed to send final NOTIFY for REFER");
            } else {
                info!(status = notify_status, "Sent final NOTIFY for REFER");
            }

            // Only this legacy blind-transfer path needs an external transfer event.
            // In-session REFER emits its event from the session.
            if notify_status == 200
                && let Some(ref gw) = server.rwi_gateway
            {
                let g = gw.read();
                g.send_to_owner(&crate::rwi::CallTransferred {
                    call_id: original_session_id.clone(),
                    transfer_target: Some(refer_to_clone.clone()),
                    transfer_target_type: crate::call::transfer_target_kind(&refer_to_clone),
                    transfer_source: g
                        .meta_store
                        .get_sync(&original_session_id)
                        .and_then(|m| m.transfer_source),
                });
            }
        });

        Ok(())
    }

    /// Resolve session ownership by bare Call-ID, then validate the SIP dialog
    /// with Replaces tags. The registry key convention is unchanged.
    async fn handle_refer_replaces(
        &mut self,
        source_dialog: &DialogId,
        replaces: &str,
        target_uri: &str,
    ) -> Result<()> {
        let mut failed_setup = None;
        let result: Result<(), (u16, String)> = async {
            let mut fields = replaces.split(';');
            let call_id = fields.next().unwrap_or_default().trim();
            let mut to_tag = None;
            let mut from_tag = None;
            let mut early_only = false;
            for field in fields {
                let field = field.trim();
                if field.eq_ignore_ascii_case("early-only") { early_only = true; }
                if let Some((name, value)) = field.split_once('=') {
                    if name.eq_ignore_ascii_case("to-tag") { to_tag = Some(value.to_string()); }
                    if name.eq_ignore_ascii_case("from-tag") { from_tag = Some(value.to_string()); }
                }
            }
            let (Some(local_tag), Some(remote_tag)) = (to_tag, from_tag) else {
                return Err((400, "Replaces requires both dialog tags".into()));
            };
            let owner = self.server.active_call_registry.get_handle_by_dialog(call_id)
                .ok_or_else(|| (481, "Replaces Call-ID is not owned by a local session".into()))?;
            let target_dialog = DialogId { call_id: call_id.into(), local_tag, remote_tag };
            let dialog = self.server.dialog_layer.get_dialog(&target_dialog)
                .ok_or_else(|| (481, "Replaces tags do not match a local dialog".into()))?;
            if !dialog.state().is_confirmed() || early_only {
                tracing::warn!(
                    session_id = %self.context.session_id,
                    state = ?dialog.state(), early_only,
                    "Replaces target dialog not established (diagnostic)"
                );
                return Err((486, "Replaces requires an established consultation".into()));
            }
            if owner.session_id() == self.context.session_id {
                return Err((501, "Same-session Replaces is not supported by this cross-session path".into()));
            }
            let transferor = self.resolve_transfer_leg();
            if self.legs.get_dialog(&transferor).is_none_or(|dialog| dialog.id() != *source_dialog) {
                return Err((481, "REFER must originate from the current callee".into()));
            }
            if self.meta.transfer_in_progress || self.conference.is_some() {
                return Err((491, "Call is already transferring or in a conference".into()));
            }
            let room = crate::call::runtime::ConferenceId::from(format!("transfer-{}", uuid::Uuid::new_v4()));
            let manager = self.server.conference_server.manager_raw().clone();
            manager.create_conference(room.clone(), Some(2)).await.map_err(|e| (500, e.to_string()))?;
            failed_setup = Some((room.clone(), owner.clone()));
            self.meta.transfer_in_progress = true;
            self.sync_rtp_timeout_pause();
            let result: Result<()> = async {
                let local_leg = self.handle_join_conference_peer(room.0.clone(), source_dialog.clone()).await?;
                let (reply, completed) = tokio::sync::oneshot::channel();
                owner.send_command(crate::call::domain::CallCommand::JoinConferencePeer {
                    conference_id: room.0.clone(), dialog_id: target_dialog, reply,
                })?;
                let remote_leg = tokio::time::timeout(std::time::Duration::from_secs(10), completed)
                    .await.map_err(|_| anyhow!("Consultation session did not complete the attachment"))??
                    .map_err(|error| anyhow!(error))?;
                if self.caller_dialog.as_ref().is_none_or(|dialog| dialog.state().is_terminated()) {
                    return Err(anyhow!("Caller left before transfer completed"));
                }
                let remote_participant = LegId::from(format!("{}-{}", owner.session_id(), remote_leg));
                manager.bind_transfer_owner(&room, &self.participant_leg(&local_leg), &remote_participant)?;
                info!(session_id = %self.id, peer_session = %owner.session_id(), %local_leg, %remote_leg,
                    room = %room.0, "Attended REFER connected existing participants across sessions");
                Ok(())
            }.await;
            self.meta.transfer_in_progress = false;
            self.sync_rtp_timeout_pause();
            result.map_err(|error| (500, error.to_string()))
        }.await;
        let status = match &result {
            Ok(()) => 200,
            Err((status, reason)) => {
                warn!(session_id = %self.id, %reason, status, "Local attended REFER failed");
                *status
            }
        };
        if status == 200 {
            let source = if let Some(leg) = self.leg_id_for_dialog(&source_dialog.to_string()) {
                self.transfer_source_snapshot(&leg).await
            } else { None };
            self.stash_transfer_source(source.clone());
            self.mark_transferred_with(Some(serde_json::json!({"target": target_uri, "kind": "sip"})));
            self.emit_typed_rwi_event(&crate::rwi::CallTransferred {
                call_id: self.context.session_id.clone(), transfer_target: Some(target_uri.to_string()),
                transfer_target_type: Some("sip".into()), transfer_source: source,
            });
        }
        // Finish the REFER subscription before any cleanup can remove B's dialog.
        // Failure to deliver NOTIFY must not prevent teardown of a failed setup.
        if let Some(Dialog::Invite(dialog)) = self.server.dialog_layer.get_dialog(source_dialog) {
            if let Err(error) = SipSession::notify_refer_bounded(&dialog, StatusCode::from(status), "terminated;reason=noresource").await {
                warn!(session_id = %self.id, %error, "Failed to send final attended REFER NOTIFY");
            } else {
                info!(session_id = %self.id, status, "Sent final attended REFER NOTIFY");
            }
        }
        if result.is_err() && let Some((room, _owner)) = failed_setup {
            // R2: roll back atomically — customer restored held with the
            // agent, both sessions stay up, no zombie room.
            let manager = self.server.conference_server.manager_raw().clone();
            let mut rolled_back_leg = None;
            if self.conference.as_ref().is_some_and(|attachment| attachment.conference_id == room) {
                let leg = self.conference.as_ref().unwrap().leg_id.clone();
                if let Err(error) = self.leave_conference_leg(&leg).await {
                    warn!(%error, %leg, "Failed to detach transfer room participant");
                }
                rolled_back_leg = Some(leg);
            }
            if let Err(error) = manager.destroy_conference(&room).await {
                warn!(%error, "Failed to destroy incomplete transfer room");
            }
            if let Some(leg) = rolled_back_leg {
                // Restore the held state media-only, then re-select the pair.
                self.update_leg_state(&leg, crate::call::domain::LegState::Hold);
                if let Err(error) = self.apply_hold_media(&leg, None).await {
                    warn!(session_id = %self.id, %leg, %error, "Failed to restore hold media after transfer rollback");
                }
                self.update_media_path().await;
                self.sync_rtp_timeout_pause();
                info!(session_id = %self.id, %leg, room = %room.0, "Attended REFER rolled back; customer restored to held state");
            }
        }
        Ok(())
    }

    /// True when the REFER target has a live registration — dial it directly
    /// instead of route-table resolution. The shared locator is cluster-wide
    /// (a peer node's registration counts; `handle_add_leg` dials the full
    /// Location so cross-node home_proxy routing is preserved). Fails open
    /// on locator errors.
    async fn refer_target_locally_registered(server: &SipServerRef, target_uri: &str) -> bool {
        let Ok(parsed) = rsipstack::sip::Uri::try_from(target_uri) else {
            return false;
        };
        if !server.is_same_realm(parsed.host().to_string().as_str()).await {
            return false;
        }
        match server.locator.lookup(&parsed).await {
            Ok(locations) => !locations.is_empty(),
            Err(error) => {
                warn!(error = %error, target = %target_uri, "REFER locator fast-path lookup failed; falling back to route resolution");
                false
            }
        }
    }

    /// Resolve REFER feature codes and application routes without executing a transfer.
    async fn resolve_refer_app_target(
        server: &SipServerRef,
        original_session_id: &str,
        target_uri: &str,
        cookie: &crate::call::cookie::TransactionCookie,
    ) -> Result<Option<String>, (u16, String)> {
        if let Some(resolver) = server.quick_route_resolver.as_ref() {
            let parsed = rsipstack::sip::Uri::try_from(target_uri).ok();
            let user = parsed.as_ref().and_then(|u| u.user().map(|u| u.to_string()));
            if let Some(user) = user
                && let Some(target) = resolver.resolve_quick_target(&user).await
            {
                info!(
                    user = %user,
                    target = %target,
                    "REFER hand-off via quick-route feature code"
                );
                return Ok(Some(target));
            }
        }
        // REFER application handoffs resolve local routes independently of
        // the switch for routing API/app-originated SIP calls.
        // Route matching keys off the request-URI user part — a bare number.
        let Ok(parsed) = rsipstack::sip::Uri::try_from(target_uri) else {
            return Ok(None);
        };
        let Some(user) = parsed.user().map(|u| u.to_string()) else {
            return Ok(None);
        };
        // Route matching uses the original caller. Application destinations
        // must also resolve on servers without an RWI gateway.
        let Some(caller_str) = server.rwi_gateway.as_ref().and_then(|gw| {
            gw.read()
                .meta_store
                .get_sync(original_session_id)
                .and_then(|m| m.caller)
        }).or_else(|| server.active_call_registry.get(original_session_id).and_then(|call| call.caller)) else {
            return Err((500, "REFER route lookup has no original caller identity".to_string()));
        };
        let caller_uri = rsipstack::sip::Uri::try_from(caller_str.as_str())
            .map_err(|_| (500, "REFER route lookup has invalid original caller identity".to_string()))?;
        let routed = crate::proxy::proxy_call::sip_session::route_outbound_leg(
            server,
            &parsed,
            &caller_uri,
            &caller_uri,
            None,
            cookie.clone(),
        )
        .await
        .map_err(|e| (500, format!("route lookup failed: {}", e)))?;

        // Map queue/application routes onto the in-session transfer targets
        // (`handle_blind_transfer_inner` vocabulary). Everything else —
        // Forward / NotHandled / None — remains a SIP transfer destination.
        // The original REFER number rides along as a `refer_to` query param:
        // the queue/IVR parsers ignore it, while the in-session hand-off's
        // `call_transferred` event carries the target string verbatim —
        // keeping the dialed number visible to consumers (docs contract:
        // "original number kept in transfer_target").
        let handoff_target = match routed {
            Some(crate::config::RouteResult::Queue { queue, .. }) => {
                format!(
                    "queue:{}?refer_to={}",
                    queue.queue_name,
                    urlencoding::encode(&user)
                )
            }
            Some(crate::config::RouteResult::Application { app_params, .. }) => {
                // P1b: an application-routed number that is a known cc agent
                // hands off straight via queue:agent:<ext>.
                match Self::refer_target_cc_agent(server, &user).await {
                    Some(ext) => {
                        let mut target =
                            format!("queue:agent:{ext}?refer_to={}", urlencoding::encode(&user));
                        if let Some(ivr) = app_params
                            .as_ref()
                            .and_then(|p| p.get("file"))
                            .and_then(|f| f.as_str())
                            .and_then(Self::ivr_display_name)
                        {
                            target.push_str(&format!(
                                "&return_app=ivr&return_target={}",
                                urlencoding::encode(&ivr)
                            ));
                        }
                        target
                    }
                    None => format!("toivr:{}?refer_to={}", user, urlencoding::encode(&user)),
                }
            }
            _ => return Ok(None),
        };

        info!(session_id = %original_session_id, %target_uri, %handoff_target,
            "Resolved REFER application handoff");
        Ok(Some(handoff_target))
    }

    /// Side-effect-free probe: is this dialled user a known cc agent extension?
    async fn refer_target_cc_agent(server: &SipServerRef, user: &str) -> Option<String> {
        let registry = server.agent_registry.as_ref()?;
        let target = format!("agent:{user}");
        registry
            .has_target(&target)
            .await
            .then(|| user.to_string())
    }

    /// `app_params.file` → IVR display name
    /// ("config/ivr/x.toml" → "x", "db://ivr/x.generated.toml" → "x").
    fn ivr_display_name(file: &str) -> Option<String> {
        let base = file.rsplit('/').next()?.trim();
        let base = base.strip_suffix(".toml").unwrap_or(base);
        let base = base.strip_suffix(".generated").unwrap_or(base);
        (!base.is_empty()).then(|| base.to_string())
    }

    /// Parse Refer-To URI, extracting the base target and optional Replaces header.
    fn parse_refer_to(refer_to: &str) -> (String, Option<String>) {
        if let Some(pos) = refer_to.find("?Replaces=") {
            let base = &refer_to[..pos];
            let encoded = &refer_to[pos + 10..];
            let decoded = urlencoding::decode(encoded).unwrap_or_else(|_| encoded.into());
            (base.to_string(), Some(decoded.into_owned()))
        } else if let Some(pos) = refer_to.find("&Replaces=") {
            let base = &refer_to[..pos];
            let encoded = &refer_to[pos + 10..];
            let decoded = urlencoding::decode(encoded).unwrap_or_else(|_| encoded.into());
            (base.to_string(), Some(decoded.into_owned()))
        } else {
            (refer_to.to_string(), None)
        }
    }

    fn refer_application_headers<'a>(
        headers: impl IntoIterator<Item = &'a rsipstack::sip::Header>,
    ) -> HashMap<String, String> {
        let mut carried = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for header in headers {
            let rsipstack::sip::Header::Other(name, value) = header else {
                continue;
            };
            if !name
                .get(..2)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("X-"))
            {
                continue;
            }
            if !seen.insert(name.to_ascii_lowercase()) {
                continue;
            }
            carried.insert(name.clone(), value.clone());
        }
        carried
    }

    /// Execute the actual transfer for an inbound REFER.
    ///
    /// This originates a new call to the target and bridges it with the original session.
    /// Returns Ok(()) on success, or Err((sip_status, reason)) on failure so the caller
    /// can send an accurate NOTIFY sipfrag.
    async fn execute_inbound_refer_transfer(
        server: &SipServerRef,
        original_handle: &crate::proxy::proxy_call::sip_session::SipSessionHandle,
        original_session_id: &str,
        target_uri: &str,
    ) -> Result<(), (u16, String)> {
        info!(target_uri, "Starting inbound REFER transfer execution");

        let destination_uri: rsipstack::sip::Uri = rsipstack::sip::Uri::try_from(target_uri)
            .map_err(|e| (400, format!("Invalid transfer target URI: {:?}", e)))?;

        let proxy_config = server.proxy_config.load();
        let realm = proxy_config
            .realms
            .as_ref()
            .and_then(|v| v.first().cloned())
            .unwrap_or_else(|| proxy_config.addr.clone());
        let caller_uri_str = format!("sip:transfer@{}", realm);
        let caller_uri: rsipstack::sip::Uri =
            rsipstack::sip::Uri::try_from(caller_uri_str.as_str())
                .map_err(|e| (500, format!("Invalid caller URI: {:?}", e)))?;

        // Resolve the root session id from the original session so the
        // transfer-target leg stays correlated with the whole logical call
        // (RFC 7433 UUI, purpose=call-center). Falls back to the original
        // session id when no meta is available.
        let root_session_id = server
            .rwi_gateway
            .as_ref()
            .and_then(|gw| {
                gw.read()
                    .meta_store
                    .get_sync(original_session_id)
                    .and_then(|m| m.session_id)
            })
            .unwrap_or_else(|| original_session_id.to_string());

        let mut headers = vec![rsipstack::sip::Header::Other(
            "Max-Forwards".into(),
            "70".into(),
        )];
        // Carry the root session id to the transfer target via UUI so an
        // external network leg can re-attach on the way back in.
        headers.push(crate::call::uui::build_uui_header(
            &root_session_id,
            None,
            None,
            None,
        ));
        let media = server.default_media_config();
        let external_ip = media
            .external_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());

        let new_call_id = uuid::Uuid::new_v4().to_string();
        let media_track =
            crate::media::RtpTrackBuilder::new(format!("inbound-refer-{}", new_call_id))
                .with_cancel_token(tokio_util::sync::CancellationToken::new())
                .with_enable_latching(media.enable_latching)
                .with_probation_max_packets(media.probation_max_packets)
                .with_external_ip(external_ip)
                // Plain-RTP builder (default mode): honor the global ICE-lite
                // knob; WebRTC is not involved on this path.
                .with_ice_lite(media.ice_lite)
                .with_cname(server.rtc_cname.clone());
        let media_track = if let Some(bind_ip) = media.bind_ip.clone() {
            media_track.with_bind_ip(bind_ip)
        } else {
            media_track
        }
        .build();

        let sdp_offer = media_track
            .local_description()
            .await
            .map_err(|e| (500, format!("Failed to generate SDP: {}", e)))?;

        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: destination_uri.clone(),
            caller: caller_uri.clone(),
            contact: caller_uri,
            content_type: Some("application/sdp".to_string()),
            offer: Some(sdp_offer.into_bytes()),
            destination: None,
            credential: None,
            headers: Some(headers),
            call_id: Some(new_call_id.clone()),
            // RFC 7989: carry the root session id when it is UUID-shaped so
            // the transfer target correlates the logical call. Legacy root
            // ids (raw Call-IDs) keep relying on the UUI header above.
            session_id: crate::call::session_id::normalize(&root_session_id),
            ..Default::default()
        };

        info!(%new_call_id, callee = %destination_uri, "Sending INVITE for inbound REFER transfer");

        let dialog_layer = server.dialog_layer.clone();
        let registry = server.active_call_registry.clone();
        let original_session_id = original_session_id.to_string();
        let target_for_log = target_uri.to_string();

        let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

        let id = SessionId::from(new_call_id.clone());
        let (new_handle, mut _cmd_rx) = SipSession::with_handle(id);

        let entry = ActiveProxyCallEntry {
            session_id: new_call_id.clone(),
            caller: Some("transfer".to_string()),
            callee: Some(target_for_log),
            direction: "outbound".to_string(),
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: ActiveProxyCallStatus::Ringing,
        };
        registry.upsert(entry, new_handle.clone());

        let (watch_tx, watch_rx) = tokio::sync::watch::channel(None);
        let timeout_secs = 60u64;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            async {
                loop {
                    tokio::select! {
                        res = &mut invitation => break res,
                        state = state_rx.recv() => {
                            let _ = watch_tx.send(state.clone());
                            if let Some(ref state) = state {
                                let state_str = match state {
                                    rsipstack::dialog::dialog::DialogState::Calling(_) => "Calling",
                                    rsipstack::dialog::dialog::DialogState::Early(_, _) => "Early",
                                    rsipstack::dialog::dialog::DialogState::Confirmed(_, _) => "Confirmed",
                                    rsipstack::dialog::dialog::DialogState::Terminated(_, _) => "Terminated",
                                    rsipstack::dialog::dialog::DialogState::Updated(_, _, _) => "Updated",
                                    rsipstack::dialog::dialog::DialogState::Refer(_, _, _) => "Refer",
                                    _ => "Other",
                                };
                                info!(state = state_str, "Inbound REFER transfer invitation state update");
                            }
                        }
                    }
                }
            },
        )
        .await;

        match result {
            Ok(Ok((dialog, Some(resp))))
                if resp.status_code().kind()
                    == rsipstack::sip::status_code::StatusCodeKind::Successful =>
            {
                crate::call::sip::spawn_client_dialog_guard(
                    dialog_layer.clone(),
                    dialog.id(),
                    watch_rx,
                );
                info!(%new_call_id, "Inbound REFER transfer target answered");

                registry.update(&new_call_id, |entry| {
                    entry.answered_at = Some(chrono::Utc::now());
                    entry.status = ActiveProxyCallStatus::Talking;
                });

                // Inherit the root session id on the transfer-target leg so
                // its events/CDR stay correlated with the logical call
                // (root = original session's root).
                if let Some(ref gw) = server.rwi_gateway {
                    let gw = gw.read();
                    let mut meta = gw.meta_store.get_sync(&new_call_id).unwrap_or_default();
                    meta.session_id = Some(root_session_id.clone());
                    // Carry the transferring agent onto the target leg so its
                    // events (call_answered, call_hangup, ...) keep the agent
                    // context — this leg never emits a `call_created` of its
                    // own. `queue_id` is deliberately NOT inherited (the leg
                    // has left queue service).
                    if meta.agent_id.is_none()
                        && let Some(original_meta) = gw.meta_store.get_sync(&original_session_id)
                    {
                        meta.agent_id = original_meta.agent_id;
                        meta.agent_name = original_meta.agent_name;
                    }
                    gw.meta_store.insert(new_call_id.clone(), meta);
                }

                let leg_a = crate::call::domain::LegId::new(&original_session_id);
                let leg_b = crate::call::domain::LegId::new(&new_call_id);

                // Mark the surviving caller session as transferred so
                // post-call hooks (CSAT) suppress the survey on this leg.
                original_handle
                    .send_command(crate::call::domain::CallCommand::MarkTransferred)
                    .map_err(|e| (500, format!("Failed to mark transferred: {}", e)))?;

                original_handle
                    .send_command(crate::call::domain::CallCommand::Bridge {
                        leg_a,
                        leg_b,
                        mode: crate::call::domain::P2PMode::Audio,
                    })
                    .map_err(|e| (500, format!("Failed to bridge calls: {}", e)))?;

                info!(%original_session_id, %new_call_id, "Bridged original and transfer target calls");
                Ok(())
            }
            Ok(Ok((_, Some(resp)))) => {
                let code = resp.status_code().code();
                warn!(%new_call_id, status = %code, "Inbound REFER transfer target rejected");
                registry.remove(&new_call_id);
                Err((code, format!("Transfer target rejected with {}", code)))
            }
            Ok(Err(e)) => {
                warn!(%new_call_id, error = %e, "Inbound REFER transfer error");
                registry.remove(&new_call_id);
                Err((500, format!("Invite failed: {}", e)))
            }
            Err(_) => {
                warn!(%new_call_id, "Inbound REFER transfer timeout");
                registry.remove(&new_call_id);
                Err((408, "Transfer target timeout".to_string()))
            }
            _ => {
                registry.remove(&new_call_id);
                Err((500, "Unexpected invite result".to_string()))
            }
        }
    }

    /// Send NOTIFY for REFER subscription
    ///
    /// Uses `ServerInviteDialog::notify_refer` which follows RFC 3515 and
    /// automatically builds the correct `message/sipfrag` body and
    /// `Subscription-State` header.
    async fn send_refer_notify(
        dialog_layer: &Arc<DialogLayer>,
        dialog_id: &DialogId,
        status_code: u16,
        _reason_phrase: &str,
        _refer_to: &str,
    ) -> Result<()> {
        let status = rsipstack::sip::StatusCode::from(status_code);
        let sub_state = if status_code >= 200 {
            "terminated;reason=noresource"
        } else {
            "active"
        };

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::Invite(d) => match d.notify_refer(status, sub_state).await {
                    Ok(Some(response)) => {
                        info!(
                            status = %response.status_code(),
                            "NOTIFY sent successfully"
                        );
                        Ok(())
                    }
                    Ok(None) => {
                        warn!("No response received for NOTIFY");
                        Ok(())
                    }
                    Err(e) => Err(anyhow!("Failed to send NOTIFY: {}", e)),
                },                _ => {
                    warn!("Dialog is not a server invite dialog, cannot send NOTIFY");
                    Ok(())
                }
            }
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refer_application_headers_preserve_extension_headers() {
        let headers = vec![
            rsipstack::sip::Header::Other("X-Route-Metadata".into(), "workflow=feedback".into()),
            rsipstack::sip::Header::Other("X-Trace-Context".into(), "trace-test".into()),
            rsipstack::sip::Header::Other(
                "X-User-Data".into(),
                "form=feedback;template=three-option".into(),
            ),
            rsipstack::sip::Header::Other("Authorization".into(), "secret".into()),
        ];

        assert_eq!(
            SipSession::refer_application_headers(&headers),
            std::collections::HashMap::from([
                (
                    "X-Route-Metadata".to_string(),
                    "workflow=feedback".to_string(),
                ),
                ("X-Trace-Context".to_string(), "trace-test".to_string()),
                (
                    "X-User-Data".to_string(),
                    "form=feedback;template=three-option".to_string(),
                ),
            ])
        );
    }

    #[test]
    fn refer_application_headers_keep_first_case_insensitive_value() {
        let headers = vec![
            rsipstack::sip::Header::Other("X-Route-Metadata".into(), "   ".into()),
            rsipstack::sip::Header::Other("x-ROUTE-metadata".into(), "second".into()),
            rsipstack::sip::Header::Other("X-Trace-Context".into(), "trace-test".into()),
        ];

        let carried = SipSession::refer_application_headers(&headers);
        assert_eq!(
            carried.get("X-Route-Metadata").map(String::as_str),
            Some("   ")
        );
        assert_eq!(
            carried.get("X-Trace-Context").map(String::as_str),
            Some("trace-test")
        );
        assert_eq!(carried.len(), 2);
    }

    #[test]
    fn test_parse_refer_to_with_replaces() {
        let refer_to = "sip:charlie@example.com?Replaces=call-id%3Bto-tag%3Dtt%3Bfrom-tag%3Dft";
        let (base, replaces) = SipSession::parse_refer_to(refer_to);
        assert_eq!(base, "sip:charlie@example.com");
        assert_eq!(replaces, Some("call-id;to-tag=tt;from-tag=ft".to_string()));
    }

    #[test]
    fn test_parse_refer_to_without_replaces() {
        let refer_to = "sip:charlie@example.com";
        let (base, replaces) = SipSession::parse_refer_to(refer_to);
        assert_eq!(base, "sip:charlie@example.com");
        assert_eq!(replaces, None);
    }

}
