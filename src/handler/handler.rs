use super::middleware::clientaddr::ClientAddr;
use crate::{
    app::AppState,
    call::{
        ActiveCallType, Command,
        active_call::{CallParams, handle_call},
    },
    event::SessionEvent,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State, WebSocketUpgrade, ws::Message},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use std::{sync::atomic::Ordering};
use tokio::{join, select};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/call", get(ws_handler))
        .route("/call/webrtc", get(webrtc_handler))
        .route("/call/sip", get(sip_handler))
        .route("/call/lists", get(list_calls))
        .route("/call/kill/{id}", post(kill_call))
        .nest("/llm/v1", super::llmproxy::router())
        .route("/iceservers", get(super::webrtc::get_iceservers))
        .route("/health", get(health_handler))
}

async fn health_handler(State(state): State<AppState>) -> Response {
    let health = serde_json::json!({
        "status": "running",
        "uptime": state.uptime,
        "version": crate::version::get_version_info(),
        "total": state.total_calls.load(Ordering::Relaxed),
        "failed": state.total_failed_calls.load(Ordering::Relaxed),
        "runnings": state.active_calls.lock().await.len(),
    });
    Json(health).into_response()
}

async fn list_calls(State(state): State<AppState>) -> Response {
    let active_calls = state.active_calls.lock().await;
    let result = serde_json::json!({
        "total": active_calls.len(),
        "calls": active_calls.iter().map(|(id, call)| {
            let call_state = match call.call_state.read() {
                Ok(call_state) => call_state,
                Err(_) => return serde_json::json!({"id": id, "error": "Failed to read call state"}),
            };
            serde_json::json!({
                "id": id,
                "callType": call.call_type,
                "startTime": call_state.start_time.to_rfc3339(),
                "ringTime": call_state.ring_time.map(|t| t.to_rfc3339()),
                "answerTime": call_state.answer_time.map(|t| t.to_rfc3339()),
                "duration": call_state.answer_time
                    .map(|t| (Utc::now() - t).num_seconds()),
            })
        }).collect::<Vec<_>>(),
    });
    Json(result).into_response()
}

async fn kill_call(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if let Some(call) = state.active_calls.lock().await.remove(&id) {
        call.cancel_token.cancel();
        info!(id, "Call killed");
    }
    Json(true).into_response()
}

pub async fn ws_handler(
    client_ip: ClientAddr,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    call_handler(client_ip, ActiveCallType::WebSocket, ws, state, params).await
}

pub async fn sip_handler(
    client_ip: ClientAddr,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    call_handler(client_ip, ActiveCallType::Sip, ws, state, params).await
}

pub async fn webrtc_handler(
    client_ip: ClientAddr,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    call_handler(client_ip, ActiveCallType::Webrtc, ws, state, params).await
}

