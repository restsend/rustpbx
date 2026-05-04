use super::SipSession;
use crate::call::domain::LegId;
use anyhow::Result;
use rsipstack::dialog::dialog::DialogState;
use rsipstack::sip::StatusCode;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

impl SipSession {
    pub(super) async fn execute_queue(
        &mut self,
        plan: &crate::call::QueuePlan,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        use crate::call::DialStrategy;

        self.queue_name = Some(plan.queue_name.clone());

        info!("Executing queue plan");

        let agents = match &plan.dial_strategy {
            Some(DialStrategy::Sequential(locations)) => locations.clone(),
            Some(DialStrategy::Parallel(locations)) => locations.clone(),
            None => {
                warn!("No dial strategy in queue plan");
                return Ok(());
            }
        };

        if agents.is_empty() {
            warn!("No agents configured in queue plan");
            return Ok(());
        }

        let resolved_agents = self
            .resolve_custom_targets(agents, plan.acd_policy.as_deref())
            .await;

        let transfer_prompt = plan
            .voice_prompts
            .as_ref()
            .and_then(|prompts| prompts.transfer_prompt.as_deref());

        if resolved_agents.is_empty() {
            warn!("No agents available after resolving queue targets");

            self.prepare_queue_fallback_audio_media().await;
            self.play_queue_transfer_prompt_before_bridge(transfer_prompt)
                .await;

            return self.execute_queue_fallback(plan).await;
        }

        if plan.accept_immediately {
            info!("Queue: answering call immediately");
            let caller_answer = self.prepare_queue_early_answer(&resolved_agents).await;
            if let Err(e) = self.accept_call(None, caller_answer, None).await {
                warn!(error = %e, "Failed to answer call in queue");
            }
        }

        self.play_queue_transfer_prompt_before_bridge(transfer_prompt)
            .await;

        let hold_handle = if let Some(ref hold) = plan.hold {
            if let Some(ref audio_file) = hold.audio_file {
                info!(file = %audio_file, "Queue: starting hold music");

                match self
                    .play_audio_file(
                        audio_file,
                        false,
                        Self::QUEUE_HOLD_TRACK_ID,
                        hold.loop_playback,
                    )
                    .await
                {
                    Ok(()) => {
                        info!(track_id = %Self::QUEUE_HOLD_TRACK_ID, "Queue: hold music started");
                        true
                    }
                    Err(error) => {
                        warn!(
                            error = %error,
                            track_id = %Self::QUEUE_HOLD_TRACK_ID,
                            "Queue: failed to start hold music"
                        );
                        false
                    }
                }
            } else {
                false
            }
        } else {
            false
        };

        let result = match &plan.dial_strategy {
            Some(DialStrategy::Sequential(_)) => {
                self.dial_queue_sequential(&resolved_agents, plan.ring_timeout, callee_state_rx)
                    .await
            }
            Some(DialStrategy::Parallel(_)) => {
                self.dial_queue_parallel(&resolved_agents, plan.ring_timeout, callee_state_rx)
                    .await
            }
            None => Ok(()),
        };

        if hold_handle {
            info!("Queue: stopping hold music");
            self.stop_playback_track(Self::QUEUE_HOLD_TRACK_ID, false)
                .await;
        }

        if self.cancel_token.is_cancelled() || self.server_dialog.state().is_terminated() {
            info!("Queue: caller ended, stopping queue execution");
            return Ok(());
        }

        match result {
            Ok(()) => {
                info!("Queue: agent connected successfully");
                Ok(())
            }
            Err(e) => {
                warn!(error = ?e, "Queue: all agents failed, executing fallback");
                self.execute_queue_fallback(plan).await
            }
        }
    }

