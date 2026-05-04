use super::SipSession;
use crate::call::domain::{CallCommand, LegId, LegState};
use crate::callrecord::CallRecordHangupReason;
use anyhow::{Result, anyhow};
use tracing::{info, warn};

impl SipSession {
    pub(super) async fn handle_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
        attended: bool,
    ) -> Result<()> {
        info!(%leg_id, %target, %attended, "Handling transfer");

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        let leg = self.legs.get(&leg_id).unwrap();
        if !matches!(leg.state, LegState::Connected | LegState::Hold) {
            return Err(anyhow!(
                "Cannot transfer leg {}: invalid state {:?}",
                leg_id,
                leg.state
            ));
        }

        if attended {
            if !target.is_empty() {
                self.handle_replace_transfer(leg_id, target).await?;
            } else {
                self.update_leg_state(&leg_id, LegState::Hold);

                info!(
                    "Attended transfer initiated - consultation call should be created externally"
                );

                if let Some(ref reporter) = self.reporter {
                    let _ = reporter;
                }
            }
        } else {
            self.handle_blind_transfer(leg_id, target).await?;
        }

        Ok(())
    }

    pub(super) async fn handle_blind_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
    ) -> Result<()> {
        if target.starts_with("queue:") {
            let remainder = target.strip_prefix("queue:").unwrap_or(&target).trim();
            let (queue_name, return_ivr) = if let Some(pos) = remainder.find("?return_ivr=") {
                let q = remainder[..pos].trim();
                let ivr = remainder[pos + "?return_ivr=".len()..].to_string();
                (q, Some(ivr))
            } else {
                (remainder, None)
            };
            if !queue_name.is_empty() {
                info!(
                    %leg_id,
                    queue = %queue_name,
                    ?return_ivr,
                    "Handling queue transfer"
                );
                return self
                    .handle_queue_transfer(leg_id, queue_name, return_ivr)
                    .await;
            }
        }

        if target.starts_with("ivr:") {
            let ivr_name = target.strip_prefix("ivr:").unwrap_or(&target).trim();
            if !ivr_name.is_empty() {
                info!(%leg_id, ivr = %ivr_name, "Handling ivr transfer by starting IvrApp");
                return self.start_ivr_app(ivr_name).await;
            }
        }

        let refer_to_str = if target.starts_with("sip:") || target.starts_with("tel:") {
            target.clone()
        } else {
            format!("sip:{}", target)
        };
        let refer_to_uri = rsipstack::sip::Uri::try_from(refer_to_str.as_str())
            .map_err(|e| anyhow!("Invalid transfer target URI: {}", e))?;

        if !self.server.proxy_config.blind_transfer_use_refer {
            info!(%leg_id, target = %refer_to_str, "Blind transfer via B-leg INVITE (B2BUA)");
            let location = crate::call::Location {
                aor: refer_to_uri,
                ..Default::default()
            };
            let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            return self
                .try_single_target(&location, &mut rx, None)
                .await
                .map_err(|(code, reason)| {
                    anyhow!(
                        "B-leg transfer failed: {:?} - {}",
                        code,
                        reason.unwrap_or_default()
                    )
                });
        }

        let referred_by = self
            .context
            .dialplan
            .caller_contact
            .clone()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "sip:rustpbx@localhost".to_string());
        let headers = vec![rsipstack::sip::Header::Other(
            "Referred-By".to_string(),
            format!("<{}>", referred_by),
        )];

        info!(%leg_id, target = %refer_to_str, "Sending REFER for blind transfer");

        match self
            .server_dialog
            .refer(refer_to_uri, Some(headers), None)
            .await
        {
            Ok(Some(response)) => {
                let status = response.status_code.code();
                info!(status = %status, "REFER response received");

                match status {
                    202 => {
                        info!("REFER accepted (202), transfer in progress");
                        self.update_leg_state(&leg_id, LegState::Ending);

                        self.emit_transfer_event(&leg_id, "accepted", None, None)
                            .await;
                        self.emit_refer_event(
                            status,
                            None,
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                    }
                    100..=199 => {
                        info!("REFER received provisional response {}", status);
                        self.emit_refer_event(
                            status,
                            None,
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                    }
                    405 | 420 | 501 => {
                        warn!(status = %status, "REFER not supported by peer, needs 3PCC fallback");
                        self.emit_transfer_event(
                            &leg_id,
                            "failed",
                            Some(status),
                            Some("refer_not_supported"),
                        )
                        .await;
                        self.emit_refer_event(
                            status,
                            Some("refer_not_supported".to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        return Err(anyhow!(
                            "REFER not supported by peer ({}), needs 3PCC fallback",
                            status
                        ));
                    }
                    _ if status >= 400 => {
                        warn!(status = %status, "REFER rejected");
                        self.emit_transfer_event(
                            &leg_id,
                            "failed",
                            Some(status),
                            Some("refer_rejected"),
                        )
                        .await;
                        self.emit_refer_event(
                            status,
                            Some("refer_rejected".to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        return Err(anyhow!("REFER rejected with status {}", status));
                    }
                    _ => {
                        warn!(status = %status, "Unexpected REFER response");
                        self.emit_transfer_event(
                            &leg_id,
                            "failed",
                            Some(status),
                            Some("unexpected_response"),
                        )
                        .await;
                        self.emit_refer_event(
                            status,
                            Some("unexpected_response".to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        return Err(anyhow!("Unexpected REFER response: {}", status));
                    }
                }
            }
            Ok(None) => {
                warn!("REFER timed out, no response received");
                self.emit_transfer_event(&leg_id, "failed", None, Some("timeout"))
                    .await;
                self.emit_refer_event(
                    408,
                    Some("timeout".to_string()),
                    crate::call::domain::ReferNotifyEventType::ReferResponse,
                )
                .await;
                return Err(anyhow!("REFER timed out"));
            }
            Err(e) => {
                warn!(error = %e, "Failed to send REFER");
                self.emit_transfer_event(&leg_id, "failed", None, Some(&e.to_string()))
                    .await;
                self.emit_refer_event(
                    500,
                    Some(e.to_string()),
                    crate::call::domain::ReferNotifyEventType::ReferResponse,
                )
                .await;
                return Err(anyhow!("Failed to send REFER: {}", e));
            }
        }

        info!(
            "Blind transfer initiated - call will be transferred to {}",
            target
        );

        Ok(())
    }

    pub(crate) async fn handle_queue_transfer(
        &mut self,
        leg_id: LegId,
        queue_name: &str,
        return_ivr: Option<String>,
    ) -> Result<()> {
        info!(%leg_id, queue = %queue_name, ?return_ivr, "Starting queue transfer");

        let queue_config = self
            .server
            .data_context
            .resolve_queue_config(queue_name)
            .map_err(|e| anyhow!("Failed to resolve queue config: {}", e))?;

        let queue_config = match queue_config {
            Some(config) => config,
            None => {
                return Err(anyhow!("Queue '{}' not found", queue_name));
            }
        };

        let mut queue_plan = queue_config
            .to_queue_plan()
            .map_err(|e| anyhow!("Invalid queue config: {}", e))?;

        let return_ivr_fallback_audio: Option<String> = if return_ivr.is_some() {
            queue_plan.failure_audio.clone().or_else(|| {
                if let Some(crate::call::QueueFallbackAction::Failure(
                    crate::call::FailureAction::PlayThenHangup { ref audio_file, .. },
                )) = queue_plan.fallback
                {
                    Some(audio_file.clone())
                } else {
                    None
                }
            })
        } else {
            None
        };
        if let Some(ref ivr_name) = return_ivr {
            info!(
                queue = %queue_name,
                ivr = %ivr_name,
                "Queue transfer: will return to IVR on fallback"
            );
            let name = ivr_name.clone();
            queue_plan.fallback = Some(crate::call::QueueFallbackAction::Failure(
                crate::call::FailureAction::Transfer(crate::call::TransferEndpoint::Ivr(name)),
            ));
            queue_plan.failure_audio = return_ivr_fallback_audio;
        }

        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        match self.execute_queue(&queue_plan, &mut rx).await {
            Ok(()) => {
                info!(queue = %queue_name, "Queue transfer completed successfully");
                Ok(())
            }
            Err((code, reason)) => {
                warn!(
                    queue = %queue_name,
                    ?code,
                    ?reason,
                    "Queue transfer failed"
                );
                if self.server_dialog.state().is_confirmed() {
                    self.last_error = Some((code.clone(), reason.clone()));
                    self.hangup_reason
                        .get_or_insert(CallRecordHangupReason::Failed);
                    self.pending_hangup.insert(self.server_dialog.id());
                    self.cancel_token.cancel();
                    info!(
                        queue = %queue_name,
                        ?code,
                        ?reason,
                        "Queue transfer failed after caller was answered; hanging up caller dialog"
                    );
                    return Ok(());
                }
                Err(anyhow!("Queue transfer failed: {:?} - {:?}", code, reason))
            }
        }
    }

    pub(crate) async fn start_ivr_app(&self, ivr_name: &str) -> Result<()> {
        use crate::call::runtime::AppRuntimeError;

        let ivr_file = format!("config/ivr/{}.toml", ivr_name);
        info!(ivr = %ivr_name, file = %ivr_file, "Starting IVR application");

        let params = Some(serde_json::json!({"file": ivr_file}));
        match self
            .app_runtime
            .start_app("ivr", params.clone(), true)
            .await
        {
            Ok(()) => {}
            Err(AppRuntimeError::AlreadyRunning(_)) => {
                warn!(
                    ivr = %ivr_name,
                    "IVR runtime still marked running, restarting app"
                );

                match self
                    .app_runtime
                    .stop_app(Some("restart ivr for queue fallback".to_string()))
                    .await
                {
                    Ok(()) | Err(AppRuntimeError::NotRunning) => {}
                    Err(stop_err) => {
                        warn!(
                            ivr = %ivr_name,
                            error = ?stop_err,
                            "Failed to stop existing app before IVR restart"
                        );
                    }
                }

                self.app_runtime
                    .start_app("ivr", params, true)
                    .await
                    .map_err(|e| anyhow!("Failed to restart IVR '{}': {:?}", ivr_name, e))?;
            }
            Err(e) => {
                return Err(anyhow!("Failed to start IVR '{}': {:?}", ivr_name, e));
            }
        }

        Ok(())
    }

    pub(super) fn build_replaces_header(&self) -> Option<String> {
        let dialog_id = self.server_dialog.id();

        let call_id = &dialog_id.call_id;
        let local_tag = &dialog_id.local_tag;
        let remote_tag = &dialog_id.remote_tag;

        if remote_tag.is_empty() {
            return None;
        }

        Some(format!(
            "{};to-tag={};from-tag={}",
            call_id, local_tag, remote_tag
        ))
    }

    pub(super) async fn handle_replace_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
    ) -> Result<()> {
        let replaces = self
            .build_replaces_header()
            .ok_or_else(|| anyhow!("Cannot build Replaces header for current dialog"))?;
        let encoded_replaces = urlencoding::encode(&replaces).into_owned();

        let refer_target = if target.contains('?') {
            format!("{}&Replaces={}", target, encoded_replaces)
        } else {
            format!("{}?Replaces={}", target, encoded_replaces)
        };

        self.handle_blind_transfer(leg_id, refer_target).await
    }

    pub(super) async fn emit_transfer_event(
        &self,
        leg_id: &LegId,
        event_type: &str,
        sip_status: Option<u16>,
        reason: Option<&str>,
    ) {
        let event_data = serde_json::json!({
            "session_id": self.id.0,
            "leg_id": leg_id.to_string(),
            "event": event_type,
            "sip_status": sip_status,
            "reason": reason,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        info!(?event_data, "Transfer event emitted");
    }

    pub(super) async fn emit_refer_event(
        &self,
        sip_status: u16,
        reason: Option<String>,
        event_type: crate::call::domain::ReferNotifyEventType,
    ) {
        let event = crate::call::domain::ReferNotifyEvent {
            call_id: self.id.0.clone(),
            sip_status,
            reason,
            event_type,
        };
        let subscribers = self.server.transfer_notify_subscribers.lock().await;
        for tx in subscribers.iter() {
            let _ = tx.send(event.clone());
        }
    }

    pub(super) async fn handle_transfer_complete(&mut self, consult_leg: LegId) -> Result<()> {
        info!(%consult_leg, "Completing attended transfer");

        if !self.legs.contains_key(&consult_leg) {
            return Err(anyhow!("Consultation leg not found: {}", consult_leg));
        }

        let original_leg = self
            .legs
            .iter()
            .find(|(_, leg)| leg.state == LegState::Hold)
            .map(|(id, _)| id.clone());

        if let Some(original_leg) = original_leg {
            if self
                .setup_bridge(original_leg.clone(), consult_leg.clone())
                .await
            {
                self.update_leg_state(&original_leg, LegState::Connected);
                self.update_leg_state(&consult_leg, LegState::Connected);
                let _ = self.handle_unhold(original_leg.clone()).await;
                info!("Attended transfer completed successfully");
            } else {
                return Err(anyhow!("Failed to setup bridge for transfer completion"));
            }
        } else {
            return Err(anyhow!("No leg on hold found for transfer completion"));
        }

        Ok(())
    }

    pub(super) async fn handle_transfer_cancel(&mut self, consult_leg: LegId) -> Result<()> {
        info!(%consult_leg, "Canceling attended transfer");

        if !self.legs.contains_key(&consult_leg) {
            return Err(anyhow!("Consultation leg not found: {}", consult_leg));
        }

        self.update_leg_state(&consult_leg, LegState::Ending);

        let original_leg = self
            .legs
            .iter()
            .find(|(_, leg)| leg.state == LegState::Hold)
            .map(|(id, _)| id.clone());

        if let Some(original_leg) = original_leg {
            self.update_leg_state(&original_leg, LegState::Connected);
            let _ = self.handle_unhold(original_leg.clone()).await;
            info!("Attended transfer canceled, original call resumed");
        }

        Ok(())
    }

    pub(super) async fn handle_transfer_complete_cross_session(
        &mut self,
        from_session: String,
        leg_id: LegId,
        into_conference: String,
    ) -> Result<()> {
        info!(
            from_session = %from_session,
            leg_id = %leg_id,
            into_conference = %into_conference,
            "Handling cross-session transfer completion"
        );

        if self.id.to_string() != from_session {
            let registry = &self.server.active_call_registry;
            if let Some(handle) = registry.get_handle(&from_session) {
                let from_session_clone = from_session.clone();
                handle
                    .send_command(CallCommand::TransferCompleteCrossSession {
                        from_session,
                        leg_id,
                        into_conference,
                    })
                    .map_err(|e| anyhow!("Failed to forward cross-session transfer: {}", e))?;
                info!(
                    "Forwarded cross-session transfer command to session {}",
                    from_session_clone
                );
                return Ok(());
            } else {
                return Err(anyhow!(
                    "from_session {} not found in registry",
                    from_session
                ));
            }
        }

        let leg = self
            .legs
            .get(&leg_id)
            .ok_or_else(|| anyhow!("Leg {} not found in session {}", leg_id, from_session))?;

        info!(
            session_id = %self.id,
            leg_id = %leg_id,
            leg_state = ?leg.state,
            "Found leg for cross-session migration"
        );

        let conference_manager = &self.server.conference_manager;
        let conf_id = crate::call::runtime::ConferenceId::from(into_conference.as_str());

        conference_manager
            .add_participant(&conf_id, LegId::new(format!("{}-{}", from_session, leg_id)))
            .await
            .map_err(|e| anyhow!("Failed to add leg to conference: {}", e))?;

        info!(
            session_id = %self.id,
            leg_id = %leg_id,
            conf_id = %into_conference,
            "Successfully migrated leg into conference"
        );

        match self
            .start_conference_media_bridge(&into_conference, &leg_id)
            .await
        {
            Ok(handle) => {
                info!(
                    session_id = %self.id,
                    leg_id = %leg_id,
                    "Conference media bridge started"
                );
                self.conference_bridge.bridge_handle = Some(handle);
                self.conference_bridge.conf_id = Some(into_conference);
            }
            Err(e) => {
                warn!(
                    session_id = %self.id,
                    leg_id = %leg_id,
                    error = %e,
                    "Failed to start conference media bridge"
                );
            }
        }

        self.update_leg_state(&leg_id, LegState::Hold);

        Ok(())
    }

    pub(super) async fn handle_bridge_cross_session(
        &mut self,
        session_a: String,
        leg_a: LegId,
        session_b: String,
        leg_b: LegId,
    ) -> Result<()> {
        let current_session = self.id.to_string();

        info!(
            current_session = %current_session,
            session_a = %session_a,
            session_b = %session_b,
            "Handling cross-session P2P bridge"
        );

        let conf_id = if session_a < session_b {
            format!("p2p-bridge-{}-{}", session_a, session_b)
        } else {
            format!("p2p-bridge-{}-{}", session_b, session_a)
        };
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());

        let (my_session, my_leg, other_session, _other_leg) = if current_session == session_a {
            (
                session_a.clone(),
                leg_a.clone(),
                session_b.clone(),
                leg_b.clone(),
            )
        } else if current_session == session_b {
            (
                session_b.clone(),
                leg_b.clone(),
                session_a.clone(),
                leg_a.clone(),
            )
        } else {
            let registry = &self.server.active_call_registry;
            if let Some(handle) = registry.get_handle(&session_a) {
                let session_a_clone = session_a.clone();
                handle
                    .send_command(CallCommand::BridgeCrossSession {
                        session_a,
                        leg_a: leg_a.clone(),
                        session_b,
                        leg_b: leg_b.clone(),
                    })
                    .map_err(|e| anyhow!("Failed to forward BridgeCrossSession: {}", e))?;
                info!(
                    "Forwarded BridgeCrossSession to session_a {}",
                    session_a_clone
                );
            }
            return Ok(());
        };

        if self
            .server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            info!(conf_id = %conf_id, "Creating P2P bridge conference");
            self.server
                .conference_manager
                .create_conference(conf_id_obj.clone(), None)
                .await
                .map_err(|e| anyhow!("Failed to create P2P conference: {}", e))?;
        }

        let participant_leg = LegId::new(format!("{}-{}", my_session, my_leg));
        match self
            .start_conference_media_bridge(&conf_id, &participant_leg)
            .await
        {
            Ok(handle) => {
                info!(
                    session_id = %current_session,
                    leg_id = %my_leg,
                    "P2P conference media bridge started"
                );
                self.conference_bridge.bridge_handle = Some(handle);
                self.conference_bridge.conf_id = Some(conf_id.clone());
            }
            Err(e) => {
                warn!(
                    session_id = %current_session,
                    leg_id = %my_leg,
                    error = %e,
                    "Failed to start P2P conference media bridge"
                );
            }
        }

        if current_session == session_a {
            let registry = &self.server.active_call_registry;
            if let Some(handle) = registry.get_handle(&other_session) {
                let _ = handle.send_command(CallCommand::BridgeCrossSession {
                    session_a: session_a.clone(),
                    leg_a: leg_a.clone(),
                    session_b: session_b.clone(),
                    leg_b: leg_b.clone(),
                });
                info!(
                    session_a = %session_a,
                    session_b = %session_b,
                    "Notified session_b to join P2P conference"
                );
            }
        }

        Ok(())
    }
}