pub async fn call_handler(
    client_ip: ClientAddr,
    call_type: ActiveCallType,
    ws: WebSocketUpgrade,
    app_state: AppState,
    params: CallParams,
) -> Response {
    let session_id = params.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let dump_events = params.dump_events.unwrap_or(true);
    let resp = ws.on_upgrade(move |socket| async move {
        let event_sender = crate::event::create_event_sender();
        let (mut ws_sender, mut ws_receiver) = socket.split();
        let cmd_sender = tokio::sync::broadcast::Sender::<Command>::new(32);
        let (audio_sender, audio_receiver) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
        
        let cancel_token = CancellationToken::new();
        let dump_cmd_receiver = cmd_sender.subscribe();
        let cmd_receiver = cmd_sender.subscribe();
        
        let recv_from_ws_loop = async {
            while let Some(Ok(message)) = ws_receiver.next().await {
                match message {
                    Message::Text(text) => {
                        let command = match serde_json::from_str::<Command>(&text) {
                            Ok(cmd) => cmd,
                            Err(e) => {
                                warn!(session_id, %client_ip, %text, "Failed to parse command {}",e);
                                continue;
                            }
                        };
                        if let Err(e) = cmd_sender.send(command) {
                            error!("Failed to send WebSocket message: {}", e);
                            break;
                        }
                    }
                    Message::Binary(bin) => {
                        audio_sender.send(bin.into()).ok();
                    }
                    Message::Close(_) => {
                        info!(session_id, %client_ip, "WebSocket closed by client");
                        break;
                    }
                    _ => {}
                }
            }
        };

        let mut event_receiver = event_sender.subscribe();
        let send_to_ws_loop = async {
            while let Ok(event) = event_receiver.recv().await {
                match event {
                    SessionEvent::Binary { data,.. } => {
                        let message = Message::Binary(data.into());
                        if let Err(e) = ws_sender.send(message).await {
                            warn!(session_id, %client_ip, "Failed to send WebSocket message: {}", e);
                            break;
                        }
                    }
                    _ => {
                        match serde_json::to_string(&event) {
                            Ok(message) => {
                                if let Err(e) = ws_sender.send(Message::Text(message.into())).await {
                                    warn!(session_id, %client_ip, "Failed to send WebSocket message: {}", e);
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!(session_id, %client_ip, "Failed to serialize event: {}", e);
                            }
                        }
                    }
                }
            }
        };

        let token_ref = cancel_token.clone();
        let handle_call_loop = async {
            match handle_call(
                cancel_token,
                call_type,
                session_id.clone(),
                dump_events,
                app_state.clone(),
                event_sender.clone(),
                audio_receiver,
                dump_cmd_receiver,
                cmd_receiver,
            )
            .await
            {
                Ok(call_record) => {
                    info!(
                        session_id,
                        %client_ip,
                        hangup_reason = ?call_record.hangup_reason,
                        call_type = ?call_record.call_type,
                        duration = Utc::now()
                            .signed_duration_since(call_record.start_time)
                            .as_seconds_f32(),
                        "Call ended"
                    );
                    if let Some(sender) = app_state.callrecord_sender.lock().await.as_ref() {
                        if let Err(e) = sender.send(call_record) {
                            warn!(session_id, %client_ip, "Failed to send call record: {}", e);
                        }
                    }
                }
                Err((e, call_record)) => {
                    warn!(session_id, %client_ip, "Call handling error: {}", e);
                    let error_event = SessionEvent::Error {
                        track_id:session_id.clone(),
                        timestamp:crate::get_timestamp(),
                        error:e.to_string(),
                        sender: "handle_call".to_string(),
                        code: None
                    };
                    event_sender.send(error_event).ok();
                    match call_record {
                        Some(record) => {
                            info!(
                                session_id,
                                %client_ip,
                                hangup_reason = ?record.hangup_reason,
                                call_type = ?record.call_type,
                                duration = Utc::now()
                                    .signed_duration_since(record.start_time)
                                    .as_seconds_f32(),
                                "Call ended with error"
                            );
                            if let Some(sender) = app_state.callrecord_sender.lock().await.as_ref() {
                                if let Err(e) = sender.send(record) {
                                    warn!(session_id, %client_ip, "Failed to send call record: {}", e);
                                }
                            }
                        }
                        None => {
                        }
                    }
                }
            };
            token_ref.cancel();
        };

        join!{
            async {
                select!{
                    _ = token_ref.cancelled() => {},
                    _ = send_to_ws_loop => {},
                    _ = recv_from_ws_loop => {},
                }
                token_ref.cancel();
            }, 
            handle_call_loop
        };
        
        // Drain remaining events
        while let Ok(event) = event_receiver.try_recv() {
            match event {
                SessionEvent::Binary { .. } => { }
                _ => {
                    match serde_json::to_string(&event) {
                        Ok(message) => {
                            if let Err(_) = ws_sender.send(Message::Text(message.into())).await {
                                break;
                            }
                        }
                        Err(_) => {}
                    }
                }
            }
        };
        debug!(session_id, %client_ip, "WebSocket connection closed");
    });
    resp
}
