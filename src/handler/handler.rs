use super::middleware::clientaddr::ClientAddr;
use crate::{
    app::AppState,
    call::{
        ActiveCallType, Command,
        active_call::{CallParams, handle_call},
    },
    event::SessionEvent,
};
use axum::{Router,
    extract::{ Query, State, WebSocketUpgrade, ws::Message},
    response::{ Response},
    routing::{get},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use tokio::{join, select};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
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
    call_handler(client_ip, ActiveCallType::B2bua, ws, state, params).await
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
        let invitation = app_state.useragent.invitation.clone();
        let handle_call_loop = async {
            match handle_call(
                cancel_token,
                call_type,
                session_id.clone(),
                dump_events,
                app_state.clone(),
                event_sender.clone(),
                Some(audio_receiver),
                dump_cmd_receiver,
                cmd_receiver,
                invitation,
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
                    if let Some(sender) = app_state.callrecord_sender.as_ref() {
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
                            if let Some(sender) = app_state.callrecord_sender.as_ref() {
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
        ws_sender.flush().await.ok();
        ws_sender.close().await.ok();
        debug!(session_id, %client_ip, "WebSocket connection closed");
    });
    resp
}
