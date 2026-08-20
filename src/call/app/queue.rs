//! Queue Application — built-in call queue with agent routing.
//!
//! Manages call distribution to agents with support for:
//! - Sequential and parallel dialing strategies
//! - Hold music while waiting
//! - Fallback actions on failure
//! - Queue position announcements (optional)
//! - Skill-based routing (via DbRegistry)
//! - SLA monitoring (built-in)
//! - Agent state tracking (via DbRegistry)
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

use super::agent_registry::{AgentRegistry, PresenceState, RoutingStrategy};
use super::{AppAction, ApplicationContext, CallApp, CallAppType, CallController, PlaybackToken};
use crate::call::{
    DialStrategy, FailureAction, Location, QueueFallbackAction, QueueHoldConfig, QueuePlan,
    VoicePrompts,
};
use crate::callrecord::CallRecordHangupReason;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

// ===================================================================
// Queue Statistics (built-in)
// ===================================================================

// ===================================================================
// Queue Configuration Extensions
// ===================================================================

/// No-answer action for CC queues.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum NoAnswerAction {
    /// Transfer to voicemail.
    #[default]
    Voicemail,
    /// Hangup the call.
    Hangup,
    /// Create a callback task.
    Callback,
    /// Fallback to another skill group.
    FallbackSkill,
    /// Go back to IVR.
    BackToIvr,
}

/// Extended queue configuration with CC features.
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
    /// Agent locations to dial (static configuration).
    pub agents: Vec<Location>,
    /// Dialing strategy.
    pub strategy: DialStrategy,
    /// Ring timeout per agent.
    pub ring_timeout: Option<Duration>,
    /// Enable skill-based routing.
    pub skill_routing_enabled: bool,
    /// Required skills for this queue.
    pub required_skills: Vec<String>,
    /// SLA threshold in seconds.
    pub sla_threshold_secs: u64,
    /// Max wait time before fallback.
    pub max_wait_secs: u64,
    /// Enable queue position announcements.
    pub announce_position: bool,
    /// Retry interval for no-answer.
    pub retry_interval_secs: u64,
    /// Max retry attempts.
    pub max_retries: u32,
    /// Enable autonomous routing (auto-assign agents).
    pub autonomous_routing: bool,
    /// Routing strategy for agent selection.
    pub routing_strategy: RoutingStrategy,
    /// No-answer action.
    pub no_answer_action: NoAnswerAction,
    /// Fallback skill group.
    pub fallback_skill_group: Option<String>,
    /// Enable SLA monitoring.
    pub sla_monitoring: bool,
    /// Enable metrics collection.
    pub metrics_enabled: bool,
    /// Built-in voice prompts for queue events.
    pub voice_prompts: Option<VoicePrompts>,
    // ── Escalation ──
    /// Escalation mode: Replace or Cumulative.
    pub escalation_mode: EscalationMode,
    /// Escalation timeline: ordered steps of (threshold_secs, skill_group_id).
    pub escalation_timeline: Vec<EscalationStep>,
}

/// Escalation mode for overflow/skill-group escalation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationMode {
    /// Replace current skill group with the next one.
    Replace,
    /// Add new skill group agents alongside existing ones (cumulative).
    Cumulative,
}

impl Default for EscalationMode {
    fn default() -> Self {
        Self::Replace
    }
}

/// A single step in the escalation timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EscalationStep {
    /// Wait threshold in seconds before this step triggers.
    pub threshold_secs: u64,
    /// Skill group to add (or switch to, depending on mode).
    pub add_skill_group: String,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            accept_immediately: true,
            hold: None,
            fallback: None,
            agents: Vec::new(),
            strategy: DialStrategy::Sequential(Vec::new()),
            ring_timeout: Some(Duration::from_secs(20)),
            skill_routing_enabled: false,
            required_skills: Vec::new(),
            sla_threshold_secs: 20,
            max_wait_secs: 300,
            announce_position: false,
            retry_interval_secs: 5,
            max_retries: 2,
            autonomous_routing: false,
            routing_strategy: RoutingStrategy::LongestIdle,
            no_answer_action: NoAnswerAction::Voicemail,
            fallback_skill_group: None,
            sla_monitoring: false,
            metrics_enabled: false,
            voice_prompts: None,
            escalation_mode: EscalationMode::Replace,
            escalation_timeline: Vec::new(),
        }
    }
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
            voice_prompts: self.voice_prompts.clone(),
            queue_name: self.name.clone(),
        }
    }
}

// ===================================================================
// Queue Application
// ===================================================================

/// Internal state of the Queue state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueState {
    /// Initial state before `on_enter`.
    Init,
    /// Answering the call (if accept_immediately is true).
    Answering,
    /// Playing hold music while waiting for an agent.
    PlayingHold { attempt: u32 },
    /// Playing the pre-connect transfer prompt while the agent is being
    /// dialed. `connected_agent` is set when the agent answers mid-prompt.
    PlayingTransferPrompt { connected_agent: Option<String> },
    /// Playing the busy prompt before executing fallback.
    PlayingBusyPrompt,
    /// Playing the no-answer prompt before executing fallback.
    PlayingNoAnswerPrompt,
    /// Dialing agents (sequential or parallel).
    DialingAgents { attempt: u32 },
    /// Call connected to an agent.
    Connected { agent_uri: String },
    /// Playing the caller-only service prompt after connect; exits on completion.
    PlayingServicePrompt { agent_uri: String },
    /// Executing fallback action.
    ExecutingFallback,
    /// Playing comfort/reassurance prompt during hold.
    PlayingComfortPrompt,
    /// Playing final-destination prompt before fallback.
    PlayingFinalPrompt,
    /// Terminal state.
    Done,
}

/// Reason why an agent is unavailable.
#[derive(Debug, Clone, Copy, PartialEq)]
enum AgentUnavailableReason {
    Busy,
    NoAnswer,
}

