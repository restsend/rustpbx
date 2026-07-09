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
use super::{AppAction, ApplicationContext, CallApp, CallAppType, CallController, PlaybackHandle};
use crate::call::{
    DialStrategy, FailureAction, Location, QueueFallbackAction, QueueHoldConfig, QueuePlan,
    VoicePrompts,
};
use crate::callrecord::CallRecordHangupReason;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ===================================================================
// Queue Statistics (built-in)
// ===================================================================

/// Statistics for a queue.
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    pub queue_id: String,
    pub calls_offered: u64,
    pub calls_answered: u64,
    pub calls_abandoned: u64,
    pub total_wait_secs: u64,
    pub total_handle_secs: u64,
    pub current_waiting: u32,
    pub available_agents: u32,
}

impl QueueStats {
    pub fn avg_wait_secs(&self) -> f64 {
        if self.calls_answered > 0 {
            self.total_wait_secs as f64 / self.calls_answered as f64
        } else {
            0.0
        }
    }

    pub fn avg_handle_secs(&self) -> f64 {
        if self.calls_answered > 0 {
            self.total_handle_secs as f64 / self.calls_answered as f64
        } else {
            0.0
        }
    }

    pub fn service_level(&self, _threshold_secs: u64) -> f64 {
        // Simplified SLA calculation
        // In production, track individual call wait times
        if self.calls_offered > 0 {
            let sla_calls = self.calls_answered; // Simplified
            (sla_calls as f64 / self.calls_offered as f64) * 100.0
        } else {
            100.0
        }
    }
}

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
    /// Enable estimated wait time announcements.
    pub announce_wait_time: bool,
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
    /// Enable callback scheduling.
    pub callback_enabled: bool,
    /// Callback retry interval in seconds.
    pub callback_retry_secs: u64,
    /// Built-in voice prompts for queue events.
    pub voice_prompts: Option<VoicePrompts>,
    // ── 按键回拨 (Queue Callback on Request) ──
    /// Enable DTMF callback request feature.
    pub callback_request_enabled: bool,
    /// Seconds to wait before offering callback option.
    pub callback_offer_after_secs: u64,
    /// DTMF key to trigger callback (default "2").
    pub callback_dtmf_key: String,
    // ── EWT 播报 (Estimated Wait Time) ──
    /// Interval between EWT announcements in seconds. 0 = once only.
    pub ewt_announce_interval_secs: u64,
    /// Maximum EWT cap in seconds (default 1800 = 30 min).
    pub ewt_max_secs: u64,
    // ── 升级策略 (Escalation) ──
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
            announce_wait_time: false,
            retry_interval_secs: 5,
            max_retries: 2,
            autonomous_routing: false,
            routing_strategy: RoutingStrategy::LongestIdle,
            no_answer_action: NoAnswerAction::Voicemail,
            fallback_skill_group: None,
            sla_monitoring: false,
            metrics_enabled: false,
            callback_enabled: false,
            callback_retry_secs: 300,
            voice_prompts: None,
            callback_request_enabled: false,
            callback_offer_after_secs: 30,
            callback_dtmf_key: "2".to_string(),
            ewt_announce_interval_secs: 0,
            ewt_max_secs: 1800,
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
            retry_codes: None,
            no_trying_timeout: None,
            voice_prompts: self.voice_prompts.clone(),
            queue_name: self.name.clone(),
            failure_audio: None,
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
    /// Playing the transfer prompt before connecting to an agent.
    PlayingTransferPrompt { agent_uri: String },
    /// Playing the busy prompt before executing fallback.
    PlayingBusyPrompt,
    /// Playing the no-answer prompt before executing fallback.
    PlayingNoAnswerPrompt,
    /// Dialing agents (sequential or parallel).
    DialingAgents { attempt: u32 },
    /// Call connected to an agent.
    Connected { agent_uri: String },
    /// Executing fallback action.
    ExecutingFallback,
    /// Playing the callback confirmation prompt after DTMF.
    PlayingCallbackConfirm,
    /// Playing comfort/reassurance prompt during hold.
    PlayingComfortPrompt,
    /// Playing EWT announcement during hold.
    PlayingEwtAnnouncement,
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
    hold_playback: Option<PlaybackHandle>,
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
    stats: Arc<RwLock<HashMap<String, QueueStats>>>,
    /// (agent_uri, call_id) for agents being dialed concurrently (parallel mode).
    /// When the first agent answers, the rest are cancelled via LegRemove.
    pending_agents: Vec<(String, String)>,
    // ── 回调/回拨 ──
    /// Whether the caller has requested a callback via DTMF.
    callback_requested: bool,
    /// Already-offered the callback option (to avoid repeating).
    #[allow(dead_code)]
    callback_offered: bool,
    // ── Comfort / EWT 播报 ──
    /// Next EWT announcement time.
    next_ewt_announce: Option<Instant>,
    /// Comfort prompt playback state.
    comfort_index: usize,
    last_comfort_played: Option<Instant>,
    // ── 升级策略 ──
    /// Skill groups already escalated (to avoid duplicates).
    escalated_groups: Vec<String>,
    /// RWI gateway captured from the application context (for queue lifecycle
    /// webhook events). Captured in `on_enter` so that `on_exit` (which has no
    /// context) can still emit abandon events.
    rwi_gateway: Option<crate::rwi::RwiGatewayRef>,
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
            stats: Arc::new(RwLock::new(HashMap::new())),
            pending_agents: Vec::new(),
            callback_requested: false,
            callback_offered: false,
            next_ewt_announce: None,
            comfort_index: 0,
            last_comfort_played: None,
            escalated_groups: Vec::new(),
            rwi_gateway: None,
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

    /// Set queue statistics tracker.
    pub fn with_stats(mut self, stats: Arc<RwLock<HashMap<String, QueueStats>>>) -> Self {
        self.stats = stats;
        self
    }

    /// Broadcast a queue lifecycle RWI event via the gateway (if captured).
    /// Mirrors the ACD engine bridge in `cc/mod.rs` (`to_legacy_event` +
    /// `broadcast_event`) so that queue events look identical regardless of
    /// which subsystem generated them.
    fn emit_rwi<E: crate::rwi::RwiEventSpec>(&self, event: &E) {
        if let Some(ref gw) = self.rwi_gateway {
            let gw = gw.read();
            gw.broadcast_event(&crate::rwi::event::to_legacy_event(event, None));
        }
    }

    /// Update queue statistics.
    async fn update_stats(&self, queue_id: &str, f: impl FnOnce(&mut QueueStats)) {
        let mut stats = self.stats.write().await;
        let stat = stats
            .entry(queue_id.to_string())
            .or_insert_with(|| QueueStats {
                queue_id: queue_id.to_string(),
                ..Default::default()
            });
        f(stat);
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
                if name.starts_with("skill-group:") {
                    let skill_group_id = name.strip_prefix("skill-group:").unwrap_or(name).trim();
                    info!(skill_group = %skill_group_id, "Queue: fallback to skill group");
                    AppAction::Transfer(format!("skill-group:{}", skill_group_id))
                } else {
                    info!(queue = %name, "Queue: fallback to another queue");
                    AppAction::Transfer(format!("queue:{}", name))
                }
            }
            None => AppAction::Hangup {
                reason: Some(CallRecordHangupReason::ServerUnavailable),
                code: Some(486),
            },
        };

        // Emit RWI queue lifecycle event: a fallback action was executed.
        let action_label = match &action {
            AppAction::Transfer(t) => format!("transfer:{}", t),
            AppAction::Hangup { .. } => "hangup".to_string(),
            _ => "other".to_string(),
        };
        self.emit_rwi(&crate::rwi::event::QueueFallbackExecuted {
            call_id: self.call_id.clone(),
            queue_id: self.config.name.clone(),
            action: action_label,
            reason: "no_agent".to_string(),
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
    async fn _stop_hold_music(&mut self) {
        if self.hold_playback.take().is_some() {
            debug!("Queue: stopping hold music");
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

    /// Announce estimated wait time.
    ///
    /// Plays `voice_prompts.wait_time_prompt` if configured; otherwise emits a
    /// warning so operators know the announcement was requested but unconfigured.
    async fn announce_wait_time(&self, ctrl: &mut CallController) -> anyhow::Result<()> {
        let prompts = self
            .plan
            .voice_prompts
            .as_ref()
            .or(self.config.voice_prompts.as_ref());

        if let Some(path) = prompts.and_then(|p| p.wait_time_prompt.as_ref()) {
            debug!(file = %path, "Queue: playing wait-time announcement");
            ctrl.play_audio(path.clone(), false).await?;
        } else {
            warn!(
                queue = %self.config.name,
                "Queue: announce_wait_time is enabled but voice_prompts.wait_time_prompt \
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
    ) -> anyhow::Result<AppAction> {
        if self.is_parallel() {
            self.pending_agents.clear();
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

        self.update_stats(&queue_id, |stats| {
            stats.calls_abandoned += 1;
        })
        .await;

        info!(
            queue = %queue_id,
            wait_secs,
            "Queue: call abandoned, playing busy prompt or fallback"
        );

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
            ctrl.play_audio(path.clone(), false).await?;
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

        self.update_stats(&queue_id, |stats| {
            stats.calls_abandoned += 1;
        })
        .await;

        info!(
            queue = %queue_id,
            wait_secs,
            "Queue: call abandoned, playing no-answer prompt or fallback"
        );

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
            ctrl.play_audio(path.clone(), false).await?;
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
            ctrl.play_audio(path.clone(), false).await?;
            return Ok(AppAction::Continue);
        }
        self.execute_fallback().await
    }

    /// Write callback metadata (notified as queue event; CDR hook captures externally).
    async fn write_callback_metadata(&self) {
        info!(
            queue = %self.config.name,
            "Queue: callback requested — emitting queue.callback_requested event"
        );
        // In production, the callback request is captured by the external CDR/webhook
        // system via the queue.callback_requested event notification.
        // The QueueApp does not have direct access to session extensions (that
        // requires the production SipSession which owns the extensions type-map).
    }

    /// Check and play comfort prompts or EWT announcements between hold music loops.
    async fn maybe_play_comfort_or_ewt(&mut self, ctrl: &mut CallController) -> anyhow::Result<()> {
        let now = Instant::now();

        // 1. EWT periodic announcement
        if self.config.announce_wait_time && self.config.ewt_announce_interval_secs > 0 {
            let should_announce = match self.next_ewt_announce {
                Some(next) => now >= next,
                None => {
                    // First time: schedule and play immediately
                    self.next_ewt_announce =
                        Some(now + Duration::from_secs(self.config.ewt_announce_interval_secs));
                    true
                }
            };
            if should_announce {
                let ewt_secs = self.calculate_ewt().await;
                debug!(
                    ewt_secs,
                    "Queue: playing EWT announcement (pre-recorded fallback)"
                );
                // Note: the production queue should wire a `TtsService` here to synthesize
                // "Your estimated wait time is N minutes" dynamically. QueueApp is a
                // testable harness; the real TTS integration belongs in the production
                // SipSession::execute_queue which has access to the TTS configuration.
                let prompts = self
                    .plan
                    .voice_prompts
                    .as_ref()
                    .or(self.config.voice_prompts.as_ref());
                if let Some(path) = prompts.and_then(|p| p.wait_time_prompt.as_ref()) {
                    self.state = QueueState::PlayingEwtAnnouncement;
                    ctrl.play_audio(path.clone(), false).await?;
                    return Ok(());
                }
                self.next_ewt_announce =
                    Some(now + Duration::from_secs(self.config.ewt_announce_interval_secs));
                return Ok(());
            }
        }

        // 2. Comfort prompts
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
                    ctrl.play_audio(prompt.audio_file.clone(), false).await?;
                    self.comfort_index += 1;
                    self.last_comfort_played = Some(now);
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    /// Calculate a simple EWT estimate based on queue statistics.
    async fn calculate_ewt(&self) -> u64 {
        let stats = self.stats.read().await;
        if let Some(qs) = stats.get(&self.config.name) {
            if qs.available_agents > 0 && qs.current_waiting > 0 {
                let avg = if qs.calls_answered > 0 {
                    qs.total_wait_secs / qs.calls_answered.max(1)
                } else {
                    60 // default 60s if no historical data
                };
                let ewt = (qs.current_waiting as u64 * avg) / qs.available_agents as u64;
                // Round to nearest 10, cap at ewt_max_secs
                let rounded = ((ewt + 5) / 10) * 10;
                return rounded.min(self.config.ewt_max_secs);
            }
        }
        self.config.ewt_max_secs
    }

    /// Dial the next agent in a sequential dialing strategy.
    async fn dial_next_agent(&mut self, ctrl: &mut CallController) -> anyhow::Result<AppAction> {
        let agents = self.get_agents();
        if self.current_agent_idx >= agents.len() {
            warn!("Queue: no more agents to dial");
            return self.play_busy_and_then_fallback(ctrl).await;
        }
        let uri = agents[self.current_agent_idx]
            .contact_raw
            .clone()
            .unwrap_or_else(|| agents[self.current_agent_idx].aor.to_string());
        info!(
            "Queue: dialing next agent {} (idx={})",
            uri, self.current_agent_idx
        );
        match ctrl.originate_call(&uri, Some(self.call_id.clone())).await {
            Ok(call_id) => {
                self.pending_agents.push((uri, call_id));
            }
            Err(e) => {
                warn!("Queue: failed to dial agent {}: {}", uri, e);
                return self.play_busy_and_then_fallback(ctrl).await;
            }
        }
        let ring_timeout = self.config.ring_timeout.unwrap_or(Duration::from_secs(20));
        ctrl.set_timeout("agent_ring_timeout", ring_timeout);
        self.state = QueueState::DialingAgents {
            attempt: self.dial_attempts,
        };
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

        // Record call offered
        self.update_stats(&queue_id, |stats| {
            stats.calls_offered += 1;
            stats.current_waiting += 1;
        })
        .await;

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

        // Announce wait time if enabled
        if self.config.announce_wait_time {
            self.announce_wait_time(ctrl).await?;
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
                let _call_id = ctrl
                    .originate_call(&agent.uri, Some(self.call_id.clone()))
                    .await?;

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
                    let uri = agent
                        .contact_raw
                        .clone()
                        .unwrap_or_else(|| agent.aor.to_string());
                    match ctrl.originate_call(&uri, Some(self.call_id.clone())).await {
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
        digit: String,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if !matches!(
            self.state,
            QueueState::PlayingHold { .. } | QueueState::DialingAgents { .. }
        ) {
            return Ok(AppAction::Continue);
        }

        // Check callback eligibility
        if !self.config.callback_request_enabled || digit != self.config.callback_dtmf_key {
            return Ok(AppAction::Continue);
        }

        if let Some(enqueued) = self.enqueued_at {
            let wait_secs = enqueued.elapsed().as_secs();
            if wait_secs < self.config.callback_offer_after_secs {
                debug!(
                    "Queue: callback DTMF received but too early ({}s < {}s)",
                    wait_secs, self.config.callback_offer_after_secs
                );
                return Ok(AppAction::Continue);
            }
        }

        info!(
            queue = %self.config.name,
            digit = %digit,
            "Queue: callback requested via DTMF"
        );
        self.callback_requested = true;
        self._stop_hold_music().await;

        let prompts = self
            .plan
            .voice_prompts
            .as_ref()
            .or(self.config.voice_prompts.as_ref());
        if let Some(path) = prompts.and_then(|p| p.callback_confirm_prompt.as_ref()) {
            info!("Queue: playing callback confirmation prompt");
            self.state = QueueState::PlayingCallbackConfirm;
            ctrl.play_audio(path.clone(), false).await?;
        } else {
            // No confirmation prompt configured — request callback directly
            self.write_callback_metadata().await;
            self.state = QueueState::Done;
            info!("Queue: hanging up after callback request (no prompt)");
            return Ok(AppAction::Hangup {
                reason: Some(CallRecordHangupReason::BySystem),
                code: None,
            });
        }

        Ok(AppAction::Continue)
    }

    async fn on_audio_complete(
        &mut self,
        _track_id: String,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        debug!("Queue: audio playback completed");

        match &self.state {
            QueueState::PlayingHold { .. } | QueueState::DialingAgents { .. } => {
                // Hold music loop completed or starting — check comfort/EWT scheduling
                self.maybe_play_comfort_or_ewt(ctrl).await?;
                self.start_hold_music(ctrl).await?;
            }
            QueueState::PlayingTransferPrompt { agent_uri } => {
                let agent_uri = agent_uri.clone();
                // Answer the caller if not already answered
                if !self.answered {
                    ctrl.answer().await?;
                    self.answered = true;
                }
                self.state = QueueState::Connected {
                    agent_uri: agent_uri.clone(),
                };
                let queue_id = self.config.name.clone();
                let wait_secs = self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                info!(
                    queue = %queue_id,
                    agent = %agent_uri,
                    wait_secs,
                    "Queue: call connected to agent (after prompt)"
                );
                return Ok(AppAction::Exit);
            }
            QueueState::PlayingBusyPrompt => {
                return self.play_final_destination_prompt_or_fallback(ctrl).await;
            }
            QueueState::PlayingNoAnswerPrompt => {
                return self.play_final_destination_prompt_or_fallback(ctrl).await;
            }
            QueueState::PlayingCallbackConfirm => {
                // Write callback metadata to CDR
                info!("Queue: callback confirmation done, logging to CDR");
                self.write_callback_metadata().await;
                self.state = QueueState::Done;
                return Ok(AppAction::Hangup {
                    reason: Some(CallRecordHangupReason::BySystem),
                    code: None,
                });
            }
            QueueState::PlayingComfortPrompt => {
                // Return to hold music; next comfort will be scheduled by maybe_play_comfort_or_ewt
                self.start_hold_music(ctrl).await?;
            }
            QueueState::PlayingEwtAnnouncement => {
                // Schedule next EWT announcement
                if self.config.ewt_announce_interval_secs > 0 {
                    self.next_ewt_announce = Some(
                        Instant::now()
                            + Duration::from_secs(self.config.ewt_announce_interval_secs),
                    );
                }
                self.start_hold_music(ctrl).await?;
            }
            QueueState::PlayingFinalPrompt => {
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
                        self._stop_hold_music().await;

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

                        let wait_secs =
                            self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);

                        self.update_stats(&queue_id, |stats| {
                            stats.calls_answered += 1;
                            stats.total_wait_secs += wait_secs;
                            stats.current_waiting = stats.current_waiting.saturating_sub(1);
                        })
                        .await;

                        if let Some(ref registry) = self.agent_registry {
                            let agent_id = data
                                .get("agent_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(agent_uri);
                            let _ = registry.start_call(agent_id).await;
                        }

                        let prompts = self
                            .plan
                            .voice_prompts
                            .as_ref()
                            .or(self.config.voice_prompts.as_ref());
                        if let Some(path) = prompts.and_then(|p| p.transfer_prompt.as_ref()) {
                            info!("Queue: playing transfer prompt before connecting agent");
                            self.state = QueueState::PlayingTransferPrompt {
                                agent_uri: agent_uri.to_string(),
                            };
                            ctrl.play_audio(path.clone(), false).await?;
                            return Ok(AppAction::Continue);
                        }

                        self.state = QueueState::Connected {
                            agent_uri: agent_uri.to_string(),
                        };

                        // Answer the caller if not already answered (needed when accept_immediately=false)
                        if !self.answered {
                            ctrl.answer().await?;
                            self.answered = true;
                        }

                        info!(
                            queue = %queue_id,
                            agent = %agent_uri,
                            wait_secs,
                            "Queue: call connected to agent (exiting app, bridge is established by SipSession)"
                        );

                        // Emit RWI queue lifecycle events: agent connected, then
                        // the call left the queue (dequeue). This mirrors the
                        // ACD engine's Connected/CallDequeued emission.
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

                        // Exit the app — the agent is already connected via LegAdd/LegConnected
                        // and the media bridge is set up by SipSession's update_media_path().
                        // No need for Transfer (which would create a new call).
                        return Ok(AppAction::Exit);
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
                    self.handle_agent_unavailable(ctrl, AgentUnavailableReason::Busy)
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
                    self.handle_agent_unavailable(ctrl, AgentUnavailableReason::NoAnswer)
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

                if let Some(ref registry) = self.agent_registry {
                    let agents = registry.list_agents().await;
                    for agent in agents {
                        if matches!(agent.presence, PresenceState::Ringing { .. }) {
                            let _ = registry
                                .update_presence(&agent.agent_id, PresenceState::Idle)
                                .await;

                            ctrl.notify_event(
                                "queue.agent_no_answer",
                                serde_json::json!({
                                    "call_id": self.call_id,
                                    "agent_id": agent.agent_id,
                                    "queue_id": self.config.name,
                                }),
                            )
                            .await?;

                            break;
                        }
                    }
                }

                self.handle_agent_unavailable(ctrl, AgentUnavailableReason::NoAnswer)
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

        // Update statistics if call was not connected (abandoned)
        if !matches!(
            self.state,
            QueueState::Connected { .. } | QueueState::PlayingTransferPrompt { .. }
        ) {
            let queue_id = self.config.name.clone();

            self.update_stats(&queue_id, |stats| {
                stats.current_waiting = stats.current_waiting.saturating_sub(1);
            })
            .await;

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

/// Build a QueueApp from a QueuePlan and QueueConfig.
pub fn build_queue_app(plan: QueuePlan, config: QueueConfig) -> QueueApp {
    QueueApp::new(plan, config)
}
