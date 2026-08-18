use crate::handler::middleware::clientaddr::ClientAddr;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::server::SipServerRef;
use crate::rwi::RwiGatewayRef;
use crate::rwi::auth::{RwiAuth, RwiIdentity};
use crate::rwi::processor::{CommandError, CommandResult, RwiCommandProcessor};
use crate::rwi::session::RwiCommandPayload;
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
///
/// RAII guard that removes the session+its CallMeta from the gateway on drop.
/// Created at the top of [`handle_websocket`] so cleanup runs on ANY exit path
/// including panic, early return, or normal completion.
struct GatewaySessionGuard {
    gateway: RwiGatewayRef,
    session_id: String,
}

impl GatewaySessionGuard {
    fn new(gateway: &RwiGatewayRef, session_id: &str) -> Self {
        Self {
            gateway: gateway.clone(),
            session_id: session_id.to_string(),
        }
    }
}

impl Drop for GatewaySessionGuard {
    fn drop(&mut self) {
        let (call_ids, meta_store) = {
            let mut gw = self.gateway.write();
            let call_ids = gw.remove_session(&self.session_id);
            let meta_store = gw.meta_store.clone();
            (call_ids, meta_store)
        };
        for call_id in &call_ids {
            meta_store.remove(call_id);
        }
    }
}

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

    // Dropped when the WebSocket closes: removes the REFER NOTIFY listener so
    // disconnected sessions don't leak a subscriber + consumer task.
    let _transfer_listener = processor.register_transfer_notify_listener().await;

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

    // RAII: on any exit path (panic/return/select-complete), remove the
    // session from the gateway and clean up its CallMeta entries.
    let _session_guard = GatewaySessionGuard::new(&gateway, &session_id);

    let write_task = crate::utils::spawn(async move {
        while let Some(msg) = ws_rx.recv().await {
            if ws_sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

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
                        processor.clone(),
                        &session_id_clone,
                        gateway_clone.clone(),
                        &ws_tx_clone,
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

    // _session_guard is dropped here → remove_session + CallMeta cleanup.
}

/// Process one text frame from the WebSocket.
///
/// Returns the JSON string to send back as a response (always — even for errors).
async fn handle_text_message(
    text: &str,
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

    if processor.is_duplicate_action(&action_id) {
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
            gw.release_call_ownership(&session_id.to_string(), call_id);
        }
        _ => {}
    }

    let call_id = command.dispatch_call_id().map(|s| s.to_string());

    // Claim before dispatch: an asynchronously spawned originate can finish
    // immediately, so its SipSession-finished notification must never run
    // before ownership exists.
    let ownership_claim = match &command {
        RwiCommandPayload::Originate(req) => Some((
            req.call_id.clone(),
            crate::rwi::session::OwnershipMode::Control,
        )),
        RwiCommandPayload::AttachCall { call_id, mode } => {
            Some((call_id.clone(), mode.clone()))
        }
        _ => None,
    };
    let (ownership_claimed, ownership_error) = match ownership_claim.as_ref() {
        Some((call_id, mode)) => match gateway.write().claim_call_ownership(
            &session_id.to_string(),
            call_id.clone(),
            mode.clone(),
        ) {
            Ok(()) => (true, None),
            Err(error) => (false, Some(error)),
        },
        None => (false, None),
    };

    tracing::info!(
        audit_event = "call_command",
        action = %action,
        call_id = %call_id.as_deref().unwrap_or(""),
        source = "rwi",
        result = "received",
        "RWI command received"
    );
    let result = match ownership_error {
        Some(error) => Err(CommandError::CommandFailed(format!(
            "cannot claim call ownership: {error:?}"
        ))),
        None => processor.process_command(command).await,
    };

    // Synchronous validation/startup failures have no SipSession to emit the
    // finished notification, so release their pre-claimed ownership here.
    if ownership_claimed
        && result.is_err()
        && let Some((claimed_call_id, _)) = ownership_claim.as_ref()
    {
        gateway
            .write()
            .release_call_ownership(&session_id.to_string(), claimed_call_id);
    }

    let outcome = match &result {
        Ok(_) => "success",
        Err(_) => "error",
    };
    tracing::info!(
        audit_event = "call_command",
        action = %action,
        call_id = %call_id.as_deref().unwrap_or(""),
        source = "rwi",
        result = outcome,
        "RWI command completed"
    );
    let event = build_command_result_event(&action_id, &action, call_id.as_deref(), result);
    if let Ok(json) = serde_json::to_string(&event) {
        let _ = ws_tx.send(json);
    }

    processor.record_action(action_id);
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

    let mut req: crate::rwi::session::RwiRequest =
        serde_json::from_value(json).map_err(|e| e.to_string())?;
    req.payload.normalize();
    Ok(req.payload)
}
