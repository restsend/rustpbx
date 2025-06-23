use super::{
    call::{handle_call, ActiveCallState, ActiveCallType, CallParams},
    middleware::clientaddr::ClientAddr,
};
use crate::{app::AppState, callrecord::CallRecord};
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use std::sync::{Arc, RwLock};
use tracing::{error, info};
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
}

async fn list_calls(State(state): State<AppState>) -> Response {
    let calls = serde_json::json!({
        "calls": state.active_calls.lock().await.iter().map(|(id, call)| {
            serde_json::json!({
                "id": id,
                "call_type": call.call_type,
                "created_at": call.call_state.read().unwrap().created_at.to_rfc3339(),
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
    state: AppState,
    params: CallParams,
) -> Response {
    let session_id = params.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let state_clone = state.clone();
    let dump_events = params.dump_events.unwrap_or(true);

    ws.on_upgrade(move |socket| async move {
        let start_time = Utc::now();
        let call_state = Arc::new(RwLock::new(ActiveCallState {
            created_at: Utc::now(),
            ring_time: None,
            answer_time: None,
            hangup_reason: None,
            last_status_code: 0,
            answer: None,
            option: None,
        }));
        match handle_call(
            call_type.clone(),
            session_id.clone(),
            socket,
            state.clone(),
            call_state.clone(),
            dump_events,
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
                let dump_events_file = state_clone.get_dump_events_file(&session_id);
                let dump_events = if tokio::fs::metadata(&dump_events_file).await.is_ok() {
                    Some(dump_events_file)
                } else {
                    None
                };

                let call_state = call_state.read().unwrap();
                let option = call_state.option.clone();
                let caller = option.as_ref().map(|o| o.caller.clone()).flatten();
                let callee = option.as_ref().map(|o| o.callee.clone()).flatten();
                let hangup_reason = call_state.hangup_reason.clone();
                let offer = option.as_ref().map(|o| o.offer.clone()).flatten();
                let answer = call_state.answer.clone();

                CallRecord {
                    call_type: call_type.clone(),
                    call_id: session_id.clone(),
                    start_time,
                    end_time: Utc::now(),
                    ring_time: call_state.ring_time.clone(),
                    answer_time: call_state.answer_time.clone(),
                    caller: caller.unwrap_or_else(|| "".to_string()),
                    callee: callee.unwrap_or_else(|| "".to_string()),
                    status_code: call_state.last_status_code,
                    offer,
                    answer,
                    hangup_reason,
                    recorder: vec![],
                    extras: None,
                    option,
                    dump_event_file: dump_events,
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
