
// ===================================================================
// Hold / unhold and re-INVITE handling for SipSession
// (moved from sip_session/session.rs — the hold-domain cluster:
// hold direction evaluation, hold propagation, re-INVITE send,
// hold transition hooks).
// ===================================================================

use super::prelude::*;
use super::session::{DialogSide, SipSession};

impl SipSession {
    pub(crate) fn is_hold_direction(
        direction: rustrtc::Direction,
        offer: Option<&rustrtc::SessionDescription>,
    ) -> bool {
        if !matches!(direction, rustrtc::Direction::SendRecv) {
            return true;
        }
        // Per RFC 4317, c=IN IP4 0.0.0.0 also signals hold even when
        // direction is sendrecv (some endpoints use this convention). WebRTC
        // uses the same zero C-line with ICE for active media, so do not treat
        // an ICE media section as held.
        if let Some(offer) = offer {
            for section in offer
                .media_sections
                .iter()
                .filter(|section| section.kind == rustrtc::MediaKind::Audio)
            {
                if section.port == 0 {
                    return true;
                }
                if Self::section_has_zero_hold_connection(
                    section,
                    offer.session.connection.as_deref(),
                ) {
                    return true;
                }
            }
        }
        false
    }

    pub(crate) async fn get_local_reinvite_pc(&self, leg_id: &LegId) -> Option<rustrtc::PeerConnection> {
        self.media_leg(leg_id).map(|peer| peer.pc().clone())
    }

    pub(crate) fn section_has_zero_hold_connection(
        section: &rustrtc::MediaSection,
        session_connection: Option<&str>,
    ) -> bool {
        let uses_ice = section.protocol.contains("UDP/TLS")
            || section
                .attributes
                .iter()
                .any(|attribute| matches!(attribute.key.as_str(), "ice-ufrag" | "candidate"));
        !uses_ice
            && section
                .connection
                .as_deref()
                .or(session_connection)
                .is_some_and(Self::is_zero_connection)
    }

    /// Propagate hold to the selected peer: updates leg state, sends a hold re-INVITE
    /// (media bypass) or starts hold music on its peer. `override_music`,
    /// if `Some`, is used instead of the normal header/extension/config chain.
    pub(crate) async fn propagate_hold_to_leg(
        &mut self,
        leg_id: &LegId,
        request_headers: &[rsipstack::sip::Header],
        override_music: Option<crate::call::domain::MediaSource>,
    ) -> Result<()> {
        info!(session_id = %self.id, %leg_id, "Propagating hold");

        self.update_leg_state(leg_id, LegState::Hold);

        let music = override_music.or_else(|| self.resolve_hold_music(request_headers));
        let session_id = self.id.clone();

        if let Some(peer) = self.media_leg(leg_id) {
            if self.media_side_for_leg(peer.id()).is_some() {
                if let Some(mb) = self.bridge_mut() {
                    mb.unbridge().await?;
                }
            }
            peer.pause_rtp_timeout();
            if let Some(music) = music {
                let path = match &music {
                    crate::call::domain::MediaSource::File { path } => path.clone(),
                    crate::call::domain::MediaSource::Url { url } => url.clone(),
                    _ => {
                        warn!(session_id = %session_id, "Unsupported hold music source type");
                        peer.set_egress_source(crate::media::egress::EgressSource::Silence)
                            .await?;
                        return Ok(());
                    }
                };
                let resolved = if path.starts_with("http://") || path.starts_with("https://") {
                    path
                } else {
                    Self::resolve_audio_file_path(&path)
                };
                match crate::media::audio_source::FileAudioSource::new(resolved.clone(), true).await
                {
                    Ok(audio) => {
                        peer.play_media(Box::new(audio), true).await?;
                        self.record_play_start(
                            format!("hold-music-{}", leg_id),
                            format!("hold music ({})", leg_id),
                        );
                    }
                    Err(e) => {
                        warn!(session_id = %session_id, %leg_id, path = %resolved, error = %e,
                            "Hold music failed to load, falling back to silence");
                        peer.set_egress_source(crate::media::egress::EgressSource::Silence)
                            .await?;
                    }
                }
            } else {
                peer.set_egress_source(crate::media::egress::EgressSource::Silence)
                    .await?;
            }
        } else {
            let hold_sdp = self.generate_sdp_for_side(leg_id, true)?;
            if leg_id.as_str() == "callee" && self.legs.get_dialog(leg_id).is_none() {
                if let Some(response_sdp) = self.send_reinvite_to_callee_dialogs(&hold_sdp).await? {
                    self.media.callee_answer_sdp = Some(response_sdp);
                }
            } else if let Err(e) = self.send_reinvite_to_leg(leg_id, hold_sdp).await {
                warn!(session_id = %self.context.session_id, %leg_id, error = %e, "Failed to send hold re-INVITE to peer");
            }
        }
        // Keep both parties' live mic audio in the recording while held.
        self.apply_hold_recording_routes();
        Ok(())
    }

