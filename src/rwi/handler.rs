use crate::handler::middleware::clientaddr::ClientAddr;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::server::SipServerRef;
use crate::rwi::RwiGatewayRef;
use crate::rwi::auth::{RwiAuth, RwiIdentity};
use crate::rwi::processor::{CommandError, CommandResult, RwiCommandProcessor};
use crate::rwi::session::{RwiCommandMessage, RwiCommandPayload};
use axum::{
    Extension,
    extract::Query,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc;

#[allow(clippy::too_many_arguments)]
pub async fn rwi_ws_handler(
    _client_addr: ClientAddr,
    ws: WebSocketUpgrade,
    Query(params): Query<std::collections::HashMap<String, String>>,
    Extension(auth): Extension<Arc<RwLock<RwiAuth>>>,
    Extension(gateway): Extension<RwiGatewayRef>,
    Extension(call_registry): Extension<Arc<ActiveProxyCallRegistry>>,
    Extension(sip_server): Extension<Option<SipServerRef>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = extract_token(&headers, &params);

    let identity = match token {
        Some(t) => {
            let auth = auth.read().await;
            auth.validate_token(&t)
        }
        None => None,
    };

    let identity = match identity {
        Some(i) => i,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                [(
                    header::WWW_AUTHENTICATE,
                    r#"Bearer realm="rwi", error="invalid_token""#,
                )],
            )
                .into_response();
        }
    };

    ws.protocols(["rwi-v1"])
        .on_upgrade(async move |socket| {
            handle_websocket(socket, identity, gateway, call_registry, sip_server).await;
        })
        .into_response()
}

fn extract_token(
    headers: &HeaderMap,
    query_params: &std::collections::HashMap<String, String>,
) -> Option<String> {
    if let Some(auth_header) = headers.get("authorization")
        && let Ok(auth_str) = auth_header.to_str()
        && auth_str.starts_with("Bearer ")
    {
        return Some(auth_str[7..].to_string());
    }

    query_params.get("token").cloned()
}

