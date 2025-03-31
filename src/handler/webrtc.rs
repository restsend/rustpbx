use crate::handler::{
    call::{ActiveCall, CallHandlerState},
    Command,
};
use anyhow::Result;
use axum::{
    extract::{ws::Message, Query, State, WebSocketUpgrade},
    response::Response,
    Form,
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument};
use uuid::Uuid;

pub async fn webrtc_handler(
    ws: WebSocketUpgrade,
    State(state): State<CallHandlerState>,
    Query(id): Query<Option<String>>,
) -> Response {
    let session_id = id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let state_clone = state.clone();

    ws.on_upgrade(|socket| async move {
        match handle_webrtc_connection(session_id.clone(), socket, state).await {
            Ok(_) => (),
            Err(e) => {
                error!("Error handling WebRTC connection: {}", e);
            }
        }
        let mut active_calls = state_clone.active_calls.lock().await;
        active_calls.remove(&session_id);
    })
}

#[instrument(name = "handle_webrtc", skip(socket, state))]
pub async fn handle_webrtc_connection(
    session_id: String,
    socket: axum::extract::ws::WebSocket,
    state: CallHandlerState,
) -> Result<()> {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let cancel_token = CancellationToken::new();

    let active_call = match ws_receiver.next().await {
        Some(Ok(Message::Text(text))) => {
            let command = serde_json::from_str::<Command>(&text);
            match command {
                Ok(Command::Invite { options }) => {
                    let options = options.unwrap_or_default();
                    let track_id = format!("webrtc-{}", session_id);
                    let call = ActiveCall::new_webrtc(
                        &state,
                        cancel_token.child_token(),
                        track_id,
                        session_id,
                        options,
                    )
                    .await?;
                    Arc::new(call)
                }
                _ => {
                    error!("Invalid command: {}", text);
                    return Err(anyhow::anyhow!("The first message must be an invite"));
                }
            }
        }
        _ => {
            return Err(anyhow::anyhow!("Invalid message"));
        }
    };

    let active_calls_len = {
        let mut active_calls = state.active_calls.lock().await;
        active_calls.insert(active_call.session_id.clone(), active_call.clone());
        active_calls.len()
    };

    info!(
        "New call: {} -> {:?}, {} active calls",
        active_call.session_id, active_call.call_type, active_calls_len
    );
    let mut event_receiver = active_call.media_stream.subscribe();

    let send_to_ws = async move {
        while let Ok(event) = event_receiver.recv().await {
            let data = match serde_json::to_string(&event) {
                Ok(data) => data,
                Err(e) => {
                    error!("Error serializing event: {} {:?}", e, event);
                    continue;
                }
            };
            if let Err(e) = ws_sender.send(data.into()).await {
                error!("Error sending event to WebSocket: {}", e);
            }
        }
    };

    let recv_from_ws = async move {
        while let Some(msg) = ws_receiver.next().await {
            let command = match msg {
                Ok(Message::Text(text)) => match serde_json::from_str::<Command>(&text) {
                    Ok(command) => Some(command),
                    Err(e) => {
                        error!("Error deserializing command: {} {}", e, text);
                        None
                    }
                },
                _ => None,
            };

            match command {
                Some(command) => match active_call.dispatch(command).await {
                    Ok(_) => (),
                    Err(e) => {
                        error!("Error dispatching command: {}", e);
                    }
                },
                None => {}
            }
        }
    };

    select! {
        _ = cancel_token.cancelled() => {
            info!("active_call: Cancelled");
        },
        _ = send_to_ws => {
            info!("send_to_ws: Websocket disconnected");
        },
        _ = recv_from_ws => {
            info!("recv_from_ws: Websocket disconnected");
        },
    }
    Ok(())
}
