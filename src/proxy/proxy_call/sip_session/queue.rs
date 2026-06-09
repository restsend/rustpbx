use super::{CalleeError, SipSession, into_callee_err};
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
    ) -> Result<(), CalleeError> {
        use crate::call::DialStrategy;

        self.meta.queue_name = Some(plan.queue_name.clone());

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

        let resolved_agents = if let Some(enricher) = &self.server.queue_location_enricher {
            let caller_headers: Vec<rsipstack::sip::Header> = self
                .server_dialog
                .initial_request()
                .headers
                .iter()
                .cloned()
                .collect();
            enricher
                .enrich(
                    resolved_agents,
                    &crate::proxy::call::QueueEnrichContext {
                        session_id: &self.context.session_id.to_string(),
                        queue_name: &plan.queue_name,
                        caller_headers: &caller_headers,
                    },
                )
                .await
        } else {
            resolved_agents
        };

        let transfer_prompt = plan
            .voice_prompts
            .as_ref()
            .and_then(|prompts| prompts.transfer_prompt.as_deref());

        if resolved_agents.is_empty() {
            warn!("No agents available after resolving queue targets");

            self.play_queue_transfer_prompt_before_bridge(transfer_prompt)
                .await;

            return self.execute_queue_fallback(plan, callee_state_rx).await;
        }

        if plan.accept_immediately {
            info!("Queue: answering call immediately");
            let caller_answer = self.prepare_app_caller_media_bridge().await;
            if let Err(e) = self.accept_call(None, caller_answer, None).await {
                warn!(error = %e, "Failed to answer call in queue");
            }
        }

        self.play_queue_transfer_prompt_before_bridge(transfer_prompt)
            .await;

        let hold_handle = if let Some(ref hold) = plan.hold {
            if let Some(ref audio_file) = hold.audio_file {
                info!(file = %audio_file, "Queue: starting hold music");

                self.prepare_queue_playback_media().await;
                match self
                    .play_audio_file(
                        audio_file,
                        false,
                        Self::QUEUE_HOLD_TRACK_ID,
                        hold.loop_playback,
                    )
                    .await
                {
                    Ok(_) => {
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
                self.execute_queue_fallback(plan, callee_state_rx).await
            }
        }
    }

    pub(super) async fn dial_queue_sequential(
        &mut self,
        agents: &[crate::call::Location],
        _ring_timeout: Option<Duration>,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        let mut last_error = into_callee_err(
            &StatusCode::TemporarilyUnavailable,
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
    ) -> Result<(), CalleeError> {
        if agents.is_empty() {
            return Err(into_callee_err(
                &StatusCode::TemporarilyUnavailable,
                Some("No agents available".to_string()),
            ));
        }

        for agent in agents {
            info!(agent = %agent.aor, "Queue: dialing agent in parallel");
        }

        // Uses the shared parallel forking method on SipSession.
        // When the first agent answers, hold music is stopped and the
        // caller is bridged. All other pending forks are cancelled.
        self.fork_targets_parallel(agents, Some(Self::QUEUE_HOLD_TRACK_ID), callee_state_rx)
            .await
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
        self.prepare_queue_playback_media().await;
        match self
            .play_audio_file(audio_file, true, "queue-transfer-prompt", false)
            .await
        {
            Ok(_) => {
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
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        use crate::call::{FailureAction, QueueFallbackAction, TransferEndpoint};

        let pre_action_audio: Option<String> =
            plan.failure_audio.clone().or_else(|| match &plan.fallback {
                Some(QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
                    audio_file,
                    ..
                })) => Some(audio_file.clone()),
                _ => None,
            });

        let wait_for_failure_audio = matches!(plan.fallback, Some(QueueFallbackAction::Failure(_)));
        if let Some(ref audio_file) = pre_action_audio {
            self.prepare_queue_playback_media().await;
            match self
                .play_audio_file(audio_file, wait_for_failure_audio, "caller", false)
                .await
            {
                Ok(()) => {}
                Err(e) => {
                    warn!(error = %e, "Failed to play queue failure audio");
                }
            }
        }

        match &plan.fallback {
            Some(QueueFallbackAction::Failure(FailureAction::Hangup { code, reason })) => {
                info!(?code, ?reason, "Queue fallback - hangup");
                let s = code.clone().unwrap_or(StatusCode::TemporarilyUnavailable);
                Err((s.code(), s.text().to_string(), reason.clone()))
            }
            Some(QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
                status_code,
                reason,
                ..
            })) => {
                info!("Queue fallback - play then hangup");
                Err((status_code.code(), status_code.text().to_string(), reason.clone()))
            }
            Some(QueueFallbackAction::Failure(FailureAction::Transfer(target))) => {
                info!(target = ?target, "Queue fallback - transfer");

                match target {
                    TransferEndpoint::Uri(uri) => Box::pin(self.handle_blind_transfer(
                        LegId::from("caller"),
                        uri.clone(),
                        callee_state_rx,
                    ))
                    .await
                    .map_err(|e| {
                        into_callee_err(
                            &StatusCode::TemporarilyUnavailable,
                            Some(format!("Transfer failed: {}", e)),
                        )
                    }),
                    TransferEndpoint::Queue(queue_name) => Box::pin(self.handle_queue_transfer(
                        LegId::from("caller"),
                        queue_name,
                        None,
                        Vec::new(),
                        callee_state_rx,
                    ))
                    .await
                    .map_err(|e| {
                        into_callee_err(
                            &StatusCode::TemporarilyUnavailable,
                            Some(format!("Transfer failed: {}", e)),
                        )
                    }),
                    TransferEndpoint::Ivr(ivr_name) => {
                        info!(ivr = %ivr_name, "Queue fallback - transferring to IVR");
                        self.start_ivr_app(ivr_name).await.map_err(|e| {
                            into_callee_err(
                                &StatusCode::ServerInternalError,
                                Some(format!("Failed to start IVR: {}", e)),
                            )
                        })?;
                        Ok(())
                    }
                }
            }
            Some(QueueFallbackAction::Redirect { target }) => {
                info!(target = %target, "Queue fallback - redirecting call");

                Box::pin(self.handle_blind_transfer(
                    LegId::from("caller"),
                    target.to_string(),
                    callee_state_rx,
                ))
                .await
                .map_err(|e| {
                    into_callee_err(
                        &StatusCode::TemporarilyUnavailable,
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
                            Box::pin(self.handle_blind_transfer(
                                LegId::from("caller"),
                                target,
                                callee_state_rx,
                            ))
                            .await
                            .map_err(|e| {
                                into_callee_err(
                                    &StatusCode::TemporarilyUnavailable,
                                    Some(format!("Transfer failed: {}", e)),
                                )
                            })
                        } else {
                            warn!(skill_group = %skill_group_id, "No agents found for this skill group");
                            Err(into_callee_err(
                                &StatusCode::TemporarilyUnavailable,
                                Some(format!(
                                    "No agents available for skill group {}",
                                    skill_group_id
                                )),
                            ))
                        }
                    } else {
                        warn!("No agent registry available for skill group resolution");
                        Err(into_callee_err(
                            &StatusCode::TemporarilyUnavailable,
                            Some("Agent registry not available".to_string()),
                        ))
                    }
                } else {
                    info!(queue = %name, "Queue fallback - transfer to another queue");
                    match Box::pin(self.handle_queue_transfer(
                        LegId::from("caller"),
                        name,
                        None,
                        Vec::new(),
                        callee_state_rx,
                    ))
                    .await
                    {
                        Ok(_) => {
                            info!(queue = %name, "Queue fallback - re-enqueue succeeded");
                            Ok(())
                        }
                        Err(e) => {
                            warn!(queue = %name, error = %e, "Queue fallback - re-enqueue operation failed");
                            Err(into_callee_err(
                                &StatusCode::TemporarilyUnavailable,
                                Some(format!("Re-enqueue failed: {}", e)),
                            ))
                        }
                    }
                }
            }
            None => {
                info!("Queue fallback - default hangup with busy tone");
                Err(into_callee_err(
                    &StatusCode::BusyHere,
                    Some("All agents unavailable".to_string()),
                ))
            }
        }
    }

    pub(super) async fn prepare_queue_playback_media(&mut self) {
        if self.server_dialog.state().is_confirmed() {
            if !self.media.caller_answer_uses_media_bridge {
                warn!("Queue playback: caller leg is already answered without media bridge");
            }
            return;
        }

        let caller_answer = self.prepare_app_caller_media_bridge().await;
        if let Err(error) = self.accept_call(None, caller_answer, None).await {
            warn!(
                error = %error,
                "Queue playback: failed to prepare caller media before audio"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::proxy::call::{QueueEnrichContext, QueueLocationEnricher};

    fn make_loc(aor: &str) -> crate::call::Location {
        crate::call::Location {
            aor: aor.parse().unwrap(),
            ..Default::default()
        }
    }

    fn header_val<'a>(loc: &'a crate::call::Location, name: &str) -> Option<&'a str> {
        loc.headers.as_ref()?.iter().find_map(|h| {
            if let rsipstack::sip::Header::Other(n, v) = h {
                if n.eq_ignore_ascii_case(name) {
                    return Some(v.as_str());
                }
            }
            None
        })
    }

    /// Minimal enricher that prepends a single `X-Test` header to every location.
    struct TagEnricher(String);

    #[async_trait::async_trait]
    impl QueueLocationEnricher for TagEnricher {
        async fn enrich(
            &self,
            mut locs: Vec<crate::call::Location>,
            _ctx: &QueueEnrichContext<'_>,
        ) -> Vec<crate::call::Location> {
            for loc in &mut locs {
                let hdrs = loc.headers.get_or_insert_with(Vec::new);
                hdrs.push(rsipstack::sip::Header::Other(
                    "X-Test".to_string(),
                    self.0.clone(),
                ));
            }
            locs
        }
    }

    #[tokio::test]
    async fn test_enricher_trait_basic() {
        let enricher = TagEnricher("hello".into());
        let locs = vec![make_loc("sip:a@pbx"), make_loc("sip:b@pbx")];
        let ctx = QueueEnrichContext {
            session_id: "s1",
            queue_name: "q1",
            caller_headers: &[],
        };
        let result = enricher.enrich(locs, &ctx).await;
        assert_eq!(result.len(), 2);
        assert_eq!(header_val(&result[0], "X-Test"), Some("hello"));
        assert_eq!(header_val(&result[1], "X-Test"), Some("hello"));
    }

    #[tokio::test]
    async fn test_enricher_context_fields_available() {
        struct CtxEchoEnricher;

        #[async_trait::async_trait]
        impl QueueLocationEnricher for CtxEchoEnricher {
            async fn enrich(
                &self,
                mut locs: Vec<crate::call::Location>,
                ctx: &QueueEnrichContext<'_>,
            ) -> Vec<crate::call::Location> {
                for loc in &mut locs {
                    let hdrs = loc.headers.get_or_insert_with(Vec::new);
                    hdrs.push(rsipstack::sip::Header::Other(
                        "X-Sid".into(),
                        ctx.session_id.to_string(),
                    ));
                    hdrs.push(rsipstack::sip::Header::Other(
                        "X-Queue".into(),
                        ctx.queue_name.to_string(),
                    ));
                }
                locs
            }
        }

        let enricher = CtxEchoEnricher;
        let ctx = QueueEnrichContext {
            session_id: "session-abc",
            queue_name: "my-queue",
            caller_headers: &[],
        };
        let result = enricher.enrich(vec![make_loc("sip:x@pbx")], &ctx).await;
        assert_eq!(header_val(&result[0], "X-Sid"), Some("session-abc"));
        assert_eq!(header_val(&result[0], "X-Queue"), Some("my-queue"));
    }

    #[tokio::test]
    async fn test_caller_headers_accessible() {
        struct CrmForwarder;

        #[async_trait::async_trait]
        impl QueueLocationEnricher for CrmForwarder {
            async fn enrich(
                &self,
                mut locs: Vec<crate::call::Location>,
                ctx: &QueueEnrichContext<'_>,
            ) -> Vec<crate::call::Location> {
                let crm: Vec<_> = ctx
                    .caller_headers
                    .iter()
                    .filter(|h| matches!(h, rsipstack::sip::Header::Other(n, _) if n.starts_with("X-CRM-")))
                    .cloned()
                    .collect();
                for loc in &mut locs {
                    loc.headers
                        .get_or_insert_with(Vec::new)
                        .extend(crm.iter().cloned());
                }
                locs
            }
        }

        let caller_headers = vec![rsipstack::sip::Header::Other(
            "X-CRM-Order".into(),
            "ORD-99".into(),
        )];
        let ctx = QueueEnrichContext {
            session_id: "s2",
            queue_name: "sales",
            caller_headers: &caller_headers,
        };
        let result = CrmForwarder.enrich(vec![make_loc("sip:c@pbx")], &ctx).await;
        assert_eq!(header_val(&result[0], "X-CRM-Order"), Some("ORD-99"));
    }
}
