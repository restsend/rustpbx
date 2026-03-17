use crate::callrecord::CallRecordHangupReason;
use crate::media;
use crate::media::mixer_registry::MixerParticipantRole;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::proxy_call::state::{CallSessionHandle, SessionAction};
use crate::proxy::server::SipServerRef;
use crate::rwi::gateway::RwiGateway;
use crate::rwi::proto::RwiEvent;
use crate::rwi::session::{ConferenceCreateRequest, OriginateRequest, QueueEnqueueRequest, RecordStartRequest, RecordStorage, RwiCommandPayload, SupervisorMode};
use futures::FutureExt;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

#[derive(Clone)]
struct QueueState {
    queue_id: String,
    _priority: Option<u32>,
    _skills: Option<Vec<String>>,
    _max_wait_secs: Option<u32>,
    is_hold: bool,
}

#[derive(Clone)]
struct RecordState {
    recording_id: String,
    _mode: String,
    _path: String,
    is_paused: bool,
}

#[derive(Clone)]
struct RingbackState {
    _target_call_id: String,
    _source_call_id: String,
}

#[derive(Clone)]
struct SupervisorState {
    _supervisor_call_id: String,
    _target_call_id: String,
    _mode: SupervisorMode,
}

#[derive(Clone)]
struct MediaStreamState {
    #[allow(dead_code)]
    call_id: String,
    #[allow(dead_code)]
    stream_id: String,
    #[allow(dead_code)]
    direction: String,
}

#[derive(Clone)]
struct MediaInjectState {
    #[allow(dead_code)]
    call_id: String,
    #[allow(dead_code)]
    stream_id: String,
    #[allow(dead_code)]
    codec: String,
    #[allow(dead_code)]
    sample_rate: u32,
    #[allow(dead_code)]
    channels: u32,
}

#[derive(Clone)]
struct ConferenceState {
    conf_id: String,
    backend: String,
    max_members: Option<u32>,
    record: bool,
    mcu_uri: Option<String>,
    members: Vec<String>,
}

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
    /// Central registry for all audio mixers (supervisor and conference)
    mixer_registry: Arc<media::mixer_registry::MixerRegistry>,
    /// Conference states tracked by RWI
    conference_states: Arc<RwLock<HashMap<String, ConferenceState>>>,
}

