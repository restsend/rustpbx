use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};

use crate::{app::AppState, callrecord::CallRecord};
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use tokio::sync::Mutex;
use tracing::{error, info};
use uuid::Uuid;

use super::{
    call::{handle_call, ActiveCallState, ActiveCallType, CallParams},
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
                "created_at": call.call_state.created_at.to_rfc3339(),
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

pub async fn call_handler(
    client_ip: ClientIp,
    call_type: ActiveCallType,
    ws: WebSocketUpgrade,
    state: AppState,
    params: CallParams,
) -> Response {
    let session_id = params.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let state_clone = state.clone();

    ws.on_upgrade(|socket| async move {
        let start_time = Utc::now();
        let call_state = Arc::new(ActiveCallState {
            created_at: Utc::now(),
            ring_time: Mutex::new(None),
            answer_time: Mutex::new(None),
            hangup_reason: Mutex::new(None),
            last_status_code: AtomicU16::new(0),
            option: Mutex::new(None),
        });

        match handle_call(
            call_type.clone(),
            session_id.clone(),
            socket,
            state.clone(),
            call_state.clone(),
        )
        .await
        {
            Ok(_) => (),
            Err(e) => {
                error!("Error handling connection {client_ip}: {}", e);
            }
        }

        let call_record = match state_clone.active_calls.lock().await.remove(&session_id) {
            Some(call) => {
                call.cancel_token.cancel();
                call.get_callrecord().await
            }
            _ => {
                let option = call_state.option.lock().await.clone();
                let caller = option.as_ref().map(|o| o.caller.clone()).flatten();
                let callee = option.as_ref().map(|o| o.callee.clone()).flatten();
                let hangup_reason = call_state.hangup_reason.lock().await.clone();
                CallRecord {
                    call_type: call_type.clone(),
                    call_id: session_id.clone(),
                    start_time,
                    end_time: Utc::now(),
                    ring_time: call_state.ring_time.lock().await.clone(),
                    answer_time: call_state.answer_time.lock().await.clone(),
                    caller: caller.unwrap_or_else(|| "".to_string()),
                    callee: callee.unwrap_or_else(|| "".to_string()),
                    status_code: call_state.last_status_code.load(Ordering::Relaxed),
                    hangup_reason,
                    recorder: vec![],
                    extras: None,
                    option,
                }
            }
        };
        info!(
            client_ip = client_ip.to_string(),
            session_id,
            hangup_reason = format!("{:?}", call_record.hangup_reason),
            "call end, duration {}s",
            Utc::now()
                .signed_duration_since(start_time)
                .as_seconds_f32()
        );
        if let Some(sender) = state_clone.callrecord_sender.lock().await.as_ref() {
            if let Err(e) = sender.send(call_record) {
                error!("Failed to send call record: {}", e);
            }
        }
    })
}