    pub(crate) async fn propagate_unhold_to_leg(&mut self, leg_id: &LegId) -> Result<()> {
        info!(session_id = %self.id, %leg_id, "Propagating unhold");
        self.update_leg_state(leg_id, LegState::Connected);
        if let Some(peer) = self.media_leg(leg_id) {
            // Restoring the route replaces hold playback with live media.
            // A separate stop could silence a relay that is already active.
            peer.resume_rtp_timeout();
            // Same logical-pair recovery as handle_unhold: direct-dialed
            // calls never populate the logical bridge, and an app-exit
            // unhold is an explicit resume request.
            if self.bridge.legs.len() != 2
                && let Some(mb) = self.media.bridge.as_ref()
            {
                let pair: Vec<LegId> = [mb.leg(LegSide::A), mb.leg(LegSide::B)]
                    .into_iter()
                    .flatten()
                    .map(|leg| leg.id().clone())
                    .collect();
                if pair.len() == 2 {
                    info!(
                        session_id = %self.id,
                        leg_a = %pair[0], leg_b = %pair[1],
                        "Propagated unhold: logical bridge pair missing — recovered from media bridge"
                    );
                    self.bridge = BridgeConfig::bridge(pair[0].clone(), pair[1].clone());
                }
            }
            self.update_media_path().await;
        } else {
            let unhold_sdp = self.generate_sdp_for_side(leg_id, false)?;
            if leg_id.as_str() == "callee" && self.legs.get_dialog(leg_id).is_none() {
                if let Some(response_sdp) =
                    self.send_reinvite_to_callee_dialogs(&unhold_sdp).await?
                {
                    self.media.callee_answer_sdp = Some(response_sdp);
                }
            } else if let Err(e) = self.send_reinvite_to_leg(leg_id, unhold_sdp).await {
                warn!(session_id = %self.context.session_id, %leg_id, error = %e, "Failed to send unhold re-INVITE to peer");
            }
        }
        self.restore_recording_routes();
        Ok(())
    }