/// A built-in Queue application for call distribution.
///
/// Routes incoming calls to available agents using configured dialing
/// strategies, with hold music and fallback handling.
///
/// # CC Features (built-in, configurable)
///
/// - **Skill-based routing**: Enable `skill_routing_enabled` and set `required_skills`
/// - **Agent state tracking**: Use `BuiltInAgentRegistry` to track agent states
/// - **SLA monitoring**: Built-in statistics tracking
/// - **Queue announcements**: Position and wait time announcements
/// - **Retry logic**: Configurable retry intervals and max attempts
pub struct QueueApp {
    /// The queue plan configuration.
    plan: QueuePlan,
    /// Extended queue configuration.
    config: QueueConfig,
    /// Current state machine state.
    state: QueueState,
    /// Current hold music playback handle (if any).
    hold_playback: Option<PlaybackToken>,
    /// Whether we've already answered the call.
    answered: bool,
    /// Current agent index for sequential dialing.
    current_agent_idx: usize,
    /// Number of dial attempts made.
    dial_attempts: u32,
    /// Dynamically fetched agents from DbRegistry.
    dynamic_agents: Option<Vec<Location>>,
    /// Optional AgentRegistry for dynamic routing.
    agent_registry: Option<Arc<dyn AgentRegistry>>,
    /// Call ID for tracking.
    call_id: String,
    /// When the call entered the queue.
    enqueued_at: Option<Instant>,
    /// Queue statistics.
    /// (agent_uri, call_id) for agents being dialed concurrently (parallel mode).
    /// When the first agent answers, the rest are cancelled via LegRemove.
    pending_agents: Vec<(String, String)>,
    // ── Comfort announcement ──
    /// Comfort prompt playback state.
    comfort_index: usize,
    last_comfort_played: Option<Instant>,
    // ── Escalation ──
    /// Skill groups already escalated (to avoid duplicates).
    escalated_groups: Vec<String>,
    /// RWI gateway captured from the application context (for queue lifecycle
    /// webhook events). Captured in `on_enter` so that `on_exit` (which has no
    /// context) can still emit abandon events.
    rwi_gateway: Option<crate::rwi::RwiGatewayRef>,
    /// Transfer prompt already started for this queue entry (no replays on
    /// agent retries / escalation re-dials).
    transfer_prompt_played: bool,
    /// In-flight prompt tokens; completions are matched by track id so stale
    /// events cannot advance the wrong state.
    transfer_token: Option<PlaybackToken>,
    service_token: Option<PlaybackToken>,
    busy_token: Option<PlaybackToken>,
    no_answer_token: Option<PlaybackToken>,
    comfort_token: Option<PlaybackToken>,
    final_token: Option<PlaybackToken>,
    abandoned_recorded: bool,
}

impl QueueApp {
    /// Create a new `QueueApp` from a [`QueuePlan`] and [`QueueConfig`].
    pub fn new(plan: QueuePlan, config: QueueConfig) -> Self {
        Self {
            plan,
            config,
            state: QueueState::Init,
            hold_playback: None,
            answered: false,
            current_agent_idx: 0,
            dial_attempts: 0,
            dynamic_agents: None,
            agent_registry: None,
            call_id: String::new(),
            enqueued_at: None,
            pending_agents: Vec::new(),
            comfort_index: 0,
            last_comfort_played: None,
            escalated_groups: Vec::new(),
            rwi_gateway: None,
            transfer_prompt_played: false,
            transfer_token: None,
            service_token: None,
            busy_token: None,
            no_answer_token: None,
            comfort_token: None,
            final_token: None,
            abandoned_recorded: false,
        }
    }

    /// Set the AgentRegistry for dynamic routing.
    pub fn with_agent_registry(mut self, registry: Arc<dyn AgentRegistry>) -> Self {
        self.agent_registry = Some(registry);
        self
    }

    /// Set the call ID for tracking.
    pub fn with_call_id(mut self, call_id: String) -> Self {
        self.call_id = call_id;
        self
    }

    /// Broadcast a queue lifecycle RWI event via the gateway (if captured).
    /// Mirrors the ACD engine bridge in `cc/mod.rs` (`broadcast`) so that queue
    /// events look identical regardless of which subsystem generated them.
    fn emit_rwi<E: crate::rwi::RwiEventSpec>(&self, event: &E) {
        if let Some(ref gw) = self.rwi_gateway {
            let gw = gw.read();
            gw.broadcast(event);
        }
    }

    /// Notify the agent dispatcher that a queued call was abandoned before any
    /// agent answered. Only meaningful for skill-group-routed queues; the CC
    /// addon translates this into `skill_group_call_abandoned`.
    async fn notify_abandoned(&self, wait_secs: u64) {
        if self.config.skill_routing_enabled
            && let Some(ref registry) = self.agent_registry
        {
            let _ = registry
                .notify_call_abandoned(&self.call_id, &self.config.name, wait_secs)
                .await;
        }
    }

    /// Notify the agent dispatcher that a queued call exceeded its max wait
    /// time. CC addon translates this into `skill_group_service_unavailable`.
    async fn notify_timeout(&self, wait_secs: u64) {
        if self.config.skill_routing_enabled
            && let Some(ref registry) = self.agent_registry
        {
            let _ = registry
                .notify_call_timeout(&self.call_id, &self.config.name, wait_secs)
                .await;
        }
    }

