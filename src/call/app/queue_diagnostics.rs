//! Typed live queries: the queue actor owns both the evidence and the recovery.
use super::{QueueApp, QueueState};
use crate::call::app::{AppAction, CallController};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueDispatchDiagnostic {
    pub session_id: String,
    pub queue_id: String,
    pub skill_group_id: Option<String>,
    pub state: String,
    pub pending_dials: usize,
    pub ringing_legs: usize,
    pub retry_armed: bool,
    pub wait_secs: u64,
    pub recoverable: bool,
    pub reason: String,
    pub outcome: String,
    pub candidate_count: usize,
    #[serde(default)]
    pub candidate_preview: Vec<String>,
    #[serde(default)]
    pub candidate_order_source: String,
    #[serde(default)]
    pub live_agent_legs: Option<usize>,
    #[serde(default)]
    pub live_transfer_in_progress: Option<bool>,
    #[serde(default)]
    pub live_caller_connected: Option<bool>,
    #[serde(default)]
    pub live_session_observed_at_ms: Option<i64>,
}

const MAX_RECOVERY_REQUESTS: usize = 64;

type DiagnosticReply = Result<QueueDispatchDiagnostic, String>;

#[derive(Debug, Clone)]
pub struct QueueDiagnosticRequest {
    pub recover: bool,
    pub request_id: String,
    pub expected_skill_group: Option<String>,
    pub deadline: tokio::time::Instant,
    reply: Arc<parking_lot::Mutex<Option<oneshot::Sender<DiagnosticReply>>>>,
    pub(crate) expected_app_execution_id: Option<u64>,
    pub(crate) live_agent_legs: Option<usize>,
    pub(crate) live_transfer_in_progress: Option<bool>,
    pub(crate) live_caller_connected: Option<bool>,
    pub(crate) live_session_observed_at_ms: Option<i64>,
}

impl QueueDiagnosticRequest {
    pub fn new(
        recover: bool,
        request_id: String,
        expected_skill_group: Option<String>,
        timeout: Duration,
    ) -> (Self, oneshot::Receiver<DiagnosticReply>) {
        let (reply, response) = oneshot::channel();
        (
            Self {
                recover,
                request_id,
                expected_skill_group,
                deadline: tokio::time::Instant::now() + timeout,
                reply: Arc::new(parking_lot::Mutex::new(Some(reply))),
                expected_app_execution_id: None,
                live_agent_legs: None,
                live_transfer_in_progress: None,
                live_caller_connected: None,
                live_session_observed_at_ms: None,
            },
            response,
        )
    }

    pub fn expired(&self) -> bool {
        tokio::time::Instant::now() >= self.deadline
            || self
                .reply
                .lock()
                .as_ref()
                .is_none_or(|reply| reply.is_closed())
    }

    pub fn respond(&self, result: DiagnosticReply) {
        if let Some(reply) = self.reply.lock().take() {
            let _ = reply.send(result);
        }
    }
}

impl QueueApp {
    fn dispatch_evidence(
        &self,
        ctrl: &CallController,
        request: &QueueDiagnosticRequest,
    ) -> QueueDispatchDiagnostic {
        let retry_armed = ctrl.timeout_is_armed("queue_retry");
        let waiting = matches!(
            self.state,
            QueueState::WaitingForAgent | QueueState::PlayingHold { .. }
        );
        // Only the initial serial kick can be repaired without rewinding a dial round.
        let missing_kick = matches!(self.state, QueueState::DialingAgents { .. })
            && self.current_agent_idx == 0
            && self.dial_attempts == 1
            && !self.is_parallel()
            && !self.get_agents().is_empty();
        let no_leg = self.pending_agents.is_empty() && self.ringing_agent_legs.is_empty();
        let live_verified = request.live_agent_legs == Some(0)
            && request.live_transfer_in_progress == Some(false)
            && request.live_caller_connected == Some(true)
            && request.expected_app_execution_id.is_some();
        let recoverable = live_verified
            && no_leg
            && !self.fallback_executed
            && !self.call_already_connected()
            && (missing_kick || (waiting && !retry_armed));
        let reason = if request.live_transfer_in_progress == Some(true) {
            "transfer_in_progress"
        } else if request.live_agent_legs.is_some_and(|count| count > 0) {
            "active_agent_leg"
        } else if request.live_caller_connected == Some(false) {
            "caller_not_connected"
        } else if !live_verified {
            "live_session_evidence_incomplete"
        } else if self.call_already_connected() {
            "already_connected"
        } else if !no_leg {
            "dial_in_progress"
        } else if self.fallback_executed {
            "fallback_in_progress"
        } else if missing_kick {
            "dispatch_stalled"
        } else if waiting && !retry_armed {
            "retry_missing"
        } else if waiting {
            "healthy_waiting"
        } else {
            "not_recoverable"
        };
        QueueDispatchDiagnostic {
            session_id: self.call_id.clone(),
            queue_id: self.config.name.clone(),
            skill_group_id: self
                .current_group
                .clone()
                .or_else(|| self.config.skill_group.clone()),
            state: format!("{:?}", self.state),
            pending_dials: self.pending_agents.len(),
            ringing_legs: self.ringing_agent_legs.len(),
            retry_armed,
            wait_secs: self
                .enqueued_at
                .map(|at| at.elapsed().as_secs())
                .unwrap_or(0),
            recoverable,
            reason: reason.into(),
            outcome: "observed".into(),
            candidate_count: self.get_agents().len(),
            candidate_preview: self
                .get_agents()
                .into_iter()
                .take(3)
                .filter_map(|location| {
                    crate::models::call_record::extract_sip_username(&location.aor.to_string())
                })
                .collect(),
            candidate_order_source: "active_dial_round_snapshot".into(),
            live_agent_legs: request.live_agent_legs,
            live_transfer_in_progress: request.live_transfer_in_progress,
            live_caller_connected: request.live_caller_connected,
            live_session_observed_at_ms: request.live_session_observed_at_ms,
        }
    }