    pub(crate) async fn apply_reinvite_hold_transition(
        &mut self,
        side: DialogSide,
        leg_id: &LegId,
        offer: &rustrtc::SessionDescription,
        request_headers: &[rsipstack::sip::Header],
    ) {
        let has_audio = offer
            .media_sections
            .iter()
            .any(|s| s.kind == rustrtc::MediaKind::Audio);
        if !has_audio {
            return;
        }

        let offer_direction = offer
            .media_sections
            .iter()
            .find(|s| s.kind == rustrtc::MediaKind::Audio)
            .map(|s| s.direction);

        let new_state = if Self::is_hold_direction(offer_direction.unwrap_or_default(), Some(offer))
        {
            LegState::Hold
        } else {
            LegState::Connected
        };

        let prev = self.leg_prev_state(&leg_id);
        self.update_leg_state(&leg_id, new_state);
        self.fire_hold_transition_hooks(&leg_id, prev, new_state)
            .await;

        // Only B's inbound consultation re-INVITE means a private-talk switch.
        // Initialization, C's offers and PBX/API media changes are not switches.
        // Once attached to a room (including attended transfer), stop inference.
        if matches!(side, DialogSide::Caller)
            && leg_id.as_str() == "caller"
            && self.meta.answer_time.is_some()
            && self
                .legs
                .get(&self.resolve_transfer_leg())
                .is_some_and(|leg| matches!(leg.state, LegState::Connected | LegState::Hold))
            && self.conference.is_none()
            && !self.meta.transferred
            && !self.meta.transfer_in_progress
            && self.pending_refers.is_empty()
            && matches!(
                (prev, new_state),
                (Some(LegState::Connected), LegState::Hold)
                    | (Some(LegState::Hold), LegState::Connected)
            )
        {
            let consultation = self
                .meta
                .root_session_id
                .as_deref()
                .is_some_and(|root| root != self.context.session_id)
                && self
                    .legs
                    .get(&leg_id)
                    .is_some_and(|leg| leg.agent_id.is_some())
                && self.pinned_agent_id().is_some();
            if consultation {
                self.emit_typed_rwi_event(&crate::rwi::ConsultSwitched {
                    call_id: self.root_session_id_str(),
                    transfer_id: self.context.session_id.clone(),
                    talking_to: if new_state == LegState::Hold {
                        "customer"
                    } else {
                        "consult"
                    }
                    .into(),
                });
            }
        }

        // Cross-leg hold propagation only applies when the proxy anchors media
        // (a MediaBridge is present). In bypass mode the peer already received
        // the relayed offer — this hold UPDATE was forwarded to the callee
        // as-is and answered there — so propagating hold with a separate
        // re-INVITE would push a redundant/conflicting offer to the peer and
        // wait for an answer that may never come, hanging the dialog.
        if self.bypasses_local_media() {
            return;
        }

        // Own-side media restore: when this leg unholds while the bridge is
        // down, the media path must be rebuilt or hold music loops forever.
        if new_state == LegState::Connected
            && self.bridge.legs.len() == 2
            && !self.bridge().is_some_and(|mb| mb.is_bridged())
        {
            self.update_media_path().await;
        }

        let Some(peer_leg_id) = self.bridged_peer_leg_id(leg_id) else {
            return;
        };
        let peer_transition = match (self.leg_prev_state(&peer_leg_id), new_state) {
            (Some(LegState::Hold), LegState::Connected) => Some(false),
            (Some(LegState::Connected), LegState::Hold) => Some(true),
            _ => None,
        };
        if let Some(is_hold) = peer_transition {
            let result = if is_hold {
                self.propagate_hold_to_leg(&peer_leg_id, request_headers, None)
                    .await
            } else {
                self.propagate_unhold_to_leg(&peer_leg_id).await
            };
            if let Err(error) = result {
                warn!(session_id = %self.id, %peer_leg_id, %error, "Failed to propagate hold state to bridged peer");
            }
        }
    }

    /// Resolve hold music source by priority:
    /// 1. X-Hold-Music header in the re-INVITE request
    /// 2. X-Hold-Music in session extensions (from initial INVITE / CC addon,
    ///    e.g. skill-group `metadata.hold_music` injected by the CC hook)
    /// 3. PBX default from ProxyConfig (`[proxy].hold_music`)
    /// 4. Built-in default hold audio (sounds/phone-calling.wav)
    pub(crate) fn resolve_hold_music(
        &self,
        request_headers: &[rsipstack::sip::Header],
    ) -> Option<crate::call::domain::MediaSource> {
        // 1. re-INVITE X-Hold-Music header
        if let Some(val) = request_headers
            .iter()
            .find(|h| h.name().eq_ignore_ascii_case("X-Hold-Music"))
            .map(|h| h.value().to_string())
        {
            return Some(Self::parse_hold_music_value(&val));
        }
        // 2. Session extensions (set by CC addon or from initial INVITE metadata)
        if let Some(val) = self.session_ext_get("X-Hold-Music") {
            return Some(Self::parse_hold_music_value(&val));
        }
        // 3. PBX default
        if let Some(path) = &self.server.proxy_config.load().hold_music {
            return Some(crate::call::domain::MediaSource::File { path: path.clone() });
        }
        // 4. Built-in default so the held party always hears hold audio.
        Some(crate::call::domain::MediaSource::File {
            path: crate::call::DEFAULT_QUEUE_HOLD_AUDIO.to_string(),
        })
    }