/// Single unified WebSocket session loop.
///
/// Architecture:
/// ```text
///   ws_receiver -> [recv_task]
///                      | parse + process command
///                      | build RwiResponse JSON
///                      v
///                  [ws_tx channel]  <- gateway event fan-out also writes here
///                      |
///                  [write_task] -> ws_sender
/// ```
async fn handle_websocket(
    socket: WebSocket,
    identity: RwiIdentity,
    gateway: RwiGatewayRef,
    call_registry: Arc<ActiveProxyCallRegistry>,
    sip_server: Option<SipServerRef>,
) {
    let (mut ws_sender, mut ws_receiver) = socket.split();

    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<String>();

    let processor = {
        let conference_manager = sip_server
            .as_ref()
            .map(|s| s.conference_manager.clone())
            .unwrap_or_else(|| Arc::new(crate::call::runtime::ConferenceManager::new()));
        let p = RwiCommandProcessor::new(call_registry, gateway.clone(), conference_manager);
        let p = if let Some(server) = sip_server {
            p.with_sip_server(server)
        } else {
            p
        };
        Arc::new(p)
    };

    processor.register_transfer_notify_listener().await;

    let session_id = {
        let mut gw = gateway.write();
        let session = gw.create_session(identity.clone());
        let id = session.read().id.clone();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let ws_tx_clone = ws_tx.clone();
        crate::utils::spawn(async move {
            while let Some(v) = event_rx.recv().await {
                if let Ok(s) = serde_json::to_string(&v) {
                    let _ = ws_tx_clone.send(s);
                }
            }
        });
        gw.set_session_event_sender(&id, event_tx);
        id
    };

    let write_task = crate::utils::spawn(async move {
        while let Some(msg) = ws_rx.recv().await {
            if ws_sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    let (command_tx, _command_rx) = mpsc::unbounded_channel::<RwiCommandMessage>();

    let session_id_clone = session_id.clone();
    let gateway_clone = gateway.clone();
    let ws_tx_clone = ws_tx.clone();
    let recv_task = crate::utils::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let text = text.to_string();
                    handle_text_message(
                        &text,
                        &command_tx,
                        processor.clone(),
                        &session_id_clone,
                        gateway_clone.clone(),
                        &ws_tx_clone,
                    )
                    .await;
                }
                Ok(Message::Binary(data)) => {
                    handle_binary_message(
                        &data,
                        processor.clone(),
                        &session_id_clone,
                        gateway_clone.clone(),
                    )
                    .await;
                }
                Ok(Message::Close(_)) => break,
                Err(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = write_task => {}
        _ = recv_task => {}
    }

    let (cleanup_call_ids, meta_store) = {
        let mut gw = gateway.write();
        let call_ids = gw.remove_session(&session_id);
        let meta = gw.meta_store.clone();
        (call_ids, meta)
    };
    for call_id in &cleanup_call_ids {
        meta_store.remove(call_id).await;
    }
}

/// Process one text frame from the WebSocket.
///
/// Returns the JSON string to send back as a response (always — even for errors).
async fn handle_text_message(
    text: &str,
    _command_tx: &mpsc::UnboundedSender<RwiCommandMessage>,
    processor: Arc<RwiCommandProcessor>,
    session_id: &str,
    gateway: RwiGatewayRef,
    ws_tx: &mpsc::UnboundedSender<String>,
) {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to parse JSON");
            let err_resp = serde_json::to_string(&serde_json::json!({
                "type": "command_failed",
                "status": "error",
                "action_id": "",
                "action": "",
                "error": format!("parse_error: {e}"),
            }))
            .unwrap_or_default();
            let _ = ws_tx.send(err_resp);
            return;
        }
    };

    let action = match value.get("action").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => {
            tracing::warn!("Missing action field");
            let action_id = value
                .get("action_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let err_resp = serde_json::to_string(&serde_json::json!({
                "type": "command_failed",
                "status": "error",
                "action_id": action_id,
                "action": "",
                "error": "missing_action",
            }))
            .unwrap_or_default();
            let _ = ws_tx.send(err_resp);
            return;
        }
    };

    let action_id = value
        .get("action_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();

    if action_id.is_empty() {
        tracing::warn!("Missing action_id field");
        return;
    }

    if processor.is_duplicate_action(&action_id).await {
        tracing::info!(%action_id, "Duplicate command detected, ignoring");
        return;
    }

    let params = value.get("params").unwrap_or(&serde_json::Value::Null);

    let command = match parse_action(&action, params, &action_id) {
        Ok(cmd) => cmd,
        Err(msg) => {
            tracing::warn!(error = %msg, "Failed to parse action");
            let err_resp = serde_json::to_string(&serde_json::json!({
                "type": "command_failed",
                "status": "error",
                "action_id": action_id,
                "action": action,
                "error": format!("unknown_action: {msg}"),
            }))
            .unwrap_or_default();
            let _ = ws_tx.send(err_resp);
            return;
        }
    };

    match &command {
        RwiCommandPayload::Subscribe { contexts, events } => {
            let mut gw = gateway.write();
            gw.subscribe(&session_id.to_string(), contexts.clone(), events.clone());
        }
        RwiCommandPayload::Unsubscribe { contexts } => {
            let mut gw = gateway.write();
            gw.unsubscribe(&session_id.to_string(), contexts);
        }
        RwiCommandPayload::DetachCall { call_id } => {
            let mut gw = gateway.write();
            if gw.release_call_ownership(&session_id.to_string(), call_id) {
                gw.detach_supervisor(&session_id.to_string(), call_id);
            }
        }
        _ => {}
    }

    let call_id = extract_call_id(&command).map(|s| s.to_string());

    // For originate/attach, remember whether we should claim ownership on success
    let should_claim_ownership = matches!(
        &command,
        RwiCommandPayload::Originate(_) | RwiCommandPayload::AttachCall { .. }
    );

    let result = processor.process_command(command).await;

    // Auto-claim call ownership when originate or attach succeeds
    if should_claim_ownership
        && let Ok(
            CommandResult::Originated { call_id: ref cid }
            | CommandResult::CallFound { call_id: ref cid },
        ) = result
    {
        let mut gw = gateway.write();
        let _ = gw.claim_call_ownership(
            &session_id.to_string(),
            cid.clone(),
            crate::rwi::session::OwnershipMode::Control,
        );
    }

    let event = build_command_result_event(&action_id, &action, call_id.as_deref(), result);
    if let Ok(json) = serde_json::to_string(&event) {
        let _ = ws_tx.send(json);
    }

    processor.record_action(action_id, None).await;
}

