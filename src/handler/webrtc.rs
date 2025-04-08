use super::call::CallParams;
use crate::handler::{
    call::{ActiveCall, CallHandlerState},
    Command,
};
use anyhow::Result;
use axum::{
    extract::{ws::Message, Query, State, WebSocketUpgrade},
    response::Response,
};
use futures::{SinkExt, StreamExt};
use std::{sync::Arc, time::Instant};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument};
use uuid::Uuid;

pub async fn webrtc_handler(
    ws: WebSocketUpgrade,
    State(state): State<CallHandlerState>,
    Query(params): Query<CallParams>,
) -> Response {
    let session_id = params.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let state_clone = state.clone();
    ws.on_upgrade(|socket| async move {
        info!("webrtc call: {session_id}");
        let start_time = Instant::now();
        match handle_webrtc_connection(session_id.clone(), socket, state).await {
            Ok(_) => (),
            Err(e) => {
                error!("Error handling WebRTC connection: {}", e);
            }
        }
        let mut active_calls = state_clone.active_calls.lock().await;
        active_calls.remove(&session_id);
        info!(
            "webrtc call: hangup, duration {}s",
            start_time.elapsed().as_secs_f32()
        );
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
    let event_sender = crate::event::create_event_sender();
    let mut event_receiver = event_sender.subscribe();

    let active_call = match ws_receiver.next().await {
        Some(Ok(Message::Text(text))) => {
            debug!("webrtc call: received text message: {}", text);
            let command = serde_json::from_str::<Command>(&text);
            match command {
                Ok(Command::Invite { options }) => {
                    let track_id = session_id.clone();
                    let call = ActiveCall::new_webrtc(
                        &state,
                        cancel_token.child_token(),
                        event_sender,
                        track_id,
                        session_id,
                        options,
                    )
                    .await?;
                    Arc::new(call)
                }
                Ok(cmd) => {
                    error!("webrtc call: invalid first command: {:?}", cmd);
                    return Err(anyhow::anyhow!("The first message must be an invite"));
                }
                Err(e) => {
                    error!(
                        "webrtc call: error parsing command: {} from text: {}",
                        e, text
                    );
                    return Err(anyhow::anyhow!("Error parsing command: {}", e));
                }
            }
        }
        Some(Ok(msg)) => {
            error!("webrtc call: invalid message type: {:?}", msg);
            return Err(anyhow::anyhow!("Invalid message type"));
        }
        Some(Err(e)) => {
            error!("webrtc call: webSocket error: {}", e);
            return Err(anyhow::anyhow!("WebSocket error: {}", e));
        }
        None => {
            error!("webrtc call: webSocket closed");
            return Err(anyhow::anyhow!("WebSocket closed"));
        }
    };

    let active_call_clone = active_call.clone();
    let active_calls_len = {
        let mut active_calls = state.active_calls.lock().await;
        active_calls.insert(active_call.session_id.clone(), active_call.clone());
        active_calls.len()
    };

    info!(
        "webrtc call: new call: {} -> {:?}, {} active calls",
        active_call.session_id, active_call.call_type, active_calls_len
    );

    let send_to_ws = async move {
        while let Ok(event) = event_receiver.recv().await {
            let data = match serde_json::to_string(&event) {
                Ok(data) => data,
                Err(e) => {
                    error!("webrtc call: error serializing event: {} {:?}", e, event);
                    continue;
                }
            };
            if let Err(e) = ws_sender.send(data.into()).await {
                error!("webrtc call: error sending event to WebSocket: {}", e);
            }
        }
    };

    let recv_from_ws = async move {
        while let Some(msg) = ws_receiver.next().await {
            let command = match msg {
                Ok(Message::Text(text)) => match serde_json::from_str::<Command>(&text) {
                    Ok(command) => Some(command),
                    Err(e) => {
                        error!("webrtc call: error deserializing command: {} {}", e, text);
                        None
                    }
                },
                _ => None,
            };

            match command {
                Some(command) => match active_call.dispatch(command).await {
                    Ok(_) => (),
                    Err(e) => {
                        error!("webrtc call: Error dispatching command: {}", e);
                    }
                },
                None => {}
            }
        }
    };
    select! {
        _ = cancel_token.cancelled() => {
            info!("webrtc call: cancelled");
        },
        _ = send_to_ws => {
            info!("send_to_ws: websocket disconnected");
        },
        _ = recv_from_ws => {
            info!("recv_from_ws: websocket disconnected");
        },
        r = active_call_clone.process_stream() => {
            info!("webrtc call: call loop disconnected {:?}", r);
        },
    }
    Ok(())
}
