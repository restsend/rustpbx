//! Wait-for-callback offer / confirm / finalize chain (see
//! [`crate::call::QueueCallbackConfig`]).
//!
//! Flow (all stages inert unless `config.callback.enabled`):
//!
//! ```text
//! T = offer_after_secs (one-shot timer armed at on_enter)
//!   ├─ caller in a hold-phase state → play the offer prompt (once)
//!   │     (caller was ringing an agent at T → play on next return to hold)
//!   ├─ caller presses the callback key (accepted from T onward while in a
//!   │     hold-phase state) → callback_requested latch, confirm prompt plays
//!   │     → finalize: trace + dispatcher notify + RWI events + hangup(BySystem)
//!   └─ agent connects meanwhile → latch cleared, normal connect flow wins
//! ```
//!
//! Conflict rules (see the state machine in `queue.rs`):
//!
//! - While `callback_requested` is set the confirm chain OWNS the call:
//!   `max_wait_timeout` defers and agent-unavailable fallback paths
//!   short-circuit until the chain finalizes (or the caller hangs up, which
//!   classifies as a plain abandon — the request was never confirmed).
//! - An agent connection always overtakes a pending callback: the latch is
//!   cleared and no callback task is created.
//! - Offer/DTMF are only active in hold-phase states (`PlayingHold`,
//!   `WaitingForAgent`, `PlayingComfortPrompt`, `PlayingCallbackOffer`) —
//!   never while an agent leg is ringing or a fallback prompt plays.
use super::{QueueApp, QueueState};
use crate::call::app::{AppAction, CallController};
use crate::callrecord::CallRecordHangupReason;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Timer id for the one-shot offer threshold (`ctrl.set_timeout`).
pub(crate) const CALLBACK_OFFER_TIMER: &str = "callback_offer";

impl QueueApp {
    /// Whether the wait-for-callback feature is configured on.
    pub(super) fn callback_enabled(&self) -> bool {
        self.config.callback.enabled
    }

