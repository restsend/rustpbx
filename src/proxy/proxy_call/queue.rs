use crate::{
    call::{DialplanFlow, Location, QueueFallbackAction, QueuePlan},
    proxy::proxy_call::{
        ActionInbox, FlowFailureHandling, MAX_QUEUE_CHAIN_DEPTH, ProxyCall, session::CallSession,
        state::SessionAction,
    },
};
use anyhow::{Result, anyhow};
use futures::{FutureExt, future::BoxFuture};

use tokio::time::timeout;
use tracing::warn;

impl ProxyCall {
    pub(super) async fn transfer_to_queue(
        &self,
        session: &mut CallSession,
        reference: &str,
    ) -> Result<()> {
        let queue_plan = self.load_queue_plan(reference).await?;
        let strategy = queue_plan
            .dial_strategy()
            .cloned()
            .ok_or_else(|| anyhow!("queue '{}' has no dial targets", reference))?;
        let next_flow = DialplanFlow::Targets(strategy);
        self.handle_session_control(
            session,
            SessionAction::SetQueueName(Some(reference.to_string())),
        )
        .await?;
        self.execute_queue_plan(session, &queue_plan, &next_flow, 0, false, None)
            .await
    }

    async fn load_queue_plan(&self, reference: &str) -> Result<QueuePlan> {
        if reference.trim().is_empty() {
            return Err(anyhow!("queue reference cannot be empty"));
        }
        let label = reference.trim().to_string();
        if let Some(config) = self.server.data_context.resolve_queue_config(reference)? {
            let mut plan = config.to_queue_plan()?;
            if plan.label.is_none() {
                plan.label = Some(label.clone());
            }
            return Ok(plan);
        }
        if let Some(config) = self.server.data_context.load_queue_file(reference)? {
            let mut plan = config.to_queue_plan()?;
            if plan.label.is_none() {
                plan.label = Some(label);
            }
            return Ok(plan);
        }
        Err(anyhow!("queue reference '{}' not found", reference))
    }

    pub(super) async fn execute_queue_plan(
        &self,
        session: &mut CallSession,
        plan: &QueuePlan,
        next: &DialplanFlow,
        depth: usize,
        propagate_failure: bool,
        inbox: ActionInbox<'_>,
    ) -> Result<()> {
        self.process_pending_actions(session, inbox).await?;
        if depth >= MAX_QUEUE_CHAIN_DEPTH {
            warn!(
                session_id = %self.session_id,
                depth,
                "queue chain exceeded maximum depth"
            );
            return self.handle_failure(session).await;
        }
        if let Some(name) = plan.label.clone() {
            self.handle_session_control(session, SessionAction::SetQueueName(Some(name)))
                .await?;
        }
        let previous_passthrough = session.queue_ringback_passthrough();
        let enable_passthrough = plan.accept_immediately && plan.passthrough_ringback();
        self.handle_session_control(
            session,
            SessionAction::SetQueueRingbackPassthrough(enable_passthrough),
        )
        .await?;

        let result = async {
            if plan.accept_immediately {
                self.apply_session_action(
                    session,
                    SessionAction::AcceptCall {
                        callee: None,
                        sdp: None,
                    },
                )
                .await?;
            } else if !session.early_media_sent {
                self
                    .apply_session_action(
                        session,
                        SessionAction::StartRinging {
                            ringback: None,
                            passthrough: false,
                        },
                    )
                    .await?;
            }

            if let Some(hold) = plan.hold.clone() {
                if let Err(e) = self
                    .handle_session_control(session, SessionAction::StartQueueHold(hold))
                    .await
                {
                    warn!(session_id = %self.session_id, error = %e, "Failed to start queue hold track");
                }
            }

            self.process_pending_actions(session, inbox).await?;
            let dial_future = self.execute_flow_with_mode(
                session,
                next,
                inbox,
                FlowFailureHandling::Propagate,
            );
            let dial_result = if let Some(timeout_duration) = plan.ring_timeout {
                match timeout(timeout_duration, dial_future).await {
                    Ok(result) => result,
                    Err(_) => {
                        warn!(
                            session_id = %self.session_id,
                            timeout = ?timeout_duration,
                            "queue dial attempt timed out"
                        );
                        Err(anyhow!("queue dial timed out after {:?}", timeout_duration))
                    }
                }
            } else {
                dial_future.await
            };

            let _ = self
                .handle_session_control(session, SessionAction::StopQueueHold)
                .await;

            match dial_result {
                Ok(_) => Ok(()),
                Err(_) => {
                    if let Some(fallback) = &plan.fallback {
                        self.execute_queue_fallback(
                            session,
                            inbox,
                            fallback,
                            next,
                            depth,
                            propagate_failure,
                        )
                            .await
                    } else if propagate_failure {
                        self.handle_failure(session).await
                    } else {
                        Err(anyhow!("queue plan execution failed"))
                    }
                }
            }
        }
        .await;

        if let Err(err) = self
            .handle_session_control(
                session,
                SessionAction::SetQueueRingbackPassthrough(previous_passthrough),
            )
            .await
        {
            warn!(
                session_id = %self.session_id,
                error = %err,
                "Failed to restore queue ringback passthrough state"
            );
        }

        if let Err(err) = self
            .handle_session_control(session, SessionAction::SetQueueName(None))
            .await
        {
            warn!(
                session_id = %self.session_id,
                error = %err,
                "Failed to clear queue name"
            );
        }
        result
    }

    fn execute_queue_fallback<'a>(
        &'a self,
        session: &'a mut CallSession,
        inbox: ActionInbox<'a>,
        fallback: &'a QueueFallbackAction,
        next: &'a DialplanFlow,
        depth: usize,
        propagate_failure: bool,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match fallback {
                QueueFallbackAction::Failure(action) => {
                    self.execute_failure_action(session, action).await
                }
                QueueFallbackAction::Redirect { target } => {
                    let mut location = Location::default();
                    location.aor = target.clone();
                    match self.try_single_target(session, &location).await {
                        Ok(_) => Ok(()),
                        Err(_) => {
                            if propagate_failure {
                                self.handle_failure(session).await
                            } else {
                                Err(anyhow!("queue fallback redirect target failed to answer"))
                            }
                        }
                    }
                }
                QueueFallbackAction::Queue { name } => {
                    let config = match self.server.data_context.resolve_queue_config(name) {
                        Ok(Some(cfg)) => cfg,
                        Ok(None) => {
                            warn!(
                                session_id = %self.session_id,
                                queue = name,
                                "queue fallback references unknown queue"
                            );
                            return self.handle_failure(session).await;
                        }
                        Err(err) => {
                            warn!(
                                session_id = %self.session_id,
                                queue = name,
                                error = %err,
                                "failed to load queue fallback config"
                            );
                            return self.handle_failure(session).await;
                        }
                    };
                    match config.to_queue_plan() {
                        Ok(mut plan) => {
                            if plan.label.is_none() {
                                plan.label = Some(name.clone());
                            }
                            self.execute_queue_plan(
                                session,
                                &plan,
                                next,
                                depth + 1,
                                propagate_failure,
                                inbox,
                            )
                            .await
                        }
                        Err(err) => {
                            warn!(
                                session_id = %self.session_id,
                                queue = name,
                                error = %err,
                                "failed to build fallback queue plan"
                            );
                            if propagate_failure {
                                self.handle_failure(session).await
                            } else {
                                Err(anyhow!("queue fallback plan failed"))
                            }
                        }
                    }
                }
            }
        }
        .boxed()
    }
}
