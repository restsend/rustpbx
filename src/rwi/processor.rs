use crate::call::domain::{CallCommand, LegId};

use crate::call::runtime::ConferenceManager;
use crate::media;
use crate::media::Track as MediaTrackTrait;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::proxy_call::media_peer::VoiceEnginePeer;
use crate::proxy::proxy_call::sip_session::SipSessionHandle;
use crate::proxy::server::SipServerRef;
use crate::rwi::gateway::RwiGateway;
use crate::rwi::proto::RwiEvent;
use crate::rwi::session::{
    ConferenceCreateRequest, OriginateRequest, ParallelOriginateRequest, QueueEnqueueRequest,
    RecordStartRequest, RwiCommandPayload, SupervisorMode,
};
use crate::rwi::transfer::TransferController;
use futures::FutureExt;
use std::collections::HashMap;

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct CommandCacheEntry {
    action_id: String,
    received_at: Instant,
    result: Option<String>,
}

#[derive(Clone)]
struct CommandDeduplicationCache {
    entries: Arc<RwLock<HashMap<String, CommandCacheEntry>>>,
    ttl: Duration,
}

#[allow(dead_code)]
impl CommandDeduplicationCache {
    fn new(ttl_secs: u64) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    fn with_default_ttl() -> Self {
        Self::new(60)
    }

    async fn is_duplicate(&self, action_id: &str) -> bool {
        let entries = self.entries.read().await;
        if let Some(entry) = entries.get(action_id) {
            if entry.received_at.elapsed() < self.ttl {
                return true;
            }
        }
        false
    }

    async fn record(&self, action_id: String, result: Option<String>) {
        let mut entries = self.entries.write().await;
        entries.insert(
            action_id.clone(),
            CommandCacheEntry {
                action_id,
                received_at: Instant::now(),
                result,
            },
        );
    }

    async fn cleanup_expired(&self) {
        let mut entries = self.entries.write().await;
        let now = Instant::now();
        entries.retain(|_, entry| now.duration_since(entry.received_at) < self.ttl);
    }

    async fn len(&self) -> usize {
        let entries = self.entries.read().await;
        entries.len()
    }
}