    /// Queue wait seconds elapsed since this queue entry was enqueued — the
    /// offer clock. Deliberately NOT the overflow stage clock
    /// (`stage_started_at`): the caller's total wait is what the offer
    /// promises, and per-stage resets must not postpone it.
    pub(super) fn callback_wait_elapsed(&self) -> u64 {
        self.enqueued_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0)
    }

    /// The offer threshold has elapsed (0 = immediately eligible).
    fn callback_threshold_reached(&self) -> bool {
        self.callback_wait_elapsed() >= self.config.callback.offer_after_secs
    }

    /// Hold-phase states where the offer may play and the callback DTMF key
    /// is accepted. `DialingAgents` counts ONLY while no agent leg is
    /// actually ringing (`pending_agents` empty — the between-rounds /
    /// awaiting-kick window where the caller hears plain hold music);
    /// interrupting active ringback with a menu is hostile. Fallback prompt
    /// states and `Connected` are never offerable.
    fn in_callback_offerable_state(&self) -> bool {
        if matches!(
            self.state,
            QueueState::Answering
                | QueueState::WaitingForAgent
                | QueueState::PlayingComfortPrompt
                | QueueState::PlayingCallbackOffer
                | QueueState::PlayingHold { .. }
        ) {
            return true;
        }
        matches!(self.state, QueueState::DialingAgents { .. })
            && self.pending_agents.is_empty()
            && self.ringing_agent_legs.is_empty()
    }

    /// The confirm chain owns the call: no fallback / abandon classification
    /// may run until the chain finalizes (or the caller hangs up first).
    pub(super) fn callback_owns_call(&self) -> bool {
        self.callback_requested && !self.callback_finalized
    }

    /// Clear the pending callback state after an agent CONNECTED — the real
    /// connection overtakes the option (the caller is now talking to
    /// someone; a callback task would be nonsense).
    pub(super) fn callback_reset_on_connect(&mut self) {
        if self.callback_requested && !self.callback_finalized {
            info!(
                call_id = %self.call_id,
                "Queue: agent connected — pending callback request discarded"
            );
        }
        self.callback_requested = false;
        self.callback_finalized = false;
        self.callback_offer_pending = false;
        self.callback_offer_token = None;
        self.callback_confirm_token = None;
    }

    /// Arm the one-shot offer threshold timer at `on_enter`.
    pub(super) fn arm_callback_offer_timer(&mut self, ctrl: &mut CallController) {
        if !self.callback_enabled() || self.config.callback.offer_after_secs == 0 {
            return;
        }
        ctrl.set_timeout(
            CALLBACK_OFFER_TIMER,
            Duration::from_secs(self.config.callback.offer_after_secs),
        );
    }

    /// `on_timeout(CALLBACK_OFFER_TIMER)`: play the offer prompt when the
    /// caller is in a hold-phase state, otherwise park the offer until the
    /// next return to hold (`maybe_resume_pending_callback_offer`).
    pub(super) async fn on_callback_offer_timeout(&mut self, ctrl: &mut CallController) {
        if !self.callback_enabled()
            || self.callback_requested
            || self.callback_finalized
            || self.fallback_executed
            || self.callback_offered
        {
            return;
        }
        if self.in_callback_offerable_state() {
            self.play_callback_offer_prompt(ctrl).await;
        } else {
            debug!(
                queue = %self.config.name,
                state = ?self.state,
                "Queue: callback offer threshold reached while not offerable — deferred to next hold return"
            );
            self.callback_offer_pending = true;
        }
    }

    /// Called at `start_hold_music` entry: if a deferred offer is waiting and
    /// the state is hold-phase, play the offer prompt instead of the music
    /// (music resumes when the prompt completes). Returns `true` when the
    /// caller consumed this hold start.
    pub(super) async fn maybe_resume_pending_callback_offer(
        &mut self,
        ctrl: &mut CallController,
    ) -> bool {
        if !self.callback_offer_pending
            || !self.callback_enabled()
            || self.callback_requested
            || self.callback_finalized
        {
            return false;
        }
        self.callback_offer_pending = false;
        if !self.in_callback_offerable_state() {
            // Started from a non-offerable context (e.g. transfer prompt
            // completion while an agent rings) — re-park.
            self.callback_offer_pending = true;
            return false;
        }
        self.play_callback_offer_prompt(ctrl).await;
        true
    }

    /// Play the one-shot offer prompt (or mark the option silently offered
    /// when no recording is configured — the DTMF key works regardless).
    async fn play_callback_offer_prompt(&mut self, ctrl: &mut CallController) {
        self.callback_offered = true;
        let Some(path) = self
            .prompts()
            .and_then(|p| p.callback_offer_prompt.clone())
        else {
            debug!(
                queue = %self.config.name,
                "Queue: callback option offered (no offer prompt configured — key-only)"
            );
            return;
        };
        info!(
            queue = %self.config.name,
            call_id = %self.call_id,
            after_secs = self.config.callback.offer_after_secs,
            "Queue: offering wait-for-callback option"
        );
        ctrl.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Queue,
                "Callback option offered to caller",
            )
            .severity(crate::call_errors::ErrSeverity::Info)
            .detail(serde_json::json!({
                "queue_name": self.config.name,
                "after_secs": self.config.callback.offer_after_secs,
                "dtmf_key": self.config.callback.dtmf_key,
            })),
        );
        // Playing the prompt replaces hold music on the caller peer.
        self.hold_playback = None;
        match ctrl.play_audio(path, false).await {
            Ok(token) => {
                self.callback_offer_token = Some(token);
                self.state = QueueState::PlayingCallbackOffer;
            }
            Err(e) => {
                warn!(
                    queue = %self.config.name,
                    error = %e,
                    "Queue: callback offer prompt failed to start — continuing hold (key stays accepted)"
                );
                // State untouched; the caller stays wherever they were.
            }
        }
    }

    /// `on_dtmf` delegate. Accepts the callback key once the threshold has
    /// elapsed and the caller sits in a hold-phase state; everything else is
    /// ignored (queue hold has no other DTMF consumers).
    pub(super) async fn handle_callback_dtmf(
        &mut self,
        digit: &str,
        ctrl: &mut CallController,
    ) -> anyhow::Result<AppAction> {
        if !self.callback_enabled()
            || self.callback_requested
            || self.callback_finalized
            || self.fallback_executed
            || digit.trim() != self.config.callback.dtmf_key
            || !self.in_callback_offerable_state()
            || !self.callback_threshold_reached()
        {
            return Ok(AppAction::Continue);
        }

        // Accept the request: from here the confirm chain owns the call.
        self.callback_requested = true;
        let wait_secs = self.callback_wait_elapsed();
        info!(
            queue = %self.config.name,
            call_id = %self.call_id,
            wait_secs,
            "Queue: callback requested via DTMF key"
        );
        ctrl.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Queue,
                "Callback requested via DTMF key",
            )
            .severity(crate::call_errors::ErrSeverity::Info)
            .detail(serde_json::json!({
                "queue_name": self.config.name,
                "dtmf_key": self.config.callback.dtmf_key,
                "wait_secs": wait_secs,
            })),
        );

        // Playing the prompt replaces hold music on the caller peer.
        self.hold_playback = None;
        let confirm_path = self
            .prompts()
            .and_then(|p| p.callback_confirm_prompt.clone());
        match confirm_path {
            Some(path) => match ctrl.play_audio(path, false).await {
                Ok(token) => {
                    self.callback_confirm_token = Some(token);
                    self.state = QueueState::PlayingCallbackConfirm;
                }
                Err(e) => {
                    warn!(
                        queue = %self.config.name,
                        error = %e,
                        "Queue: callback confirm prompt failed to start — finalizing directly"
                    );
                    return self.finalize_callback_request(ctrl).await;
                }
            },
            None => {
                // No confirmation prompt configured — register directly.
                return self.finalize_callback_request(ctrl).await;
            }
        }
        Ok(AppAction::Continue)
    }

    /// `on_audio_complete` delegate for the two callback states. The chain
    /// owns the continuation whenever the state matched: the offer returns to
    /// hold (Continue), the confirm completion returns the finalize action
    /// (BySystem hangup). A stale track match is a Continue no-op.
    pub(super) async fn handle_callback_audio_complete(
        &mut self,
        ctrl: &mut CallController,
        track_id: &str,
    ) -> anyhow::Result<AppAction> {
        match self.state {
            QueueState::PlayingCallbackOffer => {
                if !Self::take_if_matching(
                    &mut self.callback_offer_token,
                    track_id,
                    "callback offer prompt",
                ) {
                    return Ok(AppAction::Continue);
                }
                // Return to the wait the caller came from.
                if self.allows_wait_retention() && self.pending_agents.is_empty() {
                    self.state = QueueState::WaitingForAgent;
                } else {
                    self.state = QueueState::PlayingHold {
                        attempt: self.dial_attempts,
                    };
                }
                self.start_hold_music(ctrl).await?;
                Ok(AppAction::Continue)
            }
            QueueState::PlayingCallbackConfirm => {
                if !Self::take_if_matching(
                    &mut self.callback_confirm_token,
                    track_id,
                    "callback confirm prompt",
                ) {
                    return Ok(AppAction::Continue);
                }
                self.finalize_callback_request(ctrl).await
            }
            _ => Ok(AppAction::Continue),
        }
    }

    /// Complete the callback registration: CDR trace, dispatcher notify
    /// (inline — the CC addon writes the task row), app + RWI events, then a
    /// BySystem hangup. The `BySystem` reason intentionally avoids the
    /// session-level queue-abandon catch-all (which only overrides
    /// ByCaller/Canceled/Abandoned/None), so the CDR keeps the distinct exit.
    async fn finalize_callback_request(&mut self, ctrl: &mut CallController) -> anyhow::Result<AppAction> {
        self.callback_finalized = true;
        // Exit-classification latch: on_exit must not run the abandoned
        // bookkeeping for this exit — the caller did not abandon, the
        // callback chain completed.
        self.abandoned_recorded = true;
        // The wait is over — no ghost timers may outlive the chain (same
        // discipline as the max-wait fallback: a same-tick retry/escalation
        // event must not originate agent legs for this call).
        ctrl.cancel_timeout("max_wait_timeout");
        ctrl.cancel_timeout("queue_retry");
        ctrl.cancel_timeout("escalation_check");
        ctrl.cancel_timeout(CALLBACK_OFFER_TIMER);

        let queue_id = self.config.name.clone();
        let wait_secs = self.callback_wait_elapsed();
        let caller = self.caller.clone();
        let dtmf_key = self.config.callback.dtmf_key.clone();
        info!(
            queue = %queue_id,
            call_id = %self.call_id,
            caller = %caller,
            wait_secs,
            "Queue: callback request registered"
        );
        ctrl.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Queue,
                "Callback request registered",
            )
            .severity(crate::call_errors::ErrSeverity::Info)
            .detail(serde_json::json!({
                "queue_name": queue_id,
                "caller": caller,
                "wait_secs": wait_secs,
                "dtmf_key": dtmf_key,
                "outcome": "registered",
            })),
        );

        // Dispatcher notify FIRST and inline: the addon writes its task row
        // (and releases the shared-queue waiting depth) before the hangup
        // tears the session down.
        if let Some(ref registry) = self.agent_registry {
            registry
                .notify_callback_requested(
                    &self.call_id,
                    self.skill_queue_id(),
                    &caller,
                    wait_secs,
                    &dtmf_key,
                )
                .await;
        }

        // App-level event (same channel as `queue.timeout`).
        ctrl.notify_event(
            "queue.callback_requested",
            serde_json::json!({
                "call_id": self.call_id,
                "queue_id": queue_id,
                "caller": caller,
                "wait_secs": wait_secs,
            }),
        )
        .await?;

        // RWI lifecycle: the request itself (webhook consumers create the
        // follow-up ticket) + the queue exit with a distinct reason (never
        // "abandoned" — this caller did not give up).
        self.emit_rwi(&crate::rwi::event::QueueCallbackRequested {
            call_id: self.call_id.clone(),
            queue_id: queue_id.clone(),
            caller: caller.clone(),
            wait_secs,
            dtmf_key: dtmf_key.clone(),
        });
        let groups = self.all_skill_groups();
        self.emit_rwi(&crate::rwi::event::QueueLeft {
            call_id: self.call_id.clone(),
            queue_id,
            reason: Some("callback".to_string()),
            skill_groups: if groups.is_empty() {
                None
            } else {
                Some(groups)
            },
        });

        self.state = QueueState::Done;
        Ok(AppAction::Hangup {
            reason: Some(CallRecordHangupReason::BySystem),
            code: None,
        })
    }
}
