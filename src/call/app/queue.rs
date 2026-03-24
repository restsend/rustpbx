//! Queue Application — built-in call queue with agent routing.
//!
//! Manages call distribution to agents with support for:
//! - Sequential and parallel dialing strategies
//! - Hold music while waiting
//! - Fallback actions on failure
//! - Queue position announcements (optional)
//!
//! # State Machine
//!
//! ```text
//! Init → Answering ──→ DialingAgents ──→ Connected → Done
//!           │                │
//!           │                ├─ Busy/NoAnswer → Retry/Fallback
//!           │                │
//!           └─ HoldMusic ◄───┘ (while waiting)
//! ```

use super::{
    AppAction, ApplicationContext, CallApp, CallAppType, CallController, PlaybackHandle,
};
use crate::call::{
    DialStrategy, FailureAction, Location, QueueFallbackAction, QueueHoldConfig, QueuePlan,
};
use crate::callrecord::CallRecordHangupReason;
use async_trait::async_trait;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Internal state of the Queue state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueState {
    /// Initial state before `on_enter`.
    Init,
    /// Answering the call (if accept_immediately is true).
    Answering,
    /// Playing hold music while waiting for an agent.
    PlayingHold { attempt: u32 },
    /// Dialing agents (sequential or parallel).
    DialingAgents { attempt: u32 },
    /// Call connected to an agent.
    Connected { agent_uri: String },
    /// Executing fallback action.
    ExecutingFallback,
    /// Terminal state.
    Done,
}

/// A built-in Queue application for call distribution.
///
/// Routes incoming calls to available agents using configured dialing
/// strategies, with hold music and fallback handling.
pub struct QueueApp {
    /// The queue plan configuration.
    plan: QueuePlan,
    /// Current state machine state.
    state: QueueState,
    /// Current hold music playback handle (if any).
    hold_playback: Option<PlaybackHandle>,
    /// Whether we've already answered the call.
    answered: bool,
    /// Current agent index for sequential dialing.
    current_agent_idx: usize,
    /// Number of dial attempts made.
    dial_attempts: u32,
}

impl QueueApp {
    /// Create a new `QueueApp` from a [`QueuePlan`].
    pub fn new(plan: QueuePlan) -> Self {
        Self {
            plan,
            state: QueueState::Init,
            hold_playback: None,
            answered: false,
            current_agent_idx: 0,
            dial_attempts: 0,
        }
    }

    /// Get the next action based on fallback configuration.
    async fn execute_fallback(&mut self) -> anyhow::Result<AppAction> {
        info!("Queue: executing fallback action");
        self.state = QueueState::ExecutingFallback;

        let action = match &self.plan.fallback {
            Some(QueueFallbackAction::Failure(failure_action)) => {
                self.get_fallback_action(failure_action)
            }
            Some(QueueFallbackAction::Redirect { target }) => {
                info!(target = %target, "Queue: fallback redirect");
                AppAction::Transfer(target.to_string())
            }
            Some(QueueFallbackAction::Queue { name }) => {
                info!(queue = %name, "Queue: fallback to another queue");
                // For now, transfer to queue reference
                AppAction::Transfer(format!("queue:{}", name))
            }
            None => {
                // Default: hangup with service unavailable
                AppAction::Hangup {
                    reason: Some(CallRecordHangupReason::ServerUnavailable),
                    code: Some(480),
                }
            }
        };
        Ok(action)
    }

    /// Get fallback action without executing it.
    fn get_fallback_action(&self, action: &FailureAction) -> AppAction {
        match action {
            FailureAction::Hangup { code, reason } => {
                info!(?code, ?reason, "Queue: hangup fallback");
                AppAction::Hangup {
                    reason: reason
                        .as_ref()
                        .map(|_| CallRecordHangupReason::ServerUnavailable),
                    code: code.as_ref().map(|c| c.code()),
                }
            }
            FailureAction::PlayThenHangup {
                audio_file: _,
                use_early_media: _,
                status_code,
                reason,
            } => {
                info!("Queue: play then hangup fallback");
                // For simplicity, just hangup (play could be added if needed)
                AppAction::Hangup {
                    reason: reason
                        .as_ref()
                        .map(|_| CallRecordHangupReason::ServerUnavailable),
                    code: Some(status_code.code()),
                }
            }
            FailureAction::Transfer(endpoint) => {
                info!(target = ?endpoint, "Queue: transfer fallback");
                // Transfer to the endpoint
                match endpoint {
                    crate::call::TransferEndpoint::Uri(uri) => {
                        AppAction::Transfer(uri.to_string())
                    }
                    crate::call::TransferEndpoint::Queue(queue_name) => {
                        AppAction::Transfer(format!("queue:{}", queue_name))
                    }
                }
            }
        }
    }

    /// Start or restart hold music.
    async fn start_hold_music(&mut self, ctrl: &mut CallController) -> anyhow::Result<()> {
        if let Some(ref hold) = self.plan.hold {
            if let Some(ref audio_file) = hold.audio_file {
                debug!(file = %audio_file, "Queue: starting hold music");
                self.hold_playback = Some(ctrl.play_audio(audio_file, true).await?);
            }
        }
        Ok(())
    }