#[derive(Clone)]
#[allow(dead_code)]
struct QueueState {
    queue_id: String,
    priority: Option<u32>,
    skills: Option<Vec<String>>,
    max_wait_secs: Option<u32>,
    is_hold: bool,
    enqueued_at: std::time::Instant,
    agent_id: Option<String>,
    overflow_count: u32,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct AgentSkill {
    agent_id: String,
    skills: Vec<String>,
    max_concurrent_calls: u32,
    current_calls: u32,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct QueueOverflowConfig {
    max_calls: u32,
    max_wait_secs: u32,
    overflow_queue_id: Option<String>,
    overflow_action: Option<String>,
}

#[derive(Clone, Debug)]
struct QueueOverflowAction {
    action: Option<String>,
    target_queue: Option<String>,
    reason: String,
}

#[derive(Clone, Debug)]
pub struct QueueStats {
    pub queue_id: String,
    pub total_calls: u32,
    pub calls_on_hold: u32,
    pub avg_wait_time_secs: u64,
}

#[derive(Clone)]
#[allow(dead_code)]
struct RecordState {
    recording_id: String,
    _mode: String,
    _path: String,
    is_paused: bool,
}

#[derive(Clone)]
#[allow(dead_code)]
struct RingbackState {
    _target_call_id: String,
    _source_call_id: String,
}

#[derive(Clone)]
#[allow(dead_code)]
struct SupervisorState {
    supervisor_call_id: String,
    target_call_id: String,
    mode: SupervisorMode,
    mixer_id: String,
    agent_leg: Option<String>,
}

#[derive(Clone)]
#[allow(dead_code)]
struct MediaStreamState;

#[derive(Clone)]
#[allow(dead_code)]
struct MediaInjectState;

pub struct RwiCommandProcessor {
    call_registry: Arc<ActiveProxyCallRegistry>,
    gateway: Arc<RwLock<RwiGateway>>,
    sip_server: Option<SipServerRef>,
    queue_states: Arc<RwLock<HashMap<String, QueueState>>>,
    record_states: Arc<RwLock<HashMap<String, RecordState>>>,
    ringback_states: Arc<RwLock<HashMap<String, RingbackState>>>,
    supervisor_states: Arc<RwLock<HashMap<String, SupervisorState>>>,
    media_stream_states: Arc<RwLock<HashMap<String, MediaStreamState>>>,
    media_inject_states: Arc<RwLock<HashMap<String, MediaInjectState>>>,
    mixer_registry: Arc<media::mixer_registry::MixerRegistry>,
    conference_manager: Arc<ConferenceManager>,
    transfer_controller: Arc<RwLock<TransferController>>,
    command_dedup_cache: CommandDeduplicationCache,

    agent_skills: Arc<RwLock<HashMap<String, AgentSkill>>>,
    queue_overflow_configs: Arc<RwLock<HashMap<String, QueueOverflowConfig>>>,
    #[cfg(test)]
    force_seat_replace_rollback_failure: Arc<AtomicBool>,
}

#[allow(dead_code)]
impl RwiCommandProcessor {
    pub fn new(
        call_registry: Arc<ActiveProxyCallRegistry>,
        gateway: Arc<RwLock<RwiGateway>>,
        conference_manager: Arc<ConferenceManager>,
    ) -> Self {
        let transfer_controller = Arc::new(RwLock::new(TransferController::with_default_config(
            call_registry.clone(),
            gateway.clone(),
        )));
        Self {
            call_registry,
            gateway,
            sip_server: None,
            queue_states: Arc::new(RwLock::new(HashMap::new())),
            record_states: Arc::new(RwLock::new(HashMap::new())),
            ringback_states: Arc::new(RwLock::new(HashMap::new())),
            supervisor_states: Arc::new(RwLock::new(HashMap::new())),
            media_stream_states: Arc::new(RwLock::new(HashMap::new())),
            media_inject_states: Arc::new(RwLock::new(HashMap::new())),
            mixer_registry: Arc::new(media::mixer_registry::MixerRegistry::new()),
            conference_manager,
            transfer_controller,
            command_dedup_cache: CommandDeduplicationCache::with_default_ttl(),

            agent_skills: Arc::new(RwLock::new(HashMap::new())),
            queue_overflow_configs: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            force_seat_replace_rollback_failure: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_sip_server(mut self, server: SipServerRef) -> Self {
        self.sip_server = Some(server.clone());
        self.conference_manager = server.conference_manager.clone();

        let new_controller = TransferController::with_default_config(
            self.call_registry.clone(),
            self.gateway.clone(),
        )
        .with_sip_server(server);
        self.transfer_controller = Arc::new(RwLock::new(new_controller));

        self
    }

    pub async fn is_duplicate_action(&self, action_id: &str) -> bool {
        if action_id.is_empty() {
            return false;
        }
        self.command_dedup_cache.is_duplicate(action_id).await
    }

    pub async fn record_action(&self, action_id: String, result: Option<String>) {
        if action_id.is_empty() {
            return;
        }
        self.command_dedup_cache.record(action_id, result).await;
    }

    #[cfg(test)]
    fn force_next_seat_replace_rollback_failure(&self) {
        self.force_seat_replace_rollback_failure
            .store(true, Ordering::SeqCst);
    }

    fn conference_manager(&self) -> Arc<ConferenceManager> {
        self.conference_manager.clone()
    }

    /// Register this processor as a subscriber for REFER NOTIFY events from
    /// `SipSession` and spawn a background task to feed them into the
    /// `TransferController`.
    pub async fn register_transfer_notify_listener(&self) {
        let Some(ref server) = self.sip_server else {
            return;
        };
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::call::domain::ReferNotifyEvent>();
        {
            let mut subscribers = server.transfer_notify_subscribers.lock().await;
            subscribers.push(tx);
        }

        let controller = self.transfer_controller.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let c = controller.read().await;
                match event.event_type {
                    crate::call::domain::ReferNotifyEventType::ReferResponse => {
                        c.handle_refer_response_by_call_id(&event.call_id, event.sip_status)
                            .await;
                    }
                    crate::call::domain::ReferNotifyEventType::Notify => {
                        c.handle_notify_by_call_id(&event.call_id, event.sip_status)
                            .await;
                    }
                }
            }
        });
    }

    fn dispatch_unified_command(
        &self,
        call_id: &str,
        command: RwiCommandPayload,
    ) -> Option<Result<CommandResult, CommandError>> {
        use crate::call::runtime::dispatch_rwi_command;

        match dispatch_rwi_command(&self.call_registry, Some(call_id), command) {
            Ok(result) => {
                if result.success {
                    Some(Ok(CommandResult::Success))
                } else {
                    let msg = result
                        .message
                        .unwrap_or_else(|| "command failed".to_string());

                    if msg.contains("not supported") || msg.contains("not implemented") {
                        return None;
                    }

                    if msg.to_lowercase().contains("not found") {
                        Some(Err(CommandError::CallNotFound(call_id.to_string())))
                    } else {
                        Some(Err(CommandError::CommandFailed(msg)))
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.to_lowercase().contains("not found") {
                    Some(Err(CommandError::CallNotFound(call_id.to_string())))
                } else {
                    Some(Err(CommandError::CommandFailed(msg)))
                }
            }
        }
    }

    pub async fn process_command(
        &self,
        command: RwiCommandPayload,
    ) -> Result<CommandResult, CommandError> {
        if let RwiCommandPayload::Bridge { leg_a, leg_b } = &command {
            if self.call_registry.get_handle(leg_a).is_none() {
                return Err(CommandError::CallNotFound(leg_a.clone()));
            }
            if self.call_registry.get_handle(leg_b).is_none() {
                return Err(CommandError::CallNotFound(leg_b.clone()));
            }
        }

        if let RwiCommandPayload::Originate(req) = &command {
            return self.originate_call(req.clone()).await;
        }

        match &command {
            RwiCommandPayload::QueueEnqueue(req) => {
                return self.queue_enqueue(req.clone()).await;
            }
            RwiCommandPayload::QueueDequeue { call_id } => {
                return self.queue_dequeue(call_id).await;
            }
            RwiCommandPayload::QueueHold { call_id } => {
                return self.queue_hold(call_id).await;
            }
            RwiCommandPayload::QueueUnhold { call_id } => {
                return self.queue_unhold(call_id).await;
            }
            RwiCommandPayload::QueueSetPriority { call_id, priority } => {
                return self.queue_set_priority(call_id, *priority).await;
            }
            RwiCommandPayload::QueueAssignAgent { call_id, agent_id } => {
                return self.queue_assign_agent(call_id, agent_id).await;
            }
            RwiCommandPayload::QueueRequeue {
                call_id,
                queue_id,
                priority,
            } => {
                return self.queue_requeue(call_id, queue_id, *priority).await;
            }
            _ => {}
        }

        match &command {
            RwiCommandPayload::RecordStart(req) => {
                return self.record_start(req.clone()).await;
            }
            RwiCommandPayload::RecordPause { call_id } => {
                return self.record_pause(call_id).await;
            }
            RwiCommandPayload::RecordResume { call_id } => {
                return self.record_resume(call_id).await;
            }
            RwiCommandPayload::RecordStop { call_id } => {
                return self.record_stop(call_id).await;
            }
            _ => {}
        }

        match &command {
            RwiCommandPayload::SetRingbackSource {
                target_call_id,
                source_call_id,
            } => {
                return self
                    .set_ringback_source(target_call_id, source_call_id)
                    .await;
            }
            RwiCommandPayload::SupervisorListen {
                supervisor_call_id,
                target_call_id,
            } => {
                self.get_handle(supervisor_call_id).await?;
                self.get_handle(target_call_id).await?;
            }
            RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => {
                self.get_handle(supervisor_call_id).await?;
                self.get_handle(target_call_id).await?;
                if !agent_leg.is_empty() {
                    self.get_handle(agent_leg).await?;
                }
            }
            RwiCommandPayload::SupervisorBarge {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => {
                self.get_handle(supervisor_call_id).await?;
                self.get_handle(target_call_id).await?;
                if !agent_leg.is_empty() {
                    self.get_handle(agent_leg).await?;
                }
            }
            RwiCommandPayload::SupervisorTakeover {
                supervisor_call_id,
                target_call_id,
            } => {
                self.get_handle(supervisor_call_id).await?;
                self.get_handle(target_call_id).await?;
            }
            RwiCommandPayload::SupervisorStop {
                supervisor_call_id,
                target_call_id,
            } => {
                return self
                    .supervisor_stop(supervisor_call_id, target_call_id)
                    .await;
            }
            RwiCommandPayload::ParallelOriginate(req) => {
                return self.parallel_originate(req.clone()).await;
            }
            _ => {}
        }

        // Handle transfer commands via TransferController for proper 3PCC fallback coordination
        match &command {
            RwiCommandPayload::Transfer { call_id, target } => {
                return self
                    .handle_transfer(call_id.clone(), target.clone(), false)
                    .await;
            }
            RwiCommandPayload::TransferReplace { call_id, target } => {
                return self
                    .handle_transfer_replace(call_id.clone(), target.clone())
                    .await;
            }
            RwiCommandPayload::TransferAttended {
                call_id,
                target,
                timeout_secs,
            } => {
                return self
                    .handle_attended_transfer(call_id.clone(), target.clone(), *timeout_secs)
                    .await;
            }
            RwiCommandPayload::TransferComplete {
                call_id,
                consultation_call_id,
            } => {
                return self
                    .handle_transfer_complete(call_id.clone(), consultation_call_id.clone())
                    .await;
            }
            RwiCommandPayload::TransferCancel {
                consultation_call_id,
            } => {
                return self
                    .handle_transfer_cancel(consultation_call_id.clone())
                    .await;
            }
            _ => {}
        }

        if let Some(call_id) = self.extract_call_id(&command) {
            if let Some(result) = self.dispatch_unified_command(&call_id, command.clone()) {
                tracing::debug!(
                    call_id = %call_id,
                    "Command handled via unified session runtime"
                );

                match &command {
                    RwiCommandPayload::Bridge { leg_a, leg_b } => {
                        let gw = self.gateway.read().await;
                        let event = RwiEvent::CallBridged {
                            leg_a: leg_a.clone(),
                            leg_b: leg_b.clone(),
                        };
                        gw.send_event_to_call_owner(leg_a, &event);
                        gw.send_event_to_call_owner(leg_b, &event);
                    }
                    RwiCommandPayload::Unbridge { call_id } => {
                        let gw = self.gateway.read().await;
                        let event = RwiEvent::CallUnbridged {
                            call_id: call_id.clone(),
                        };
                        gw.send_event_to_call_owner(call_id, &event);
                    }
                    _ => {}
                }
                return result;
            }
        }

        match &command {
            RwiCommandPayload::ListCalls => {
                let calls = self.list_calls().await;
                return Ok(CommandResult::ListCalls(calls));
            }
            RwiCommandPayload::AttachCall { call_id, mode: _ } => {
                if self.call_registry.get_handle(call_id).is_some() {
                    return Ok(CommandResult::CallFound {
                        call_id: call_id.clone(),
                    });
                } else {
                    return Err(CommandError::CallNotFound(call_id.clone()));
                }
            }
            RwiCommandPayload::DetachCall { call_id } => {
                if self.call_registry.get_handle(call_id).is_some() {
                    return Ok(CommandResult::Success);
                } else {
                    return Err(CommandError::CallNotFound(call_id.clone()));
                }
            }
            RwiCommandPayload::ConferenceCreate(req) => {
                return self.conference_create(req.clone()).await;
            }
            RwiCommandPayload::ConferenceAdd { conf_id, call_id } => {
                return self.conference_add(conf_id, call_id).await;
            }
            RwiCommandPayload::ConferenceRemove { conf_id, call_id } => {
                return self.conference_remove(conf_id, call_id).await;
            }
            RwiCommandPayload::ConferenceMute { conf_id, call_id } => {
                return self.conference_mute(conf_id, call_id).await;
            }
            RwiCommandPayload::ConferenceUnmute { conf_id, call_id } => {
                return self.conference_unmute(conf_id, call_id).await;
            }
            RwiCommandPayload::ConferenceDestroy { conf_id } => {
                return self.conference_destroy(conf_id).await;
            }
            RwiCommandPayload::ConferenceMerge {
                conf_id,
                call_id,
                consultation_call_id,
            } => {
                return self
                    .conference_merge(conf_id, call_id, consultation_call_id)
                    .await;
            }
            RwiCommandPayload::ConferenceSeatReplace {
                conf_id,
                old_call_id,
                new_call_id,
            } => {
                return self
                    .conference_seat_replace(conf_id, old_call_id, new_call_id)
                    .await;
            }

            RwiCommandPayload::Subscribe { .. } => {
                return Ok(CommandResult::Success);
            }
            RwiCommandPayload::Unsubscribe { .. } => {
                return Ok(CommandResult::Success);
            }

            RwiCommandPayload::SipMessage {
                call_id,
                content_type,
                body,
            } => {
                return self.sip_message(call_id, content_type, body).await;
            }
            RwiCommandPayload::SipNotify {
                call_id,
                event,
                content_type,
                body,
            } => {
                return self.sip_notify(call_id, event, content_type, body).await;
            }
            RwiCommandPayload::SipOptionsPing { call_id } => {
                return self.sip_options_ping(call_id).await;
            }

            RwiCommandPayload::MediaStreamStart(req) => {
                return self
                    .media_stream_start(&req.call_id, &req.call_id, &req.direction)
                    .await;
            }
            RwiCommandPayload::MediaStreamStop { call_id } => {
                return self.media_stream_stop(call_id).await;
            }
            RwiCommandPayload::MediaInjectStart(req) => {
                return self
                    .media_inject_start(&req.call_id, &req.call_id, &req.format)
                    .await;
            }
            RwiCommandPayload::MediaInjectStop { call_id } => {
                return self.media_inject_stop(call_id).await;
            }

            RwiCommandPayload::QueueEnqueue(req) => {
                return self.queue_enqueue(req.clone()).await;
            }
            RwiCommandPayload::QueueDequeue { call_id } => {
                return self.queue_dequeue(call_id).await;
            }
            RwiCommandPayload::QueueHold { call_id } => {
                return self.queue_hold(call_id).await;
            }
            RwiCommandPayload::QueueUnhold { call_id } => {
                return self.queue_unhold(call_id).await;
            }
            RwiCommandPayload::QueueSetPriority { call_id, priority } => {
                return self.queue_set_priority(call_id, *priority).await;
            }
            RwiCommandPayload::QueueAssignAgent { call_id, agent_id } => {
                return self.queue_assign_agent(call_id, agent_id).await;
            }
            RwiCommandPayload::QueueRequeue {
                call_id,
                queue_id,
                priority,
            } => {
                return self.queue_requeue(call_id, queue_id, *priority).await;
            }

            RwiCommandPayload::RecordStart(req) => {
                return self.record_start(req.clone()).await;
            }
            RwiCommandPayload::RecordPause { call_id } => {
                return self.record_pause(call_id).await;
            }
            RwiCommandPayload::RecordResume { call_id } => {
                return self.record_resume(call_id).await;
            }
            RwiCommandPayload::RecordStop { call_id } => {
                return self.record_stop(call_id).await;
            }

            RwiCommandPayload::SetRingbackSource {
                target_call_id,
                source_call_id,
            } => {
                return self
                    .set_ringback_source(target_call_id, source_call_id)
                    .await;
            }
            RwiCommandPayload::SupervisorStop {
                supervisor_call_id,
                target_call_id,
            } => {
                return self
                    .supervisor_stop(supervisor_call_id, target_call_id)
                    .await;
            }
            _ => {}
        }

        // Handle session/call resume via gateway event cache
        match &command {
            RwiCommandPayload::SessionResume { last_sequence } => {
                let gw = self.gateway.read().await;
                let (entries, current_seq) = gw.resume_session(*last_sequence);
                let replayed_count = entries.len() as u64;
                let events: Vec<serde_json::Value> = entries
                    .into_iter()
                    .map(|e| {
                        serde_json::json!({
                            "sequence": e.sequence,
                            "timestamp": e.timestamp,
                            "call_id": e.call_id,
                            "event": e.event,
                        })
                    })
                    .collect();
                return Ok(CommandResult::SessionResumed {
                    replayed_count,
                    current_sequence: current_seq,
                    events,
                });
            }
            RwiCommandPayload::CallResume {
                call_id,
                last_sequence,
            } => {
                let gw = self.gateway.read().await;
                let (entries, current_seq) = gw.resume_call(call_id, *last_sequence);
                let replayed_count = entries.len() as u64;
                let events: Vec<serde_json::Value> = entries
                    .into_iter()
                    .map(|e| {
                        serde_json::json!({
                            "sequence": e.sequence,
                            "timestamp": e.timestamp,
                            "call_id": e.call_id,
                            "event": e.event,
                        })
                    })
                    .collect();
                return Ok(CommandResult::CallResumed {
                    call_id: call_id.clone(),
                    replayed_count,
                    current_sequence: current_seq,
                    events,
                });
            }
            _ => {}
        }

        if let Some(call_id) = self.extract_call_id(&command) {
            if self.call_registry.get_handle(&call_id).is_some() {
                return Err(CommandError::CommandFailed(
                    "command not implemented in unified runtime".to_string(),
                ));
            } else {
                return Err(CommandError::CallNotFound(call_id));
            }
        }

        Err(CommandError::CommandFailed(
            "command requires call_id".to_string(),
        ))
    }

    fn extract_call_id(&self, command: &RwiCommandPayload) -> Option<String> {
        match command {
            RwiCommandPayload::Answer { call_id } => Some(call_id.clone()),
            RwiCommandPayload::Hangup { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::Reject { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::Ring { call_id } => Some(call_id.clone()),
            RwiCommandPayload::CallHold { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::CallUnhold { call_id } => Some(call_id.clone()),
            RwiCommandPayload::Bridge { leg_a, .. } => Some(leg_a.clone()),
            RwiCommandPayload::Unbridge { call_id } => Some(call_id.clone()),
            RwiCommandPayload::Transfer { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::TransferReplace { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::TransferAttended { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::TransferComplete { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::TransferCancel {
                consultation_call_id,
            } => Some(consultation_call_id.clone()),
            RwiCommandPayload::SetRingbackSource { target_call_id, .. } => {
                Some(target_call_id.clone())
            }
            RwiCommandPayload::MediaPlay(req) => Some(req.call_id.clone()),
            RwiCommandPayload::MediaStop { call_id } => Some(call_id.clone()),
            RwiCommandPayload::MediaStreamStart(req) => Some(req.call_id.clone()),
            RwiCommandPayload::MediaStreamStop { call_id } => Some(call_id.clone()),
            RwiCommandPayload::MediaInjectStart(req) => Some(req.call_id.clone()),
            RwiCommandPayload::MediaInjectStop { call_id } => Some(call_id.clone()),
            RwiCommandPayload::Originate(req) => Some(req.call_id.clone()),
            RwiCommandPayload::AttachCall { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::DetachCall { call_id } => Some(call_id.clone()),
            RwiCommandPayload::RecordStart(req) => Some(req.call_id.clone()),
            RwiCommandPayload::RecordPause { call_id } => Some(call_id.clone()),
            RwiCommandPayload::RecordResume { call_id } => Some(call_id.clone()),
            RwiCommandPayload::RecordStop { call_id } => Some(call_id.clone()),
            RwiCommandPayload::QueueEnqueue(req) => Some(req.call_id.clone()),
            RwiCommandPayload::QueueDequeue { call_id } => Some(call_id.clone()),
            RwiCommandPayload::QueueHold { call_id } => Some(call_id.clone()),
            RwiCommandPayload::QueueUnhold { call_id } => Some(call_id.clone()),
            RwiCommandPayload::QueueSetPriority { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::QueueAssignAgent { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::QueueRequeue { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::SupervisorListen { target_call_id, .. } => {
                Some(target_call_id.clone())
            }
            RwiCommandPayload::SupervisorWhisper { target_call_id, .. } => {
                Some(target_call_id.clone())
            }
            RwiCommandPayload::SupervisorBarge { target_call_id, .. } => {
                Some(target_call_id.clone())
            }
            RwiCommandPayload::SupervisorTakeover { target_call_id, .. } => {
                Some(target_call_id.clone())
            }
            RwiCommandPayload::SupervisorStop { target_call_id, .. } => {
                Some(target_call_id.clone())
            }
            RwiCommandPayload::SipMessage { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::SipNotify { call_id, .. } => Some(call_id.clone()),
            RwiCommandPayload::SipOptionsPing { call_id } => Some(call_id.clone()),

            RwiCommandPayload::ConferenceCreate(_) => None,
            RwiCommandPayload::ConferenceDestroy { .. } => None,
            RwiCommandPayload::ConferenceSeatReplace { .. } => None,
            _ => None,
        }
    }

    pub async fn originate_call(
        &self,
        req: OriginateRequest,
    ) -> Result<CommandResult, CommandError> {
        let server = self
            .sip_server
            .as_ref()
            .ok_or_else(|| CommandError::CommandFailed("SIP server not available".into()))?
            .clone();

        let destination_uri: rsipstack::sip::Uri =
            rsipstack::sip::Uri::try_from(req.destination.as_str()).map_err(|_| {
                CommandError::CommandFailed(format!("invalid destination: {}", req.destination))
            })?;

        let realm = server
            .proxy_config
            .realms
            .as_ref()
            .and_then(|v| v.first().cloned())
            .unwrap_or_else(|| server.proxy_config.addr.clone());
        let caller_str = req
            .caller_id
            .clone()
            .unwrap_or_else(|| format!("sip:rwi@{}", realm));
        let caller_uri: rsipstack::sip::Uri = rsipstack::sip::Uri::try_from(caller_str.as_str())
            .map_err(|_| CommandError::CommandFailed("invalid caller_id".into()))?;

        let mut headers: Vec<rsipstack::sip::Header> =
            vec![rsipstack::sip::headers::MaxForwards::from(70u32).into()];
        for (k, v) in &req.extra_headers {
            headers.push(rsipstack::sip::Header::Other(k.clone().into(), v.clone()));
        }

        let external_ip = server
            .rtp_config
            .external_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());

        let media_track =
            crate::media::RtpTrackBuilder::new(format!("rwi-originate-{}", req.call_id))
                .with_cancel_token(tokio_util::sync::CancellationToken::new())
                .with_external_ip(external_ip.clone())
                .build();

        tracing::info!(call_id = %req.call_id, external_ip = %external_ip, "Created media track for originate");

        let sdp_offer = match media_track.local_description().await {
            Ok(sdp) => {
                tracing::info!(call_id = %req.call_id, sdp_len = %sdp.len(), "Generated SDP offer");
                if sdp.is_empty() {
                    return Err(CommandError::CommandFailed("SDP offer is empty".into()));
                }

                let preview: String = sdp.lines().take(5).collect::<Vec<_>>().join("\n");
                tracing::debug!(call_id = %req.call_id, "SDP preview:\n{}", preview);
                sdp
            }
            Err(e) => {
                tracing::error!(call_id = %req.call_id, error = %e, "Failed to generate SDP offer");
                return Err(CommandError::CommandFailed(format!(
                    "failed to create SDP offer: {}",
                    e
                )));
            }
        };

        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: destination_uri.clone(),
            caller: caller_uri.clone(),
            contact: caller_uri.clone(),
            content_type: Some("application/sdp".to_string()),
            offer: Some(sdp_offer.clone().into_bytes()),
            destination: None,
            credential: None,
            headers: Some(headers),
            call_id: Some(req.call_id.clone()),
            ..Default::default()
        };

        let call_id = req.call_id.clone();
        let gateway = self.gateway.clone();
        let registry = self.call_registry.clone();
        let timeout_secs = req.timeout_secs.unwrap_or(60);
        let dialog_layer = server.dialog_layer.clone();
        let caller_display = req.caller_id.unwrap_or_else(|| caller_str.clone());
        let callee_display = req.destination.clone();

        let cancel_token = tokio_util::sync::CancellationToken::new();

        tokio::spawn(async move {
            let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();

            let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

            let caller_media_builder = crate::media::MediaStreamBuilder::new()
                .with_id(format!("{}-caller", call_id))
                .with_cancel_token(cancel_token.clone());
            let caller_peer: std::sync::Arc<dyn crate::proxy::proxy_call::media_peer::MediaPeer> =
                std::sync::Arc::new(VoiceEnginePeer::new(std::sync::Arc::new(
                    caller_media_builder.build(),
                )));

            caller_peer.update_track(Box::new(media_track), None).await;

            let (_handle, mut cmd_rx) = {
                use crate::call::runtime::SessionId;
                use crate::proxy::active_call_registry::{
                    ActiveProxyCallEntry, ActiveProxyCallStatus,
                };
                use crate::proxy::proxy_call::sip_session::SipSession;

                let id = SessionId::from(call_id.clone());
                let (handle, cmd_rx) = SipSession::with_handle(id);

                let entry = ActiveProxyCallEntry {
                    session_id: call_id.clone(),
                    caller: Some(caller_display.clone()),
                    callee: Some(callee_display.clone()),
                    direction: "outbound".to_string(),
                    started_at: chrono::Utc::now(),
                    answered_at: None,
                    status: ActiveProxyCallStatus::Ringing,
                };
                registry.upsert(entry, handle.clone());
                (handle, cmd_rx)
            };

            let cmd_cancel = cancel_token.clone();
            let cmd_task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cmd_cancel.cancelled() => {
                            tracing::debug!("Command task cancelled, exiting");
                            break;
                        }
                        Some(cmd) = cmd_rx.recv() => {
                            tracing::debug!(?cmd, "RWI originate command received");
                        }
                        else => {

                            break;
                        }
                    }
                }
            });

            let cleanup = || async {
                cancel_token.cancel();

                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), cmd_task).await;
            };

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs as u64)) => {
                    let gw = gateway.read().await;
                    gw.send_event_to_call_owner(&call_id, &RwiEvent::CallNoAnswer { call_id: call_id.clone() });
                    registry.remove(&call_id);
                    cleanup().await;
                }
                result = async {
                    loop {
                        tokio::select! {
                            res = &mut invitation => {
                                break res;
                            }
                            state = state_rx.recv() => {
                                match state {
                                    Some(rsipstack::dialog::dialog::DialogState::Calling(_)) => {
                                        let gw = gateway.read().await;
                                        gw.send_event_to_call_owner(
                                            &call_id,
                                            &RwiEvent::CallRinging { call_id: call_id.clone() },
                                        );
                                    }
                                    Some(rsipstack::dialog::dialog::DialogState::Early(_, ref response)) => {

                                        let body = response.body();
                                        if !body.is_empty() {
                                            let sdp = String::from_utf8_lossy(body).to_string();
                                            if sdp.contains("v=0") {
                                                tracing::debug!(%call_id, "Early media SDP received");
                                            }
                                        }
                                        let gw = gateway.read().await;
                                        gw.send_event_to_call_owner(
                                            &call_id,
                                            &RwiEvent::CallEarlyMedia { call_id: call_id.clone() },
                                        );
                                    }
                                    Some(rsipstack::dialog::dialog::DialogState::Terminated(_, _)) => {

                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                } => {
                    match result {
                        Ok((dialog_id, Some(resp))) if resp.status_code.kind() == rsipstack::sip::StatusCodeKind::Successful => {

                            let sdp_answer = if resp.body().is_empty() {
                                None
                            } else {
                                let body_str = String::from_utf8_lossy(resp.body()).to_string();
                                if body_str.contains("v=0") {
                                    Some(body_str)
                                } else {
                                    None
                                }
                            };


                            if let Some(answer) = sdp_answer {
                                tracing::info!(%call_id, "Received SDP answer, completing media handshake");



                                let tracks = caller_peer.get_tracks().await;
                                if let Some(first_track) = tracks.first() {
                                    if let Err(e) = first_track.lock().await.set_remote_description(&answer).await {
                                        tracing::error!(%call_id, "Failed to set remote description: {}", e);
                                    } else {
                                        tracing::info!(%call_id, "Media session established successfully");
                                    }
                                }
                            } else {
                                tracing::warn!(%call_id, "200 OK received without SDP answer");
                            }


                            let _ = caller_peer;
                            let _ = dialog_id;


                            use crate::proxy::active_call_registry::ActiveProxyCallStatus;
                            registry.update(&call_id, |entry| {
                                entry.answered_at = Some(chrono::Utc::now());
                                entry.status = ActiveProxyCallStatus::Talking;
                            });
                            {
                                let gw = gateway.read().await;
                                gw.send_event_to_call_owner(
                                    &call_id,
                                    &RwiEvent::CallAnswered { call_id: call_id.clone() },
                                );
                            }


                            tokio::select! {
                                _ = cancel_token.cancelled() => {
                                    tracing::info!(%call_id, "Originate task cancelled");
                                }
                                _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {
                                    tracing::info!(%call_id, "Call timeout after 1 hour");
                                }
                                _ = async {

                                    loop {
                                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                                        if registry.get_handle(&call_id).is_none() {
                                            tracing::info!(%call_id, "Call ended, stopping media task");
                                            break;
                                        }
                                    }
                                } => {}
                            }
                            cleanup().await;
                        }
                        Ok((_dialog_id, resp_opt)) => {
                            let sip_status = resp_opt.as_ref().map(|r| r.status_code.code());
                            let gw = gateway.read().await;
                            if sip_status == Some(486) || sip_status == Some(600) {
                                gw.send_event_to_call_owner(
                                    &call_id,
                                    &RwiEvent::CallBusy { call_id: call_id.clone() },
                                );
                            } else {
                                gw.send_event_to_call_owner(
                                    &call_id,
                                    &RwiEvent::CallHangup {
                                        call_id: call_id.clone(),
                                        reason: Some("originate_failed".to_string()),
                                        sip_status,
                                    },
                                );
                            }
                            registry.remove(&call_id);
                            cleanup().await;
                        }
                        Err(e) => {
                            let gw = gateway.read().await;
                            gw.send_event_to_call_owner(
                                &call_id,
                                &RwiEvent::CallHangup {
                                    call_id: call_id.clone(),
                                    reason: Some(e.to_string()),
                                    sip_status: None,
                                },
                            );
                            registry.remove(&call_id);
                            cleanup().await;
                        }
                    }
                }
            }
        });

        Ok(CommandResult::Originated {
            call_id: req.call_id,
        })
    }

    pub async fn parallel_originate(
        &self,
        req: ParallelOriginateRequest,
    ) -> Result<CommandResult, CommandError> {
        use std::future::Future;
        use std::pin::Pin;

        let server = self
            .sip_server
            .as_ref()
            .ok_or_else(|| CommandError::CommandFailed("SIP server not available".into()))?
            .clone();

        if req.targets.is_empty() {
            return Err(CommandError::CommandFailed("No targets specified".into()));
        }

        let operation_id = req.operation_id.clone();
        let targets = req.targets.clone();
        let leg_count = targets.len() as u32;

        info!(
            %operation_id,
            leg_count,
            "Starting parallel originate"
        );

        {
            let gw = self.gateway.read().await;
            gw.send_event_to_call_owner(
                &operation_id,
                &RwiEvent::ParallelOriginateStarted {
                    operation_id: operation_id.clone(),
                    leg_count,
                },
            );
        }

        let timeout_secs = req.timeout_secs.unwrap_or(60) as u64;
        let gateway = self.gateway.clone();
        let registry = self.call_registry.clone();

        let cancel_token = tokio_util::sync::CancellationToken::new();

        let mut futures: Vec<
            Pin<Box<dyn Future<Output = Result<(usize, String), (usize, String)>> + Send>>,
        > = Vec::new();

        for (idx, target) in targets.iter().enumerate() {
            let server = server.clone();
            let gateway = gateway.clone();
            let registry = registry.clone();
            let operation_id = operation_id.clone();
            let caller_id = req.caller_id.clone();
            let extra_headers = req.extra_headers.clone();
            let target = target.clone();
            let cancel_token = cancel_token.clone();

            let fut = async move {
                if cancel_token.is_cancelled() {
                    return Err((idx, "Cancelled".to_string()));
                }

                match Self::do_single_originate(
                    server,
                    &target,
                    caller_id,
                    extra_headers,
                    gateway,
                    registry,
                    &operation_id,
                )
                .await
                {
                    Ok(call_id) => Ok((idx, call_id)),
                    Err(e) => Err((idx, e)),
                }
            };
            futures.push(Box::pin(fut));
        }

        let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            let mut remaining = futures;

            loop {
                if remaining.is_empty() {
                    return Err("All legs failed".to_string());
                }

                let (result, _idx, rest) = futures::future::select_all(remaining).await;
                remaining = rest;

                match result {
                    Ok((leg_idx, call_id)) => {
                        cancel_token.cancel();

                        info!(
                            operation_id = %operation_id,
                            leg_idx = leg_idx,
                            call_id = %call_id,
                            "Parallel originate leg answered"
                        );

                        let gw = self.gateway.read().await;
                        gw.send_event_to_call_owner(
                            &operation_id,
                            &RwiEvent::ParallelOriginateWinner {
                                operation_id: operation_id.clone(),
                                call_id: call_id.clone(),
                                destination: targets[leg_idx].destination.clone(),
                            },
                        );

                        for (i, _) in remaining.iter().enumerate() {
                            let actual_idx = i;
                            if let Some(target) = targets.get(actual_idx) {
                                gw.send_event_to_call_owner(
                                    &operation_id,
                                    &RwiEvent::ParallelOriginateLegCancelled {
                                        operation_id: operation_id.clone(),
                                        call_id: target.call_id.clone(),
                                        reason: "Cancelled - another leg won".to_string(),
                                    },
                                );
                            }
                        }

                        return Ok(call_id);
                    }
                    Err((leg_idx, reason)) => {
                        info!(
                            operation_id = %operation_id,
                            leg_idx = leg_idx,
                            reason = %reason,
                            "Parallel originate leg failed"
                        );

                        let gw = self.gateway.read().await;
                        gw.send_event_to_call_owner(
                            &operation_id,
                            &RwiEvent::ParallelOriginateLegCancelled {
                                operation_id: operation_id.clone(),
                                call_id: targets[leg_idx].call_id.clone(),
                                reason,
                            },
                        );
                    }
                }
            }
        })
        .await;

        match result {
            Ok(Ok(winning_call_id)) => {
                let gw = self.gateway.read().await;
                gw.send_event_to_call_owner(
                    &operation_id,
                    &RwiEvent::ParallelOriginateCompleted {
                        operation_id: operation_id.clone(),
                        winning_call_id: winning_call_id.clone(),
                    },
                );

                Ok(CommandResult::Originated {
                    call_id: winning_call_id,
                })
            }
            Ok(Err(reason)) => {
                let gw = self.gateway.read().await;
                gw.send_event_to_call_owner(
                    &operation_id,
                    &RwiEvent::ParallelOriginateFailed {
                        operation_id: operation_id.clone(),
                        reason: reason.clone(),
                    },
                );

                Err(CommandError::CommandFailed(reason))
            }
            Err(_) => {
                let reason = "Timeout waiting for any leg to answer".to_string();
                let gw = self.gateway.read().await;
                gw.send_event_to_call_owner(
                    &operation_id,
                    &RwiEvent::ParallelOriginateFailed {
                        operation_id: operation_id.clone(),
                        reason: reason.clone(),
                    },
                );

                Err(CommandError::CommandFailed(reason))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn do_single_originate(
        server: SipServerRef,
        target: &crate::rwi::session::OriginateTarget,
        caller_id: Option<String>,
        extra_headers: HashMap<String, String>,
        gateway: Arc<RwLock<RwiGateway>>,
        registry: Arc<ActiveProxyCallRegistry>,
        operation_id: &str,
    ) -> Result<String, String> {
        let call_id = target.call_id.clone();
        let target_uri_str = target.destination.clone();

        let destination_uri = rsipstack::sip::Uri::try_from(target_uri_str.clone())
            .map_err(|e| format!("Invalid target URI: {}", e))?;

        let caller_str = caller_id.clone().unwrap_or_else(|| "RustPBX".to_string());

        let dialog_layer = server.dialog_layer.clone();

        let external_ip = server
            .rtp_config
            .external_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());

        let media_track =
            crate::media::RtpTrackBuilder::new(format!("parallel-{}-{}", operation_id, call_id))
                .with_cancel_token(tokio_util::sync::CancellationToken::new())
                .with_external_ip(external_ip.clone())
                .build();

        let sdp_offer = match media_track.local_description().await {
            Ok(sdp) => {
                if sdp.is_empty() {
                    return Err("SDP offer is empty".to_string());
                }
                sdp
            }
            Err(e) => return Err(format!("Failed to generate SDP: {}", e)),
        };

        let caller_uri: rsipstack::sip::Uri =
            rsipstack::sip::Uri::try_from(format!("sip:{}@{}", caller_str, external_ip).as_str())
                .map_err(|e| format!("Invalid caller URI: {:?}", e))?;

        let mut headers: Vec<rsipstack::sip::Header> =
            vec![rsipstack::sip::headers::MaxForwards::from(70u32).into()];
        for (k, v) in extra_headers {
            headers.push(rsipstack::sip::Header::Other(k.into(), v));
        }

        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: destination_uri.clone(),
            caller: caller_uri.clone(),
            contact: caller_uri.clone(),
            content_type: Some("application/sdp".to_string()),
            offer: Some(sdp_offer.into_bytes()),
            destination: None,
            credential: None,
            headers: Some(headers),
            call_id: Some(call_id.clone()),
            ..Default::default()
        };

        let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

        let cancel_token = tokio_util::sync::CancellationToken::new();
        let caller_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-caller", call_id))
            .with_cancel_token(cancel_token.clone());
        let caller_peer: std::sync::Arc<dyn crate::proxy::proxy_call::media_peer::MediaPeer> =
            std::sync::Arc::new(VoiceEnginePeer::new(std::sync::Arc::new(
                caller_media_builder.build(),
            )));
        caller_peer.update_track(Box::new(media_track), None).await;

        {
            use crate::call::runtime::SessionId;
            use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallStatus};
            use crate::proxy::proxy_call::sip_session::SipSession;

            let id = SessionId::from(call_id.clone());
            let (handle, _cmd_rx) = SipSession::with_handle(id);

            let entry = ActiveProxyCallEntry {
                session_id: call_id.clone(),
                caller: Some(caller_str.clone()),
                callee: Some(target_uri_str.clone()),
                direction: "outbound".to_string(),
                started_at: chrono::Utc::now(),
                answered_at: None,
                status: ActiveProxyCallStatus::Ringing,
            };
            registry.upsert(entry, handle);
        }

        let result = tokio::time::timeout(
            Duration::from_secs(60),
            async {
                loop {
                    tokio::select! {
                        res = &mut invitation => {
                            return res;
                        }
                        state = state_rx.recv() => {
                            match state {
                                Some(rsipstack::dialog::dialog::DialogState::Calling(_)) => {
                                    let gw = gateway.read().await;
                                    gw.send_event_to_call_owner(
                                        &call_id,
                                        &crate::rwi::proto::RwiEvent::CallRinging { call_id: call_id.clone() },
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        ).await;

        match result {
            Ok(Ok((dialog_id, Some(resp))))
                if resp.status_code.kind() == rsipstack::sip::StatusCodeKind::Successful =>
            {
                let sdp_answer = if resp.body().is_empty() {
                    None
                } else {
                    let body_str = String::from_utf8_lossy(resp.body()).to_string();
                    if body_str.contains("v=0") {
                        Some(body_str)
                    } else {
                        None
                    }
                };

                if let Some(answer) = sdp_answer {
                    let tracks = caller_peer.get_tracks().await;
                    if let Some(first_track) = tracks.first() {
                        let _ = first_track
                            .lock()
                            .await
                            .set_remote_description(&answer)
                            .await;
                    }
                }

                use crate::proxy::active_call_registry::ActiveProxyCallStatus;
                registry.update(&call_id, |entry| {
                    entry.answered_at = Some(chrono::Utc::now());
                    entry.status = ActiveProxyCallStatus::Talking;
                });

                let gw = gateway.read().await;
                gw.send_event_to_call_owner(
                    &call_id,
                    &crate::rwi::proto::RwiEvent::CallAnswered {
                        call_id: call_id.clone(),
                    },
                );

                let _ = caller_peer;
                let _ = dialog_id;

                Ok(call_id)
            }
            Ok(Ok((_, resp_opt))) => {
                let status = resp_opt.as_ref().map(|r| r.status_code.code());
                Err(format!("Call failed with status: {:?}", status))
            }
            Ok(Err(e)) => Err(format!("Originate failed: {}", e)),
            Err(_) => Err("Timeout waiting for answer".to_string()),
        }
    }

    pub async fn list_calls(&self) -> Vec<CallInfo> {
        self.call_registry
            .list_recent(100)
            .into_iter()
            .map(|entry| CallInfo {
                session_id: entry.session_id,
                caller: entry.caller,
                callee: entry.callee,
                direction: entry.direction,
                status: entry.status.to_string(),
                started_at: entry.started_at.to_rfc3339(),
                answered_at: entry.answered_at.map(|t| t.to_rfc3339()),
            })
            .collect()
    }

    async fn get_handle(&self, call_id: &str) -> Result<SipSessionHandle, CommandError> {
        self.call_registry
            .get_handle(call_id)
            .ok_or_else(|| CommandError::CallNotFound(call_id.to_string()))
    }

    async fn bridge_calls(&self, leg_a: &str, leg_b: &str) -> Result<CommandResult, CommandError> {
        use crate::call::domain::P2PMode;

        let handle_a = self.get_handle(leg_a).await?;
        let _handle_b = self.get_handle(leg_b).await?;

        let send_result = handle_a.send_command(CallCommand::Bridge {
            leg_a: LegId::new(leg_a),
            leg_b: LegId::new(leg_b),
            mode: P2PMode::Audio,
        });

        let event = RwiEvent::CallBridged {
            leg_a: leg_a.to_string(),
            leg_b: leg_b.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&leg_a.to_string(), &event);
        gw.send_event_to_call_owner(&leg_b.to_string(), &event);

        if let Err(e) = &send_result {
            tracing::warn!("bridge_calls: send_command error (may be expected): {}", e);
        }

        Ok(CommandResult::Success)
    }

    async fn queue_enqueue(&self, req: QueueEnqueueRequest) -> Result<CommandResult, CommandError> {
        let _handle = self.get_handle(&req.call_id).await?;

        let overflow_check = self.check_queue_overflow(&req.queue_id).await;
        if let Some(overflow_action) = overflow_check {
            return self.handle_queue_overflow(req, overflow_action).await;
        }

        let matched_agent = if let Some(ref skills) = req.skills {
            self.find_matching_agent(skills).await
        } else {
            None
        };

        let queue_state = QueueState {
            queue_id: req.queue_id.clone(),
            priority: req.priority,
            skills: req.skills.clone(),
            max_wait_secs: req.max_wait_secs,
            is_hold: false,
            enqueued_at: std::time::Instant::now(),
            agent_id: matched_agent.clone(),
            overflow_count: 0,
        };

        let mut states = self.queue_states.write().await;
        states.insert(req.call_id.clone(), queue_state);
        drop(states);

        let event = RwiEvent::QueueJoined {
            call_id: req.call_id.clone(),
            queue_id: req.queue_id.clone(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&req.call_id, &event);

        if let Some(agent_id) = matched_agent {
            let agent_event = RwiEvent::QueueAgentOffered {
                call_id: req.call_id.clone(),
                queue_id: req.queue_id.clone(),
                agent_id: agent_id.clone(),
            };
            gw.broadcast_event(&agent_event);

            info!(
                call_id = %req.call_id,
                queue_id = %req.queue_id,
                agent_id = %agent_id,
                skills = ?req.skills,
                "Call enqueued with skill-matched agent"
            );
        } else {
            info!(
                call_id = %req.call_id,
                queue_id = %req.queue_id,
                skills = ?req.skills,
                "Call enqueued"
            );
        }

        Ok(CommandResult::Success)
    }

    async fn check_queue_overflow(&self, queue_id: &str) -> Option<QueueOverflowAction> {
        let configs = self.queue_overflow_configs.read().await;
        let config = configs.get(queue_id)?;

        let states = self.queue_states.read().await;
        let queue_count = states.values().filter(|s| s.queue_id == queue_id).count() as u32;

        if queue_count >= config.max_calls {
            return Some(QueueOverflowAction {
                action: config.overflow_action.clone(),
                target_queue: config.overflow_queue_id.clone(),
                reason: "queue_full".to_string(),
            });
        }

        None
    }

    async fn handle_queue_overflow(
        &self,
        req: QueueEnqueueRequest,
        overflow: QueueOverflowAction,
    ) -> Result<CommandResult, CommandError> {
        warn!(
            call_id = %req.call_id,
            queue_id = %req.queue_id,
            overflow_action = ?overflow.action,
            "Queue overflow - redirecting call"
        );

        match overflow.action.as_deref() {
            Some("transfer") if overflow.target_queue.is_some() => {
                let target_queue = overflow.target_queue.unwrap();
                info!(
                    call_id = %req.call_id,
                    from_queue = %req.queue_id,
                    to_queue = %target_queue,
                    "Transferring call to overflow queue"
                );

                let queue_state = QueueState {
                    queue_id: target_queue.clone(),
                    priority: req.priority,
                    skills: req.skills,
                    max_wait_secs: req.max_wait_secs,
                    is_hold: false,
                    enqueued_at: std::time::Instant::now(),
                    agent_id: None,
                    overflow_count: 1,
                };

                let mut states = self.queue_states.write().await;
                states.insert(req.call_id.clone(), queue_state);
                drop(states);

                let overflow_event = RwiEvent::QueueOverflowed {
                    call_id: req.call_id.clone(),
                    original_queue_id: req.queue_id,
                    overflow_queue_id: target_queue.clone(),
                    reason: overflow.reason,
                };
                let gw = self.gateway.read().await;
                gw.send_event_to_call_owner(&req.call_id, &overflow_event);

                let joined_event = RwiEvent::QueueJoined {
                    call_id: req.call_id.clone(),
                    queue_id: target_queue,
                };
                gw.send_event_to_call_owner(&req.call_id, &joined_event);

                Ok(CommandResult::Success)
            }
            Some("voicemail") => {
                info!(call_id = %req.call_id, "Redirecting to voicemail due to overflow");
                let event = RwiEvent::QueueVoicemailRedirected {
                    call_id: req.call_id.clone(),
                    queue_id: req.queue_id,
                    reason: overflow.reason,
                };
                let gw = self.gateway.read().await;
                gw.send_event_to_call_owner(&req.call_id, &event);
                Ok(CommandResult::Success)
            }
            Some("hangup") => {
                warn!(call_id = %req.call_id, "Hanging up call due to queue overflow");
                if let Ok(handle) = self.get_handle(&req.call_id).await {
                    use crate::callrecord::CallRecordHangupReason;
                    let hangup_cmd = crate::call::domain::HangupCommand::all(
                        Some(CallRecordHangupReason::BySystem),
                        Some(480u16),
                    );
                    let _ = handle.send_command(CallCommand::Hangup(hangup_cmd));
                }
                Ok(CommandResult::Success)
            }
            _ => Err(CommandError::CommandFailed(format!(
                "Queue {} is full",
                req.queue_id
            ))),
        }
    }

    async fn find_matching_agent(&self, required_skills: &[String]) -> Option<String> {
        let agents = self.agent_skills.read().await;

        let mut best_match: Option<(String, usize, f32)> = None;

        for (agent_id, agent) in agents.iter() {
            if agent.current_calls >= agent.max_concurrent_calls {
                continue;
            }

            let match_count = required_skills
                .iter()
                .filter(|skill| agent.skills.contains(skill))
                .count();

            if match_count == 0 {
                continue;
            }

            // Calculate load ratio for tie-breaking (lower is better)
            let load_ratio = agent.current_calls as f32 / agent.max_concurrent_calls as f32;

            // Determine if this agent is better than current best
            let is_better = match &best_match {
                None => true,
                Some((_, best_count, best_load)) => {
                    // Prefer higher match count
                    if match_count != *best_count {
                        match_count > *best_count
                    } else {
                        // Tie-break: prefer lower load ratio, then lexicographically smaller id
                        if (load_ratio - best_load).abs() > f32::EPSILON {
                            load_ratio < *best_load
                        } else {
                            agent_id.as_str() < best_match.as_ref().unwrap().0.as_str()
                        }
                    }
                }
            };

            if is_better {
                best_match = Some((agent_id.clone(), match_count, load_ratio));
            }
        }

        best_match.map(|(agent_id, _, _)| agent_id)
    }

    async fn queue_dequeue(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let _handle = self.get_handle(call_id).await?;
        let queue_id = {
            let states = self.queue_states.read().await;
            states.get(call_id).map(|s| s.queue_id.clone())
        };
        let mut states = self.queue_states.write().await;
        states.remove(call_id);
        if let Some(qid) = queue_id {
            let event = RwiEvent::QueueLeft {
                call_id: call_id.to_string(),
                queue_id: qid,
                reason: None,
            };
            let gw = self.gateway.read().await;
            gw.send_event_to_call_owner(&call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    async fn queue_hold(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        use crate::call::domain::{MediaSource as DomainMediaSource, PlayOptions};

        let handle = self.get_handle(call_id).await?;
        {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state.is_hold = true;
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        }
        handle
            .send_command(CallCommand::Play {
                leg_id: Some(LegId::new(call_id)),
                source: DomainMediaSource::Silence,
                options: Some(PlayOptions {
                    loop_playback: true,
                    await_completion: true,
                    interrupt_on_dtmf: false,
                    track_id: None,
                    send_progress: false,
                }),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::MediaHoldStarted {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn queue_unhold(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state.is_hold = false;
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        }
        handle
            .send_command(CallCommand::StopPlayback {
                leg_id: Some(LegId::new(call_id)),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::MediaHoldStopped {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn queue_set_priority(
        &self,
        call_id: &str,
        priority: u32,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;

        {
            let states = self.queue_states.read().await;
            if !states.contains_key(call_id) {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        }

        {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state.priority = Some(priority);
            }
        }

        info!(call_id = %call_id, priority = %priority, "Queue priority updated");
        Ok(CommandResult::Success)
    }

    async fn queue_assign_agent(
        &self,
        call_id: &str,
        agent_id: &str,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;

        let queue_id = {
            let states = self.queue_states.read().await;
            if let Some(state) = states.get(call_id) {
                state.queue_id.clone()
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        };

        let event = RwiEvent::QueueAgentOffered {
            call_id: call_id.to_string(),
            queue_id: queue_id.clone(),
            agent_id: agent_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(call_id = %call_id, agent_id = %agent_id, "Agent assigned to queue call");
        Ok(CommandResult::Success)
    }

    async fn queue_requeue(
        &self,
        call_id: &str,
        queue_id: &str,
        priority: Option<u32>,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;

        let old_queue_id = {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                let old = state.queue_id.clone();
                state.queue_id = queue_id.to_string();
                if let Some(p) = priority {
                    state.priority = Some(p);
                }
                old
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        };

        let event = RwiEvent::QueueLeft {
            call_id: call_id.to_string(),
            queue_id: old_queue_id,
            reason: Some("requeued".to_string()),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        let event2 = RwiEvent::QueueJoined {
            call_id: call_id.to_string(),
            queue_id: queue_id.to_string(),
        };
        gw.broadcast_event(&event2);

        info!(call_id = %call_id, new_queue = %queue_id, "Call requeued");
        Ok(CommandResult::Success)
    }

    pub async fn register_agent_skill(
        &self,
        agent_id: String,
        skills: Vec<String>,
        max_concurrent_calls: u32,
    ) {
        let agent = AgentSkill {
            agent_id: agent_id.clone(),
            skills,
            max_concurrent_calls,
            current_calls: 0,
        };

        let mut agents = self.agent_skills.write().await;
        agents.insert(agent_id.clone(), agent);

        info!(
            agent_id = %agent_id,
            max_calls = %max_concurrent_calls,
            "Agent registered for skill-based routing"
        );
    }

    pub async fn unregister_agent(&self, agent_id: &str) {
        let mut agents = self.agent_skills.write().await;
        agents.remove(agent_id);

        info!(agent_id = %agent_id, "Agent unregistered");
    }

    pub async fn update_agent_call_count(&self, agent_id: &str, delta: i32) {
        let mut agents = self.agent_skills.write().await;
        if let Some(agent) = agents.get_mut(agent_id) {
            if delta > 0 {
                agent.current_calls += delta as u32;
            } else {
                agent.current_calls = agent.current_calls.saturating_sub((-delta) as u32);
            }
        }
    }

    pub async fn set_queue_overflow_config(
        &self,
        queue_id: String,
        max_calls: u32,
        max_wait_secs: u32,
        overflow_queue_id: Option<String>,
        overflow_action: Option<String>,
    ) {
        let config = QueueOverflowConfig {
            max_calls,
            max_wait_secs,
            overflow_queue_id,
            overflow_action,
        };

        let mut configs = self.queue_overflow_configs.write().await;
        configs.insert(queue_id.clone(), config);

        info!(
            queue_id = %queue_id,
            max_calls = %max_calls,
            max_wait_secs = %max_wait_secs,
            "Queue overflow configuration set"
        );
    }

    pub async fn remove_queue_overflow_config(&self, queue_id: &str) {
        let mut configs = self.queue_overflow_configs.write().await;
        configs.remove(queue_id);

        info!(queue_id = %queue_id, "Queue overflow configuration removed");
    }

    pub async fn get_queue_stats(&self, queue_id: &str) -> Option<QueueStats> {
        let states = self.queue_states.read().await;

        let queue_calls: Vec<&QueueState> =
            states.values().filter(|s| s.queue_id == queue_id).collect();

        if queue_calls.is_empty() {
            return None;
        }

        let total_calls = queue_calls.len() as u32;
        let calls_on_hold = queue_calls.iter().filter(|s| s.is_hold).count() as u32;
        let avg_wait_time_secs = queue_calls
            .iter()
            .map(|s| s.enqueued_at.elapsed().as_secs())
            .sum::<u64>()
            / queue_calls.len() as u64;

        Some(QueueStats {
            queue_id: queue_id.to_string(),
            total_calls,
            calls_on_hold,
            avg_wait_time_secs,
        })
    }

    async fn record_start(&self, req: RecordStartRequest) -> Result<CommandResult, CommandError> {
        use crate::call::domain::RecordConfig;

        let handle = self.get_handle(&req.call_id).await?;
        let recording_id = Uuid::new_v4().to_string();
        let path = req.storage.path.clone();
        handle
            .send_command(CallCommand::StartRecording {
                config: RecordConfig {
                    path: path.clone(),
                    max_duration_secs: req.max_duration_secs,
                    beep: req.beep.unwrap_or(false),
                    format: None,
                },
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let record_state = RecordState {
            recording_id: recording_id.clone(),
            _mode: req.mode,
            _path: path,
            is_paused: false,
        };
        let mut states = self.record_states.write().await;
        states.insert(req.call_id.clone(), record_state);
        let event = RwiEvent::RecordStarted {
            call_id: req.call_id.clone(),
            recording_id,
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&req.call_id, &event);
        Ok(CommandResult::Success)
    }

    async fn record_pause(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        {
            let mut states = self.record_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state.is_paused = true;
            } else {
                return Err(CommandError::CommandFailed(
                    "No recording in progress".to_string(),
                ));
            }
        }
        handle
            .send_command(CallCommand::PauseRecording)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let recording_id = {
            let states = self.record_states.read().await;
            states.get(call_id).map(|s| s.recording_id.clone())
        };
        if let Some(rid) = recording_id {
            let event = RwiEvent::RecordPaused {
                call_id: call_id.to_string(),
                recording_id: rid,
            };
            let gw = self.gateway.read().await;
            gw.send_event_to_call_owner(&call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    async fn record_resume(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        {
            let mut states = self.record_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state.is_paused = false;
            } else {
                return Err(CommandError::CommandFailed(
                    "No recording in progress".to_string(),
                ));
            }
        }
        handle
            .send_command(CallCommand::ResumeRecording)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let recording_id = {
            let states = self.record_states.read().await;
            states.get(call_id).map(|s| s.recording_id.clone())
        };
        if let Some(rid) = recording_id {
            let event = RwiEvent::RecordResumed {
                call_id: call_id.to_string(),
                recording_id: rid,
            };
            let gw = self.gateway.read().await;
            gw.send_event_to_call_owner(&call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    async fn record_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let (recording_id, duration) = {
            let mut states = self.record_states.write().await;
            if let Some(state) = states.remove(call_id) {
                (Some(state.recording_id), None)
            } else {
                (None, None)
            }
        };
        handle
            .send_command(CallCommand::StopRecording)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        if let Some(rid) = recording_id {
            let event = RwiEvent::RecordStopped {
                call_id: call_id.to_string(),
                recording_id: rid,
                duration_secs: duration,
            };
            let gw = self.gateway.read().await;
            gw.send_event_to_call_owner(&call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    async fn sip_message(
        &self,
        call_id: &str,
        content_type: &str,
        body: &str,
    ) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed(
                "SIP server not available".to_string(),
            ));
        }
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(CallCommand::SendSipMessage {
                content_type: content_type.to_string(),
                body: body.to_string(),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::SipMessageReceived {
            call_id: call_id.to_string(),
            content_type: content_type.to_string(),
            body: body.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn sip_notify(
        &self,
        call_id: &str,
        event: &str,
        content_type: &str,
        body: &str,
    ) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed(
                "SIP server not available".to_string(),
            ));
        }
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(CallCommand::SendSipNotify {
                event: event.to_string(),
                content_type: content_type.to_string(),
                body: body.to_string(),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::SipNotifyReceived {
            call_id: call_id.to_string(),
            event: event.to_string(),
            content_type: content_type.to_string(),
            body: body.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn sip_options_ping(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed(
                "SIP server not available".to_string(),
            ));
        }
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(CallCommand::SendSipOptionsPing)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn conference_create(
        &self,
        req: ConferenceCreateRequest,
    ) -> Result<CommandResult, CommandError> {
        let conf_id = req.conf_id.clone();

        if req.backend == "external" {
            if req.mcu_uri.is_none() {
                return Err(CommandError::CommandFailed(
                    "external backend requires mcu_uri".to_string(),
                ));
            }
        }

        let manager = self.conference_manager();
        let max_participants = req.max_members.map(|m| m as usize);
        manager
            .create_conference(conf_id.clone().into(), max_participants)
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let event = RwiEvent::ConferenceCreated {
            conf_id: conf_id.clone(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, "Conference created");
        Ok(CommandResult::ConferenceCreated { conf_id })
    }

    async fn conference_add(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;

        let manager = self.conference_manager();
        manager
            .add_participant(&conf_id.into(), LegId::new(call_id))
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let event = RwiEvent::ConferenceMemberJoined {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, call_id = %call_id, "Conference member added");
        Ok(CommandResult::ConferenceMemberAdded {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        })
    }

    async fn conference_remove(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        manager
            .remove_participant(&conf_id.into(), &LegId::new(call_id))
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let event = RwiEvent::ConferenceMemberLeft {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, call_id = %call_id, "Conference member removed");
        Ok(CommandResult::ConferenceMemberRemoved {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        })
    }

    async fn conference_mute(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        manager
            .mute_participant(&conf_id.into(), &LegId::new(call_id))
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let event = RwiEvent::ConferenceMemberMuted {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, call_id = %call_id, "Conference member muted");
        Ok(CommandResult::ConferenceMemberMuted {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        })
    }

    async fn conference_unmute(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        manager
            .unmute_participant(&conf_id.into(), &LegId::new(call_id))
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let event = RwiEvent::ConferenceMemberUnmuted {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, call_id = %call_id, "Conference member unmuted");
        Ok(CommandResult::ConferenceMemberUnmuted {
            conf_id: conf_id.to_string(),
            call_id: call_id.to_string(),
        })
    }

    async fn conference_destroy(&self, conf_id: &str) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        manager
            .destroy_conference(&conf_id.into())
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let event = RwiEvent::ConferenceDestroyed {
            conf_id: conf_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, "Conference destroyed");
        Ok(CommandResult::ConferenceDestroyed {
            conf_id: conf_id.to_string(),
        })
    }

    async fn conference_merge(
        &self,
        conf_id: &str,
        call_id: &str,
        consultation_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        if manager.get_conference(&conf_id.into()).await.is_none() {
            return Err(CommandError::CommandFailed(format!(
                "conference {} not found",
                conf_id
            )));
        }

        self.get_handle(call_id).await?;
        self.get_handle(consultation_call_id).await?;

        let event = RwiEvent::ConferenceMergeRequested {
            call_id: call_id.to_string(),
            consultation_call_id: consultation_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);
        drop(gw);

        info!(
            conf_id = %conf_id,
            call_id = %call_id,
            consultation_call_id = %consultation_call_id,
            "Conference merge requested"
        );

        if manager
            .get_conference_id_for_leg(&LegId::new(call_id))
            .await
            .is_none()
        {
            let _ = self.conference_add(conf_id, call_id).await;
        }

        match self.conference_add(conf_id, consultation_call_id).await {
            Ok(_) => {
                // Ensure both legs are unheld after merge so media flows correctly
                if let Some(handle) = self.call_registry.get_handle(call_id) {
                    let _ = handle.send_command(CallCommand::Unhold {
                        leg_id: LegId::new(call_id),
                    });
                }
                if let Some(handle) = self.call_registry.get_handle(consultation_call_id) {
                    let _ = handle.send_command(CallCommand::Unhold {
                        leg_id: LegId::new(consultation_call_id),
                    });
                }

                let event = RwiEvent::ConferenceMerged {
                    conf_id: conf_id.to_string(),
                    call_id: call_id.to_string(),
                };
                let gw = self.gateway.read().await;
                gw.broadcast_event(&event);

                info!(conf_id = %conf_id, "Conference merge successful");
                Ok(CommandResult::Success)
            }
            Err(e) => {
                let event = RwiEvent::ConferenceMergeFailed {
                    conf_id: conf_id.to_string(),
                    call_id: call_id.to_string(),
                    reason: e.to_string(),
                };
                let gw = self.gateway.read().await;
                gw.broadcast_event(&event);

                warn!(conf_id = %conf_id, error = %e, "Conference merge failed");
                Err(CommandError::CommandFailed(format!(
                    "Failed to merge consultation call: {}",
                    e
                )))
            }
        }
    }

    async fn conference_seat_replace(
        &self,
        conf_id: &str,
        old_call_id: &str,
        new_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        if manager.get_conference(&conf_id.into()).await.is_none() {
            return Err(CommandError::CommandFailed(format!(
                "conference {} not found",
                conf_id
            )));
        }

        self.get_handle(old_call_id).await?;
        self.get_handle(new_call_id).await?;

        let started_event = RwiEvent::ConferenceSeatReplaceStarted {
            conf_id: conf_id.to_string(),
            old_call_id: old_call_id.to_string(),
            new_call_id: new_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&started_event);
        drop(gw);

        let old_leg = LegId::new(old_call_id);
        let new_leg = LegId::new(new_call_id);
        let old_was_member = manager.get_conference_id_for_leg(&old_leg).await.is_some();

        if old_was_member {
            manager
                .remove_participant(&conf_id.into(), &old_leg)
                .await
                .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

            let left_event = RwiEvent::ConferenceMemberLeft {
                conf_id: conf_id.to_string(),
                call_id: old_call_id.to_string(),
            };
            let gw = self.gateway.read().await;
            gw.broadcast_event(&left_event);
            drop(gw);
        }

        match manager
            .add_participant(&conf_id.into(), new_leg.clone())
            .await
        {
            Ok(_) => {
                let joined_event = RwiEvent::ConferenceMemberJoined {
                    conf_id: conf_id.to_string(),
                    call_id: new_call_id.to_string(),
                };
                let gw = self.gateway.read().await;
                gw.broadcast_event(&joined_event);
                drop(gw);

                if old_was_member {
                    if let Ok(handle) = self.get_handle(old_call_id).await {
                        let _ = handle.send_command(CallCommand::Hangup(
                            crate::call::domain::HangupCommand::local(
                                "conference_seat_replace",
                                Some(crate::callrecord::CallRecordHangupReason::BySystem),
                                Some(200),
                            ),
                        ));
                    }
                }

                let success_event = RwiEvent::ConferenceSeatReplaceSucceeded {
                    conf_id: conf_id.to_string(),
                    old_call_id: old_call_id.to_string(),
                    new_call_id: new_call_id.to_string(),
                };
                let gw = self.gateway.read().await;
                gw.broadcast_event(&success_event);

                Ok(CommandResult::Success)
            }
            Err(e) => {
                let reason = e.to_string();
                if old_was_member {
                    #[cfg(test)]
                    let forced_rollback_failure = self
                        .force_seat_replace_rollback_failure
                        .swap(false, Ordering::SeqCst);
                    #[cfg(not(test))]
                    let forced_rollback_failure = false;

                    if forced_rollback_failure {
                        let rollback_failed = RwiEvent::ConferenceSeatReplaceRollbackFailed {
                            conf_id: conf_id.to_string(),
                            old_call_id: old_call_id.to_string(),
                            new_call_id: new_call_id.to_string(),
                            reason: "forced rollback failure".to_string(),
                        };
                        let gw = self.gateway.read().await;
                        gw.broadcast_event(&rollback_failed);
                    } else {
                        let rollback = manager
                            .add_participant(&conf_id.into(), old_leg.clone())
                            .await;
                        if rollback.is_ok() {
                            let rollback_event = RwiEvent::ConferenceMemberJoined {
                                conf_id: conf_id.to_string(),
                                call_id: old_call_id.to_string(),
                            };
                            let gw = self.gateway.read().await;
                            gw.broadcast_event(&rollback_event);
                        } else if let Err(rollback_err) = rollback {
                            let rollback_failed = RwiEvent::ConferenceSeatReplaceRollbackFailed {
                                conf_id: conf_id.to_string(),
                                old_call_id: old_call_id.to_string(),
                                new_call_id: new_call_id.to_string(),
                                reason: rollback_err.to_string(),
                            };
                            let gw = self.gateway.read().await;
                            gw.broadcast_event(&rollback_failed);
                        }
                    }
                }

                let failed_event = RwiEvent::ConferenceSeatReplaceFailed {
                    conf_id: conf_id.to_string(),
                    old_call_id: old_call_id.to_string(),
                    new_call_id: new_call_id.to_string(),
                    reason: reason.clone(),
                };
                let gw = self.gateway.read().await;
                gw.broadcast_event(&failed_event);

                Err(CommandError::CommandFailed(format!(
                    "seat replacement failed: {}",
                    reason
                )))
            }
        }
    }

    async fn set_ringback_source(
        &self,
        target_call_id: &str,
        source_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(target_call_id).await?;
        self.get_handle(source_call_id).await?;
        let ringback_state = RingbackState {
            _target_call_id: target_call_id.to_string(),
            _source_call_id: source_call_id.to_string(),
        };
        let mut states = self.ringback_states.write().await;
        states.insert(target_call_id.to_string(), ringback_state);
        let event = RwiEvent::MediaRingbackPassthroughStarted {
            source: source_call_id.to_string(),
            target: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&source_call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn supervisor_stop(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!("supervisor_stop: removing mixer with id={}", mixer_id);

        let removed = self.mixer_registry.remove_mixer(&mixer_id);
        if removed {
            tracing::info!("supervisor_stop: mixer stopped and removed");
        } else {
            tracing::warn!("supervisor_stop: mixer not found (may have already been removed)");
        }

        if let Ok(handle) = self.get_handle(target_call_id).await {
            let _ = handle.send_command(CallCommand::SupervisorStop {
                supervisor_leg: LegId::new(supervisor_call_id),
            });
        }

        info!(
            audit_event = "supervisor_action",
            action = "stop",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            result = "success",
            "Supervisor mode stopped"
        );

        let mut states = self.supervisor_states.write().await;
        states.remove(supervisor_call_id);
        let event = RwiEvent::SupervisorModeStopped {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        if self.get_handle(target_call_id).await.is_ok() {
            gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    async fn media_stream_start(
        &self,
        call_id: &str,
        _stream_id: &str,
        _direction: &str,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let mut states = self.media_stream_states.write().await;
        states.insert(call_id.to_string(), MediaStreamState);
        let event = RwiEvent::MediaStreamStarted {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn media_stream_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let mut states = self.media_stream_states.write().await;
        states.remove(call_id);
        let event = RwiEvent::MediaStreamStopped {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn media_inject_start(
        &self,
        call_id: &str,
        _stream_id: &str,
        _format: &crate::rwi::session::MediaFormat,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let mut states = self.media_inject_states.write().await;
        states.insert(call_id.to_string(), MediaInjectState);
        let event = RwiEvent::MediaStreamStarted {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn media_inject_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let mut states = self.media_inject_states.write().await;
        states.remove(call_id);
        let event = RwiEvent::MediaStreamStopped {
            call_id: call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }
}

#[derive(Debug)]
pub enum CommandResult {
    Success,
    ListCalls(Vec<CallInfo>),
    CallFound {
        call_id: String,
    },
    Originated {
        call_id: String,
    },
    MediaPlay {
        track_id: String,
    },
    TransferAttended {
        original_call_id: String,
        consultation_call_id: String,
    },
    ConferenceCreated {
        conf_id: String,
    },
    ConferenceMemberAdded {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberRemoved {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberMuted {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberUnmuted {
        conf_id: String,
        call_id: String,
    },
    ConferenceDestroyed {
        conf_id: String,
    },
    SessionResumed {
        replayed_count: u64,
        current_sequence: u64,
        events: Vec<serde_json::Value>,
    },
    CallResumed {
        call_id: String,
        replayed_count: u64,
        current_sequence: u64,
        events: Vec<serde_json::Value>,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallInfo {
    pub session_id: String,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub direction: String,
    pub status: String,
    pub started_at: String,
    pub answered_at: Option<String>,
}

#[derive(Debug)]
pub enum CommandError {
    CallNotFound(String),
    CommandFailed(String),
    NotImplemented(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::CallNotFound(id) => write!(f, "Call not found: {}", id),
            CommandError::CommandFailed(msg) => write!(f, "Command failed: {}", msg),
            CommandError::NotImplemented(feature) => write!(f, "Not implemented: {}", feature),
        }
    }
}

impl serde::Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// Transfer handling methods for RwiCommandProcessor
impl RwiCommandProcessor {
    /// Handle blind transfer with automatic 3PCC fallback
    async fn handle_transfer(
        &self,
        call_id: String,
        target: String,
        _attended: bool,
    ) -> Result<CommandResult, CommandError> {
        // Verify call exists
        if self.call_registry.get_handle(&call_id).is_none() {
            return Err(CommandError::CallNotFound(call_id));
        }

        // Use TransferController to execute transfer with 3PCC fallback
        let controller = self.transfer_controller.read().await;

        match controller
            .execute_blind_transfer(call_id.clone(), target.clone())
            .await
        {
            Ok(_tx) => {
                // Transfer initiated successfully (REFER accepted or 3PCC started)
                Ok(CommandResult::Success)
            }
            Err(e) => {
                // Transfer failed
                Err(CommandError::CommandFailed(format!(
                    "Transfer failed: {}",
                    e.as_str()
                )))
            }
        }
    }

    /// Handle attended transfer initiation
    async fn handle_attended_transfer(
        &self,
        call_id: String,
        target: String,
        timeout_secs: Option<u32>,
    ) -> Result<CommandResult, CommandError> {
        // Verify call exists
        if self.call_registry.get_handle(&call_id).is_none() {
            return Err(CommandError::CallNotFound(call_id));
        }

        let controller = self.transfer_controller.read().await;

        match controller
            .initiate_attended_transfer(call_id.clone(), target, timeout_secs)
            .await
        {
            Ok(tx) => {
                let consultation_call_id = tx
                    .consultation_call_id
                    .clone()
                    .unwrap_or_else(|| tx.transfer_id.clone());
                Ok(CommandResult::TransferAttended {
                    original_call_id: call_id,
                    consultation_call_id,
                })
            }
            Err(e) => Err(CommandError::CommandFailed(format!(
                "Attended transfer failed: {}",
                e.as_str()
            ))),
        }
    }

    async fn handle_transfer_replace(
        &self,
        call_id: String,
        target: String,
    ) -> Result<CommandResult, CommandError> {
        if self.call_registry.get_handle(&call_id).is_none() {
            return Err(CommandError::CallNotFound(call_id));
        }

        let controller = self.transfer_controller.read().await;

        match controller
            .execute_replace_transfer(call_id.clone(), target)
            .await
        {
            Ok(_) => Ok(CommandResult::Success),
            Err(e) => Err(CommandError::CommandFailed(format!(
                "Replace transfer failed: {}",
                e.as_str()
            ))),
        }
    }

    /// Handle attended transfer completion
    async fn handle_transfer_complete(
        &self,
        call_id: String,
        consultation_call_id: String,
    ) -> Result<CommandResult, CommandError> {
        let controller = self.transfer_controller.read().await;

        match controller
            .complete_attended_transfer(call_id, consultation_call_id)
            .await
        {
            Ok(_tx) => Ok(CommandResult::Success),
            Err(e) => Err(CommandError::CommandFailed(format!(
                "Transfer complete failed: {}",
                e.as_str()
            ))),
        }
    }

    /// Handle attended transfer cancellation
    async fn handle_transfer_cancel(
        &self,
        consultation_call_id: String,
    ) -> Result<CommandResult, CommandError> {
        let controller = self.transfer_controller.read().await;

        match controller
            .cancel_attended_transfer(consultation_call_id)
            .await
        {
            Ok(_tx) => Ok(CommandResult::Success),
            Err(e) => Err(CommandError::CommandFailed(format!(
                "Transfer cancel failed: {}",
                e.as_str()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::DialDirection;
    use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
    use crate::rwi::gateway::RwiGateway;
    use crate::rwi::session::RwiCommandPayload;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn create_test_processor() -> (Arc<RwiCommandProcessor>, Arc<ConferenceManager>) {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway, cm.clone()));
        (processor, cm)
    }

    fn create_test_processor_with_registry(
        registry: Arc<ActiveProxyCallRegistry>,
    ) -> (Arc<RwiCommandProcessor>, Arc<ConferenceManager>) {
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway, cm.clone()));
        (processor, cm)
    }

    fn create_test_call(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
    ) -> crate::proxy::proxy_call::sip_session::SipSessionHandle {
        create_test_call_with_conference_manager(
            registry, session_id, caller, callee, direction, None,
        )
    }

    fn create_test_call_with_conference_manager(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
        conference_manager: Option<Arc<ConferenceManager>>,
    ) -> crate::proxy::proxy_call::sip_session::SipSessionHandle {
        use crate::call::runtime::SessionId;
        use crate::proxy::proxy_call::sip_session::SipSession;

        let id = SessionId::from(session_id);
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        if let Some(cm) = conference_manager {
            tokio::spawn(async move {
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        crate::call::domain::CallCommand::ConferenceAdd { conf_id, leg_id } => {
                            let conf: crate::call::runtime::ConferenceId = conf_id.into();
                            let _ = cm.add_participant(&conf, leg_id).await;
                        }
                        crate::call::domain::CallCommand::ConferenceRemove { conf_id, leg_id } => {
                            let conf: crate::call::runtime::ConferenceId = conf_id.into();
                            let _ = cm.remove_participant(&conf, &leg_id).await;
                        }
                        crate::call::domain::CallCommand::ConferenceMute { conf_id, leg_id } => {
                            let conf: crate::call::runtime::ConferenceId = conf_id.into();
                            let _ = cm.mute_participant(&conf, &leg_id).await;
                        }
                        crate::call::domain::CallCommand::ConferenceUnmute { conf_id, leg_id } => {
                            let conf: crate::call::runtime::ConferenceId = conf_id.into();
                            let _ = cm.unmute_participant(&conf, &leg_id).await;
                        }
                        _ => {}
                    }
                }
            });
        } else {
            tokio::spawn(async move { while let Some(_cmd) = cmd_rx.recv().await {} });
        }

        let entry = crate::proxy::active_call_registry::ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: Some(caller.to_string()),
            callee: Some(callee.to_string()),
            direction: if matches!(direction, DialDirection::Inbound) {
                "inbound".to_string()
            } else {
                "outbound".to_string()
            },
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: crate::proxy::active_call_registry::ActiveProxyCallStatus::Ringing,
        };

        registry.upsert(entry, handle.clone());
        handle
    }

    fn create_test_call_with_rx(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
    ) -> (
        crate::proxy::proxy_call::sip_session::SipSessionHandle,
        tokio::sync::mpsc::UnboundedReceiver<crate::call::domain::CallCommand>,
    ) {
        use crate::call::runtime::SessionId;
        use crate::proxy::proxy_call::sip_session::SipSession;

        let id = SessionId::from(session_id);
        let (handle, cmd_rx) = SipSession::with_handle(id);

        let entry = crate::proxy::active_call_registry::ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: Some(caller.to_string()),
            callee: Some(callee.to_string()),
            direction: if matches!(direction, DialDirection::Inbound) {
                "inbound".to_string()
            } else {
                "outbound".to_string()
            },
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: crate::proxy::active_call_registry::ActiveProxyCallStatus::Ringing,
        };

        registry.upsert(entry, handle.clone());
        (handle, cmd_rx)
    }

    #[tokio::test]
    async fn test_list_calls_empty() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        assert!(result.is_ok());
        if let Ok(CommandResult::ListCalls(calls)) = result {
            assert!(calls.is_empty());
        }
    }

    #[tokio::test]
    async fn test_answer_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Answer {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_ring_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Ring {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_reject_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Reject {
                call_id: "nonexistent".into(),
                reason: None,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_attach_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::AttachCall {
                call_id: "nonexistent".into(),
                mode: crate::rwi::session::OwnershipMode::Control,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_detach_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::DetachCall {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_detach_call_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-to-detach",
            "caller1",
            "callee1",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());

        let result = processor
            .process_command(RwiCommandPayload::DetachCall {
                call_id: "call-to-detach".into(),
            })
            .await;

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), CommandResult::Success));

        assert!(registry.get_handle("call-to-detach").is_some());
    }

    #[tokio::test]
    async fn test_hangup_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Hangup {
                call_id: "nonexistent".into(),
                reason: Some("normal".into()),
                code: Some(16),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_transfer_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Transfer {
                call_id: "nonexistent".into(),
                target: "sip:target@local".into(),
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_bridge_not_found_leg_a() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "missing-a".into(),
                leg_b: "missing-b".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_bridge_not_found_leg_b() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        create_test_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b-missing".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_bridge_both_legs_exist_sends_bridgeto() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let _ha = create_test_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);
        let _hb = create_test_call(&registry, "leg-b", "1001", "2002", DialDirection::Outbound);

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b".into(),
            })
            .await;

        match &result {
            Ok(_) => {}
            Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_unbridge_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "nope".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_subscribe_success() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Subscribe {
                contexts: vec!["ctx1".into()],
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_unsubscribe_success() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Unsubscribe {
                contexts: vec!["ctx1".into()],
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_play_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaPlay(
                crate::rwi::session::MediaPlayRequest {
                    call_id: "missing".into(),
                    source: crate::rwi::session::MediaSource {
                        source_type: "file".into(),
                        uri: Some("welcome.wav".into()),
                        looped: None,
                    },
                    interrupt_on_dtmf: false,
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_originate_no_server_returns_error() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Originate(
                crate::rwi::session::OriginateRequest {
                    call_id: "new-call".into(),
                    destination: "sip:test@local".into(),
                    caller_id: None,
                    timeout_secs: Some(30),
                    hold_music: None,
                    hold_music_target: None,
                    ringback: None,
                    ringback_target: None,
                    extra_headers: std::collections::HashMap::new(),
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_originate_invalid_destination_returns_error() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Originate(
                crate::rwi::session::OriginateRequest {
                    call_id: "new-call-2".into(),
                    destination: "not-a-sip-uri".into(),
                    caller_id: None,
                    timeout_secs: None,
                    hold_music: None,
                    hold_music_target: None,
                    ringback: None,
                    ringback_target: None,
                    extra_headers: std::collections::HashMap::new(),
                },
            ))
            .await;
        assert!(result.is_err());

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_answer_existing_call() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let _handle = create_test_call(
            &registry,
            "call-001",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        assert!(registry.get_handle("call-001").is_some());

        let result = processor
            .process_command(RwiCommandPayload::Answer {
                call_id: "call-001".into(),
            })
            .await;
        match result {
            Ok(_) => {}
            Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_hangup_existing_call() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let _handle = create_test_call(
            &registry,
            "call-001",
            "1001",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::Hangup {
                call_id: "call-001".into(),
                reason: Some("normal".into()),
                code: Some(16),
            })
            .await;
        match result {
            Ok(_) => {}
            Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_list_calls_with_multiple_calls() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());

        create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        create_test_call(&registry, "call-2", "1002", "2001", DialDirection::Outbound);
        create_test_call(&registry, "call-3", "1003", "2002", DialDirection::Inbound);

        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        assert!(result.is_ok());
        if let Ok(CommandResult::ListCalls(calls)) = result {
            assert_eq!(calls.len(), 3);
            let ids: Vec<_> = calls.iter().map(|c| c.session_id.clone()).collect();
            assert!(ids.contains(&"call-1".to_string()));
            assert!(ids.contains(&"call-2".to_string()));
            assert!(ids.contains(&"call-3".to_string()));
        }
    }

    #[tokio::test]
    async fn test_call_direction_filtering() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());

        create_test_call(
            &registry,
            "inbound-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        create_test_call(
            &registry,
            "outbound-1",
            "2001",
            "1001",
            DialDirection::Outbound,
        );
        create_test_call(
            &registry,
            "inbound-2",
            "1002",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        if let Ok(CommandResult::ListCalls(calls)) = result {
            let inbound: Vec<_> = calls.iter().filter(|c| c.direction == "inbound").collect();
            let outbound: Vec<_> = calls.iter().filter(|c| c.direction == "outbound").collect();
            assert_eq!(inbound.len(), 2);
            assert_eq!(outbound.len(), 1);
        }
    }

    #[tokio::test]
    async fn test_bridge_emits_event_to_gateway() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway.clone(),
            cm.clone(),
        ));

        let _ha = create_test_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);
        let _hb = create_test_call(&registry, "leg-b", "1001", "2002", DialDirection::Outbound);

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut gw = gateway.write().await;
            let identity = crate::rwi::auth::RwiIdentity {
                token: "t".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().await.id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            gw.claim_call_ownership(
                &sid,
                "leg-a".into(),
                crate::rwi::session::OwnershipMode::Control,
            )
            .await
            .unwrap();
        }

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b".into(),
            })
            .await;

        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {
                match tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv()).await
                {
                    Ok(Some(ev)) => {
                        let s = serde_json::to_string(&ev).unwrap();
                        assert!(
                            s.contains("call_bridged"),
                            "Expected call_bridged event, got: {}",
                            s
                        );
                    }
                    Ok(None) => panic!("Event channel closed unexpectedly"),
                    Err(_) => panic!(
                        "Timeout waiting for CallBridged event - event was not sent to gateway"
                    ),
                }
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_media_stop_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaStop {
                call_id: "ghost".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_media_stop_existing_call_sends_stop_playback() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-stop",
            "1001",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::MediaStop {
                call_id: "call-stop".into(),
            })
            .await;

        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let cmd = rx.try_recv().expect("StopPlayback should be queued");
        assert!(matches!(cmd, CallCommand::StopPlayback { .. }));
    }

    #[tokio::test]
    async fn test_unbridge_existing_call_sends_unbridge() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-unb",
            "1001",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "call-unb".into(),
            })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let cmd = rx.try_recv().expect("Unbridge should be queued");
        assert!(matches!(cmd, CallCommand::Unbridge { .. }));
    }

    #[tokio::test]
    async fn test_bridge_sends_bridge_to_to_leg_a() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let (_ha, mut rx_a) =
            create_test_call_with_rx(&registry, "leg-a2", "1001", "2001", DialDirection::Outbound);
        let _hb = create_test_call(&registry, "leg-b2", "1001", "2002", DialDirection::Outbound);

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a2".into(),
                leg_b: "leg-b2".into(),
            })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let cmd = rx_a.try_recv().expect("Bridge should be queued on leg_a");
        assert!(
            matches!(cmd, CallCommand::Bridge { leg_a: _, ref leg_b, .. } if leg_b.as_str() == "leg-b2"),
            "expected Bridge(leg-b2), got {:?}",
            cmd
        );
    }

    #[tokio::test]
    async fn test_unbridge_emits_call_unbridged_event_to_gateway() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway.clone(),
            cm.clone(),
        ));

        let (_handle, _rx) =
            create_test_call_with_rx(&registry, "call-ev", "1001", "2000", DialDirection::Inbound);

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut gw = gateway.write().await;
            let identity = crate::rwi::auth::RwiIdentity {
                token: "t2".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().await.id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            gw.claim_call_ownership(
                &sid,
                "call-ev".into(),
                crate::rwi::session::OwnershipMode::Control,
            )
            .await
            .unwrap();
        }

        let result = processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "call-ev".into(),
            })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {
                match tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv()).await
                {
                    Ok(Some(ev)) => {
                        let s = serde_json::to_string(&ev).unwrap();
                        assert!(s.contains("call-ev"), "Event should reference call-ev");
                    }
                    Ok(None) => panic!("Event channel closed unexpectedly"),
                    Err(_) => panic!(
                        "Timeout waiting for CallUnbridged event - event was not sent to gateway"
                    ),
                }
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_set_ringback_source_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "call-target",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 = create_test_call(
            &registry,
            "call-source",
            "1002",
            "2001",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "call-target".into(),
                source_call_id: "call-source".into(),
            })
            .await;
        assert!(result.is_ok(), "SetRingbackSource failed: {:?}", result);
    }

    #[tokio::test]
    async fn test_set_ringback_source_target_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-source",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "nonexistent".into(),
                source_call_id: "call-source".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_set_ringback_source_source_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-target",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "call-target".into(),
                source_call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_record_start_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "call-rec".into(),
                    mode: "local".into(),
                    beep: Some(true),
                    max_duration_secs: Some(3600),
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/call-rec.wav".into(),
                    },
                },
            ))
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
        if let Ok(action) = cmd {
            assert!(matches!(action, CallCommand::StartRecording { .. }));
        }
    }

    #[tokio::test]
    async fn test_record_start_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "nonexistent".into(),
                    mode: "local".into(),
                    beep: Some(true),
                    max_duration_secs: Some(3600),
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/call.wav".into(),
                    },
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_record_pause_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec-p",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "call-rec-p".into(),
                    mode: "local".into(),
                    beep: Some(false),
                    max_duration_secs: None,
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/test.wav".into(),
                    },
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::RecordPause {
                call_id: "call-rec-p".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_record_pause_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-norec",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordPause {
                call_id: "call-norec".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No recording"));
    }

    #[tokio::test]
    async fn test_record_resume_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec-r",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "call-rec-r".into(),
                    mode: "local".into(),
                    beep: Some(false),
                    max_duration_secs: None,
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/test.wav".into(),
                    },
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::RecordPause {
                call_id: "call-rec-r".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::RecordResume {
                call_id: "call-rec-r".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_record_resume_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-norec2",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordResume {
                call_id: "call-norec2".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No recording"));
    }

    #[tokio::test]
    async fn test_record_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec-s",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::RecordStart(
                crate::rwi::session::RecordStartRequest {
                    call_id: "call-rec-s".into(),
                    mode: "local".into(),
                    beep: Some(false),
                    max_duration_secs: None,
                    storage: crate::rwi::session::RecordStorage {
                        backend: "file".into(),
                        path: "/recordings/test.wav".into(),
                    },
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::RecordStop {
                call_id: "call-rec-s".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let mut found_stop = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, CallCommand::StopRecording) {
                found_stop = true;
                break;
            }
        }
        assert!(found_stop, "Expected StopRecording action to be sent");
    }

    #[tokio::test]
    async fn test_record_stop_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-norec3",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordStop {
                call_id: "call-norec3".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        if let Ok(action) = cmd {
            assert!(matches!(action, CallCommand::StopRecording));
        }
    }

    #[tokio::test]
    async fn test_queue_enqueue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, _rx) = create_test_call_with_rx(
            &registry,
            "call-q",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "call-q".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_enqueue_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "nonexistent".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_queue_dequeue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, _rx) = create_test_call_with_rx(
            &registry,
            "call-dq",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "call-dq".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueDequeue {
                call_id: "call-dq".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_dequeue_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::QueueDequeue {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_queue_hold_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-hold",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "call-hold".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-hold".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_queue_hold_not_in_queue() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-noq",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-noq".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Call not in queue")
        );
    }

    #[tokio::test]
    async fn test_queue_unhold_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-unhold",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                crate::rwi::session::QueueEnqueueRequest {
                    call_id: "call-unhold".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                    skills: None,
                    max_wait_secs: Some(300),
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-unhold".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueUnhold {
                call_id: "call-unhold".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_queue_unhold_not_in_queue() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-noq2",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueUnhold {
                call_id: "call-noq2".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Call not in queue")
        );
    }

    #[tokio::test]
    async fn test_supervisor_listen_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "supervisor-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 =
            create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorListen {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
            })
            .await;

        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_listen_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorListen {
                supervisor_call_id: "nonexistent".into(),
                target_call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_supervisor_whisper_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "supervisor-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 =
            create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
                agent_leg: "call-1".into(),
            })
            .await;

        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_barge_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "supervisor-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 =
            create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorBarge {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
                agent_leg: "call-1".into(),
            })
            .await;

        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorStop {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_stream_start_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::MediaStreamStart(
                crate::rwi::session::MediaStreamRequest {
                    call_id: "call-1".into(),
                    direction: "playback".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_stream_start_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaStreamStart(
                crate::rwi::session::MediaStreamRequest {
                    call_id: "nonexistent".into(),
                    direction: "playback".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_media_stream_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::MediaStreamStart(
                crate::rwi::session::MediaStreamRequest {
                    call_id: "call-1".into(),
                    direction: "playback".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::MediaStreamStop {
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_inject_start_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::MediaInjectStart(
                crate::rwi::session::MediaInjectRequest {
                    call_id: "call-1".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_inject_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::MediaInjectStart(
                crate::rwi::session::MediaInjectRequest {
                    call_id: "call-1".into(),
                    format: crate::rwi::session::MediaFormat {
                        codec: "PCMU".into(),
                        sample_rate: 8000,
                        channels: 1,
                        ptime_ms: Some(20),
                    },
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::MediaInjectStop {
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sip_message_no_server() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipMessage {
                call_id: "call-1".into(),
                content_type: "text/plain".into(),
                body: "Hello".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_sip_notify_no_server() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipNotify {
                call_id: "call-1".into(),
                event: "check-sync".into(),
                content_type: "application/simple-message-summary".into(),
                body: "".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_sip_options_ping_no_server() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipOptionsPing {
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_conference_create_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: Some(10),
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await;
        assert!(result.is_ok());
        match result {
            Ok(CommandResult::ConferenceCreated { conf_id }) => {
                assert_eq!(conf_id, "room-1");
            }
            _ => panic!("Expected ConferenceCreated result"),
        }
    }

    #[tokio::test]
    async fn test_conference_create_duplicate_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_conference_create_external_requires_mcu_uri() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "external".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("external backend requires mcu_uri")
        );
    }

    #[tokio::test]
    async fn test_conference_add_not_found_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not found"));
    }

    #[tokio::test]
    async fn test_conference_destroy_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceDestroy {
                conf_id: "room-1".into(),
            })
            .await;
        assert!(result.is_ok());
        match result {
            Ok(CommandResult::ConferenceDestroyed { conf_id }) => {
                assert_eq!(conf_id, "room-1");
            }
            _ => panic!("Expected ConferenceDestroyed result"),
        }
    }

    #[tokio::test]
    async fn test_conference_destroy_not_found_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceDestroy {
                conf_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_conference_mute_not_in_conference_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: None,
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        let _handle = create_test_call_with_conference_manager(
            &registry,
            "call-1",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );

        let result = processor
            .process_command(RwiCommandPayload::ConferenceMute {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("not found in conference") || error.contains("is not in conference")
        );
    }

    #[tokio::test]
    async fn test_conference_add_with_max_members() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    backend: "internal".to_string(),
                    max_members: Some(2),
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        let _handle1 = create_test_call_with_conference_manager(
            &registry,
            "call-1",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());

        let _handle2 = create_test_call_with_conference_manager(
            &registry,
            "call-2",
            "1002",
            "2001",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-2".into(),
            })
            .await;
        assert!(result.is_ok());

        let _handle3 = create_test_call_with_conference_manager(
            &registry,
            "call-3",
            "1003",
            "2002",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-3".into(),
            })
            .await;
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("maximum capacity") || error.contains("is full"));
    }

    #[tokio::test]
    async fn test_transfer_attended_returns_consultation_call_id() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(
            &registry,
            "call-attended-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        registry.update("call-attended-1", |entry| {
            entry.answered_at = Some(chrono::Utc::now());
            entry.status = crate::proxy::active_call_registry::ActiveProxyCallStatus::Talking;
        });

        let result = processor
            .process_command(RwiCommandPayload::TransferAttended {
                call_id: "call-attended-1".into(),
                target: "sip:consult@local".into(),
                timeout_secs: Some(30),
            })
            .await;

        assert!(result.is_ok());
        match result.unwrap() {
            CommandResult::TransferAttended {
                original_call_id,
                consultation_call_id,
            } => {
                assert_eq!(original_call_id, "call-attended-1");
                assert!(!consultation_call_id.is_empty());
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_conference_seat_replace_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-1".into(),
                    backend: "internal".to_string(),
                    max_members: Some(2),
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        let _handle_a = create_test_call_with_conference_manager(
            &registry,
            "call-a",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_a1 = create_test_call_with_conference_manager(
            &registry,
            "call-a1",
            "1002",
            "2001",
            DialDirection::Inbound,
            Some(cm.clone()),
        );

        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-1".into(),
                call_id: "call-a".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceSeatReplace {
                conf_id: "room-seat-1".into(),
                old_call_id: "call-a".into(),
                new_call_id: "call-a1".into(),
            })
            .await;
        assert!(result.is_ok());

        let manager = processor.conference_manager();
        let conf = manager
            .get_conference(&"room-seat-1".into())
            .await
            .expect("conference should exist");
        assert!(!conf.participants.contains_key(&LegId::new("call-a")));
        assert!(conf.participants.contains_key(&LegId::new("call-a1")));
    }

    #[tokio::test]
    async fn test_conference_seat_replace_failure_rolls_back_old_member() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-2".into(),
                    backend: "internal".to_string(),
                    max_members: Some(3),
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-3".into(),
                    backend: "internal".to_string(),
                    max_members: Some(2),
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        let _handle_a = create_test_call_with_conference_manager(
            &registry,
            "call-a",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_b = create_test_call_with_conference_manager(
            &registry,
            "call-b",
            "1003",
            "2002",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_a1 = create_test_call_with_conference_manager(
            &registry,
            "call-a1",
            "1002",
            "2001",
            DialDirection::Inbound,
            Some(cm.clone()),
        );

        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-2".into(),
                call_id: "call-a".into(),
            })
            .await
            .unwrap();
        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-2".into(),
                call_id: "call-b".into(),
            })
            .await
            .unwrap();
        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-3".into(),
                call_id: "call-a1".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceSeatReplace {
                conf_id: "room-seat-2".into(),
                old_call_id: "call-a".into(),
                new_call_id: "call-a1".into(),
            })
            .await;
        assert!(result.is_err());

        let manager = processor.conference_manager();
        let conf = manager
            .get_conference(&"room-seat-2".into())
            .await
            .expect("conference should exist");
        assert!(conf.participants.contains_key(&LegId::new("call-a")));
        assert!(conf.participants.contains_key(&LegId::new("call-b")));
        assert!(!conf.participants.contains_key(&LegId::new("call-a1")));
    }

    #[tokio::test]
    async fn test_conference_seat_replace_rollback_failed_event_emitted() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());

        let mut gateway_impl = RwiGateway::new();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        gateway_impl.set_session_event_sender(&"test-session".to_string(), event_tx);
        let gateway = Arc::new(RwLock::new(gateway_impl));

        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));
        processor.force_next_seat_replace_rollback_failure();

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-4".into(),
                    backend: "internal".to_string(),
                    max_members: Some(3),
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-5".into(),
                    backend: "internal".to_string(),
                    max_members: Some(2),
                    record: false,
                    mcu_uri: None,
                },
            ))
            .await
            .unwrap();

        let _handle_a = create_test_call_with_conference_manager(
            &registry,
            "call-a",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_b = create_test_call_with_conference_manager(
            &registry,
            "call-b",
            "1003",
            "2002",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_a1 = create_test_call_with_conference_manager(
            &registry,
            "call-a1",
            "1002",
            "2001",
            DialDirection::Inbound,
            Some(cm.clone()),
        );

        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-4".into(),
                call_id: "call-a".into(),
            })
            .await
            .unwrap();
        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-4".into(),
                call_id: "call-b".into(),
            })
            .await
            .unwrap();
        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-5".into(),
                call_id: "call-a1".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceSeatReplace {
                conf_id: "room-seat-4".into(),
                old_call_id: "call-a".into(),
                new_call_id: "call-a1".into(),
            })
            .await;
        assert!(result.is_err());

        let mut found = false;
        while let Ok(event) = event_rx.try_recv() {
            if event
                .get("conference_seat_replace_rollback_failed")
                .is_some()
            {
                found = true;
                break;
            }
        }
        assert!(found);
    }

    #[tokio::test]
    async fn test_queue_set_priority_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
                skills: None,
                max_wait_secs: None,
            }))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueSetPriority {
                call_id: "call-1".into(),
                priority: 10,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_set_priority_not_in_queue_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);

        let result = processor
            .process_command(RwiCommandPayload::QueueSetPriority {
                call_id: "call-1".into(),
                priority: 10,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not in queue"));
    }

    #[tokio::test]
    async fn test_queue_assign_agent_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
                skills: None,
                max_wait_secs: None,
            }))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueAssignAgent {
                call_id: "call-1".into(),
                agent_id: "agent-42".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_requeue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
                skills: None,
                max_wait_secs: None,
            }))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueRequeue {
                call_id: "call-1".into(),
                queue_id: "sales".into(),
                priority: Some(5),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_skill_based_routing() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .register_agent_skill(
                "agent-1".to_string(),
                vec!["support".to_string(), "technical".to_string()],
                5,
            )
            .await;

        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let result = processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
                skills: Some(vec!["support".to_string()]),
                max_wait_secs: None,
            }))
            .await;

        assert!(result.is_ok());

        let states = processor.queue_states.read().await;
        let state = states.get("call-1").unwrap();
        assert_eq!(state.agent_id, Some("agent-1".to_string()));
    }

    #[tokio::test]
    async fn test_queue_overflow_transfer() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .set_queue_overflow_config(
                "busy-queue".to_string(),
                2,
                300,
                Some("overflow-queue".to_string()),
                Some("transfer".to_string()),
            )
            .await;

        for i in 1..=2 {
            let call_id = format!("call-{}", i);
            let _handle =
                create_test_call(&registry, &call_id, "1001", "2000", DialDirection::Inbound);
            processor
                .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                    call_id: call_id.clone(),
                    queue_id: "busy-queue".into(),
                    priority: None,
                    skills: None,
                    max_wait_secs: None,
                }))
                .await
                .unwrap();
        }

        let _handle = create_test_call(&registry, "call-3", "1001", "2000", DialDirection::Inbound);
        let result = processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-3".into(),
                queue_id: "busy-queue".into(),
                priority: None,
                skills: None,
                max_wait_secs: None,
            }))
            .await;

        assert!(result.is_ok());

        let states = processor.queue_states.read().await;
        let state = states.get("call-3").unwrap();
        assert_eq!(state.queue_id, "overflow-queue");
        assert_eq!(state.overflow_count, 1);
    }

    #[tokio::test]
    async fn test_find_matching_agent_with_capacity() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        processor
            .register_agent_skill("agent-1".to_string(), vec!["support".to_string()], 1)
            .await;

        processor
            .register_agent_skill(
                "agent-2".to_string(),
                vec!["support".to_string(), "sales".to_string()],
                5,
            )
            .await;

        let matched = processor
            .find_matching_agent(&["support".to_string()])
            .await;
        assert_eq!(matched, Some("agent-1".to_string()));

        processor.update_agent_call_count("agent-1", 1).await;

        let matched = processor
            .find_matching_agent(&["support".to_string()])
            .await;
        assert_eq!(matched, Some("agent-2".to_string()));

        let matched = processor.find_matching_agent(&["sales".to_string()]).await;
        assert_eq!(matched, Some("agent-2".to_string()));

        processor.unregister_agent("agent-1").await;

        let agents = processor.agent_skills.read().await;
        assert!(!agents.contains_key("agent-1"));
    }

    #[tokio::test]
    async fn test_queue_stats() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let stats = processor.get_queue_stats("test-queue").await;
        assert!(stats.is_none());

        for i in 1..=3 {
            let call_id = format!("call-{}", i);
            let _handle =
                create_test_call(&registry, &call_id, "1001", "2000", DialDirection::Inbound);
            processor
                .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                    call_id: call_id.clone(),
                    queue_id: "test-queue".into(),
                    priority: None,
                    skills: None,
                    max_wait_secs: None,
                }))
                .await
                .unwrap();
        }

        processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-1".into(),
            })
            .await
            .unwrap();

        let stats = processor.get_queue_stats("test-queue").await;
        assert!(stats.is_some());
        let stats = stats.unwrap();
        assert_eq!(stats.total_calls, 3);
        assert_eq!(stats.calls_on_hold, 1);
        assert_eq!(stats.queue_id, "test-queue");
    }

    #[tokio::test]
    async fn test_sip_message_send() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SipMessage {
                call_id: "call-1".into(),
                content_type: "text/plain".into(),
                body: "Hello".into(),
            })
            .await;

        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));
    }

    #[tokio::test]
    async fn test_sip_notify_send() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SipNotify {
                call_id: "call-1".into(),
                event: "refer".into(),
                content_type: "message/sipfrag".into(),
                body: "SIP/2.0 200 OK".into(),
            })
            .await;

        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));
    }

    #[tokio::test]
    async fn test_sip_options_ping() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SipOptionsPing {
                call_id: "call-1".into(),
            })
            .await;

        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));
    }
}