    pub(super) async fn diagnose_dispatch(
        &mut self,
        request: &QueueDiagnosticRequest,
        ctrl: &mut CallController,
    ) -> anyhow::Result<AppAction> {
        if request.expired() {
            tracing::info!(session_id = %self.call_id, request_id = %request.request_id,
                recover = request.recover, reason = "request_expired",
                "Queue: live diagnostic request expired before execution");
            request.respond(Err("request_expired".into()));
            return Ok(AppAction::Continue);
        }
        let mut evidence = self.dispatch_evidence(ctrl, request);
        if request
            .expected_skill_group
            .as_ref()
            .is_some_and(|group| Some(group) != evidence.skill_group_id.as_ref())
        {
            evidence.recoverable = false;
            evidence.reason = "skill_group_changed".into();
            evidence.outcome = "rejected".into();
        } else if request.recover
            && self
                .recovery_requests
                .iter()
                .any(|(id, _)| id == &request.request_id)
        {
            evidence.outcome = "already_processed".into();
        } else if request.recover && evidence.recoverable {
            tracing::info!(session_id = %self.call_id, request_id = %request.request_id,
                queue_id = %evidence.queue_id, skill_group_id = ?evidence.skill_group_id,
                state = %evidence.state, pending_dials = evidence.pending_dials,
                ringing_legs = evidence.ringing_legs, retry_armed = evidence.retry_armed,
                candidate_count = evidence.candidate_count, wait_secs = evidence.wait_secs,
                expected_skill_group = ?request.expected_skill_group,
                reason = %evidence.reason, "Queue: targeted dispatch recovery preconditions passed");
            // Reuse normal routing, but never reconcile unrelated agents as a side effect.
            self.targeted_recovery = true;
            ctrl.set_queue_recovery_request(Some(request.clone()));
            // Do not let delayed DB work originate a leg after the session guard expires.
            let result = tokio::time::timeout_at(request.deadline, async {
                if evidence.reason == "dispatch_stalled" {
                    self.dial_next_agent(ctrl).await
                } else {
                    self.try_offer_from_wait(ctrl).await
                }
            })
            .await;
            let result = match result {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("result_unknown_timeout")),
            };
            if result.is_err() {
                self.release_unanswered_recovery_reservations(Some(&request.request_id), true)
                    .await;
            }
            ctrl.set_queue_recovery_request(None);
            self.targeted_recovery = false;
            self.recovery_requests.push_back((
                request.request_id.clone(),
                self.pending_agents
                    .iter()
                    .map(|(_, leg)| leg.clone())
                    .collect(),
            ));
            if self.recovery_requests.len() > MAX_RECOVERY_REQUESTS {
                self.recovery_requests.pop_front();
            }
            match result {
                Ok(action) => {
                    evidence = self.dispatch_evidence(ctrl, request);
                    evidence.outcome = if evidence.pending_dials > 0 {
                        "dial_requested"
                    } else if evidence.retry_armed {
                        "retry_armed"
                    } else {
                        "no_dispatch"
                    }
                    .into();
                    for (uri, leg_id) in &self.pending_agents {
                        tracing::info!(session_id = %self.call_id, request_id = %request.request_id,
                            %leg_id, direction = "outbound", leg = "agent",
                            agent_id = ?crate::models::call_record::extract_sip_username(uri),
                            "Queue: targeted recovery requested agent dial; awaiting SIP progress");
                    }
                    Self::log_dispatch_evidence(&request.request_id, &evidence);
                    request.respond(Ok(evidence));
                    return Ok(action);
                }
                Err(error) => {
                    tracing::warn!(session_id = %self.call_id, request_id = %request.request_id,
                        pending_dials = self.pending_agents.len(),
                        shared_dispatch_claim_may_remain = true,
                        %error, "Queue: targeted recovery result unknown; shared claim is not proof of waiting or ringing");
                    request.respond(Err(format!(
                        "recovery_failed: {error}; shared_dispatch_claim_may_remain"
                    )));
                    return Ok(AppAction::Continue);
                }
            }
        } else if request.recover {
            evidence.outcome = "rejected".into();
        }
        Self::log_dispatch_evidence(&request.request_id, &evidence);
        request.respond(Ok(evidence));
        Ok(AppAction::Continue)
    }

    pub(super) async fn release_unanswered_recovery_reservations(
        &self,
        request_id: Option<&str>,
        preserve_pending: bool,
    ) {
        if let Some(registry) = self.agent_registry.as_ref() {
            let mut pending_agent_ids = Vec::new();
            if preserve_pending {
                for (uri, _) in &self.pending_agents {
                    pending_agent_ids.push(self.agent_id_for_uri(uri).await);
                }
            }
            for agent_id in registry.agents_ringing_for_call(&self.call_id).await {
                // Once LegAdd is queued, later prompt errors cannot cancel that dial.
                if pending_agent_ids.contains(&agent_id) {
                    continue;
                }
                // The atomic backend operation rechecks Ringing and call ownership;
                // a concurrent answer or another call must never be released.
                let released = registry
                    .release_unanswered_reservation(&agent_id, &self.call_id)
                    .await;
                tracing::info!(session_id = %self.call_id, request_id = ?request_id,
                    %agent_id, released, leg = "agent", direction = "outbound",
                    "Queue: scoped unanswered recovery reservation cleanup");
            }
        }
    }

    pub(super) async fn recovery_dial_failed(
        &mut self,
        data: &serde_json::Value,
        ctrl: &mut CallController,
    ) -> anyhow::Result<AppAction> {
        let Some(request_id) = data.get("request_id").and_then(|v| v.as_str()) else {
            return Ok(AppAction::Continue);
        };
        let Some(leg_id) = data.get("leg_id").and_then(|v| v.as_str()) else {
            return Ok(AppAction::Continue);
        };
        if self.call_already_connected()
            || !self.recovery_requests.iter().any(|(request, legs)| {
                request == request_id && legs.iter().any(|leg| leg == leg_id)
            })
        {
            return Ok(AppAction::Continue);
        }
        let Some(index) = self
            .pending_agents
            .iter()
            .position(|(_, leg)| leg == leg_id)
        else {
            return Ok(AppAction::Continue);
        };
        let (uri, _) = self.pending_agents.remove(index);
        let agent_id = self.agent_id_for_uri(&uri).await;
        self.ringing_agent_legs.remove(leg_id);
        if let Some(registry) = &self.agent_registry {
            registry
                .release_unanswered_reservation(&agent_id, &self.call_id)
                .await;
        }
        tracing::info!(session_id = %self.call_id, %request_id, %leg_id,
            leg = "agent", direction = "outbound", %agent_id,
            reason = ?data.get("reason"), "Queue: rejected recovery dial returned to waiting");
        if !self.pending_agents.is_empty() {
            return Ok(AppAction::Continue);
        }
        // No INVITE was started: this is a retry, never an agent no-answer.
        ctrl.cancel_timeout("agent_ring_timeout");
        self.current_agent_idx = 0;
        if self.allows_wait_retention() {
            self.dynamic_agents = None;
            self.targeted_recovery = true;
            let result = self.enter_waiting_for_agent(ctrl).await;
            self.targeted_recovery = false;
            result
        } else {
            self.state = QueueState::DialingAgents {
                attempt: self.dial_attempts,
            };
            self.arm_queue_retry(ctrl);
            Ok(AppAction::Continue)
        }
    }

    fn log_dispatch_evidence(request_id: &str, evidence: &QueueDispatchDiagnostic) {
        tracing::info!(session_id = %evidence.session_id, request_id,
            queue_id = %evidence.queue_id, skill_group_id = ?evidence.skill_group_id,
            state = %evidence.state, pending_dials = evidence.pending_dials,
            ringing_legs = evidence.ringing_legs, retry_armed = evidence.retry_armed,
            wait_secs = evidence.wait_secs, candidate_count = evidence.candidate_count,
            candidate_preview = ?evidence.candidate_preview,
            candidate_order_source = %evidence.candidate_order_source,
            live_agent_legs = ?evidence.live_agent_legs,
            live_transfer_in_progress = ?evidence.live_transfer_in_progress,
            live_caller_connected = ?evidence.live_caller_connected,
            live_session_observed_at_ms = ?evidence.live_session_observed_at_ms,
            recoverable = evidence.recoverable, reason = %evidence.reason,
            outcome = %evidence.outcome, "Queue: live dispatch diagnostic evidence");
    }
}
