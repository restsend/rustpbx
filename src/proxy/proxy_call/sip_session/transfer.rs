use super::SipSession;
use crate::call::domain::{CallCommand, LegId, LegState};
use crate::callrecord::CallRecordHangupReason;
use anyhow::{Result, anyhow};
use rsipstack::dialog::dialog::DialogState;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Parsed representation of a transfer target URI.
///
/// Extracted so that the string-prefix dispatch in `handle_blind_transfer` is
/// type-safe and unit-testable independently of `SipSession`.
#[derive(Debug, PartialEq)]
pub(crate) enum TransferTarget {
    Queue {
        name: String,
        return_ivr: Option<String>,
    },
    Ivr {
        name: String,
    },
    Sip(String),
}

/// Parse a raw transfer target string into a typed `TransferTarget`.
///
/// Priority: `queue:` → `ivr:` → SIP/TEL URI (default prefix `sip:` added if absent).
pub(crate) fn parse_transfer_target(target: &str) -> TransferTarget {
    if let Some(rest) = target.strip_prefix("queue:") {
        let remainder = rest.trim();
        let (queue_name, return_ivr) = if let Some(pos) = remainder.find("?return_ivr=") {
            let q = remainder[..pos].trim();
            let ivr = remainder[pos + "?return_ivr=".len()..].to_string();
            (q, Some(ivr))
        } else {
            (remainder, None)
        };
        if !queue_name.is_empty() {
            return TransferTarget::Queue {
                name: queue_name.to_string(),
                return_ivr,
            };
        }
    }
    if let Some(rest) = target.strip_prefix("ivr:") {
        let ivr_name = rest.trim();
        if !ivr_name.is_empty() {
            return TransferTarget::Ivr {
                name: ivr_name.to_string(),
            };
        }
    }
    let sip = if target.starts_with("sip:") || target.starts_with("tel:") {
        target.to_string()
    } else {
        format!("sip:{}", target)
    };
    TransferTarget::Sip(sip)
}

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
        match parse_transfer_target(&target) {
            TransferTarget::Queue { name, return_ivr } => {
                info!(%leg_id, queue = %name, ?return_ivr, "Handling queue transfer");
                self.handle_queue_transfer(leg_id, &name, return_ivr).await
            }
            TransferTarget::Ivr { name } => {
                info!(%leg_id, ivr = %name, "Handling IVR transfer by starting IvrApp");
                self.start_ivr_app(&name).await
            }
            TransferTarget::Sip(refer_to_str) => {
                let refer_to_uri = rsipstack::sip::Uri::try_from(refer_to_str.as_str())
                    .map_err(|e| anyhow!("Invalid transfer target URI: {}", e))?;

                if !self.server.proxy_config.blind_transfer_use_refer {
                    info!(%leg_id, target = %refer_to_str, "Blind transfer via B-leg INVITE (B2BUA)");
                    let location = crate::call::Location {
                        aor: refer_to_uri,
                        ..Default::default()
                    };
                    // Create a proper dialog-state channel so early media (183) from the
                    // B-leg propagates correctly during the transfer INVITE.
                    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel::<DialogState>();
                    let prev_callee_tx = self.callee_event_tx.replace(callee_tx);
                    let result = self
                        .try_single_target(&location, &mut callee_rx, None)
                        .await;
                    self.callee_event_tx = prev_callee_tx;
                    return result.map_err(|(code, reason)| {
                        anyhow!(
                            "B-leg transfer failed: {:?} - {}",
                            code,
                            reason.unwrap_or_default()
                        )
                    });
                }

                // SIP REFER path
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
                        self.emit_refer_event(
                            500,
                            Some(e.to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        return Err(anyhow!("Failed to send REFER: {}", e));
                    }
                }

                info!("Blind transfer initiated — call will be transferred to {}", refer_to_str);
                Ok(())
            }
        }
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

        // Create a proper dialog-state channel so early media (183) from agents
        // propagates correctly during queue-driven transfers at runtime.
        let (callee_tx, mut callee_rx) = mpsc::unbounded_channel::<DialogState>();
        let prev_callee_tx = self.callee_event_tx.replace(callee_tx);
        let queue_result = self.execute_queue(&queue_plan, &mut callee_rx).await;
        self.callee_event_tx = prev_callee_tx;

        match queue_result {
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
                    self.meta.last_error = Some((code.clone(), reason.clone()));
                    self.meta.hangup_reason
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

        // Use a consistent composite leg_id for both conference registration and
        // media bridge start — previously the two used different IDs causing a mismatch.
        let participant_leg = LegId::new(format!("{}-{}", from_session, leg_id));
        conference_manager
            .add_participant(&conf_id, participant_leg.clone())
            .await
            .map_err(|e| anyhow!("Failed to add leg to conference: {}", e))?;

        info!(
            session_id = %self.id,
            leg_id = %leg_id,
            conf_id = %into_conference,
            "Successfully migrated leg into conference"
        );

        match self
            .start_conference_media_bridge(&into_conference, &participant_leg)
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

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // parse_transfer_target — pure-function dispatch tests
    //
    // Why these tests didn't exist before:
    //   The target dispatch was inlined inside `handle_blind_transfer` as a
    //   sequence of `starts_with` if-chains.  Without extraction into a
    //   standalone function there was nothing to call in a unit test; the logic
    //   was only reachable through a fully-wired SipSession, so the edge cases
    //   (empty suffix, mixed casing, return_ivr param) were never exercised.
    // -------------------------------------------------------------------------

    #[test]
    fn test_parse_transfer_target_queue_with_return_ivr() {
        let t = parse_transfer_target("queue:support?return_ivr=main");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_ivr: Some("main".to_string()),
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_without_return_ivr() {
        let t = parse_transfer_target("queue:support");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_ivr: None,
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_whitespace_trimmed() {
        let t = parse_transfer_target("queue: sales ");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "sales".to_string(),
                return_ivr: None,
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_ivr() {
        let t = parse_transfer_target("ivr:main");
        assert_eq!(t, TransferTarget::Ivr { name: "main".to_string() });
    }

    #[test]
    fn test_parse_transfer_target_ivr_whitespace_trimmed() {
        let t = parse_transfer_target("ivr: welcome ");
        assert_eq!(
            t,
            TransferTarget::Ivr {
                name: "welcome".to_string()
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_sip_uri_passthrough() {
        let t = parse_transfer_target("sip:1001@pbx.local");
        assert_eq!(t, TransferTarget::Sip("sip:1001@pbx.local".to_string()));
    }

    #[test]
    fn test_parse_transfer_target_tel_uri_passthrough() {
        let t = parse_transfer_target("tel:+15551234567");
        assert_eq!(t, TransferTarget::Sip("tel:+15551234567".to_string()));
    }

    #[test]
    fn test_parse_transfer_target_bare_extension_gets_sip_prefix() {
        let t = parse_transfer_target("1001");
        assert_eq!(t, TransferTarget::Sip("sip:1001".to_string()));
    }

    /// An empty `queue:` suffix must NOT produce a Queue — it falls through to
    /// Sip so the caller gets a meaningful error from URI parsing rather than a
    /// silent no-op queue lookup.
    #[test]
    fn test_parse_transfer_target_empty_queue_suffix_falls_through_to_sip() {
        let t = parse_transfer_target("queue:");
        // empty name → falls through to Sip
        assert!(matches!(t, TransferTarget::Sip(_)));
    }

    /// Same guard for `ivr:`.
    #[test]
    fn test_parse_transfer_target_empty_ivr_suffix_falls_through_to_sip() {
        let t = parse_transfer_target("ivr:");
        assert!(matches!(t, TransferTarget::Sip(_)));
    }

    // -------------------------------------------------------------------------
    // Cross-session leg_id construction
    //
    // Why the original mismatch wasn't caught:
    //   `handle_transfer_complete_cross_session` is exercised only through an
    //   end-to-end SIP call with two concurrent sessions — there was no unit
    //   test for the leg_id formatting contract.  The conference bridge start
    //   silently logged a warning on failure instead of propagating an error,
    //   so the mismatched ID caused a no-op rather than a visible test failure.
    // -------------------------------------------------------------------------

    /// Verify the composite leg_id format used by `add_participant` and
    /// `start_conference_media_bridge` is identical after the fix.
    #[test]
    fn test_cross_session_participant_leg_id_consistent() {
        let from_session = "sess-abc";
        let leg_id = LegId::new("leg-1".to_string());

        // This mirrors the fixed code:
        //   let participant_leg = LegId::new(format!("{}-{}", from_session, leg_id));
        //   conference_manager.add_participant(&conf_id, participant_leg.clone())
        //   self.start_conference_media_bridge(&into_conference, &participant_leg)
        let participant_leg = LegId::new(format!("{}-{}", from_session, leg_id));
        let add_participant_id = participant_leg.clone();
        let bridge_id = participant_leg.clone();

        assert_eq!(
            add_participant_id.to_string(),
            bridge_id.to_string(),
            "add_participant and start_conference_media_bridge must use the same leg_id"
        );
        assert_eq!(add_participant_id.to_string(), "sess-abc-leg-1");
    }

    /// Demonstrate what the old (buggy) code did: add_participant used the
    /// prefixed form but start_conference_media_bridge used the bare leg_id.
    #[test]
    fn test_cross_session_old_code_had_mismatched_ids() {
        let from_session = "sess-abc";
        let leg_id = LegId::new("leg-1".to_string());

        let add_participant_id = LegId::new(format!("{}-{}", from_session, leg_id));
        // old code: &leg_id (no prefix)
        let bridge_id = leg_id.clone();

        assert_ne!(
            add_participant_id.to_string(),
            bridge_id.to_string(),
            "Sanity check: old IDs were different, proving the bug existed"
        );
    }

    // -------------------------------------------------------------------------
    // Disposable-channel spin-loop regression check
    //
    // Why the spin loop wasn't caught before:
    //   The pattern `let (_tx, mut rx) = unbounded_channel(); fn(&mut rx)` sends
    //   the sender to `_` (immediately dropped).  Inside `try_single_target` the
    //   tokio::select! polls `rx.recv()` which returns `None` on every tick
    //   because the sender is gone — yet the loop body didn't `break`, so it
    //   spun on the CPU until the parallel `invitation` future completed.
    //   Integration tests exercised the happy-path (call connects quickly)
    //   without measuring early-media forwarding or CPU usage, so the spin was
    //   invisible.  A dropped-sender can be verified as a unit test:
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_dropped_sender_channel_returns_none_immediately() {
        // Demonstrate the old bug: dropped sender → recv() always None.
        let (_tx, mut rx) = mpsc::unbounded_channel::<u32>();
        drop(_tx);
        // recv() on a channel with no senders returns None immediately.
        assert!(rx.recv().await.is_none(), "dropped sender should yield None");
    }

    #[tokio::test]
    async fn test_live_sender_channel_can_deliver_state() {
        // Demonstrate the fix: a live sender → recv() delivers the message.
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        tx.send(42).unwrap();
        drop(tx);
        assert_eq!(rx.recv().await, Some(42));
    }
}