    /// Notify the agent dispatcher that a queued call could not be serviced
    /// and a fallback action executed. CC addon translates this into
    /// `skill_group_service_unavailable`.
    async fn notify_fallback(&self, reason: &str, action: &str) {
        if self.config.skill_routing_enabled
            && let Some(ref registry) = self.agent_registry
        {
            let _ = registry
                .notify_call_fallback(&self.call_id, &self.config.name, reason, action)
                .await;
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
            None => AppAction::Hangup {
                reason: Some(CallRecordHangupReason::ServerUnavailable),
                code: Some(486),
            },
        };

        let action_label = match &action {
            AppAction::Transfer(t) => format!("transfer:{}", t),
            AppAction::Hangup { .. } => "hangup".to_string(),
            _ => "other".to_string(),
        };

        // Notify the skill-group dispatcher that the call could not be serviced.
        let reason = "no_agent";
        self.notify_fallback(reason, &action_label).await;

        // Emit RWI queue lifecycle event: a fallback action was executed.
        self.emit_rwi(&crate::rwi::event::QueueFallbackExecuted {
            call_id: self.call_id.clone(),
            queue_id: self.config.name.clone(),
            action: action_label,
            reason: reason.to_string(),
            trace_id: self.call_id.clone(),
        });

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
                AppAction::Hangup {
                    reason: reason
                        .as_ref()
                        .map(|_| CallRecordHangupReason::ServerUnavailable),
                    code: Some(status_code.code()),
                }
            }
            FailureAction::Transfer(endpoint) => {
                info!(target = ?endpoint, "Queue: transfer fallback");
                match endpoint {
                    crate::call::TransferEndpoint::Uri(uri) => AppAction::Transfer(uri.to_string()),
                    crate::call::TransferEndpoint::Queue(queue_name) => {
                        AppAction::Transfer(format!("queue:{}", queue_name))
                    }
                    crate::call::TransferEndpoint::Ivr(ivr_name) => {
                        AppAction::Transfer(format!("ivr:{}", ivr_name))
                    }
                    crate::call::TransferEndpoint::Voicemail(ext) => {
                        AppAction::Transfer(format!("voicemail:{}", ext))
                    }
                    crate::call::TransferEndpoint::Conference(id) => {
                        AppAction::Transfer(format!("conference:{}", id))
                    }
                }
            }
        }
    }

    /// Start or restart hold music.
    async fn start_hold_music(&mut self, ctrl: &mut CallController) -> anyhow::Result<()> {
        if let Some(ref hold) = self.plan.hold
            && let Some(ref audio_file) = hold.audio_file
        {
            debug!(file = %audio_file, "Queue: starting hold music");
            self.hold_playback = Some(ctrl.play_audio(audio_file, true).await?);
        }
        Ok(())
    }

    /// Stop hold music.
    async fn _stop_hold_music(&mut self, ctrl: &mut CallController) {
        if self.hold_playback.take().is_some() {
            debug!("Queue: stopping hold music");
            if let Err(e) = ctrl.stop_audio().await {
                warn!(error = %e, "Queue: failed to stop hold music");
            }
        }
    }

    /// Get agent locations from dial strategy or dynamic agents.
    fn get_agents(&self) -> Vec<&Location> {
        if let Some(ref agents) = self.dynamic_agents {
            return agents.iter().collect();
        }
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

    /// Resolve agents dynamically if agent registry is available.
    async fn resolve_agents(&mut self) {
        if let Some(ref registry) = self.agent_registry {
            let queue_id = self.config.name.as_str();
            let skills = &self.config.required_skills;

            let agents = registry.find_available_agents(skills).await;
            if !agents.is_empty() {
                let locations: Vec<Location> = agents
                    .into_iter()
                    .map(|agent| Location {
                        aor: agent.uri.parse().unwrap_or_default(),
                        contact_raw: Some(agent.uri),
                        ..Default::default()
                    })
                    .collect();

                info!(
                    "Queue: resolved {} dynamic agents for queue '{}'",
                    locations.len(),
                    queue_id
                );
                self.dynamic_agents = Some(locations);
            }
        }
    }

    /// Announce queue position.
    ///
    /// Plays `voice_prompts.position_prompt` if configured; otherwise emits a
    /// warning so operators know the announcement was requested but unconfigured.
    async fn announce_position(&self, ctrl: &mut CallController) -> anyhow::Result<()> {
        let prompts = self
            .plan
            .voice_prompts
            .as_ref()
            .or(self.config.voice_prompts.as_ref());

        if let Some(path) = prompts.and_then(|p| p.position_prompt.as_ref()) {
            debug!(file = %path, "Queue: playing position announcement");
            ctrl.play_audio(path.clone(), false).await?;
        } else {
            warn!(
                queue = %self.config.name,
                "Queue: announce_position is enabled but voice_prompts.position_prompt \
                 is not configured — skipping announcement"
            );
        }
        Ok(())
    }

    /// Handle agent unavailable (busy or no answer).
    /// Tries next agent if available; otherwise plays fallback prompt.
    async fn handle_agent_unavailable(
        &mut self,
        ctrl: &mut CallController,
        reason: AgentUnavailableReason,
        failed_leg_id: Option<&str>,
    ) -> anyhow::Result<AppAction> {
        if self.is_parallel() {
            if let Some(failed_leg_id) = failed_leg_id {
                let pending_before = self.pending_agents.len();
                self.pending_agents
                    .retain(|(_, call_id)| call_id != failed_leg_id);
                if self.pending_agents.len() == pending_before {
                    warn!(
                        %failed_leg_id,
                        "Queue: failed parallel leg was not in the pending agent list"
                    );
                }
            } else if !self.pending_agents.is_empty() {
                self.pending_agents.pop();
            }

            if !self.pending_agents.is_empty() {
                info!(
                    remaining_agents = self.pending_agents.len(),
                    "Queue: parallel agent failed; waiting for remaining agents"
                );
                return Ok(AppAction::Continue);
            }

            return match reason {
                AgentUnavailableReason::Busy => self.play_busy_and_then_fallback(ctrl).await,
                AgentUnavailableReason::NoAnswer => {
                    self.play_no_answer_and_then_fallback(ctrl).await
                }
            };
        }
        self.current_agent_idx += 1;
        self.dial_attempts += 1;

        let agents = self.get_agents();
        if self.current_agent_idx >= agents.len() {
            return match reason {
                AgentUnavailableReason::Busy => self.play_busy_and_then_fallback(ctrl).await,
                AgentUnavailableReason::NoAnswer => {
                    self.play_no_answer_and_then_fallback(ctrl).await
                }
            };
        }

        // More agents remaining — dial the next one immediately
        self.dial_next_agent(ctrl).await
    }

    /// Record abandoned call, then play busy prompt (if configured) before fallback.
    async fn play_busy_and_then_fallback(
        &mut self,
        ctrl: &mut CallController,
    ) -> anyhow::Result<AppAction> {
        let queue_id = self.config.name.clone();
        let wait_secs = self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);

        self.abandoned_recorded = true;

        info!(
            queue = %queue_id,
            wait_secs,
            "Queue: call abandoned, playing busy prompt or fallback"
        );

        // Notify the skill-group dispatcher that the call was abandoned.
        self.notify_abandoned(wait_secs).await;

        // Emit RWI queue lifecycle event: the call abandoned the queue.
        self.emit_rwi(&crate::rwi::event::QueueLeft {
            call_id: self.call_id.clone(),
            queue_id: queue_id.clone(),
            reason: Some("abandoned".to_string()),
        });

        let prompts = self
            .plan
            .voice_prompts
            .as_ref()
            .or(self.config.voice_prompts.as_ref());
        if let Some(path) = prompts.and_then(|p| p.busy_prompt.as_ref()) {
            info!("Queue: playing busy prompt before fallback");
            self.state = QueueState::PlayingBusyPrompt;
            let token = ctrl.play_audio(path.clone(), false).await?;
            self.busy_token = Some(token);
            return Ok(AppAction::Continue);
        }

        self.play_final_destination_prompt_or_fallback(ctrl).await
    }

    /// Record abandoned call, then play no-answer prompt (if configured) before fallback.
    async fn play_no_answer_and_then_fallback(
        &mut self,
        ctrl: &mut CallController,
    ) -> anyhow::Result<AppAction> {
        let queue_id = self.config.name.clone();
        let wait_secs = self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);

        self.abandoned_recorded = true;

        info!(
            queue = %queue_id,
            wait_secs,
            "Queue: call abandoned, playing no-answer prompt or fallback"
        );

        // Notify the skill-group dispatcher that the call was abandoned.
        self.notify_abandoned(wait_secs).await;

        // Emit RWI queue lifecycle event: the call abandoned the queue.
        self.emit_rwi(&crate::rwi::event::QueueLeft {
            call_id: self.call_id.clone(),
            queue_id: queue_id.clone(),
            reason: Some("abandoned".to_string()),
        });

        let prompts = self
            .plan
            .voice_prompts
            .as_ref()
            .or(self.config.voice_prompts.as_ref());
        if let Some(path) = prompts.and_then(|p| p.no_answer_prompt.as_ref()) {
            info!("Queue: playing no-answer prompt before fallback");
            self.state = QueueState::PlayingNoAnswerPrompt;
            let token = ctrl.play_audio(path.clone(), false).await?;
            self.no_answer_token = Some(token);
            return Ok(AppAction::Continue);
        }

        self.play_final_destination_prompt_or_fallback(ctrl).await
    }

    /// Try to play the final_destination_prompt before fallback.
    /// If no prompt is configured, falls through to execute_fallback directly.
    async fn play_final_destination_prompt_or_fallback(
        &mut self,
        ctrl: &mut CallController,
    ) -> anyhow::Result<AppAction> {
        let prompts = self
            .plan
            .voice_prompts
            .as_ref()
            .or(self.config.voice_prompts.as_ref());
        if let Some(path) = prompts.and_then(|p| p.final_destination_prompt.as_ref()) {
            info!("Queue: playing final destination prompt before fallback");
            self.state = QueueState::PlayingFinalPrompt;
            let token = ctrl.play_audio(path.clone(), false).await?;
            self.final_token = Some(token);
            return Ok(AppAction::Continue);
        }
        self.execute_fallback().await
    }

    /// Check and play comfort prompts between hold music loops.
    async fn maybe_play_comfort_or_ewt(&mut self, ctrl: &mut CallController) -> anyhow::Result<()> {
        let now = Instant::now();

        // 1. Comfort prompts
        let prompts = self
            .plan
            .voice_prompts
            .as_ref()
            .or(self.config.voice_prompts.as_ref());
        if let Some(comfort_list) = prompts.map(|p| &p.comfort_prompts) {
            if !comfort_list.is_empty() {
                let elapsed = self.last_comfort_played.map(|t| now.duration_since(t));
                let idx = self.comfort_index % comfort_list.len();
                let prompt = &comfort_list[idx];
                let should_play = match elapsed {
                    Some(d) => d.as_secs() >= prompt.interval_secs as u64,
                    None => true, // play first comfort immediately after hold loop
                };
                if should_play {
                    debug!(
                        comfort_idx = idx,
                        file = %prompt.audio_file,
                        "Queue: playing comfort prompt"
                    );
                    self.state = QueueState::PlayingComfortPrompt;
                    let token = ctrl.play_audio(prompt.audio_file.clone(), false).await?;
                    self.comfort_token = Some(token);
                    self.comfort_index += 1;
                    self.last_comfort_played = Some(now);
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    /// Dial the next agent in a sequential dialing strategy.
    async fn dial_next_agent(&mut self, ctrl: &mut CallController) -> anyhow::Result<AppAction> {
        let agents = self.get_agents();
        if self.current_agent_idx >= agents.len() {
            warn!("Queue: no more agents to dial");
            return self.play_busy_and_then_fallback(ctrl).await;
        }
        // Dial by addr-spec (RFC 3261 Request-URI). `contact_raw` is the raw
        // Contact header value (a `contact-addr` with `<...>` and contact-params)
        // and is not valid as a dial target; `aor` is the registered URI.
        let uri = agents[self.current_agent_idx].aor.to_string();
        let leg_headers = agents[self.current_agent_idx]
            .headers
            .clone()
            .unwrap_or_default();
        info!(
            "Queue: dialing next agent {} (idx={})",
            uri, self.current_agent_idx
        );
        // In sequential mode only one agent rings at a time; clear stale
        // entries so the ring-timeout handler sees the correct agent.
        if !self.is_parallel() {
            self.pending_agents.clear();
        }
        match ctrl
            .originate_call_with_headers(&uri, Some(self.call_id.clone()), leg_headers)
            .await
        {
            Ok(call_id) => {
                self.pending_agents.push((uri, call_id));
            }
            Err(e) => {
                warn!("Queue: failed to dial agent {}: {}", uri, e);
                // Advance to the next agent instead of falling back immediately.
                self.current_agent_idx += 1;
                self.dial_attempts += 1;
                if self.current_agent_idx >= self.get_agents().len() {
                    return self.play_busy_and_then_fallback(ctrl).await;
                }
                return Box::pin(self.dial_next_agent(ctrl)).await;
            }
        }
        let ring_timeout = self.config.ring_timeout.unwrap_or(Duration::from_secs(20));
        ctrl.set_timeout("agent_ring_timeout", ring_timeout);
        self.state = QueueState::DialingAgents {
            attempt: self.dial_attempts,
        };
        self.maybe_start_transfer_prompt(ctrl).await?;
        Ok(AppAction::Continue)
    }

    /// Check escalation timeline and add/switch skill groups.
    async fn check_escalation(&mut self, ctrl: &mut CallController) -> anyhow::Result<()> {
        if self.config.escalation_timeline.is_empty() {
            return Ok(());
        }
        let wait_secs = self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);

        for step in &self.config.escalation_timeline {
            if wait_secs >= step.threshold_secs
                && !self.escalated_groups.contains(&step.add_skill_group)
            {
                info!(
                    queue = %self.config.name,
                    wait_secs,
                    threshold = step.threshold_secs,
                    skill_group = %step.add_skill_group,
                    mode = ?self.config.escalation_mode,
                    "Queue: escalation triggered"
                );

                if let Some(ref registry) = self.agent_registry {
                    let skill_uri = format!("skill-group:{}", step.add_skill_group);
                    let agent_uris = registry.resolve_target(&skill_uri).await;

                    match self.config.escalation_mode {
                        EscalationMode::Cumulative => {
                            // Add new agents alongside existing
                            for uri in &agent_uris {
                                match ctrl.originate_call(uri, Some(self.call_id.clone())).await {
                                    Ok(call_id) => {
                                        info!(agent = %uri, call_id = %call_id, "Queue: cumulative escalation - added agent");
                                        self.pending_agents.push((uri.clone(), call_id));
                                    }
                                    Err(e) => {
                                        warn!(agent = %uri, error = %e, "Queue: cumulative escalation - failed to add agent");
                                    }
                                }
                            }
                        }
                        EscalationMode::Replace => {
                            // Cancel existing legs and dial new agents
                            if !self.pending_agents.is_empty() {
                                let old_legs: Vec<String> = self
                                    .pending_agents
                                    .iter()
                                    .map(|(_, cid)| cid.clone())
                                    .collect();
                                ctrl.remove_legs(&old_legs);
                                self.pending_agents.clear();
                            }
                            // Also reset dynamic agents for new skill group
                            self.dynamic_agents = None;
                            self.current_agent_idx = 0;

                            for uri in &agent_uris {
                                match ctrl.originate_call(uri, Some(self.call_id.clone())).await {
                                    Ok(call_id) => {
                                        info!(agent = %uri, call_id = %call_id, "Queue: replace escalation - dialed agent");
                                        self.pending_agents.push((uri.clone(), call_id));
                                    }
                                    Err(e) => {
                                        warn!(agent = %uri, error = %e, "Queue: replace escalation - failed to dial agent");
                                    }
                                }
                            }
                        }
                    }
                }

                self.escalated_groups.push(step.add_skill_group.clone());
                break; // Only trigger one escalation step per check
            }
        }

        Ok(())
    }

    fn track_matches(token: Option<&PlaybackToken>, track_id: &str) -> bool {
        token.is_none_or(|t| t.track_id == track_id)
    }

    /// Play the transfer prompt on the first originate of this queue entry:
    /// the caller hears it while the agent is being dialed, before any
    /// connection. Caller-only; replaces the hold music.
    async fn maybe_start_transfer_prompt(
        &mut self,
        ctrl: &mut CallController,
    ) -> anyhow::Result<()> {
        if self.transfer_prompt_played {
            return Ok(());
        }
        let prompts = self
            .plan
            .voice_prompts
            .as_ref()
            .or(self.config.voice_prompts.as_ref());
        let Some(path) = prompts.and_then(|p| p.transfer_prompt.clone()) else {
            return Ok(());
        };
        self.transfer_prompt_played = true;

        self._stop_hold_music(ctrl).await;
        info!(
            queue = %self.config.name,
            file = %path,
            "Queue: playing transfer prompt before connecting agent"
        );
        let token = ctrl.play_audio_caller_only(path, false).await?;
        self.transfer_token = Some(token);
        self.state = QueueState::PlayingTransferPrompt {
            connected_agent: None,
        };
        Ok(())
    }

    /// Display name of the answering agent (registry lookup by URI, falling
    /// back to the URI user part).
    async fn resolve_agent_display_name(&self, agent_uri: &str) -> String {
        let user_part = agent_uri
            .strip_prefix("sips:")
            .or_else(|| agent_uri.strip_prefix("sip:"))
            .unwrap_or(agent_uri)
            .split('@')
            .next()
            .unwrap_or(agent_uri)
            .to_string();
        let Some(ref registry) = self.agent_registry else {
            return user_part;
        };
        let agents = registry.list_agents().await;
        let uri_user = |uri: &str| {
            uri.strip_prefix("sips:")
                .or_else(|| uri.strip_prefix("sip:"))
                .unwrap_or(uri)
                .split('@')
                .next()
                .unwrap_or(uri)
                .to_string()
        };
        agents
            .iter()
            .find(|a| a.uri == agent_uri)
            .or_else(|| agents.iter().find(|a| uri_user(&a.uri) == user_part))
            .map(|a| {
                if a.display_name.is_empty() {
                    user_part.clone()
                } else {
                    a.display_name.clone()
                }
            })
            .unwrap_or(user_part)
    }

    /// Post-connect flow: play the caller-only service prompt when configured,
    /// otherwise exit. The app exits once the prompt finishes.
    async fn play_service_prompt_or_exit(
        &mut self,
        ctrl: &mut CallController,
        agent_uri: String,
    ) -> anyhow::Result<AppAction> {
        if !self.answered {
            ctrl.answer().await?;
            self.answered = true;
        }

        let prompts = self
            .plan
            .voice_prompts
            .as_ref()
            .or(self.config.voice_prompts.as_ref());
        let Some(template) = prompts.and_then(|p| p.service_prompt.clone()) else {
            self.state = QueueState::Connected {
                agent_uri: agent_uri.clone(),
            };
            let queue_id = self.config.name.clone();
            let wait_secs = self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            info!(
                queue = %queue_id,
                agent = %agent_uri,
                wait_secs,
                "Queue: call connected to agent (exiting app, bridge is established by SipSession)"
            );
            return Ok(AppAction::Exit);
        };

        let agent_name = self.resolve_agent_display_name(&agent_uri).await;
        // Template = local audio path or http(s) URL; the agent name is
        // percent-encoded inside URLs.
        let is_url = template.starts_with("http://") || template.starts_with("https://");
        let replacement = if is_url {
            urlencoding::encode(&agent_name).into_owned()
        } else {
            agent_name.clone()
        };
        let path = template.replace("{agent}", &replacement);

        info!(
            queue = %self.config.name,
            agent = %agent_uri,
            agent_name = %agent_name,
            file = %path,
            "Queue: playing caller-only service prompt after connect"
        );
        let token = ctrl.play_audio_caller_only(path, false).await?;
        self.service_token = Some(token);
        self.state = QueueState::PlayingServicePrompt { agent_uri };
        Ok(AppAction::Continue)
    }
}

#[async_trait]
impl CallApp for QueueApp {
    fn app_type(&self) -> CallAppType {
        CallAppType::Queue
    }

    fn name(&self) -> &str {
        self.plan.label.as_deref().unwrap_or("queue")
    }

    async fn on_enter(
        &mut self,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let queue_id = self.config.name.clone();
        info!(queue = %queue_id, "Queue: entering queue application");
        self.state = QueueState::Answering;
        self.enqueued_at = Some(Instant::now());

        // Capture the RWI gateway so that `on_exit` (which has no context) can
        // still emit abandon events later in the lifecycle.
        self.rwi_gateway = ctx.rwi_gateway.clone();

        ctx.set_queue_name(&queue_id).await;

        // Notify external systems that the call entered the queue.
        self.emit_rwi(&crate::rwi::event::QueueJoined {
            call_id: self.call_id.clone(),
            queue_id: queue_id.clone(),
        });

        // Resolve agents dynamically if skill routing is enabled
        if self.config.skill_routing_enabled {
            self.resolve_agents().await;
        }

        // Check if we have agents configured
        let agents = self.get_agents();
        if agents.is_empty() {
            warn!("Queue: no agents configured, executing fallback");
            // Answer first if we need to play a busy prompt (needs media path)
            if !self.answered {
                let prompts = self
                    .plan
                    .voice_prompts
                    .as_ref()
                    .or(self.config.voice_prompts.as_ref());
                if prompts.and_then(|p| p.busy_prompt.as_ref()).is_some() {
                    ctrl.answer().await?;
                    self.answered = true;
                }
            }
            return self.play_busy_and_then_fallback(ctrl).await;
        }

        // Answer immediately if configured
        if self.plan.accept_immediately {
            info!("Queue: answering call immediately");
            ctrl.answer().await?;
            self.answered = true;
        }

        // Start hold music if configured
        self.start_hold_music(ctrl).await?;

        // Announce position if enabled
        if self.config.announce_position {
            self.announce_position(ctrl).await?;
        }

        // Start dialing agents if autonomous routing is enabled
        if self.config.autonomous_routing
            && let Some(ref registry) = self.agent_registry
        {
            let skills = &self.config.required_skills;
            let strategy = self.config.routing_strategy;

            if let Some(agent) = registry
                .select_agent_with_policy(skills, strategy, None, &self.call_id)
                .await
            {
                info!(agent_id = %agent.agent_id, uri = %agent.uri, "Queue: auto-selecting agent");

                // Update agent presence to ringing
                let _ = registry
                    .update_presence(
                        &agent.agent_id,
                        PresenceState::Ringing {
                            call_id: Some(self.call_id.clone()),
                        },
                    )
                    .await;

                // Originate call to agent
                let call_id = ctrl
                    .originate_call(&agent.uri, Some(self.call_id.clone()))
                    .await?;

                self.pending_agents.push((agent.uri.clone(), call_id));

                self.maybe_start_transfer_prompt(ctrl).await?;

                // Notify external systems
                ctrl.notify_event(
                    "queue.agent_ringing",
                    serde_json::json!({
                        "call_id": self.call_id,
                        "agent_id": agent.agent_id,
                        "agent_uri": agent.uri,
                        "queue_id": queue_id,
                    }),
                )
                .await?;

                // Emit RWI queue lifecycle event: an agent is being offered.
                self.emit_rwi(&crate::rwi::event::QueueAgentOffered {
                    call_id: self.call_id.clone(),
                    queue_id: queue_id.clone(),
                    agent_id: agent.agent_id.clone(),
                });

                self.state = QueueState::DialingAgents { attempt: 1 };
                self.dial_attempts = 1;

                // Set timeout for agent answer
                let ring_timeout = self.config.ring_timeout.unwrap_or(Duration::from_secs(20));
                ctrl.set_timeout("agent_ring_timeout", ring_timeout);

                return Ok(AppAction::Continue);
            } else {
                warn!("Queue: no available agents for skill routing");
                // Answer first if we need to play a busy prompt (needs media path)
                if !self.answered {
                    let prompts = self
                        .plan
                        .voice_prompts
                        .as_ref()
                        .or(self.config.voice_prompts.as_ref());
                    if prompts.and_then(|p| p.busy_prompt.as_ref()).is_some() {
                        ctrl.answer().await?;
                        self.answered = true;
                    }
                }
                return self.play_busy_and_then_fallback(ctrl).await;
            }
        }

        // Parallel mode: originate calls to ALL static agents concurrently.
        // When the first agent answers via agent_connected event, the rest
        // are cancelled via remove_legs.
        if self.is_parallel() {
            let agents = self.get_agents();
            if !agents.is_empty() {
                info!(
                    "Queue: originating {} parallel calls to static agents",
                    agents.len()
                );
                let mut pending = Vec::with_capacity(agents.len());
                for (idx, agent) in agents.iter().enumerate() {
                    let uri = agent.aor.to_string();
                    let leg_headers = agent.headers.clone().unwrap_or_default();
                    match ctrl
                        .originate_call_with_headers(&uri, Some(self.call_id.clone()), leg_headers)
                        .await
                    {
                        Ok(call_id) => {
                            info!(
                                index = idx,
                                call_id = %call_id,
                                "Queue: parallel originate to agent"
                            );
                            pending.push((uri, call_id));
                        }
                        Err(e) => {
                            warn!(
                                index = idx,
                                error = %e,
                                "Queue: failed to originate parallel call"
                            );
                        }
                    }
                }
                self.pending_agents = pending;

                self.state = QueueState::DialingAgents { attempt: 1 };
                self.dial_attempts = 1;

                let ring_timeout = self.config.ring_timeout.unwrap_or(Duration::from_secs(20));
                ctrl.set_timeout("agent_ring_timeout", ring_timeout);

                self.maybe_start_transfer_prompt(ctrl).await?;

                return Ok(AppAction::Continue);
            }
        }

        // Sequential mode: transition to DialingAgents state.
        // The production execute_flow will inject a "dial_next_agent" event
        // to kick off dialing. In tests, the test sends custom events manually.
        self.state = QueueState::DialingAgents { attempt: 1 };
        self.dial_attempts = 1;

        Ok(AppAction::Continue)
    }

    async fn on_dtmf(
        &mut self,
        _digit: String,
        _ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        // DTMF during queue hold is ignored (callback feature removed).
        Ok(AppAction::Continue)
    }

    async fn on_audio_complete(
        &mut self,
        track_id: String,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        debug!(track_id = %track_id, "Queue: audio playback completed");

        match &self.state {
            QueueState::PlayingHold { .. } | QueueState::DialingAgents { .. } => {
                if !Self::track_matches(self.hold_playback.as_ref(), &track_id) {
                    debug!(track_id = %track_id, "Queue: ignoring stale audio completion (hold)");
                    return Ok(AppAction::Continue);
                }
                // Hold music loop completed or starting — check comfort/EWT scheduling
                self.maybe_play_comfort_or_ewt(ctrl).await?;
                self.start_hold_music(ctrl).await?;
            }
            QueueState::PlayingTransferPrompt { connected_agent } => {
                if !Self::track_matches(self.transfer_token.as_ref(), &track_id) {
                    debug!(track_id = %track_id, "Queue: ignoring stale audio completion (transfer prompt)");
                    return Ok(AppAction::Continue);
                }
                self.transfer_token = None;
                match connected_agent {
                    // The prompt finished while the agent is still ringing —
                    // resume hold music and keep waiting for the answer.
                    None => {
                        info!(
                            "Queue: transfer prompt completed before agent answered, resuming hold music"
                        );
                        self.state = QueueState::DialingAgents {
                            attempt: self.dial_attempts,
                        };
                        self.start_hold_music(ctrl).await?;
                    }
                    // The agent answered while the prompt was playing and the
                    // connection flow already ran; this late natural completion
                    // only needs to advance past the prompt state.
                    Some(agent_uri) => {
                        let agent_uri = agent_uri.clone();
                        return self.play_service_prompt_or_exit(ctrl, agent_uri).await;
                    }
                }
            }
            QueueState::PlayingServicePrompt { agent_uri } => {
                if !Self::track_matches(self.service_token.as_ref(), &track_id) {
                    debug!(track_id = %track_id, "Queue: ignoring stale audio completion (service prompt)");
                    return Ok(AppAction::Continue);
                }
                self.service_token = None;
                let agent_uri = agent_uri.clone();
                self.state = QueueState::Connected {
                    agent_uri: agent_uri.clone(),
                };
                let queue_id = self.config.name.clone();
                let wait_secs = self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                info!(
                    queue = %queue_id,
                    agent = %agent_uri,
                    wait_secs,
                    "Queue: service prompt finished, call in progress"
                );
                return Ok(AppAction::Exit);
            }
            QueueState::PlayingBusyPrompt => {
                if !Self::track_matches(self.busy_token.as_ref(), &track_id) {
                    debug!(track_id = %track_id, "Queue: ignoring stale audio completion (busy prompt)");
                    return Ok(AppAction::Continue);
                }
                self.busy_token = None;
                return self.play_final_destination_prompt_or_fallback(ctrl).await;
            }
            QueueState::PlayingNoAnswerPrompt => {
                if !Self::track_matches(self.no_answer_token.as_ref(), &track_id) {
                    debug!(track_id = %track_id, "Queue: ignoring stale audio completion (no-answer prompt)");
                    return Ok(AppAction::Continue);
                }
                self.no_answer_token = None;
                return self.play_final_destination_prompt_or_fallback(ctrl).await;
            }
            QueueState::PlayingComfortPrompt => {
                if !Self::track_matches(self.comfort_token.as_ref(), &track_id) {
                    debug!(track_id = %track_id, "Queue: ignoring stale audio completion (comfort prompt)");
                    return Ok(AppAction::Continue);
                }
                self.comfort_token = None;
                // Return to hold music; next comfort will be scheduled by maybe_play_comfort_or_ewt
                self.start_hold_music(ctrl).await?;
            }
            QueueState::PlayingFinalPrompt => {
                if !Self::track_matches(self.final_token.as_ref(), &track_id) {
                    debug!(track_id = %track_id, "Queue: ignoring stale audio completion (final prompt)");
                    return Ok(AppAction::Continue);
                }
                self.final_token = None;
                return self.execute_fallback().await;
            }
            _ => {}
        }

        Ok(AppAction::Continue)
    }

    async fn on_external_event(
        &mut self,
        event: super::AppEvent,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let queue_id = self.config.name.clone();

        match event {
            super::AppEvent::Custom { name, data } => match name.as_str() {
                "agent_connected" => {
                    if let Some(agent_uri) = data.get("agent_uri").and_then(|v| v.as_str()) {
                        info!(agent = %agent_uri, "Queue: agent connected");
                        self._stop_hold_music(ctrl).await;

                        // In parallel mode, cancel all remaining pending agent legs
                        // EXCEPT the one that just answered.
                        if !self.pending_agents.is_empty() {
                            let mut other_legs: Vec<String> = Vec::new();
                            let all = std::mem::take(&mut self.pending_agents);
                            for (u, cid) in all {
                                if u != agent_uri {
                                    other_legs.push(cid);
                                }
                            }
                            if !other_legs.is_empty() {
                                info!(
                                    "Queue: cancelling {} non-answering parallel legs",
                                    other_legs.len()
                                );
                                ctrl.remove_legs(&other_legs);
                            }
                        }

                        if let Some(ref registry) = self.agent_registry {
                            let agent_id = data
                                .get("agent_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(agent_uri);
                            let _ = registry.start_call(agent_id).await;
                        }

                        // Emit RWI queue lifecycle events at connect time: agent
                        // connected, then the call left the queue (dequeue). This
                        // mirrors the ACD engine's Connected/CallDequeued emission.
                        let connected_agent_id = data
                            .get("agent_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or(agent_uri)
                            .to_string();
                        self.emit_rwi(&crate::rwi::event::QueueAgentConnected {
                            call_id: self.call_id.clone(),
                            queue_id: queue_id.clone(),
                            agent_id: connected_agent_id.clone(),
                        });
                        self.emit_rwi(&crate::rwi::event::QueueLeft {
                            call_id: self.call_id.clone(),
                            queue_id: queue_id.clone(),
                            reason: Some("connected".to_string()),
                        });

                        // The pre-connect transfer prompt may still be playing
                        // (it starts when dialing began). Connect immediately:
                        // cut the prompt — the interrupted completion is
                        // swallowed by the event loop, and any late natural
                        // completion is ignored via track-id matching.
                        if matches!(self.state, QueueState::PlayingTransferPrompt { .. }) {
                            info!(
                                "Queue: agent answered during transfer prompt — cutting prompt and connecting"
                            );
                            ctrl.stop_audio().await?;
                        }

                        // The agent is already connected via LegAdd/LegConnected and
                        // the media bridge is set up by SipSession. Play the
                        // caller-only service prompt if configured, then exit.
                        return self
                            .play_service_prompt_or_exit(ctrl, agent_uri.to_string())
                            .await;
                    }
                    Ok(AppAction::Continue)
                }
                "agent_ringing" => {
                    if let Some(agent_id) = data.get("agent_id").and_then(|v| v.as_str()) {
                        info!(agent = %agent_id, "Queue: agent ringing");

                        if let Some(ref registry) = self.agent_registry {
                            let _ = registry
                                .update_presence(
                                    agent_id,
                                    PresenceState::Ringing {
                                        call_id: Some(self.call_id.clone()),
                                    },
                                )
                                .await;
                        }

                        // Emit RWI queue lifecycle event: an agent is being offered.
                        self.emit_rwi(&crate::rwi::event::QueueAgentOffered {
                            call_id: self.call_id.clone(),
                            queue_id: queue_id.clone(),
                            agent_id: agent_id.to_string(),
                        });
                    }
                    Ok(AppAction::Continue)
                }
                "agent_busy" => {
                    info!("Queue: agent busy");
                    if let Some(agent_id) = data.get("agent_id").and_then(|v| v.as_str())
                        && let Some(ref registry) = self.agent_registry
                    {
                        let _ = registry
                            .update_presence(
                                agent_id,
                                PresenceState::Busy {
                                    call_id: Some(self.call_id.clone()),
                                },
                            )
                            .await;
                    }
                    self.handle_agent_unavailable(
                        ctrl,
                        AgentUnavailableReason::Busy,
                        data.get("leg_id").and_then(|value| value.as_str()),
                    )
                    .await
                }
                "agent_no_answer" => {
                    info!("Queue: agent no answer");
                    if let Some(agent_id) = data.get("agent_id").and_then(|v| v.as_str())
                        && let Some(ref registry) = self.agent_registry
                    {
                        let _ = registry
                            .update_presence(agent_id, PresenceState::Idle)
                            .await;
                    }
                    self.handle_agent_unavailable(
                        ctrl,
                        AgentUnavailableReason::NoAnswer,
                        data.get("leg_id").and_then(|value| value.as_str()),
                    )
                    .await
                }
                "all_agents_busy" => {
                    warn!("Queue: all agents busy");
                    self.play_busy_and_then_fallback(ctrl).await
                }
                "dial_next_agent" => self.dial_next_agent(ctrl).await,
                _ => Ok(AppAction::Continue),
            },
            _ => Ok(AppAction::Continue),
        }
    }

    async fn on_timeout(
        &mut self,
        id: String,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        match id.as_str() {
            "agent_ring_timeout" => {
                info!("Queue: agent ring timeout, handling no-answer");

                // Only reset agent(s) that THIS call is currently dialing.
                // Look up the canonical agent_id via the registry so URI
                // user-parts that differ from the registered agent_id are
                // handled correctly (e.g. DbRegistry custom URIs).
                let timed_out_uris: Vec<String> = std::mem::take(&mut self.pending_agents)
                    .into_iter()
                    .map(|(uri, _)| uri)
                    .collect();

                if let Some(ref registry) = self.agent_registry {
                    let all_agents = registry.list_agents().await;
                    for uri in &timed_out_uris {
                        let agent_id = all_agents
                            .iter()
                            .find(|a| a.uri == *uri)
                            .map(|a| a.agent_id.clone())
                            .unwrap_or_else(|| {
                                uri.strip_prefix("sip:")
                                    .and_then(|s| s.split('@').next())
                                    .unwrap_or(uri)
                                    .to_string()
                            });
                        let _ = registry
                            .update_presence(&agent_id, PresenceState::Idle)
                            .await;

                        ctrl.notify_event(
                            "queue.agent_no_answer",
                            serde_json::json!({
                                "call_id": self.call_id,
                                "agent_id": &agent_id,
                                "queue_id": self.config.name,
                            }),
                        )
                        .await?;

                        self.emit_rwi(&crate::rwi::event::QueueAgentNoAnswer {
                            call_id: self.call_id.clone(),
                            queue_id: self.config.name.clone(),
                            agent_id: agent_id.clone(),
                            attempt: self.dial_attempts,
                            trace_id: self.call_id.clone(),
                        });
                    }
                }

                self.handle_agent_unavailable(ctrl, AgentUnavailableReason::NoAnswer, None)
                    .await
            }
            "max_wait_timeout" => {
                info!("Queue: max wait timeout, executing fallback");

                // Notify queue timeout
                ctrl.notify_event(
                    "queue.timeout",
                    serde_json::json!({
                        "call_id": self.call_id,
                        "queue_id": self.config.name,
                        "wait_secs": self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0),
                    }),
                )
                .await?;

                // Notify the skill-group dispatcher that the wait timed out.
                let wait_secs = self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                self.notify_timeout(wait_secs).await;

                // Emit RWI queue lifecycle event: the wait timed out.
                self.emit_rwi(&crate::rwi::event::QueueWaitTimeout {
                    call_id: self.call_id.clone(),
                    queue_id: self.config.name.clone(),
                });

                self.play_busy_and_then_fallback(ctrl).await
            }
            "escalation_check" => {
                debug!("Queue: escalation check");
                self.check_escalation(ctrl).await?;
                // Re-register the escalation timer
                if !self.config.escalation_timeline.is_empty() {
                    ctrl.set_timeout("escalation_check", Duration::from_secs(10));
                }
                Ok(AppAction::Continue)
            }
            _ => Ok(AppAction::Continue),
        }
    }

    async fn on_exit(&mut self, reason: super::ExitReason) -> anyhow::Result<()> {
        info!(?reason, "Queue: exiting queue application");

        // Update statistics if call was not connected (abandoned). A transfer
        // prompt with `connected_agent: Some(_)` means the agent already
        // answered; `None` means the caller left while the agent was ringing.
        let was_connected = matches!(
            self.state,
            QueueState::Connected { .. } | QueueState::PlayingServicePrompt { .. }
        ) || matches!(
            self.state,
            QueueState::PlayingTransferPrompt {
                connected_agent: Some(_)
            }
        );
        if !was_connected && !self.abandoned_recorded {
            let queue_id = self.config.name.clone();
            // Notify the skill-group dispatcher that the call was abandoned
            // (e.g. caller hung up while waiting).
            let wait_secs = self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            self.notify_abandoned(wait_secs).await;

            // Emit RWI queue lifecycle event: the caller abandoned (e.g. hung
            // up while waiting). The gateway was captured in `on_enter`.
            // Guarded so that already-connected calls don't emit a duplicate
            // abandon (they emit QueueLeft{reason:"connected"} instead).
            self.emit_rwi(&crate::rwi::event::QueueLeft {
                call_id: self.call_id.clone(),
                queue_id,
                reason: Some("abandoned".to_string()),
            });
        }

        self.state = QueueState::Done;
        Ok(())
    }
}