fn build_command_result_event(
    action_id: &str,
    action: &str,
    call_id: Option<&str>,
    result: Result<CommandResult, CommandError>,
) -> serde_json::Value {
    match result {
        Ok(cmd_result) => {
            let mut event = serde_json::json!({
                "type": "command_completed",
                "action_id": action_id,
                "action": action,
            });
            if let Some(cid) = call_id {
                event["call_id"] = serde_json::json!(cid);
            }
            match cmd_result {
                CommandResult::Success => {
                    event["status"] = serde_json::json!("success");
                }
                CommandResult::ListCalls(calls) => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!(calls);
                }
                CommandResult::CallFound { call_id } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "call_id": call_id });
                }
                CommandResult::Originated { call_id } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "call_id": call_id });
                }
                CommandResult::MediaPlay { track_id } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "track_id": track_id });
                }
                CommandResult::TransferAttended {
                    original_call_id,
                    consultation_call_id,
                } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({
                        "original_call_id": original_call_id,
                        "consultation_call_id": consultation_call_id
                    });
                }
                CommandResult::ConsultInitiated {
                    call_id: orig,
                    consultation_call_id,
                } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({
                        "call_id": orig,
                        "consultation_call_id": consultation_call_id
                    });
                }
                CommandResult::ConferenceCreated { conf_id } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "conf_id": conf_id });
                }
                CommandResult::ConferenceMemberAdded { conf_id, call_id } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "conf_id": conf_id, "call_id": call_id });
                }
                CommandResult::ConferenceMemberRemoved { conf_id, call_id } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "conf_id": conf_id, "call_id": call_id });
                }
                CommandResult::ConferenceMemberMuted { conf_id, call_id } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "conf_id": conf_id, "call_id": call_id });
                }
                CommandResult::ConferenceMemberUnmuted { conf_id, call_id } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "conf_id": conf_id, "call_id": call_id });
                }
                CommandResult::ConferenceDestroyed { conf_id } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "conf_id": conf_id });
                }
                CommandResult::SessionResumed {
                    replayed_count,
                    current_sequence,
                    events,
                } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({
                        "replayed_count": replayed_count,
                        "current_sequence": current_sequence,
                        "events": events,
                    });
                }
                CommandResult::CallResumed {
                    call_id: cid,
                    replayed_count,
                    current_sequence,
                    events,
                } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({
                        "call_id": cid,
                        "replayed_count": replayed_count,
                        "current_sequence": current_sequence,
                        "events": events,
                    });
                }
                CommandResult::CallVar { key, value } => {
                    event["status"] = serde_json::json!("success");
                    event["data"] = serde_json::json!({ "key": key, "value": value });
                }
            }
            event
        }
        Err(cmd_error) => {
            let mut event = serde_json::json!({
                "type": "command_failed",
                "status": "error",
                "action_id": action_id,
                "action": action,
                "error": cmd_error.to_string(),
            });
            if let Some(cid) = call_id {
                event["call_id"] = serde_json::json!(cid);
            }
            event
        }
    }
}

async fn handle_binary_message(
    data: &[u8],
    _processor: Arc<RwiCommandProcessor>,
    session_id: &str,
    gateway: RwiGatewayRef,
) {
    if data.len() < 16 {
        tracing::warn!(session_id = %session_id, "Received invalid binary frame: too small");
        return;
    }

    let call_id_bytes = &data[0..8];
    let call_id = String::from_utf8_lossy(call_id_bytes)
        .trim_end_matches('\0')
        .trim()
        .to_string();

    if call_id.is_empty() {
        tracing::warn!(session_id = %session_id, "Received binary frame with empty call_id");
        return;
    }

    let timestamp_ms = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as u64;
    let sample_rate = u16::from_be_bytes([data[12], data[13]]);
    let flags = u16::from_be_bytes([data[14], data[15]]);
    let is_last_frame = (flags & 0x01) != 0;

    let pcm_data = &data[16..];
    let num_samples = pcm_data.len() / 2;

    tracing::trace!(
        session_id = %session_id,
        call_id = %call_id,
        timestamp_ms = timestamp_ms,
        sample_rate = sample_rate,
        is_last_frame = is_last_frame,
        pcm_bytes = pcm_data.len(),
        num_samples = num_samples,
        "Received PCM binary frame"
    );

    let owns_call = {
        let gw = gateway.read();
        gw.session_owns_call(&session_id.to_string(), &call_id)
    };

    if !owns_call {
        tracing::warn!(
            session_id = %session_id,
            call_id = %call_id,
            "Session does not own call, dropping PCM frame"
        );
        return;
    }

    tracing::debug!(
        call_id = %call_id,
        pcm_bytes = pcm_data.len(),
        "PCM frame received"
    );
}

fn parse_action(
    action: &str,
    params: &serde_json::Value,
    action_id: &str,
) -> Result<RwiCommandPayload, String> {
    const UNIT_VARIANTS: &[&str] = &["session.list_calls"];
    const NEED_EMPTY_PARAMS: &[&str] = &["session.resume", "call.resume"];

    let json = if params.is_null() {
        serde_json::json!({
            "action": action,
            "action_id": action_id,
        })
    } else if let serde_json::Value::Object(obj) = params {
        if obj.is_empty() {
            if UNIT_VARIANTS.contains(&action) {
                serde_json::json!({
                    "action": action,
                    "action_id": action_id,
                })
            } else if NEED_EMPTY_PARAMS.contains(&action) {
                serde_json::json!({
                    "action": action,
                    "action_id": action_id,
                    "params": params
                })
            } else {
                serde_json::json!({
                    "action": action,
                    "action_id": action_id,
                })
            }
        } else {
            // Non-empty params
            serde_json::json!({
                "action": action,
                "action_id": action_id,
                "params": params
            })
        }
    } else {
        // Non-object params
        serde_json::json!({
            "action": action,
            "action_id": action_id,
            "params": params
        })
    };

    let req: crate::rwi::session::RwiRequest =
        serde_json::from_value(json).map_err(|e| e.to_string())?;

    Ok(req.into())
}