    pub(crate) fn parse_hold_music_value(value: &str) -> crate::call::domain::MediaSource {
        let trimmed = value.trim().to_string();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            crate::call::domain::MediaSource::Url { url: trimmed }
        } else {
            crate::call::domain::MediaSource::File { path: trimmed }
        }
    }

    pub(crate) async fn send_reinvite_to_callee_dialogs(&mut self, sdp: &str) -> Result<Option<String>> {
        let dialog_layer = self.server.dialog_layer.clone();
        let headers = Self::sdp_headers();
        let body = sdp.as_bytes().to_vec();

        let callee_ids: Vec<DialogId> = self
            .callee_dialogs
            .iter()
            .filter(|entry| {
                self.leg_id_for_dialog(&entry.key().to_string())
                    .is_some_and(|id| self.is_caller_peer(&id))
            })
            .map(|entry| entry.key().clone())
            .collect();

        let mut answer: Option<String> = None;
        for callee_dialog_id in callee_ids {
            if let Some(mut dialog) = dialog_layer.get_dialog(&callee_dialog_id) {
                let resp = match &mut dialog {
                    rsipstack::dialog::dialog::Dialog::Invite(d) => d
                        .reinvite(Some(headers.clone()), Some(body.clone()))
                        .await
                        .map_err(|e| anyhow!("re-INVITE to callee failed: {}", e))?,
                    _ => continue,
                };
                if let Some(response) = resp
                    && !response.body().is_empty()
                {
                    answer = Some(String::from_utf8_lossy(response.body()).to_string());
                }
            }
        }
        Ok(answer)
    }

    pub async fn handle_reinvite(
        &mut self,
        method: rsipstack::sip::Method,
        sdp: Option<String>,
    ) -> Result<Option<String>> {
        debug!(session_id = %self.id,
            ?method,
            sdp_present = sdp.is_some(),
            "Handling re-INVITE in B2BUA mode"
        );

        if method != rsipstack::sip::Method::Invite {
            return Err(anyhow!("Expected INVITE method, got {:?}", method));
        }

        let offer_sdp = match sdp {
            Some(s) => s,
            None => {
                return Ok(self.media.answer.clone());
            }
        };
        if !self.bypasses_local_media() {
            self.media.caller_offer = Some(offer_sdp.clone());
        }

        let callee_dialogs: Vec<DialogId> = self
            .callee_dialogs
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        if callee_dialogs.is_empty() {
            return Err(anyhow!("No callee dialogs available for B2BUA forwarding"));
        }

        let mut final_answer: Option<String> = None;
        let dialog_layer = self.server.dialog_layer.clone();

        for callee_dialog_id in callee_dialogs {
            if let Some(mut dialog) = dialog_layer.get_dialog(&callee_dialog_id) {
                let body = offer_sdp.clone().into_bytes();
                let headers = Self::sdp_headers();

                let resp: Option<rsipstack::sip::Response> = match &mut dialog {
                    Dialog::Invite(d) => d
                        .reinvite(Some(headers), Some(body))
                        .await
                        .map_err(|e| anyhow!("re-INVITE to callee failed: {}", e))?,
                    _ => continue,
                };

                if let Some(response) = resp
                    && !response.body().is_empty()
                {
                    let answer_sdp = String::from_utf8_lossy(response.body()).to_string();
                    // Keep the per-leg snapshots in sync with the forwarded
                    // re-INVITE so session refreshes reuse the latest
                    // negotiated SDP (same rationale as the bypass relay).
                    self.media.callee_offer = Some(offer_sdp.clone());
                    self.media.callee_answer_sdp = Some(answer_sdp.clone());
                    if self.media_profile.path == MediaPathMode::Anchored
                        || self.media.bridge.is_some()
                    {
                        final_answer = self
                            .prepare_caller_answer_from_callee_sdp(
                                Some(answer_sdp),
                                true,
                                rustrtc::SdpType::Answer,
                            )
                            .await?;
                    } else {
                        final_answer = Some(answer_sdp.clone());
                    }
                }
            }
        }

        if let Some(ref answer_sdp) = final_answer {
            let mut headers = Self::sdp_headers();
            let caller_dialog_id = self.caller_dialog_id();
            if let Some(timer_headers) = self.successful_refresh_response_headers(&caller_dialog_id)
            {
                headers.extend(timer_headers);
            }
            if let Some(dialog) = self.caller_dialog.as_ref() {
                dialog
                    .accept(Some(headers), Some(answer_sdp.clone().into_bytes()))
                    .map_err(|e| anyhow!("Failed to send 200 OK for re-INVITE: {}", e))?;
            }
        }

        Ok(final_answer)
    }

    pub(crate) fn should_fallback_to_reinvite(status: StatusCode) -> bool {
        matches!(
            status,
            StatusCode::MethodNotAllowed | StatusCode::NotImplemented
        )
    }

    /// Fire `on_call_held` / `on_call_unheld` session hooks and emit the core
    /// `call_held` / `call_unheld` events when a leg transitions to/from
    /// [`LegState::Hold`].
    ///
    /// This centralizes hold detection so it works regardless of whether the
    /// transition is caused by an explicit `CallCommand::Hold/Unhold` or by an
    /// inbound re-INVITE carrying `sendonly`/`inactive` (or back to
    /// `sendrecv`). No-op when there is no transition.
    pub(crate) async fn fire_hold_transition_hooks(
        &mut self,
        leg_id: &LegId,
        prev: Option<LegState>,
        new: LegState,
    ) {
        let entered_hold = !matches!(prev, Some(LegState::Hold)) && new == LegState::Hold;
        let left_hold = matches!(prev, Some(LegState::Hold)) && new != LegState::Hold;
        if !entered_hold && !left_hold {
            return;
        }
        if entered_hold {
            self.record_trace(
                crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::Hold,
                    format!("Leg {} placed on hold", leg_id),
                )
                .severity(crate::call_errors::ErrSeverity::Info),
            );
        } else {
            self.record_trace(
                crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::Resume,
                    format!("Leg {} resumed from hold", leg_id),
                )
                .severity(crate::call_errors::ErrSeverity::Info),
            );
        }
        let ctx = self.session_hook_ctx();
        let leg_id_str = leg_id.to_string();
        for hook in self.server.session_hooks.iter() {
            if entered_hold {
                hook.on_call_held(&ctx, &leg_id_str).await;
            } else {
                hook.on_call_unheld(&ctx, &leg_id_str).await;
            }
        }
        // Core hold/unheld events (agent attribution comes from the meta
        // already published at ringing/connected time). Child-leg session:
        // the callee slot (the agent dialog) carries the declared role on
        // hold/unheld; the dispatcher-hop caller leg keeps its positional
        // role (same scoping as answered/hangup).
        if entered_hold {
            self.emit_typed_rwi_event(&crate::rwi::CallHeld {
                leg_role: self.leg_role_of(&leg_id_str),
                call_id: ctx.session_id.clone(),
                leg_id: leg_id_str,
            });
        } else {
            self.emit_typed_rwi_event(&crate::rwi::CallUnheld {
                leg_role: self.leg_role_of(&leg_id_str),
                call_id: ctx.session_id.clone(),
                leg_id: leg_id_str,
            });
        }
    }

    pub(crate) async fn handle_reinvite_command(&mut self, leg_id: LegId, sdp: String) -> Result<()> {
        info!(session_id = %self.id, %leg_id, "Handling re-INVITE command");

        self.require_leg(&leg_id)?;

        self.handle_reinvite(rsipstack::sip::Method::Invite, Some(sdp))
            .await?;

        info!(session_id = %self.id, %leg_id, "Re-INVITE command handled");
        Ok(())
    }

    pub(crate) async fn handle_hold(
        &mut self,
        leg_id: LegId,
        music: Option<crate::call::domain::MediaSource>,
    ) -> Result<()> {
        info!(session_id = %self.id, %leg_id, ?music, "Handling hold with SDP renegotiation");

        self.require_leg(&leg_id)?;

        // Fall back through the standard hold-music chain (X-Hold-Music
        // header/extension -> [proxy].hold_music -> built-in default) so the
        // held party always hears hold audio instead of silence.
        let music = music.or_else(|| self.resolve_hold_music(&[]));

        let hold_sdp = self.generate_sdp_for_side(&leg_id, true)?;

        match self.send_reinvite_to_leg(&leg_id, hold_sdp).await {
            Ok(_) => {
                self.update_leg_state(&leg_id, LegState::Hold);
                info!(session_id = %self.id, %leg_id, "Hold re-INVITE sent successfully");

                if !self.server.session_hooks.is_empty() {
                    let ctx = self.session_hook_ctx();
                    let leg_id_str = leg_id.to_string();
                    for hook in self.server.session_hooks.iter() {
                        hook.on_call_held(&ctx, &leg_id_str).await;
                    }
                }

                // Core hold event — agent attribution (agent_id / agent_name /
                // queue_id) is injected from the RWI call meta published at
                // ringing/connected time (replaces the former `cc_held`).
                self.emit_typed_rwi_event(&crate::rwi::CallHeld {
                    leg_role: self.leg_role_of(leg_id.as_str()),
                    call_id: self.context.session_id.clone(),
                    leg_id: leg_id.to_string(),
                });

                // Switch the held leg's media egress to hold music (looping) or
                // CNG/silence. Mirrors propagate_hold_to_callee / _to_caller: without
                // this the leg stays on EgressSource::RewriteRelay, which parks the
                // ptime-paced egress loop and emits nothing to the held party.
                self.apply_hold_media(&leg_id, music).await?;
                Ok(())
            }
            Err(e) => {
                warn!(session_id = %self.id, %leg_id, error = %e, "Failed to send hold re-INVITE");
                Err(e)
            }
        }
    }

    /// Media-only hold: switch egress to hold music / CNG without a
    /// re-INVITE (shared by hold and the attended-REFER rollback).
    pub(super) async fn apply_hold_media(
        &mut self,
        leg_id: &LegId,
        music: Option<crate::call::domain::MediaSource>,
    ) -> Result<()> {
        let Some(peer) = self.media_leg(leg_id) else {
            return Ok(());
        };
        if self
            .bridge()
            .is_some_and(|mb| mb.side_for_leg(leg_id).is_some())
        {
            if let Some(mb) = self.bridge_mut() {
                mb.unbridge().await?;
            }
        }
        peer.pause_rtp_timeout();
        let music = music.or_else(|| self.resolve_hold_music(&[]));
        let source = match &music {
            Some(crate::call::domain::MediaSource::File { path })
            | Some(crate::call::domain::MediaSource::Url { url: path }) => {
                let resolved = if path.starts_with("http://") || path.starts_with("https://") {
                    path.clone()
                } else {
                    Self::resolve_audio_file_path(path)
                };
                match crate::media::audio_source::FileAudioSource::new(resolved, true).await {
                    Ok(audio) => crate::media::egress::EgressSource::Media {
                        audio: Box::new(audio),
                        loop_playback: true,
                        on_end: None,
                    },
                    Err(error) => {
                        warn!(session_id = %self.id, %leg_id, %error, "Hold music failed; using silence");
                        crate::media::egress::EgressSource::Silence
                    }
                }
            }
            _ => crate::media::egress::EgressSource::Silence,
        };
        match source {
            crate::media::egress::EgressSource::Media {
                audio,
                loop_playback,
                ..
            } => {
                peer.play_media(audio, loop_playback).await?;
            }
            source => peer.set_egress_source(source).await?,
        }
        // The bridge is down: retarget the recorder so the hold window keeps
        // both parties' live mic audio instead of going silent.
        self.apply_hold_recording_routes();
        Ok(())
    }

    /// While held, record the selected parties' ingress rather than MOH.
    pub(crate) fn apply_hold_recording_routes(&self) {
        let caller_id = LegId::from("caller");
        // The selected pair survives unbridge. Direct-dial calls may only
        // have the media pair; a different logical pair must not reuse it.
        let pair = if self.bridge.legs.len() == 2 {
            self.bridge.legs.clone()
        } else {
            self.bridge()
                .map(|bridge| {
                    [bridge.leg(LegSide::A), bridge.leg(LegSide::B)]
                        .into_iter()
                        .flatten()
                        .map(|leg| leg.id().clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let selected_peer = pair
            .iter()
            .find(|id| **id != caller_id && pair.contains(&caller_id));
        // Disable old B sources before enabling the selected peer.
        for (id, _) in self.legs.iter() {
            if let Some(leg) = self.media_leg(id) {
                let channel = (*id == caller_id)
                    .then_some(crate::media::media_recorder::CaptureChannel::Caller);
                leg.ingress_tap().set_capture_route(channel, None);
            }
        }
        if let Some(peer) = selected_peer.and_then(|id| self.media_leg(id)) {
            peer.ingress_tap().set_capture_route(
                Some(crate::media::media_recorder::CaptureChannel::Callee),
                None,
            );
        }
    }

    pub(crate) async fn handle_unhold(&mut self, leg_id: LegId) -> Result<()> {
        info!(session_id = %self.id, %leg_id, "Handling unhold with SDP renegotiation");

        self.require_leg(&leg_id)?;

        let Some(leg) = self.legs.get(&leg_id) else {
            warn!(session_id = %self.id, %leg_id, "Leg disappeared between require_leg and access");
            return Err(anyhow::anyhow!("Leg not found: {}", leg_id));
        };
        if leg.state != LegState::Hold {
            // Leg state may have drifted from the media config; when the pair
            // exists but the route is down, unhold must still restore it.
            let media_bridge_paired = self
                .media
                .bridge
                .as_ref()
                .is_some_and(|mb| mb.leg(LegSide::A).is_some() && mb.leg(LegSide::B).is_some());
            let held_media_config = self.conference_bridge.conf_id.is_none()
                && !self.bridge().is_some_and(|mb| mb.is_bridged())
                && (self.bridge.legs.len() == 2 || media_bridge_paired);
            if !held_media_config {
                info!(session_id = %self.id, %leg_id, state = ?leg.state, "Leg is not on hold, skipping unhold");
                return Ok(());
            }
            info!(session_id = %self.id, %leg_id, state = ?leg.state,
                "Unhold: leg state drifted from media config — restoring media path without re-INVITE");
            self.update_leg_state(&leg_id, LegState::Connected);
            if self.bridge.legs.len() != 2
                && let Some(mb) = self.media.bridge.as_ref()
            {
                let pair: Vec<LegId> = [mb.leg(LegSide::A), mb.leg(LegSide::B)]
                    .into_iter()
                    .flatten()
                    .map(|leg| leg.id().clone())
                    .collect();
                if pair.len() == 2 {
                    self.bridge = BridgeConfig::bridge(pair[0].clone(), pair[1].clone());
                }
            }
            if self.conference_bridge.conf_id.is_none() {
                self.update_media_path().await;
            }
            if let Some(peer) = self.media_leg(&leg_id) {
                peer.resume_rtp_timeout();
            }
            return Ok(());
        }

        let unhold_sdp = self.generate_sdp_for_side(&leg_id, false)?;
        self.send_reinvite_to_leg(&leg_id, unhold_sdp).await?;

        // Joining Charlie already selected mixer output. Resume direct A/B
        // only when consultation was cancelled and the mixer was left.
        self.update_leg_state(&leg_id, LegState::Connected);
        // Direct-dialed calls select the media pair directly and never
        // populate the logical bridge (see the ResumeMedia recovery).
        // Unhold is an explicit request to resume audio — recover the pair
        // from the live media bridge so the route actually comes back.
        if self.bridge.legs.len() != 2
            && let Some(mb) = self.media.bridge.as_ref()
        {
            let pair: Vec<LegId> = [mb.leg(LegSide::A), mb.leg(LegSide::B)]
                .into_iter()
                .flatten()
                .map(|leg| leg.id().clone())
                .collect();
            if pair.len() == 2 {
                info!(
                    session_id = %self.id,
                    leg_a = %pair[0], leg_b = %pair[1],
                    "Unhold: logical bridge pair missing — recovered from media bridge"
                );
                self.bridge = BridgeConfig::bridge(pair[0].clone(), pair[1].clone());
            }
        }
        if self.conference_bridge.conf_id.is_none() {
            self.update_media_path().await;
        }
        if let Some(peer) = self.media_leg(&leg_id) {
            peer.resume_rtp_timeout();
        }
        self.update_leg_state(&leg_id, LegState::Connected);
        if !self.server.session_hooks.is_empty() {
            let ctx = self.session_hook_ctx();
            let leg_id_str = leg_id.to_string();
            for hook in self.server.session_hooks.iter() {
                hook.on_call_unheld(&ctx, &leg_id_str).await;
            }
        }
        // Core unhold event (replaces the former `cc_unheld`).
        self.emit_typed_rwi_event(&crate::rwi::CallUnheld {
            leg_role: self.leg_role_of(leg_id.as_str()),
            call_id: self.context.session_id.clone(),
            leg_id: leg_id.to_string(),
        });
        self.restore_recording_routes();
        Ok(())
    }

    /// Send a re-INVITE (e.g. hold/unhold SDP) to the dialog of the target leg
    /// ("caller" → the primary caller dialog; "callee" → the callee dialog).
    pub(crate) async fn send_reinvite_to_leg(&mut self, leg_id: &LegId, sdp: String) -> Result<()> {
        let has_dialog = if leg_id.as_str() == "caller" {
            self.caller_dialog.is_some()
        } else {
            matches!(
                self.legs.get_dialog(leg_id),
                Some(rsipstack::dialog::dialog::Dialog::Invite(_))
            )
        };
        if !has_dialog {
            debug!(session_id = %self.id, %leg_id, "No dialog to re-INVITE for leg");
            return Ok(());
        }
        let headers = Self::sdp_headers();
        match self
            .send_mid_dialog_request_to_leg(
                leg_id,
                rsipstack::sip::Method::Invite,
                headers,
                Some(sdp.into_bytes()),
            )
            .await?
        {
            Some(response) => {
                let status = response.status_code.code();
                if StatusCode::from(status).kind() == rsipstack::sip::StatusCodeKind::Successful {
                    info!(session_id = %self.id, status = %status, "re-INVITE accepted");
                    Ok(())
                } else {
                    Err(anyhow!("re-INVITE rejected with status {}", status))
                }
            }
            None => Err(anyhow!("re-INVITE timed out")),
        }
    }
}