    pub(super) async fn dial_queue_sequential(
        &mut self,
        agents: &[crate::call::Location],
        _ring_timeout: Option<Duration>,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let mut last_error = (
            StatusCode::TemporarilyUnavailable,
            Some("All agents unavailable".to_string()),
        );

        for (idx, agent) in agents.iter().enumerate() {
            if self.cancel_token.is_cancelled() || self.server_dialog.state().is_terminated() {
                info!("Queue: caller ended before next agent");
                return Ok(());
            }

            info!(index = idx, agent = %agent.aor, "Queue: trying agent");

            match self
                .try_single_target(agent, callee_state_rx, Some(Self::QUEUE_HOLD_TRACK_ID))
                .await
            {
                Ok(()) => {
                    info!(index = idx, "Queue: agent connected");
                    return Ok(());
                }
                Err(e) => {
                    warn!(index = idx, error = ?e, "Queue: agent failed");
                    last_error = e;
                }
            }
        }

        Err(last_error)
    }

    pub(super) async fn dial_queue_parallel(
        &mut self,
        agents: &[crate::call::Location],
        _ring_timeout: Option<Duration>,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        if let Some(agent) = agents.first() {
            info!(agent = %agent.aor, "Queue: trying parallel agent");
            self.try_single_target(agent, callee_state_rx, Some(Self::QUEUE_HOLD_TRACK_ID))
                .await
        } else {
            Err((
                StatusCode::TemporarilyUnavailable,
                Some("No agents available".to_string()),
            ))
        }
    }

    pub(super) async fn play_queue_transfer_prompt_before_bridge(
        &mut self,
        transfer_prompt: Option<&str>,
    ) {
        let Some(audio_file) = transfer_prompt
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            debug!("Queue: no transfer prompt configured");
            return;
        };

