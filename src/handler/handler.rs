use crate::app::AppState;
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::time::Instant;
use tracing::{error, info};
use uuid::Uuid;

use super::{
    call::{handle_call, ActiveCallType, CallParams},
    middleware::clientip::ClientIp,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/call", get(ws_handler))
        .route("/call/webrtc", get(webrtc_handler))
        .route("/call/sip", get(sip_handler))
        .route("/call/lists", get(list_calls))
        .route("/call/kill/{id}", post(kill_call))
        .nest("/llm/v1", super::llmproxy::router())
        .route("/iceservers", get(super::webrtc::get_iceservers))
}

async fn list_calls(State(state): State<AppState>) -> Response {
    let calls = serde_json::json!({
        "calls": state.active_calls.lock().await.iter().map(|(id, call)| {
            serde_json::json!({
                "id": id,
                "call_type": call.call_type,
                "created_at": call.created_at.to_rfc3339(),
                "option": call.option,
            })
        }).collect::<Vec<_>>(),
    });
    Json(calls).into_response()
}

async fn kill_call(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if let Some(call) = state.active_calls.lock().await.remove(&id) {
        call.cancel_token.cancel();
        info!("Call {} killed", id);
    }
    Json(true).into_response()
}

pub async fn ws_handler(
    client_ip: ClientIp,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    call_handler(client_ip, ActiveCallType::WebSocket, ws, state, params).await
}

pub async fn sip_handler(
    client_ip: ClientIp,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    call_handler(client_ip, ActiveCallType::Sip, ws, state, params).await
}

pub async fn webrtc_handler(
    client_ip: ClientIp,
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    call_handler(client_ip, ActiveCallType::Webrtc, ws, state, params).await
}

async fn call_handler(
    client_ip: ClientIp,
    call_type: ActiveCallType,
    ws: WebSocketUpgrade,
    state: AppState,
    params: CallParams,
) -> Response {
    let session_id = params.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let state_clone = state.clone();
    ws.on_upgrade(|socket| async move {
        let start_time = Instant::now();
        match handle_call(call_type.clone(), session_id.clone(), socket, state).await {
            Ok(_) => (),
            Err(e) => {
                error!("Error handling connection {client_ip}: {}", e);
            }
        }
        let mut active_calls = state_clone.active_calls.lock().await;
        match active_calls.remove(&session_id) {
            Some(call) => {
                info!(
                    "{client_ip} call end, duration {}s",
                    start_time.elapsed().as_secs_f32()
                );
                call.cancel_token.cancel();
            }
            None => {}
        }
    })
}