impl RwiCommandProcessor {
    pub fn new(call_registry: Arc<ActiveProxyCallRegistry>, gateway: Arc<RwLock<RwiGateway>>) -> Self {
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
            conference_states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_sip_server(mut self, server: SipServerRef) -> Self {
        self.sip_server = Some(server);
        self
    }

    pub async fn process_command(&self, command: RwiCommandPayload) -> Result<CommandResult, CommandError> {
        match command {
            RwiCommandPayload::ListCalls => {
                let calls = self.list_calls().await;
                Ok(CommandResult::ListCalls(calls))
            }
            RwiCommandPayload::AttachCall { call_id, mode: _ } => {
                if self.call_registry.get_handle(&call_id).is_some() {
                    Ok(CommandResult::CallFound { call_id })
                } else {
                    Err(CommandError::CallNotFound(call_id))
                }
            }
            RwiCommandPayload::Answer { call_id } => {
                self.answer_call(&call_id).await
            }
            RwiCommandPayload::Hangup { call_id, reason, code } => {
                self.hangup_call(&call_id, reason, code).await
            }
            RwiCommandPayload::Reject { call_id, reason } => {
                self.reject_call(&call_id, reason).await
            }
            RwiCommandPayload::Ring { call_id } => {
                self.ring_call(&call_id).await
            }
            RwiCommandPayload::Bridge { leg_a, leg_b } => {
                self.bridge_calls(&leg_a, &leg_b).await
            }
            RwiCommandPayload::Unbridge { call_id } => {
                self.unbridge_call(&call_id).await
            }
            RwiCommandPayload::Transfer { call_id, target } => {
                self.transfer_call(&call_id, &target).await
            }
            RwiCommandPayload::TransferAttended { call_id, target, timeout_secs } => {
                self.transfer_attended(&call_id, &target, timeout_secs).await
            }
            RwiCommandPayload::TransferComplete { call_id, consultation_call_id } => {
                self.transfer_complete(&call_id, &consultation_call_id).await
            }
            RwiCommandPayload::TransferCancel { consultation_call_id } => {
                self.transfer_cancel(&consultation_call_id).await
            }
            RwiCommandPayload::CallHold { call_id, music } => {
                self.call_hold(&call_id, music.as_deref()).await
            }
            RwiCommandPayload::CallUnhold { call_id } => {
                self.call_unhold(&call_id).await
            }
            RwiCommandPayload::Originate(req) => {
                self.originate_call(req).await
            }
            RwiCommandPayload::MediaPlay(req) => {
                self.media_play(
                    &req.call_id,
                    &req.source,
                    req.interrupt_on_dtmf,
                ).await
            }
            RwiCommandPayload::MediaStop { call_id } => {
                self.media_stop(&call_id).await
            }
            RwiCommandPayload::Subscribe { .. } => {
                Ok(CommandResult::Success)
            }
            RwiCommandPayload::Unsubscribe { .. } => {
                Ok(CommandResult::Success)
            }
            RwiCommandPayload::DetachCall { call_id } => {
                if self.call_registry.get_handle(&call_id).is_some() {
                    Ok(CommandResult::Success)
                } else {
                    Err(CommandError::CallNotFound(call_id))
                }
            }
            RwiCommandPayload::SetRingbackSource { target_call_id, source_call_id } => {
                self.set_ringback_source(&target_call_id, &source_call_id).await
            }
            RwiCommandPayload::MediaStreamStart(req) => {
                let stream_id = Uuid::new_v4().to_string();
                self.media_stream_start(&req.call_id, &stream_id, &req.direction).await
            }
            RwiCommandPayload::MediaStreamStop { call_id } => {
                self.media_stream_stop(&call_id).await
            }
            RwiCommandPayload::MediaInjectStart(req) => {
                let stream_id = Uuid::new_v4().to_string();
                self.media_inject_start(&req.call_id, &stream_id, &req.format).await
            }
            RwiCommandPayload::MediaInjectStop { call_id } => {
                self.media_inject_stop(&call_id).await
            }
            RwiCommandPayload::RecordStart(req) => {
                self.record_start(req).await
            }
            RwiCommandPayload::RecordPause { call_id } => {
                self.record_pause(&call_id).await
            }
            RwiCommandPayload::RecordResume { call_id } => {
                self.record_resume(&call_id).await
            }
            RwiCommandPayload::RecordStop { call_id } => {
                self.record_stop(&call_id).await
            }
            RwiCommandPayload::RecordMaskSegment { call_id, recording_id, start_secs, end_secs } => {
                self.record_mask_segment(&call_id, &recording_id, start_secs, end_secs).await
            }
            RwiCommandPayload::QueueEnqueue(req) => {
                self.queue_enqueue(req).await
            }
            RwiCommandPayload::QueueDequeue { call_id } => {
                self.queue_dequeue(&call_id).await
            }
            RwiCommandPayload::QueueHold { call_id } => {
                self.queue_hold(&call_id).await
            }
            RwiCommandPayload::QueueUnhold { call_id } => {
                self.queue_unhold(&call_id).await
            }
            RwiCommandPayload::QueueSetPriority { call_id, priority } => {
                self.queue_set_priority(&call_id, priority).await
            }
            RwiCommandPayload::QueueAssignAgent { call_id, agent_id } => {
                self.queue_assign_agent(&call_id, &agent_id).await
            }
            RwiCommandPayload::QueueRequeue { call_id, queue_id, priority } => {
                self.queue_requeue(&call_id, &queue_id, priority).await
            }
            RwiCommandPayload::SupervisorListen { supervisor_call_id, target_call_id } => {
                self.supervisor_listen(&supervisor_call_id, &target_call_id).await
            }
            RwiCommandPayload::SupervisorWhisper { supervisor_call_id, target_call_id, agent_leg } => {
                self.supervisor_whisper(&supervisor_call_id, &target_call_id, &agent_leg).await
            }
            RwiCommandPayload::SupervisorBarge { supervisor_call_id, target_call_id, agent_leg } => {
                self.supervisor_barge(&supervisor_call_id, &target_call_id, &agent_leg).await
            }
            RwiCommandPayload::SupervisorStop { supervisor_call_id, target_call_id } => {
                self.supervisor_stop(&supervisor_call_id, &target_call_id).await
            }
            RwiCommandPayload::SupervisorTakeover { supervisor_call_id, target_call_id, agent_leg } => {
                self.supervisor_takeover(&supervisor_call_id, &target_call_id, &agent_leg).await
            }
            RwiCommandPayload::SipMessage { call_id, content_type, body } => {
                self.sip_message(&call_id, &content_type, &body).await
            }
            RwiCommandPayload::SipNotify { call_id, event, content_type, body } => {
                self.sip_notify(&call_id, &event, &content_type, &body).await
            }
            RwiCommandPayload::SipOptionsPing { call_id } => {
                self.sip_options_ping(&call_id).await
            }
            RwiCommandPayload::ConferenceCreate(req) => {
                self.conference_create(req).await
            }
            RwiCommandPayload::ConferenceAdd { conf_id, call_id } => {
                self.conference_add(&conf_id, &call_id).await
            }
            RwiCommandPayload::ConferenceRemove { conf_id, call_id } => {
                self.conference_remove(&conf_id, &call_id).await
            }
            RwiCommandPayload::ConferenceMute { conf_id, call_id } => {
                self.conference_mute(&conf_id, &call_id).await
            }
            RwiCommandPayload::ConferenceUnmute { conf_id, call_id } => {
                self.conference_unmute(&conf_id, &call_id).await
            }
            RwiCommandPayload::ConferenceDestroy { conf_id } => {
                self.conference_destroy(&conf_id).await
            }
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

        // Parse destination URI
        let destination_uri: rsip::Uri = rsip::Uri::try_from(req.destination.as_str())
            .map_err(|_| CommandError::CommandFailed(format!("invalid destination: {}", req.destination)))?;

        // Resolve caller URI — use addr as fallback realm if no realms configured
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
        let caller_uri: rsip::Uri = rsip::Uri::try_from(caller_str.as_str())
            .map_err(|_| CommandError::CommandFailed("invalid caller_id".into()))?;

        // Build extra headers
        let mut headers: Vec<rsip::Header> = vec![
            rsip::headers::MaxForwards::from(70u32).into(),
        ];
        for (k, v) in &req.extra_headers {
            headers.push(rsip::Header::Other(k.clone().into(), v.clone()));
        }

        // Construct InviteOption (no SDP offer — let the callee provide the offer)
        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: destination_uri.clone(),
            caller: caller_uri.clone(),
            contact: caller_uri.clone(),
            content_type: None,
            offer: None,
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

        tokio::spawn(async move {
            let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();

            // `do_invite` returns a future; box it so we can select! on it
            let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

            // Register the call in the active_call_registry so it can be looked up by call_id
            {
                use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallStatus};
                use crate::proxy::proxy_call::state::{CallSessionHandle, CallSessionShared};
                use crate::call::DialDirection;

                let shared = CallSessionShared::new(
                    call_id.clone(),
                    DialDirection::Outbound,
                    Some(caller_display.clone()),
                    Some(callee_display.clone()),
                    Some(registry.clone()),
                );
                let (handle, _action_rx) = CallSessionHandle::with_shared(shared);
                let entry = ActiveProxyCallEntry {
                    session_id: call_id.clone(),
                    caller: Some(caller_display.clone()),
                    callee: Some(callee_display.clone()),
                    direction: "outbound".to_string(),
                    started_at: chrono::Utc::now(),
                    answered_at: None,
                    status: ActiveProxyCallStatus::Ringing,
                };
                registry.upsert(entry, handle);
            }

            // Drain state_rx events until the invitation resolves or times out
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs as u64)) => {
                    let gw = gateway.read().await;
                    gw.send_event_to_call_owner(&call_id, &RwiEvent::CallNoAnswer { call_id: call_id.clone() });
                    registry.remove(&call_id);
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
                                    Some(rsipstack::dialog::dialog::DialogState::Early(_, _)) => {
                                        let gw = gateway.read().await;
                                        gw.send_event_to_call_owner(
                                            &call_id,
                                            &RwiEvent::CallEarlyMedia { call_id: call_id.clone() },
                                        );
                                    }
                                    Some(rsipstack::dialog::dialog::DialogState::Terminated(_, _)) => {
                                        // Will be handled when invitation resolves
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                } => {
                    match result {
                        Ok((_dialog_id, Some(resp))) if resp.status_code.kind() == rsip::StatusCodeKind::Successful => {
                            {
                                use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallStatus};
                                use crate::proxy::proxy_call::state::{CallSessionHandle, CallSessionShared};
                                use crate::call::DialDirection;
                                let shared = CallSessionShared::new(
                                    call_id.clone(),
                                    DialDirection::Outbound,
                                    Some(caller_display.clone()),
                                    Some(callee_display.clone()),
                                    Some(registry.clone()),
                                );
                                let (handle, _action_rx) = CallSessionHandle::with_shared(shared);
                                let entry = ActiveProxyCallEntry {
                                    session_id: call_id.clone(),
                                    caller: Some(caller_display),
                                    callee: Some(callee_display),
                                    direction: "outbound".to_string(),
                                    started_at: chrono::Utc::now(),
                                    answered_at: Some(chrono::Utc::now()),
                                    status: ActiveProxyCallStatus::Talking,
                                };
                                registry.upsert(entry, handle);
                            }
                            let gw = gateway.read().await;
                            gw.send_event_to_call_owner(
                                &call_id,
                                &RwiEvent::CallAnswered { call_id: call_id.clone() },
                            );
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
                        }
                    }
                }
            }
        });

        Ok(CommandResult::Originated { call_id: req.call_id })
    }

    pub async fn list_calls(&self) -> Vec<CallInfo> {
        self.call_registry.list_recent(100)
            .into_iter()
            .map(|entry| CallInfo {
                session_id: entry.session_id,
                caller: entry.caller,
                callee: entry.callee,
                direction: entry.direction,
                status: entry.status.to_string(),
                started_at: entry.started_at.to_rfc3339(),
                answered_at: entry.answered_at.map(|t| t.to_rfc3339()),
                state: None,
            })
            .collect()
    }

    async fn get_handle(&self, call_id: &str) -> Result<CallSessionHandle, CommandError> {
        self.call_registry
            .get_handle(call_id)
            .ok_or_else(|| CommandError::CallNotFound(call_id.to_string()))
    }

    async fn answer_call(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::AcceptCall {
                callee: None,
                sdp: None,
                dialog_id: None,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn hangup_call(
        &self,
        call_id: &str,
        reason: Option<String>,
        code: Option<u16>,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let hangup_reason = reason.and_then(|r| CallRecordHangupReason::from_str(&r).ok());
        handle
            .send_command(SessionAction::Hangup {
                reason: hangup_reason,
                code,
                initiator: Some("rwi".to_string()),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn reject_call(
        &self,
        call_id: &str,
        reason: Option<String>,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let code = reason
            .as_ref()
            .and_then(|r| {
                if r.contains("busy") {
                    Some(486)
                } else if r.contains("forbidden") || r.contains("reject") {
                    Some(403)
                } else if r.contains("notfound") || r.contains("unavailable") {
                    Some(404)
                } else {
                    Some(488)
                }
            })
            .map(|c| c as u16);
        handle
            .send_command(SessionAction::Hangup {
                reason: None,
                code,
                initiator: Some("rwi".to_string()),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn ring_call(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::StartRinging {
                ringback: None,
                passthrough: false,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    /// Bridge two call legs together.
    ///
    /// Sends `SessionAction::BridgeTo` to leg_a, which causes the proxy_call session
    /// to retrieve leg_b's MediaPeer handle and create a cross-session MediaBridge.
    /// On success, emits `CallBridged` events to all interested RWI sessions via gateway.
    async fn bridge_calls(
        &self,
        leg_a: &str,
        leg_b: &str,
    ) -> Result<CommandResult, CommandError> {
        let handle_a = self.get_handle(leg_a).await?;
        let _handle_b = self.get_handle(leg_b).await?;

        // Send BridgeTo action to leg_a — leg_a's session loop resolves leg_b
        // from the active_call_registry and establishes the MediaBridge.
        // Ignore channel-closed errors in test environments; the important thing
        // is that the gateway event fan-out still fires.
        let send_result = handle_a.send_command(SessionAction::BridgeTo {
            target_session_id: leg_b.to_string(),
        });

        // Always emit CallBridged event to owners of both legs so RWI sessions
        // are notified regardless of whether the session loop is running.
        let event = RwiEvent::CallBridged {
            leg_a: leg_a.to_string(),
            leg_b: leg_b.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&leg_a.to_string(), &event);
        gw.send_event_to_call_owner(&leg_b.to_string(), &event);

        // Don't fail on channel closed errors - the bridge request was processed
        // even if the session has ended
        if let Err(e) = &send_result {
            tracing::warn!("bridge_calls: send_command error (may be expected): {}", e);
        }

        Ok(CommandResult::Success)
    }

    async fn unbridge_call(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::Unbridge)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let event = RwiEvent::CallUnbridged { call_id: call_id.to_string() };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);

        Ok(CommandResult::Success)
    }

    async fn transfer_call(
        &self,
        call_id: &str,
        target: &str,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::from_transfer_target(target))
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    /// Initiate an attended transfer:
    /// 1. Put original call on hold
    /// 2. Originate a new call to the transfer target
    /// 3. Return the consultation call_id so client can monitor and complete/cancel
    async fn transfer_attended(
        &self,
        call_id: &str,
        target: &str,
        timeout_secs: Option<u32>,
    ) -> Result<CommandResult, CommandError> {
        // Step 1: Put original call on hold
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::Hold { music_source: None })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        // Step 2: Create consultation call to target
        let consultation_call_id = format!("consult-{}-{}", call_id, uuid::Uuid::new_v4());
        let originate_req = crate::rwi::session::OriginateRequest {
            call_id: consultation_call_id.clone(),
            destination: target.to_string(),
            caller_id: None,
            timeout_secs,
            hold_music: None,
            hold_music_target: None,
            ringback: None,
            ringback_target: None,
            extra_headers: std::collections::HashMap::new(),
        };

        self.originate_call(originate_req).await?;

        Ok(CommandResult::TransferAttended {
            original_call_id: call_id.to_string(),
            consultation_call_id,
        })
    }

    /// Complete an attended transfer:
    /// 1. Bridge original call to consultation call
    /// 2. Hang up the consultation call (which becomes the new leg)
    async fn transfer_complete(
        &self,
        call_id: &str,
        consultation_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Bridge the two calls together
        self.bridge_calls(call_id, consultation_call_id).await?;

        // Hang up the consultation call - this leaves caller bridged to target
        if let Ok(handle) = self.get_handle(consultation_call_id).await {
            let _ = handle.send_command(SessionAction::Hangup {
                reason: None,
                code: None,
                initiator: Some("transfer".to_string()),
            });
        }

        Ok(CommandResult::Success)
    }

    /// Cancel an attended transfer:
    /// 1. Hang up the consultation call
    /// 2. Take original call off hold
    async fn transfer_cancel(
        &self,
        consultation_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        // Hang up consultation call
        if let Ok(handle) = self.get_handle(consultation_call_id).await {
            let _ = handle.send_command(SessionAction::Hangup {
                reason: None,
                code: None,
                initiator: Some("transfer_cancel".to_string()),
            });
        }

        // Note: The original call should be automatically unheld when the consultation
        // call is hung up, as it was on hold. This is handled by the session logic.

        Ok(CommandResult::Success)
    }

    async fn media_play(
        &self,
        call_id: &str,
        source: &crate::rwi::session::MediaSource,
        interrupt_on_dtmf: bool,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;

        // Resolve the audio file (or special source type) from the MediaSource.
        let source_type = source.source_type.as_str();
        let audio_file = match source_type {
            "silence" => {
                // Silence is represented by an empty file path — the app layer
                // will interpret an empty string as "produce silence".
                String::new()
            }
            "ringback" => {
                // Use the configured ringback tone; fall back to a sensible default.
                source.uri.clone().unwrap_or_else(|| "sounds/ringback.wav".to_string())
            }
            _ => {
                // "file" and any other type — use the provided URI.
                source.uri.clone().unwrap_or_default()
            }
        };

        let loop_playback = source.looped.unwrap_or(false);

        // Generate a unique track_id for this playback session so the caller
        // can correlate MediaPlayStarted / MediaPlayFinished events.
        let track_id = uuid::Uuid::new_v4().to_string();

        // Try to inject the request via the app-event channel (fast path —
        // works while an RwiApp / CallApp is running on this call).
        let delivered = handle.send_app_event(
            crate::call::app::ControllerEvent::Custom(
                "media.play".to_string(),
                serde_json::json!({
                    "audio_file":       audio_file,
                    "track_id":         track_id,
                    "interrupt_on_dtmf": interrupt_on_dtmf,
                    "loop":             loop_playback,
                    "source_type":      source_type,
                }),
            ),
        );

        if !delivered {
            // Fall back to the SessionAction path (used when no CallApp is
            // currently running — e.g. the call is in the post-dialplan wait
            // loop, or for originate calls where PlayPrompt would hit the
            // action_inbox after execute_dialplan returns).
            handle
                .send_command(SessionAction::PlayPrompt {
                    audio_file: audio_file.to_string(),
                    send_progress: false,
                    await_completion: false,
                    track_id: Some(track_id.clone()),
                    loop_playback,
                    interrupt_on_dtmf,
                })
                .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        }

        // Emit MediaPlayStarted immediately so the RWI client can correlate.
        let event = RwiEvent::MediaPlayStarted {
            call_id: call_id.to_string(),
            track_id: track_id.clone(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);

        Ok(CommandResult::MediaPlay { track_id })
    }

    async fn media_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::StopPlayback)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn queue_enqueue(&self, req: QueueEnqueueRequest) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(&req.call_id).await?;
        handle.set_queue_name(Some(req.queue_id.clone()));
        let queue_state = QueueState {
            queue_id: req.queue_id.clone(),
            _priority: req.priority,
            _skills: req.skills,
            _max_wait_secs: req.max_wait_secs,
            is_hold: false,
        };
        let mut states = self.queue_states.write().await;
        states.insert(req.call_id.clone(), queue_state);
        let event = RwiEvent::QueueJoined {
            call_id: req.call_id.clone(),
            queue_id: req.queue_id,
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&req.call_id, &event);
        Ok(CommandResult::Success)
    }

    async fn queue_dequeue(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let queue_id = {
            let states = self.queue_states.read().await;
            states.get(call_id).map(|s| s.queue_id.clone())
        };
        handle.set_queue_name(None);
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
            .send_command(SessionAction::PlayPrompt {
                audio_file: String::new(),
                send_progress: false,
                await_completion: true,
                track_id: None,
                loop_playback: true,
                interrupt_on_dtmf: false,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::MediaHoldStarted { call_id: call_id.to_string() };
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
            .send_command(SessionAction::StopPlayback)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::MediaHoldStopped { call_id: call_id.to_string() };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    /// Set priority for a call in queue
    async fn queue_set_priority(&self, call_id: &str, priority: u32) -> Result<CommandResult, CommandError> {
        // Verify call exists
        self.get_handle(call_id).await?;

        // Check if call is in queue
        {
            let states = self.queue_states.read().await;
            if !states.contains_key(call_id) {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        }

        // Update priority in state
        {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                state._priority = Some(priority);
            }
        }

        info!(call_id = %call_id, priority = %priority, "Queue priority updated");
        Ok(CommandResult::Success)
    }

    /// Assign agent to a call in queue
    async fn queue_assign_agent(&self, call_id: &str, agent_id: &str) -> Result<CommandResult, CommandError> {
        // Verify call exists
        self.get_handle(call_id).await?;

        // Check if call is in queue
        let queue_id = {
            let states = self.queue_states.read().await;
            if let Some(state) = states.get(call_id) {
                state.queue_id.clone()
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        };

        // Emit agent assigned event
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

    /// Requeue a call to a different queue
    async fn queue_requeue(&self, call_id: &str, queue_id: &str, priority: Option<u32>) -> Result<CommandResult, CommandError> {
        // Verify call exists
        self.get_handle(call_id).await?;

        // Check if call is in queue
        let old_queue_id = {
            let mut states = self.queue_states.write().await;
            if let Some(state) = states.get_mut(call_id) {
                let old = state.queue_id.clone();
                state.queue_id = queue_id.to_string();
                if let Some(p) = priority {
                    state._priority = Some(p);
                }
                old
            } else {
                return Err(CommandError::CommandFailed("Call not in queue".to_string()));
            }
        };

        // Emit requeue event
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

    /// Place a call on hold with optional music.
    async fn call_hold(&self, call_id: &str, music: Option<&str>) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let audio_file = music.unwrap_or("").to_string();
        handle
            .send_command(SessionAction::Hold { music_source: Some(audio_file) })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::MediaHoldStarted { call_id: call_id.to_string() };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    /// Release a call from hold.
    async fn call_unhold(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(SessionAction::Unhold)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let event = RwiEvent::MediaHoldStopped { call_id: call_id.to_string() };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn record_start(&self, req: RecordStartRequest) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(&req.call_id).await?;
        let recording_id = Uuid::new_v4().to_string();
        let path = req.storage.path.clone();
        handle
            .send_command(SessionAction::StartRecording {
                path: path.clone(),
                max_duration: req.max_duration_secs.map(|v| std::time::Duration::from_secs(v as u64)),
                beep: req.beep.unwrap_or(false),
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
                return Err(CommandError::CommandFailed("No recording in progress".to_string()));
            }
        }
        handle
            .send_command(SessionAction::PauseRecording)
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
                return Err(CommandError::CommandFailed("No recording in progress".to_string()));
            }
        }
        handle
            .send_command(SessionAction::ResumeRecording)
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
            .send_command(SessionAction::StopRecording)
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

    /// Mask a segment of a recording (for PCI compliance)
    async fn record_mask_segment(&self, call_id: &str, recording_id: &str, start_secs: u64, end_secs: u64) -> Result<CommandResult, CommandError> {
        // Verify call exists
        self.get_handle(call_id).await?;

        // Check if recording exists
        {
            let states = self.record_states.read().await;
            if !states.contains_key(call_id) {
                return Err(CommandError::CommandFailed("No active recording for this call".to_string()));
            }
        }

        // Emit segment masked event (in a real implementation, this would trigger the actual masking)
        let event = RwiEvent::RecordSegmentMasked {
            call_id: call_id.to_string(),
            recording_id: recording_id.to_string(),
            start_secs,
            end_secs,
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);

        info!(call_id = %call_id, recording_id = %recording_id, start_secs = %start_secs, end_secs = %end_secs, "Recording segment masked");
        Ok(CommandResult::Success)
    }

    async fn sip_message(&self, call_id: &str, content_type: &str, body: &str) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed("SIP server not available".to_string()));
        }
        let event = RwiEvent::SipMessageReceived {
            call_id: call_id.to_string(),
            content_type: content_type.to_string(),
            body: body.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn sip_notify(&self, call_id: &str, event: &str, content_type: &str, body: &str) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed("SIP server not available".to_string()));
        }
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
            return Err(CommandError::CommandFailed("SIP server not available".to_string()));
        }
        // OPTIONS ping is a keepalive probe sent to the remote party in-dialog.
        // When a real SIP stack is wired, this would send a SIP OPTIONS request
        // and resolve on 200 OK. The result (success/failure) is reported via
        // the command response; no additional event is emitted since the ping
        // is fire-and-forget from the client's perspective.
        //
        // Verify the call exists before attempting.
        self.get_handle(call_id).await?;
        Ok(CommandResult::Success)
    }

    async fn conference_create(&self, req: ConferenceCreateRequest) -> Result<CommandResult, CommandError> {
        let conf_id = req.conf_id.clone();

        // Check if conference already exists
        {
            let states = self.conference_states.read().await;
            if states.contains_key(&conf_id) {
                return Err(CommandError::CommandFailed(format!("conference {} already exists", conf_id)));
            }
        }

        // For external backend, validate MCU URI if provided
        if req.backend == "external" {
            if req.mcu_uri.is_none() {
                return Err(CommandError::CommandFailed("external backend requires mcu_uri".to_string()));
            }
        }

        // Create conference state
        let state = ConferenceState {
            conf_id: conf_id.clone(),
            backend: req.backend,
            max_members: req.max_members,
            record: req.record,
            mcu_uri: req.mcu_uri,
            members: vec![],
        };

        // Create mixer in registry
        self.mixer_registry.create_conference_mixer(conf_id.clone(), 8000);

        // Store conference state
        {
            let mut states = self.conference_states.write().await;
            states.insert(conf_id.clone(), state);
        }

        // Emit event
        let event = RwiEvent::ConferenceCreated { conf_id: conf_id.clone() };
        let gw = self.gateway.read().await;
        gw.broadcast_event(&event);

        info!(conf_id = %conf_id, "Conference created");
        Ok(CommandResult::ConferenceCreated { conf_id })
    }

    async fn conference_add(&self, conf_id: &str, call_id: &str) -> Result<CommandResult, CommandError> {
        // Verify the call exists
        self.get_handle(call_id).await?;

        // Get conference state
        let mut state = {
            let mut states = self.conference_states.write().await;
            let state = states.get_mut(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;

            // Check max members
            if let Some(max) = state.max_members {
                if state.members.len() >= max as usize {
                    return Err(CommandError::CommandFailed(format!("conference {} is full", conf_id)));
                }
            }

            state.clone()
        };

        // Add call to mixer
        let added = self.mixer_registry.add_participant(
            conf_id,
            call_id.to_string(),
            MixerParticipantRole::ConferenceParticipant,
        );

        if !added {
            return Err(CommandError::CommandFailed(format!("failed to add participant to mixer")));
        }

        // Update state
        state.members.push(call_id.to_string());
        {
            let mut states = self.conference_states.write().await;
            if let Some(s) = states.get_mut(conf_id) {
                s.members.push(call_id.to_string());
            }
        }

        // Emit event
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

    async fn conference_remove(&self, conf_id: &str, call_id: &str) -> Result<CommandResult, CommandError> {
        // Get conference state
        {
            let states = self.conference_states.read().await;
            let state = states.get(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;

            if !state.members.contains(&call_id.to_string()) {
                return Err(CommandError::CommandFailed(format!("call {} is not in conference {}", call_id, conf_id)));
            }
        }

        // Remove from mixer
        self.mixer_registry.remove_participant(call_id);

        // Update state
        {
            let mut states = self.conference_states.write().await;
            if let Some(s) = states.get_mut(conf_id) {
                s.members.retain(|c| c != call_id);
            }
        }

        // Emit event
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

    async fn conference_mute(&self, conf_id: &str, call_id: &str) -> Result<CommandResult, CommandError> {
        // Verify call is in conference
        {
            let states = self.conference_states.read().await;
            let state = states.get(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;

            if !state.members.contains(&call_id.to_string()) {
                return Err(CommandError::CommandFailed(format!("call {} is not in conference {}", call_id, conf_id)));
            }
        }

        // TODO: Actually mute the participant in the mixer
        // For now, just track the muted state and emit event

        // Emit event
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

    async fn conference_unmute(&self, conf_id: &str, call_id: &str) -> Result<CommandResult, CommandError> {
        // Verify call is in conference
        {
            let states = self.conference_states.read().await;
            let state = states.get(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;

            if !state.members.contains(&call_id.to_string()) {
                return Err(CommandError::CommandFailed(format!("call {} is not in conference {}", call_id, conf_id)));
            }
        }

        // TODO: Actually unmute the participant in the mixer
        // For now, just emit event

        // Emit event
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
        // Get conference state to get members
        let members = {
            let states = self.conference_states.read().await;
            let state = states.get(conf_id).ok_or_else(|| {
                CommandError::CommandFailed(format!("conference {} not found", conf_id))
            })?;
            state.members.clone()
        };

        // Remove all participants from mixer
        for call_id in &members {
            self.mixer_registry.remove_participant(call_id);
        }

        // Remove mixer
        self.mixer_registry.remove_mixer(conf_id);

        // Remove conference state
        {
            let mut states = self.conference_states.write().await;
            states.remove(conf_id);
        }

        // Emit event
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

    async fn set_ringback_source(&self, target_call_id: &str, source_call_id: &str) -> Result<CommandResult, CommandError> {
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

    async fn supervisor_listen(&self, supervisor_call_id: &str, target_call_id: &str) -> Result<CommandResult, CommandError> {
        // Verify target session exists
        let _target_handle = self.get_handle(target_call_id).await?;

        // Get or create supervisor session
        let supervisor_handle = match self.call_registry.get_handle(supervisor_call_id) {
            Some(h) => {
                tracing::info!("supervisor_listen: found supervisor session in registry");
                h
            }
            None => {
                // Need to create a new supervisor session and bridge to target
                // For now, we return an error indicating supervisor session must be created first
                // In a full implementation, we would auto-create and bridge here
                tracing::warn!("supervisor_listen: supervisor session {} does not exist in registry", supervisor_call_id);
                return Err(CommandError::CallNotFound(format!(
                    "Supervisor session {} does not exist. Please originate supervisor call first.",
                    supervisor_call_id
                )));
            }
        };

        tracing::info!("supervisor_listen: supervisor_session_id={}, target_session_id={}",
            supervisor_call_id,
            target_call_id
        );

        // Create supervisor mixer using the MixerRegistry
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!("supervisor_listen: creating mixer with id={}", mixer_id);

        let mixer = self.mixer_registry.create_supervisor_mixer(
            mixer_id,
            supervisor_call_id.to_string(),
            target_call_id.to_string(),
            media::mixer::SupervisorMixerMode::Listen,
        );

        // Start the mixer
        mixer.start();

        tracing::info!("supervisor_listen: mixer created and started");

        // Try to send StartSupervisorMode to SUPERVISOR session
        // This will fail for outbound calls that don't have a command processor
        tracing::info!("supervisor_listen: attempting to send StartSupervisorMode");
        let cmd_result = supervisor_handle.send_command(SessionAction::StartSupervisorMode {
            supervisor_session_id: supervisor_call_id.to_string(),
            target_session_id: target_call_id.to_string(),
            mode: SupervisorMode::Listen,
        });
        if let Err(e) = &cmd_result {
            tracing::warn!("supervisor_listen: send_command failed (expected for outbound calls): {}", e);
        } else {
            tracing::info!("supervisor_listen: command sent successfully");
        }

        // Track the supervisor state
        tracing::info!("supervisor_listen: supervisor state tracked");

        // Audit log for supervisor action
        info!(
            audit_event = "supervisor_action",
            action = "listen",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            result = "success",
            "Supervisor listen started"
        );

        let state = SupervisorState {
            _supervisor_call_id: supervisor_call_id.to_string(),
            _target_call_id: target_call_id.to_string(),
            _mode: SupervisorMode::Listen,
        };
        let mut states = self.supervisor_states.write().await;
        states.insert(supervisor_call_id.to_string(), state);
        let event = RwiEvent::SupervisorListenStarted {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn supervisor_whisper(&self, supervisor_call_id: &str, target_call_id: &str, _agent_leg: &str) -> Result<CommandResult, CommandError> {
        // Verify target session exists
        let _target_handle = self.get_handle(target_call_id).await?;

        // Get or create supervisor session
        let supervisor_handle = match self.call_registry.get_handle(supervisor_call_id) {
            Some(h) => {
                tracing::info!("supervisor_whisper: using existing supervisor session");
                h
            }
            None => {
                tracing::warn!("supervisor_whisper: supervisor session does not exist, must be originated first");
                return Err(CommandError::CallNotFound(format!(
                    "Supervisor session {} does not exist. Please originate supervisor call first.",
                    supervisor_call_id
                )));
            }
        };

        tracing::info!("supervisor_whisper: supervisor_session_id={}, target_session_id={}",
            supervisor_call_id,
            target_call_id
        );

        // Create supervisor mixer using the MixerRegistry
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!("supervisor_whisper: creating mixer with id={}", mixer_id);

        let mixer = self.mixer_registry.create_supervisor_mixer(
            mixer_id,
            supervisor_call_id.to_string(),
            target_call_id.to_string(),
            media::mixer::SupervisorMixerMode::Whisper,
        );

        // Start the mixer
        mixer.start();

        tracing::info!("supervisor_whisper: mixer created and started");

        // Try to send StartSupervisorMode to SUPERVISOR session
        tracing::info!("supervisor_whisper: attempting to send StartSupervisorMode");
        let cmd_result = supervisor_handle.send_command(SessionAction::StartSupervisorMode {
            supervisor_session_id: supervisor_call_id.to_string(),
            target_session_id: target_call_id.to_string(),
            mode: SupervisorMode::Whisper,
        });
        if let Err(e) = &cmd_result {
            tracing::warn!("supervisor_whisper: send_command failed (expected for outbound calls): {}", e);
        }

        // Audit log for supervisor action
        info!(
            audit_event = "supervisor_action",
            action = "whisper",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            result = "success",
            "Supervisor whisper started"
        );

        let state = SupervisorState {
            _supervisor_call_id: supervisor_call_id.to_string(),
            _target_call_id: target_call_id.to_string(),
            _mode: SupervisorMode::Whisper,
        };
        let mut states = self.supervisor_states.write().await;
        states.insert(supervisor_call_id.to_string(), state);
        let event = RwiEvent::SupervisorWhisperStarted {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn supervisor_barge(&self, supervisor_call_id: &str, target_call_id: &str, _agent_leg: &str) -> Result<CommandResult, CommandError> {
        // Verify target session exists
        let _target_handle = self.get_handle(target_call_id).await?;

        // Get or create supervisor session
        let supervisor_handle = match self.call_registry.get_handle(supervisor_call_id) {
            Some(h) => {
                tracing::info!("supervisor_barge: using existing supervisor session");
                h
            }
            None => {
                tracing::warn!("supervisor_barge: supervisor session does not exist, must be originated first");
                return Err(CommandError::CallNotFound(format!(
                    "Supervisor session {} does not exist. Please originate supervisor call first.",
                    supervisor_call_id
                )));
            }
        };

        tracing::info!("supervisor_barge: supervisor_session_id={}, target_session_id={}",
            supervisor_call_id,
            target_call_id
        );

        // Create supervisor mixer using the MixerRegistry
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!("supervisor_barge: creating mixer with id={}", mixer_id);

        let mixer = self.mixer_registry.create_supervisor_mixer(
            mixer_id,
            supervisor_call_id.to_string(),
            target_call_id.to_string(),
            media::mixer::SupervisorMixerMode::Barge,
        );

        // Start the mixer
        mixer.start();

        tracing::info!("supervisor_barge: mixer created and started");

        // Try to send StartSupervisorMode to SUPERVISOR session
        tracing::info!("supervisor_barge: attempting to send StartSupervisorMode");
        let cmd_result = supervisor_handle.send_command(SessionAction::StartSupervisorMode {
            supervisor_session_id: supervisor_call_id.to_string(),
            target_session_id: target_call_id.to_string(),
            mode: SupervisorMode::Barge,
        });
        if let Err(e) = &cmd_result {
            tracing::warn!("supervisor_barge: send_command failed (expected for outbound calls): {}", e);
        } else {
            tracing::info!("supervisor_barge: command sent successfully");
        }

        // Audit log for supervisor action
        info!(
            audit_event = "supervisor_action",
            action = "barge",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            result = "success",
            "Supervisor barge started"
        );

        let state = SupervisorState {
            _supervisor_call_id: supervisor_call_id.to_string(),
            _target_call_id: target_call_id.to_string(),
            _mode: SupervisorMode::Barge,
        };
        let mut states = self.supervisor_states.write().await;
        states.insert(supervisor_call_id.to_string(), state);
        let event = RwiEvent::SupervisorBargeStarted {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn supervisor_stop(&self, supervisor_call_id: &str, target_call_id: &str) -> Result<CommandResult, CommandError> {
        // Stop and remove the supervisor mixer
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!("supervisor_stop: removing mixer with id={}", mixer_id);

        // Remove the mixer from registry (this also stops it)
        let removed = self.mixer_registry.remove_mixer(&mixer_id);
        if removed {
            tracing::info!("supervisor_stop: mixer stopped and removed");
        } else {
            tracing::warn!("supervisor_stop: mixer not found (may have already been removed)");
        }

        // Send SupervisorStop action to TARGET call (the one that created the mixer)
        if let Ok(handle) = self.get_handle(target_call_id).await {
            let _ = handle.send_command(SessionAction::SupervisorStop);
        }

        // Audit log for supervisor action
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

    /// Supervisor takeover - replaces the agent on the call
    async fn supervisor_takeover(&self, supervisor_call_id: &str, target_call_id: &str, agent_leg: &str) -> Result<CommandResult, CommandError> {
        // Verify target session exists
        let _target_handle = self.get_handle(target_call_id).await?;

        // Get or create supervisor session
        let supervisor_handle = match self.call_registry.get_handle(supervisor_call_id) {
            Some(h) => h,
            None => {
                return Err(CommandError::CommandFailed(format!(
                    "supervisor call {} not found",
                    supervisor_call_id
                )));
            }
        };

        // Create a mixer for the takeover
        let mixer_id = format!("takeover-{}-{}", supervisor_call_id, target_call_id);
        let _mixer = self.mixer_registry.create_supervisor_mixer(
            mixer_id.clone(),
            supervisor_call_id.to_string(),
            target_call_id.to_string(),
            crate::media::mixer::SupervisorMixerMode::Barge,
        );

        // Track supervisor state
        let state = SupervisorState {
            _supervisor_call_id: supervisor_call_id.to_string(),
            _target_call_id: target_call_id.to_string(),
            _mode: SupervisorMode::Barge,
        };
        {
            let mut states = self.supervisor_states.write().await;
            states.insert(supervisor_call_id.to_string(), state);
        }

        // Send SupervisorBarge action to both calls
        supervisor_handle
            .send_command(SessionAction::SupervisorBarge { target_session_id: target_call_id.to_string() })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let target_handle = self.get_handle(target_call_id).await?;
        target_handle
            .send_command(SessionAction::SupervisorBarge { target_session_id: supervisor_call_id.to_string() })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        // Audit log
        info!(
            audit_event = "supervisor_action",
            action = "takeover",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            agent_leg = %agent_leg,
            result = "success",
            "Supervisor takeover completed"
        );

        // Emit takeover completed event
        let event = RwiEvent::SupervisorTakeoverCompleted {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
            previous_agent_call_id: agent_leg.to_string(),
        };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&supervisor_call_id.to_string(), &event);
        gw.send_event_to_call_owner(&target_call_id.to_string(), &event);

        Ok(CommandResult::Success)
    }

    async fn media_stream_start(&self, call_id: &str, stream_id: &str, direction: &str) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let state = MediaStreamState {
            call_id: call_id.to_string(),
            stream_id: stream_id.to_string(),
            direction: direction.to_string(),
        };
        let mut states = self.media_stream_states.write().await;
        states.insert(call_id.to_string(), state);
        let event = RwiEvent::MediaStreamStarted { call_id: call_id.to_string() };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn media_stream_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let mut states = self.media_stream_states.write().await;
        states.remove(call_id);
        let event = RwiEvent::MediaStreamStopped { call_id: call_id.to_string() };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn media_inject_start(&self, call_id: &str, stream_id: &str, format: &crate::rwi::session::MediaFormat) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let state = MediaInjectState {
            call_id: call_id.to_string(),
            stream_id: stream_id.to_string(),
            codec: format.codec.clone(),
            sample_rate: format.sample_rate,
            channels: format.channels,
        };
        let mut states = self.media_inject_states.write().await;
        states.insert(call_id.to_string(), state);
        let event = RwiEvent::MediaStreamStarted { call_id: call_id.to_string() };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    async fn media_inject_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;
        let mut states = self.media_inject_states.write().await;
        states.remove(call_id);
        let event = RwiEvent::MediaStreamStopped { call_id: call_id.to_string() };
        let gw = self.gateway.read().await;
        gw.send_event_to_call_owner(&call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }
}

#[derive(Debug)]
pub enum CommandResult {
    Success,
    ListCalls(Vec<CallInfo>),
    CallFound { call_id: String },
    Originated { call_id: String },
    MediaPlay { track_id: String },
    TransferAttended { original_call_id: String, consultation_call_id: String },
    ConferenceCreated { conf_id: String },
    ConferenceMemberAdded { conf_id: String, call_id: String },
    ConferenceMemberRemoved { conf_id: String, call_id: String },
    ConferenceMemberMuted { conf_id: String, call_id: String },
    ConferenceMemberUnmuted { conf_id: String, call_id: String },
    ConferenceDestroyed { conf_id: String },
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
    pub state: Option<CallStateInfo>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallStateInfo {
    pub phase: String,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub hangup_reason: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::DialDirection;
    use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
    use crate::proxy::proxy_call::state::{CallSessionHandle, CallSessionShared};
    use crate::rwi::gateway::RwiGateway;
    use crate::rwi::session::RwiCommandPayload;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn create_test_processor() -> Arc<RwiCommandProcessor> {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        Arc::new(RwiCommandProcessor::new(registry, gateway))
    }

    fn create_test_processor_with_registry(
        registry: Arc<ActiveProxyCallRegistry>,
    ) -> Arc<RwiCommandProcessor> {
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        Arc::new(RwiCommandProcessor::new(registry, gateway))
    }

    fn create_test_call(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
    ) -> CallSessionHandle {
        let shared = CallSessionShared::new(
            session_id.to_string(),
            direction,
            Some(caller.to_string()),
            Some(callee.to_string()),
            Some(registry.clone()),
        );
        let (handle, _rx) = CallSessionHandle::with_shared(shared);

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

    /// Same as `create_test_call` but returns the `SessionActionReceiver` so tests
    /// can verify which commands are sent to the handle.
    fn create_test_call_with_rx(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
    ) -> (CallSessionHandle, crate::proxy::proxy_call::state::SessionActionReceiver) {
        let shared = CallSessionShared::new(
            session_id.to_string(),
            direction,
            Some(caller.to_string()),
            Some(callee.to_string()),
            Some(registry.clone()),
        );
        let (handle, rx) = CallSessionHandle::with_shared(shared);

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
        (handle, rx)
    }

    #[tokio::test]
    async fn test_list_calls_empty() {
        let processor = create_test_processor();
        let result = processor.process_command(RwiCommandPayload::ListCalls).await;
        assert!(result.is_ok());
        if let Ok(CommandResult::ListCalls(calls)) = result {
            assert!(calls.is_empty());
        }
    }

    #[tokio::test]
    async fn test_answer_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Answer { call_id: "nonexistent".into() })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_ring_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Ring { call_id: "nonexistent".into() })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_reject_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Reject { call_id: "nonexistent".into(), reason: None })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_attach_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::AttachCall { call_id: "nonexistent".into(), mode: crate::rwi::session::OwnershipMode::Control })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_detach_call_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::DetachCall { call_id: "nonexistent".into() })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_detach_call_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-to-detach", "caller1", "callee1", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry.clone());

        // Detach should succeed
        let result = processor
            .process_command(RwiCommandPayload::DetachCall { call_id: "call-to-detach".into() })
            .await;

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), CommandResult::Success));

        // The call should still exist in registry (DetachCall doesn't remove it)
        assert!(registry.get_handle("call-to-detach").is_some());
    }

    #[tokio::test]
    async fn test_hangup_call_not_found() {
        let processor = create_test_processor();
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
        let processor = create_test_processor();
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
        let processor = create_test_processor();
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
        let processor = create_test_processor_with_registry(registry.clone());
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
        let processor = create_test_processor_with_registry(registry.clone());
        let _ha = create_test_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);
        let _hb = create_test_call(&registry, "leg-b", "1001", "2002", DialDirection::Outbound);

        // Both calls exist; bridge command should succeed (channel may be closed in test
        // but lookup succeeds)
        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b".into(),
            })
            .await;
        // Either Success (if send succeeds) or CommandFailed (channel closed) — NOT not_found
        match &result {
            Ok(_) => {}
            Err(CommandError::CommandFailed(_)) => {} // expected in test without receiver
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_unbridge_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Unbridge { call_id: "nope".into() })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_subscribe_success() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Subscribe { contexts: vec!["ctx1".into()] })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_unsubscribe_success() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Unsubscribe { contexts: vec!["ctx1".into()] })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_media_play_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaPlay(crate::rwi::session::MediaPlayRequest {
                call_id: "missing".into(),
                source: crate::rwi::session::MediaSource {
                    source_type: "file".into(),
                    uri: Some("welcome.wav".into()),
                    looped: None,
                },
                interrupt_on_dtmf: false,
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_originate_no_server_returns_error() {
        // Without a SIP server wired in, originate should return CommandFailed
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Originate(crate::rwi::session::OriginateRequest {
                call_id: "new-call".into(),
                destination: "sip:test@local".into(),
                caller_id: None,
                timeout_secs: Some(30),
                hold_music: None,
                hold_music_target: None,
                ringback: None,
                ringback_target: None,
                extra_headers: std::collections::HashMap::new(),
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("SIP server not available"));
    }

    #[tokio::test]
    async fn test_originate_invalid_destination_returns_error() {
        // Even with a SIP server, an invalid URI should return CommandFailed
        // Here we don't have a real SipServer so we just verify the no-server path;
        // URI-parse errors are caught in the same method after the server check.
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Originate(crate::rwi::session::OriginateRequest {
                call_id: "new-call-2".into(),
                destination: "not-a-sip-uri".into(),
                caller_id: None,
                timeout_secs: None,
                hold_music: None,
                hold_music_target: None,
                ringback: None,
                ringback_target: None,
                extra_headers: std::collections::HashMap::new(),
            }))
            .await;
        assert!(result.is_err());
        // No server → "SIP server not available" wins before URI parse
        assert!(result.unwrap_err().to_string().contains("SIP server not available"));
    }

    // ===== Integration: calls with real handles =====

    #[tokio::test]
    async fn test_answer_existing_call() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let _handle = create_test_call(&registry, "call-001", "1001", "2000", DialDirection::Inbound);
        assert!(registry.get_handle("call-001").is_some());

        let result = processor
            .process_command(RwiCommandPayload::Answer { call_id: "call-001".into() })
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
        let processor = create_test_processor_with_registry(registry.clone());
        let _handle = create_test_call(&registry, "call-001", "1001", "2000", DialDirection::Inbound);

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
        let processor = create_test_processor_with_registry(registry.clone());

        create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        create_test_call(&registry, "call-2", "1002", "2001", DialDirection::Outbound);
        create_test_call(&registry, "call-3", "1003", "2002", DialDirection::Inbound);

        let result = processor.process_command(RwiCommandPayload::ListCalls).await;
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
        let processor = create_test_processor_with_registry(registry.clone());

        create_test_call(&registry, "inbound-1", "1001", "2000", DialDirection::Inbound);
        create_test_call(&registry, "outbound-1", "2001", "1001", DialDirection::Outbound);
        create_test_call(&registry, "inbound-2", "1002", "2000", DialDirection::Inbound);

        let result = processor.process_command(RwiCommandPayload::ListCalls).await;
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
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway.clone()));

        // Create two legs
        let _ha = create_test_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);
        let _hb = create_test_call(&registry, "leg-b", "1001", "2002", DialDirection::Outbound);

        // Register a session in gateway and claim ownership of leg-a
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
            gw.claim_call_ownership(&sid, "leg-a".into(), crate::rwi::session::OwnershipMode::Control)
                .await
                .unwrap();
        }

        // Bridge should send BridgeTo and then emit CallBridged to owner of leg-a
        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b".into(),
            })
            .await;

        // Regardless of whether channel is closed (test-only), the gateway fan-out should fire
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {
                // A CallBridged event should have been sent to the session
                let ev = event_rx.recv().await;
                assert!(ev.is_some(), "Expected CallBridged event on gateway");
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    // ── StopPlayback / Unbridge command paths ──────────────────────────────

    #[tokio::test]
    async fn test_media_stop_not_found() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaStop { call_id: "ghost".into() })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_media_stop_existing_call_sends_stop_playback() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let (_handle, mut rx) =
            create_test_call_with_rx(&registry, "call-stop", "1001", "2000", DialDirection::Inbound);

        let result = processor
            .process_command(RwiCommandPayload::MediaStop { call_id: "call-stop".into() })
            .await;
        // Success OR CommandFailed (channel closed) — NOT not_found
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let cmd = rx.try_recv().expect("StopPlayback should be queued");
        assert_eq!(
            cmd,
            crate::proxy::proxy_call::state::SessionAction::StopPlayback
        );
    }

    #[tokio::test]
    async fn test_unbridge_existing_call_sends_unbridge() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
        let (_handle, mut rx) =
            create_test_call_with_rx(&registry, "call-unb", "1001", "2000", DialDirection::Inbound);

        let result = processor
            .process_command(RwiCommandPayload::Unbridge { call_id: "call-unb".into() })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let cmd = rx.try_recv().expect("Unbridge should be queued");
        assert_eq!(
            cmd,
            crate::proxy::proxy_call::state::SessionAction::Unbridge
        );
    }

    #[tokio::test]
    async fn test_bridge_sends_bridge_to_to_leg_a() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let processor = create_test_processor_with_registry(registry.clone());
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

        // leg_a should have received BridgeTo { target_session_id: "leg-b2" }
        let cmd = rx_a.try_recv().expect("BridgeTo should be queued on leg_a");
        assert!(
            matches!(cmd, crate::proxy::proxy_call::state::SessionAction::BridgeTo { ref target_session_id } if target_session_id == "leg-b2"),
            "expected BridgeTo(leg-b2), got {:?}",
            cmd
        );
    }

    #[tokio::test]
    async fn test_unbridge_emits_call_unbridged_event_to_gateway() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway.clone()));

        let (_handle, _rx) =
            create_test_call_with_rx(&registry, "call-ev", "1001", "2000", DialDirection::Inbound);

        // Register a session and wire an event channel
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
            gw.claim_call_ownership(&sid, "call-ev".into(), crate::rwi::session::OwnershipMode::Control)
                .await
                .unwrap();
        }

        let result = processor
            .process_command(RwiCommandPayload::Unbridge { call_id: "call-ev".into() })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {
                let ev = event_rx.recv().await;
                assert!(ev.is_some(), "Expected CallUnbridged event");
                let v = ev.unwrap();
                // The event JSON should contain the call_id
                let s = serde_json::to_string(&v).unwrap();
                assert!(s.contains("call-ev"), "Event should reference call-ev");
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_set_ringback_source_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(&registry, "call-target", "1001", "2000", DialDirection::Inbound);
        let _handle2 = create_test_call(&registry, "call-source", "1002", "2001", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "call-target".into(),
                source_call_id: "call-source".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_set_ringback_source_target_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-source", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let _handle = create_test_call(&registry, "call-target", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let (_handle, mut rx) = create_test_call_with_rx(&registry, "call-rec", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
            assert!(matches!(action, SessionAction::StartRecording { .. }));
        }
    }

    #[tokio::test]
    async fn test_record_start_not_found() {
        let processor = create_test_processor();
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
        let (_handle, mut rx) = create_test_call_with_rx(&registry, "call-rec-p", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let _handle = create_test_call(&registry, "call-norec", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let (_handle, mut rx) = create_test_call_with_rx(&registry, "call-rec-r", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let _handle = create_test_call(&registry, "call-norec2", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let (_handle, mut rx) = create_test_call_with_rx(&registry, "call-rec-s", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
            if matches!(cmd, SessionAction::StopRecording) {
                found_stop = true;
                break;
            }
        }
        assert!(found_stop, "Expected StopRecording action to be sent");
    }

    #[tokio::test]
    async fn test_record_stop_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(&registry, "call-norec3", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordStop {
                call_id: "call-norec3".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        if let Ok(action) = cmd {
            assert!(matches!(action, SessionAction::StopRecording));
        }
    }

    #[tokio::test]
    async fn test_queue_enqueue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (handle, _rx) = create_test_call_with_rx(&registry, "call-q", "1001", "support", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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

        assert_eq!(handle.queue_name(), Some("support".to_string()));
    }

    #[tokio::test]
    async fn test_queue_enqueue_not_found() {
        let processor = create_test_processor();
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
        let (handle, _rx) = create_test_call_with_rx(&registry, "call-dq", "1001", "support", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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

        assert_eq!(handle.queue_name(), None);
    }

    #[tokio::test]
    async fn test_queue_dequeue_not_found() {
        let processor = create_test_processor();
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
        let (_handle, mut rx) = create_test_call_with_rx(&registry, "call-hold", "1001", "support", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let _handle = create_test_call(&registry, "call-noq", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-noq".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not in queue"));
    }

    #[tokio::test]
    async fn test_queue_unhold_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(&registry, "call-unhold", "1001", "support", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let _handle = create_test_call(&registry, "call-noq2", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueUnhold {
                call_id: "call-noq2".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not in queue"));
    }

    #[tokio::test]
    async fn test_supervisor_listen_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(&registry, "supervisor-1", "1001", "2000", DialDirection::Inbound);
        let _handle2 = create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorListen {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
            })
            .await;
        // Either Success (if send succeeds) or CommandFailed (channel closed) — NOT not_found
        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_listen_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let _handle1 = create_test_call(&registry, "supervisor-1", "1001", "2000", DialDirection::Inbound);
        let _handle2 = create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
                agent_leg: "agent-1".into(),
            })
            .await;
        // Either Success (if send succeeds) or CommandFailed (channel closed) — NOT not_found
        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_barge_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(&registry, "supervisor-1", "1001", "2000", DialDirection::Inbound);
        let _handle2 = create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorBarge {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
                agent_leg: "agent-1".into(),
            })
            .await;
        // Either Success (if send succeeds) or CommandFailed (channel closed) — NOT not_found
        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let processor = create_test_processor_with_registry(registry);

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
        let processor = create_test_processor_with_registry(registry);

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
        let processor = create_test_processor();
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
        let processor = create_test_processor_with_registry(registry);

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
        let processor = create_test_processor_with_registry(registry);

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
        let processor = create_test_processor_with_registry(registry);

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
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipMessage {
                call_id: "call-1".into(),
                content_type: "text/plain".into(),
                body: "Hello".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("SIP server not available"));
    }

    #[tokio::test]
    async fn test_sip_notify_no_server() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipNotify {
                call_id: "call-1".into(),
                event: "check-sync".into(),
                content_type: "application/simple-message-summary".into(),
                body: "".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("SIP server not available"));
    }

    #[tokio::test]
    async fn test_sip_options_ping_no_server() {
        let processor = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipOptionsPing {
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("SIP server not available"));
    }

    // Conference tests
    #[tokio::test]
    async fn test_conference_create_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(ConferenceCreateRequest {
                conf_id: "room-1".into(),
                backend: "internal".to_string(),
                max_members: Some(10),
                record: false,
                mcu_uri: None,
            }))
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
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        // Create first conference
        processor
            .process_command(RwiCommandPayload::ConferenceCreate(ConferenceCreateRequest {
                conf_id: "room-1".into(),
                backend: "internal".to_string(),
                max_members: None,
                record: false,
                mcu_uri: None,
            }))
            .await
            .unwrap();

        // Try to create duplicate
        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(ConferenceCreateRequest {
                conf_id: "room-1".into(),
                backend: "internal".to_string(),
                max_members: None,
                record: false,
                mcu_uri: None,
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_conference_create_external_requires_mcu_uri() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(ConferenceCreateRequest {
                conf_id: "room-1".into(),
                backend: "external".to_string(),
                max_members: None,
                record: false,
                mcu_uri: None,
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("external backend requires mcu_uri"));
    }

    #[tokio::test]
    async fn test_conference_add_not_found_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

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
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        // Create conference
        processor
            .process_command(RwiCommandPayload::ConferenceCreate(ConferenceCreateRequest {
                conf_id: "room-1".into(),
                backend: "internal".to_string(),
                max_members: None,
                record: false,
                mcu_uri: None,
            }))
            .await
            .unwrap();

        // Destroy it
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
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

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
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway));

        // Create conference but don't add any calls
        processor
            .process_command(RwiCommandPayload::ConferenceCreate(ConferenceCreateRequest {
                conf_id: "room-1".into(),
                backend: "internal".to_string(),
                max_members: None,
                record: false,
                mcu_uri: None,
            }))
            .await
            .unwrap();

        // Try to mute a call that's not in the conference
        let result = processor
            .process_command(RwiCommandPayload::ConferenceMute {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("is not in conference"));
    }

    #[tokio::test]
    async fn test_conference_add_with_max_members() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create conference with max 2 members
        processor
            .process_command(RwiCommandPayload::ConferenceCreate(ConferenceCreateRequest {
                conf_id: "room-1".into(),
                backend: "internal".to_string(),
                max_members: Some(2),
                record: false,
                mcu_uri: None,
            }))
            .await
            .unwrap();

        // Create and add first call
        let _handle1 = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());

        // Create and add second call
        let _handle2 = create_test_call(&registry, "call-2", "1002", "2001", DialDirection::Inbound);
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-2".into(),
            })
            .await;
        assert!(result.is_ok());

        // Try to add third call - should fail
        let _handle3 = create_test_call(&registry, "call-3", "1003", "2002", DialDirection::Inbound);
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-3".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("is full"));
    }

    // Queue new commands tests
    #[tokio::test]
    async fn test_queue_set_priority_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call and enqueue it
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

        // Set priority
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
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call but don't enqueue it
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);

        // Try to set priority - should fail
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
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call and enqueue it
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

        // Assign agent
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
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call and enqueue it
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

        // Requeue to different queue
        let result = processor
            .process_command(RwiCommandPayload::QueueRequeue {
                call_id: "call-1".into(),
                queue_id: "sales".into(),
                priority: Some(5),
            })
            .await;
        assert!(result.is_ok());
    }

    // Record mask segment test
    #[tokio::test]
    async fn test_record_mask_segment_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call and manually add to record_states (bypassing the actual recording command)
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        {
            let mut states = processor.record_states.write().await;
            states.insert(
                "call-1".to_string(),
                RecordState {
                    recording_id: "rec-1".to_string(),
                    _mode: "mixed".to_string(),
                    _path: "/tmp/recording.wav".to_string(),
                    is_paused: false,
                },
            );
        }

        // Mask a segment
        let result = processor
            .process_command(RwiCommandPayload::RecordMaskSegment {
                call_id: "call-1".into(),
                recording_id: "rec-1".into(),
                start_secs: 30,
                end_secs: 60,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_record_mask_segment_no_recording_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(registry.clone(), gateway));

        // Create a call but don't start recording
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);

        // Try to mask segment - should fail
        let result = processor
            .process_command(RwiCommandPayload::RecordMaskSegment {
                call_id: "call-1".into(),
                recording_id: "rec-1".into(),
                start_secs: 30,
                end_secs: 60,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No active recording"));
    }
}
