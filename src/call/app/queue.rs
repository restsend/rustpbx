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
    /// Optional ACD policy name.
    pub acd_policy: Option<String>,
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
            acd_policy: None,
            no_answer_action: NoAnswerAction::Voicemail,
            fallback_skill_group: None,
            sla_monitoring: false,
            metrics_enabled: false,
            callback_enabled: false,
            callback_retry_secs: 300,
            voice_prompts: None,
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
            acd_policy: self.acd_policy.clone(),
            label: Some(self.name.clone()),
            retry_codes: None,
            no_trying_timeout: None,
            voice_prompts: self.voice_prompts.clone(),
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
    async fn announce_position(&self, _ctrl: &mut CallController) -> anyhow::Result<()> {
        // TODO: Implement position announcement using TTS
        // For now, just log
        debug!("Queue: position announcement (not implemented)");
        Ok(())
    }

    /// Announce estimated wait time.
    async fn announce_wait_time(&self, _ctrl: &mut CallController) -> anyhow::Result<()> {
        // TODO: Implement wait time announcement using TTS
        // For now, just log
        debug!("Queue: wait time announcement (not implemented)");
        Ok(())
    }

    /// Handle agent unavailable (busy or no answer).
    async fn handle_agent_unavailable(
        &mut self,
        ctrl: &mut CallController,
    ) -> anyhow::Result<AppAction> {
        if !self.is_parallel() {
            self.current_agent_idx += 1;
        }
        self.dial_attempts += 1;

        let agents = self.get_agents();
        if self.current_agent_idx >= agents.len() {
            return self.play_busy_and_then_fallback(ctrl).await;
        }

        self.state = QueueState::DialingAgents {
            attempt: self.dial_attempts,
        };
        Ok(AppAction::Continue)
    }

    async fn handle_fallback_with_abandoned(&mut self) -> anyhow::Result<AppAction> {
        let queue_id = self.config.name.clone();
        let wait_secs = self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);

        self.update_stats(&queue_id, |stats| {
            stats.calls_abandoned += 1;
        })
        .await;

        info!(
            queue = %queue_id,
            wait_secs,
            "Queue: call abandoned, executing fallback"
        );

        self.execute_fallback().await
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

        self.execute_fallback().await
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
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let queue_id = self.config.name.clone();
        info!(queue = %queue_id, "Queue: entering queue application");
        self.state = QueueState::Answering;
        self.enqueued_at = Some(Instant::now());

        // Record call offered
        self.update_stats(&queue_id, |stats| {
            stats.calls_offered += 1;
            stats.current_waiting += 1;
        })
        .await;

        // Resolve agents dynamically if skill routing is enabled
        if self.config.skill_routing_enabled {
            self.resolve_agents().await;
        }

        // Check if we have agents configured
        let agents = self.get_agents();
        if agents.is_empty() {
            warn!("Queue: no agents configured, executing fallback");
            return self.handle_fallback_with_abandoned().await;
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
                .select_agent_with_policy(skills, strategy, self.config.acd_policy.as_deref())
                .await
            {
                info!(agent_id = %agent.agent_id, uri = %agent.uri, "Queue: auto-selecting agent");

                // Update agent presence to ringing
                let _ = registry
                    .update_presence(&agent.agent_id, PresenceState::Ringing)
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

                self.state = QueueState::DialingAgents { attempt: 1 };
                self.dial_attempts = 1;

                // Set timeout for agent answer
                let ring_timeout = self.config.ring_timeout.unwrap_or(Duration::from_secs(20));
                ctrl.set_timeout("agent_ring_timeout", ring_timeout);

                return Ok(AppAction::Continue);
            } else {
                warn!("Queue: no available agents for skill routing");
                return self.handle_fallback_with_abandoned().await;
            }
        }

        // Transition to dialing state (for static agent configuration)
        self.state = QueueState::DialingAgents { attempt: 1 };
        self.dial_attempts = 1;

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
            QueueState::PlayingHold { .. } => {
                self.start_hold_music(ctrl).await?;
            }
            QueueState::PlayingTransferPrompt { agent_uri } => {
                let agent_uri = agent_uri.clone();
                self.state = QueueState::Connected {
                    agent_uri: agent_uri.clone(),
                };
                let queue_id = self.config.name.clone();
                let wait_secs =
                    self.enqueued_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                info!(
                    queue = %queue_id,
                    agent = %agent_uri,
                    wait_secs,
                    "Queue: call connected to agent (after prompt)"
                );
                return Ok(AppAction::Transfer(agent_uri));
            }
            QueueState::PlayingBusyPrompt => {
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
                        if let Some(path) =
                            prompts.and_then(|p| p.transfer_prompt.as_ref())
                        {
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

                        info!(
                            queue = %queue_id,
                            agent = %agent_uri,
                            wait_secs,
                            "Queue: call connected to agent"
                        );

                        return Ok(AppAction::Transfer(agent_uri.to_string()));
                    }
                    Ok(AppAction::Continue)
                }
                "agent_ringing" => {
                    if let Some(agent_id) = data.get("agent_id").and_then(|v| v.as_str()) {
                        info!(agent = %agent_id, "Queue: agent ringing");

                        if let Some(ref registry) = self.agent_registry {
                            let _ = registry
                                .update_presence(agent_id, PresenceState::Ringing)
                                .await;
                        }
                    }
                    Ok(AppAction::Continue)
                }
                "agent_busy" => {
                    info!("Queue: agent busy");
                    if let Some(agent_id) = data.get("agent_id").and_then(|v| v.as_str())
                        && let Some(ref registry) = self.agent_registry
                    {
                        let _ = registry
                            .update_presence(agent_id, PresenceState::Busy)
                            .await;
                    }
                    self.handle_agent_unavailable(ctrl).await
                }
                "agent_no_answer" => {
                    info!("Queue: agent no answer");
                    if let Some(agent_id) = data.get("agent_id").and_then(|v| v.as_str())
                        && let Some(ref registry) = self.agent_registry
                    {
                        let _ = registry
                            .update_presence(agent_id, PresenceState::Available)
                            .await;
                    }
                    self.handle_agent_unavailable(ctrl).await
                }
                "all_agents_busy" => {
                    warn!("Queue: all agents busy");
                    self.play_busy_and_then_fallback(ctrl).await
                }
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

                // Notify that agent didn't answer
                if let Some(ref registry) = self.agent_registry {
                    // Find the agent that was ringing and set back to available
                    let agents = registry.list_agents().await;
                    for agent in agents {
                        if matches!(agent.presence, PresenceState::Ringing) {
                            let _ = registry
                                .update_presence(&agent.agent_id, PresenceState::Available)
                                .await;

                            // Notify external systems
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

                self.handle_agent_unavailable(ctrl).await
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

                self.play_busy_and_then_fallback(ctrl).await
            }
            _ => Ok(AppAction::Continue),
        }
    }

    async fn on_exit(&mut self, reason: super::ExitReason) -> anyhow::Result<()> {
        info!(?reason, "Queue: exiting queue application");

        // Update statistics if call was not connected (abandoned)
        if !matches!(self.state, QueueState::Connected { .. } | QueueState::PlayingTransferPrompt { .. }) {
            let queue_id = self.config.name.clone();

            self.update_stats(&queue_id, |stats| {
                stats.current_waiting = stats.current_waiting.saturating_sub(1);
            })
            .await;
        }

        self.state = QueueState::Done;
        Ok(())
    }
}

/// Build a QueueApp from a QueuePlan and QueueConfig.
pub fn build_queue_app(plan: QueuePlan, config: QueueConfig) -> QueueApp {
    QueueApp::new(plan, config)
}