    /// Stop hold music (placeholder - actual cleanup handled by controller).
    async fn _stop_hold_music(&mut self) {
        if self.hold_playback.take().is_some() {
            debug!("Queue: stopping hold music");
            // Playback handles are automatically cleaned up by the controller
        }
    }

    /// Get agent locations from dial strategy.
    fn get_agents(&self) -> Vec<&Location> {
        match &self.plan.dial_strategy {
            Some(DialStrategy::Sequential(locations)) => locations.iter().collect(),
            Some(DialStrategy::Parallel(locations)) => locations.iter().collect(),
            None => Vec::new(),
        }
    }

    /// Check if we should use parallel dialing.
    fn is_parallel(&self) -> bool {
        matches!(self.plan.dial_strategy, Some(DialStrategy::Parallel(_)))
    }
}

#[async_trait]
impl CallApp for QueueApp {
    fn app_type(&self) -> CallAppType {
        CallAppType::Queue
    }

    fn name(&self) -> &str {
        self.plan
            .label
            .as_deref()
            .unwrap_or("queue")
    }

    async fn on_enter(
        &mut self,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        info!("Queue: entering queue application");
        self.state = QueueState::Answering;

        // Check if we have agents configured
        let agents = self.get_agents();
        if agents.is_empty() {
            warn!("Queue: no agents configured, executing fallback");
            return self.execute_fallback().await;
        }

        // Answer immediately if configured
        if self.plan.accept_immediately {
            info!("Queue: answering call immediately");
            ctrl.answer().await?;
            self.answered = true;
        }

        // Start hold music if configured
        self.start_hold_music(ctrl).await?;

        // Transition to dialing state
        self.state = QueueState::DialingAgents { attempt: 1 };
        self.dial_attempts = 1;

        // Return Continue to proceed with dialing
        // The actual dialing is handled by the session layer
        Ok(AppAction::Continue)
    }

    async fn on_audio_complete(
        &mut self,
        _track_id: String,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        debug!("Queue: audio playback completed");

        // If we're in hold state, restart hold music (it loops)
        if let QueueState::PlayingHold { attempt: _ } = self.state {
            // Hold music loop - restart it
            self.start_hold_music(ctrl).await?;
        }

        Ok(AppAction::Continue)
    }

    async fn on_external_event(
        &mut self,
        event: super::AppEvent,
        _ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        match event {
            super::AppEvent::Custom { name, data } => match name.as_str() {
                "agent_connected" => {
                    if let Some(agent_uri) = data.get("agent_uri").and_then(|v| v.as_str()) {
                        info!(agent = %agent_uri, "Queue: agent connected");
                        self._stop_hold_music().await;
                        self.state = QueueState::Connected {
                            agent_uri: agent_uri.to_string(),
                        };
                        // Transfer to the connected agent
                        return Ok(AppAction::Transfer(agent_uri.to_string()));
                    }
                    Ok(AppAction::Continue)
                }
                "agent_busy" | "agent_no_answer" => {
                    info!(event = %name, "Queue: agent unavailable, retrying");
                    // Increment agent index for sequential
                    if !self.is_parallel() {
                        self.current_agent_idx += 1;
                    }
                    self.dial_attempts += 1;

                    // Check if we should retry or fallback
                    let agents = self.get_agents();
                    if self.current_agent_idx >= agents.len() {
                        // Tried all agents
                        return self.execute_fallback().await;
                    }

                    // Continue with next agent
                    self.state = QueueState::DialingAgents {
                        attempt: self.dial_attempts,
                    };
                    Ok(AppAction::Continue)
                }
                "all_agents_busy" => {
                    warn!("Queue: all agents busy");
                    return self.execute_fallback().await;
                }
                _ => Ok(AppAction::Continue),
            },
            _ => Ok(AppAction::Continue),
        }
    }

    async fn on_exit(&mut self, reason: super::ExitReason) -> anyhow::Result<()> {
        info!(?reason, "Queue: exiting queue application");
        // Note: hold music cleanup is handled by the controller when app exits
        self.state = QueueState::Done;
        Ok(())
    }
}

/// Configuration for queue behavior.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Queue name/identifier.
    pub name: String,
    /// Whether to answer immediately or wait for agent.
    pub accept_immediately: bool,
    /// Hold music configuration.
    pub hold: Option<QueueHoldConfig>,
    /// Fallback action when all agents fail.
    pub fallback: Option<QueueFallbackAction>,
    /// Agent locations to dial.
    pub agents: Vec<Location>,
    /// Dialing strategy.
    pub strategy: DialStrategy,
    /// Ring timeout per agent.
    pub ring_timeout: Option<Duration>,
}

impl QueueConfig {
    /// Convert to a QueuePlan.
    pub fn to_plan(&self) -> QueuePlan {
        QueuePlan {
            accept_immediately: self.accept_immediately,
            passthrough_ringback: false,
            hold: self.hold.clone(),
            fallback: self.fallback.clone(),
            dial_strategy: Some(self.strategy.clone()),
            ring_timeout: self.ring_timeout,
            label: Some(self.name.clone()),
            retry_codes: None,
            no_trying_timeout: None,
        }
    }
}

/// Build a QueueApp from a QueuePlan.
pub fn build_queue_app(plan: QueuePlan) -> QueueApp {
    QueueApp::new(plan)
}
