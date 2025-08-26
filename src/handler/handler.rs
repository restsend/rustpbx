use std::{sync::Arc, time::{Duration}};
use super::middleware::clientaddr::ClientAddr;
use crate::{
    app::{AppState},
    call::{
        active_call::{ CallParams}, ActiveCall, ActiveCallType, Command
    },
    event::SessionEvent, media::track::TrackConfig,
};
use axum::{extract::{ ws::Message, Query, State, WebSocketUpgrade}, response::{IntoResponse, Response}, routing::get, Json, Router
};
use bytes::Bytes;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use tokio::{join, select};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

pub fn router(app_state: AppState) -> Router<AppState> {
    Router::new()
        .route("/call", get(ws_handler))
        .route("/call/webrtc", get(webrtc_handler))
        .route("/call/sip", get(sip_handler))
        .nest("/llm/v1", super::llmproxy::router())
        .route("/iceservers", get(super::webrtc::get_iceservers))
        .route("/health", get(super::ami::health_handler))
        .nest("/ami/v1", super::ami::router(app_state))

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
    let ping_interval = params.ping_interval.unwrap_or(20);
    let useragent = match app_state.useragent.clone() {
        Some(ua) => ua,
        None => {
                return Json(serde_json::json!({
                    "error": "User agent not initialized",
                    "message": "User agent must be initialized in app state for WebSocket calls"
                })).into_response();
        }
    };

    let resp = ws.on_upgrade(move |socket| async move {
        let (mut ws_sender, mut ws_receiver) = socket.split();
        let (audio_sender, audio_receiver) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
        
        let cancel_token = CancellationToken::new();
        let track_config = TrackConfig::default();
        let active_call = Arc::new(ActiveCall::new(
            call_type.clone(),
            cancel_token.clone(),
            session_id.clone(),
            useragent.invitation.clone(),
            app_state.clone(),
            track_config,
            Some(audio_receiver),
            dump_events,
            None, // No extra data for now
        ));
        
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
                        if let Err(_) = active_call.enqueue_command(command).await {
                            break
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

        let mut event_receiver = active_call.event_sender.subscribe();
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
                    SessionEvent::Ping { timestamp, payload }=>{
                        let payload = payload.unwrap_or_else(|| timestamp.to_string());
                        if let Err(_) =ws_sender.send(Message::Ping(payload.into())).await {
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

        let send_ping_loop = async {
            if ping_interval == 0 {
                active_call.cancel_token.cancelled().await;
                return;
            }
            let mut ticker = tokio::time::interval(Duration::from_secs(ping_interval.into()));
            loop {
                ticker.tick().await;
                let payload = Utc::now().to_rfc3339();
                let event = SessionEvent::Ping { timestamp: crate::get_timestamp(), payload:Some(payload)};
                if let Err(_) = active_call.event_sender.send(event) {
                    break;
                }
            }
        };

        let active_calls = {
            let mut calls = app_state.active_calls.lock().await;
            calls.insert(session_id.clone(), active_call.clone());
            calls.len()
        };
        info!(session_id, %client_ip, active_calls, ?call_type,"new call started");

        let (r,_) = join!{
            active_call.serve(),
            async {
                select!{
                    _ = send_ping_loop => {},
                    _ = cancel_token.cancelled() => {},
                    _ = send_to_ws_loop => { cancel_token.cancel() },
                    _ = recv_from_ws_loop => {
                        info!(session_id, %client_ip, "WebSocket closed by client");
                        cancel_token.cancel()
                    },
                }
            }, 
        };

        match r {
            Ok(_) => info!(session_id, %client_ip, "call ended successfully"),
            Err(e) => warn!(session_id, %client_ip, "call ended with error: {}", e),
        }

        app_state.active_calls.lock().await.remove(&session_id);

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
        ws_sender.flush().await.ok();
        ws_sender.close().await.ok();
        debug!(session_id, %client_ip, "WebSocket connection closed");
    });
    resp
}