        info!(file = %audio_file, "Queue: playing transfer prompt before bridging agent audio");
        match self
            .play_audio_file(audio_file, true, "queue-transfer-prompt", false)
            .await
        {
            Ok(()) => {
                let maybe_track = self.playback_tracks.get("queue-transfer-prompt").cloned();
                let cancel = self.cancel_token.clone();
                if let Some(track) = maybe_track {
                    tokio::select! {
                        _ = track.wait_for_completion() => {}
                        _ = cancel.cancelled() => {}
                    }
                }
                self.stop_playback_track("queue-transfer-prompt", false)
                    .await;
                info!(file = %audio_file, "Queue: transfer prompt completed");
            }
            Err(error) => {
                warn!(
                    error = %error,
                    file = %audio_file,
                    "Queue: failed to play transfer prompt before bridging agent audio"
                );
            }
        }
    }

    pub(super) async fn execute_queue_fallback(
        &mut self,
        plan: &crate::call::QueuePlan,
    ) -> Result<(), (StatusCode, Option<String>)> {
        use crate::call::{FailureAction, QueueFallbackAction, TransferEndpoint};

        let pre_action_audio: Option<String> =
            plan.failure_audio.clone().or_else(|| match &plan.fallback {
                Some(QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
                    audio_file,
                    ..
                })) => Some(audio_file.clone()),
                _ => None,
            });

        if let Some(ref audio_file) = pre_action_audio {
            self.prepare_queue_fallback_audio_media().await;
            if let Err(e) = self
                .play_audio_file(audio_file, true, "caller", false)
                .await
            {
                warn!(error = %e, "Failed to play queue failure audio");
            } else {
                let maybe_track = self.playback_tracks.get("caller").cloned();
                let cancel = self.cancel_token.clone();
                if let Some(track) = maybe_track {
                    tokio::select! {
                        _ = track.wait_for_completion() => {}
                        _ = cancel.cancelled() => {}
                    }
                }
            }
        }

        match &plan.fallback {
            Some(QueueFallbackAction::Failure(FailureAction::Hangup { code, reason })) => {
                info!(?code, ?reason, "Queue fallback - hangup");
                Err((
                    code.clone().unwrap_or(StatusCode::TemporarilyUnavailable),
                    reason.clone(),
                ))
            }
            Some(QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
                status_code,
                reason,
                ..
            })) => {
                info!("Queue fallback - play then hangup");
                Err((status_code.clone(), reason.clone()))
            }
            Some(QueueFallbackAction::Failure(FailureAction::Transfer(target))) => {
                info!(target = ?target, "Queue fallback - transfer");

                match target {
                    TransferEndpoint::Uri(uri) => {
                        Box::pin(self.handle_blind_transfer(LegId::from("caller"), uri.clone()))
                            .await
                            .map_err(|e| {
                                (
                                    StatusCode::TemporarilyUnavailable,
                                    Some(format!("Transfer failed: {}", e)),
                                )
                            })
                    }
                    TransferEndpoint::Queue(queue_name) => Box::pin(self.handle_queue_transfer(
                        LegId::from("caller"),
                        queue_name,
                        None,
                    ))
                    .await
                    .map_err(|e| {
                        (
                            StatusCode::TemporarilyUnavailable,
                            Some(format!("Transfer failed: {}", e)),
                        )
                    }),
                    TransferEndpoint::Ivr(ivr_name) => {
                        info!(ivr = %ivr_name, "Queue fallback - transferring to IVR");
                        self.start_ivr_app(ivr_name).await.map_err(|e| {
                            (
                                StatusCode::ServerInternalError,
                                Some(format!("Failed to start IVR: {}", e)),
                            )
                        })?;
                        Ok(())
                    }
                }
            }
            Some(QueueFallbackAction::Redirect { target }) => {
                info!(target = %target, "Queue fallback - redirecting call");

                Box::pin(self.handle_blind_transfer(LegId::from("caller"), target.to_string()))
                    .await
                    .map_err(|e| {
                        (
                            StatusCode::TemporarilyUnavailable,
                            Some(format!("Redirect failed: {}", e)),
                        )
                    })
            }
            Some(QueueFallbackAction::Queue { name }) => {
                if name.starts_with("skill-group:") {
                    let skill_group_id = name.strip_prefix("skill-group:").unwrap_or(name).trim();
                    info!(skill_group = %skill_group_id, "Queue fallback - transfer to skill group");

                    if let Some(registry) = self.server.agent_registry.clone() {
                        let skill_group_uri = format!("skill-group:{}", skill_group_id);
                        let agents = registry.resolve_target(&skill_group_uri).await;
                        if !agents.is_empty() {
                            info!(agents = ?agents, "Resolved skill group to agents");
                            let target = agents[0].clone();
                            Box::pin(self.handle_blind_transfer(LegId::from("caller"), target))
                                .await
                                .map_err(|e| {
                                    (
                                        StatusCode::TemporarilyUnavailable,
                                        Some(format!("Transfer failed: {}", e)),
                                    )
                                })
                        } else {
                            warn!(skill_group = %skill_group_id, "No agents found for this skill group");
                            Err((
                                StatusCode::TemporarilyUnavailable,
                                Some(format!(
                                    "No agents available for skill group {}",
                                    skill_group_id
                                )),
                            ))
                        }
                    } else {
                        warn!("No agent registry available for skill group resolution");
                        Err((
                            StatusCode::TemporarilyUnavailable,
                            Some("Agent registry not available".to_string()),
                        ))
                    }
                } else {
                    info!(queue = %name, "Queue fallback - transfer to another queue");
                    match Box::pin(self.handle_queue_transfer(LegId::from("caller"), name, None))
                        .await
                    {
                        Ok(_) => {
                            info!(queue = %name, "Queue fallback - re-enqueue succeeded");
                            Ok(())
                        }
                        Err(e) => {
                            warn!(queue = %name, error = %e, "Queue fallback - re-enqueue operation failed");
                            Err((
                                StatusCode::TemporarilyUnavailable,
                                Some(format!("Re-enqueue failed: {}", e)),
                            ))
                        }
                    }
                }
            }
            None => {
                info!("Queue fallback - default hangup with busy tone");
                Err((
                    StatusCode::BusyHere,
                    Some("All agents unavailable".to_string()),
                ))
            }
        }
    }

    pub(super) async fn prepare_queue_fallback_audio_media(&mut self) {
        if self.server_dialog.state().is_confirmed() {
            let _ = self.ensure_caller_answer_sdp().await;
            return;
        }

        let caller_answer = self.ensure_caller_answer_sdp().await;
        if let Err(error) = self.accept_call(None, caller_answer, None).await {
            warn!(
                error = %error,
                "Queue fallback: failed to prepare caller media before fallback audio"
            );
        }
    }
}
