use crate::app::AppState;
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::time::{Instant, SystemTime};
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

pub async fn call_handler(
    client_ip: ClientIp,
    call_type: ActiveCallType,
    ws: WebSocketUpgrade,
    state: AppState,
    params: CallParams,
) -> Response {
    let session_id = params.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let state_clone = state.clone();
    let call_type_clone = call_type.clone();
    let session_id_clone = session_id.clone();

    ws.on_upgrade(|socket| async move {
        let start_time = Instant::now();
        let call_start_time = SystemTime::now();

        match handle_call(call_type.clone(), session_id.clone(), socket, state.clone()).await {
            Ok(_) => (),
            Err(e) => {
                error!("Error handling connection {client_ip}: {}", e);
            }
        }

        let call_end_time = SystemTime::now();
        let duration = start_time.elapsed().as_secs();

        // Get call information after call ends
        let (option, recorder_files, caller, callee, hangup_reason) = {
            let mut active_calls = state_clone.active_calls.lock().await;
            match active_calls.remove(&session_id) {
                Some(call) => {
                    info!(
                        "{client_ip} call end, duration {}s",
                        start_time.elapsed().as_secs_f32()
                    );
                    call.cancel_token.cancel();

                    // Get caller and callee from call option
                    let caller = call
                        .option
                        .caller
                        .clone()
                        .unwrap_or_else(|| client_ip.to_string());
                    let callee = call
                        .option
                        .callee
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());

                    // Get hangup reason
                    let hangup_reason = call
                        .hangup_reason
                        .lock()
                        .await
                        .clone()
                        .unwrap_or(crate::callrecord::CallRecordHangupReason::BySystem);

                    // Get recorder files if any
                    let recorder_files = if call.option.recorder.is_some() {
                        let recorder_file = state_clone.get_recorder_file(&session_id);
                        if std::path::Path::new(&recorder_file).exists() {
                            let file_size = std::fs::metadata(&recorder_file)
                                .map(|m| m.len())
                                .unwrap_or(0);
                            vec![crate::callrecord::CallRecordMedia {
                                track_id: session_id.clone(),
                                r#type: "audio/wav".to_string(),
                                path: recorder_file,
                                size: file_size,
                                duration: duration * 1000, // Convert to milliseconds
                                start_time: call_start_time,
                                end_time: call_end_time,
                                extra: std::collections::HashMap::new(),
                            }]
                        } else {
                            vec![]
                        }
                    } else {
                        vec![]
                    };

                    (
                        call.option.clone(),
                        recorder_files,
                        caller,
                        callee,
                        hangup_reason,
                    )
                }
                None => {
                    info!(
                        "{client_ip} call end (not found in active calls), duration {}s",
                        start_time.elapsed().as_secs_f32()
                    );
                    (
                        Default::default(),
                        vec![],
                        client_ip.to_string(),
                        "unknown".to_string(),
                        crate::callrecord::CallRecordHangupReason::BySystem,
                    )
                }
            }
        };

        // Create and send call record
        let call_record = crate::callrecord::CallRecord {
            call_type: call_type_clone,
            option,
            call_id: session_id_clone.clone(),
            start_time: call_start_time,
            end_time: call_end_time,
            duration,
            caller,
            callee,
            status_code: 200, // Default to success, could be enhanced to track actual status
            hangup_reason,
            recorder: recorder_files,
            extras: std::collections::HashMap::new(),
        };

        // Send call record to manager
        if let Err(e) = state_clone.callrecord.sender.send(call_record) {
            error!("Failed to send call record: {}", e);
        } else {
            info!("Call record sent for session: {}", session_id_clone);
        }
    })
}