fn extract_call_id<'a>(cmd: &'a RwiCommandPayload) -> Option<&'a str> {
    match cmd {
        RwiCommandPayload::Subscribe { .. } => None,
        RwiCommandPayload::Unsubscribe { .. } => None,
        RwiCommandPayload::ListCalls => None,
        RwiCommandPayload::AttachCall { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::DetachCall { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::Originate(r) => Some(r.call_id.as_str()),
        RwiCommandPayload::Answer { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::Reject { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::Ring { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::Hangup { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::Bridge { leg_a, .. } => Some(leg_a.as_str()),
        RwiCommandPayload::Unbridge { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::Transfer { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::TransferReplace { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::TransferAttended { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::TransferComplete { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::TransferCancel {
            consultation_call_id,
        } => Some(consultation_call_id.as_str()),
        RwiCommandPayload::CallHold { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::CallUnhold { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::SetRingbackSource { target_call_id, .. } => {
            Some(target_call_id.as_str())
        }
        RwiCommandPayload::SetVar { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::GetVar { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::MediaPlay(r) => Some(r.call_id.as_str()),
        RwiCommandPayload::MediaStop { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::MediaStreamStart(r) => Some(r.call_id.as_str()),
        RwiCommandPayload::MediaStreamStop { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::MediaInjectStart(r) => Some(r.call_id.as_str()),
        RwiCommandPayload::MediaInjectStop { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::CallSendDtmf { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::DtmfCollect(r) => Some(r.call_id.as_str()),
        RwiCommandPayload::RecordStart(r) => Some(r.call_id.as_str()),
        RwiCommandPayload::RecordPause { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::RecordResume { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::RecordStop { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::QueueEnqueue(r) => Some(r.call_id.as_str()),
        RwiCommandPayload::QueueDequeue { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::QueueHold { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::QueueUnhold { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::QueueSetPriority { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::QueueAssignAgent { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::QueueRequeue { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::SupervisorListen { target_call_id, .. } => Some(target_call_id.as_str()),
        RwiCommandPayload::SupervisorWhisper { target_call_id, .. } => {
            Some(target_call_id.as_str())
        }
        RwiCommandPayload::SupervisorBarge { target_call_id, .. } => Some(target_call_id.as_str()),
        RwiCommandPayload::SupervisorTakeover { target_call_id, .. } => {
            Some(target_call_id.as_str())
        }
        RwiCommandPayload::SupervisorStop { target_call_id, .. } => Some(target_call_id.as_str()),
        RwiCommandPayload::SipMessage { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::SipNotify { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::SipOptionsPing { call_id } => Some(call_id.as_str()),
        RwiCommandPayload::LegAdd { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::LegRemove { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::AppStart { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::AppStop { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::AppChain { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::ConferenceCreate(req) => Some(req.conf_id.as_str()),
        RwiCommandPayload::ConferenceAdd { conf_id, .. } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceRemove { conf_id, .. } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceMute { conf_id, .. } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceUnmute { conf_id, .. } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceDestroy { conf_id } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceEnd { conf_id, .. } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceKick { conf_id, .. } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceMuteAll { conf_id } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceInfo { conf_id } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceList => None,
        RwiCommandPayload::ConferenceMerge { conf_id, .. } => Some(conf_id.as_str()),
        RwiCommandPayload::ConferenceSeatReplace { conf_id, .. } => Some(conf_id.as_str()),
        RwiCommandPayload::ParallelOriginate(req) => Some(req.operation_id.as_str()),
        RwiCommandPayload::SessionResume { .. } => None,
        RwiCommandPayload::CallResume { call_id, .. } => Some(call_id.as_str()),
        // CC addon commands
        RwiCommandPayload::AgentRegister { agent_id, .. } => Some(agent_id.as_str()),
        RwiCommandPayload::AgentUnregister { agent_id } => Some(agent_id.as_str()),
        RwiCommandPayload::AgentStatusUpdate { agent_id, .. } => Some(agent_id.as_str()),
        RwiCommandPayload::AgentStats { agent_id } => agent_id.as_deref(),
        RwiCommandPayload::QueueStats { queue_id } => queue_id.as_deref(),
        RwiCommandPayload::ConsultInitiate { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::ConsultMerge { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::ConsultComplete { call_id, .. } => Some(call_id.as_str()),
        RwiCommandPayload::ConsultCancel {
            consultation_call_id,
        } => Some(consultation_call_id.as_str()),
    }
}
