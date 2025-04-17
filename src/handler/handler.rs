use super::call::CallHandlerState;
use crate::useragent::UserAgent;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;
use tracing::info;

pub fn router(useragent: Arc<UserAgent>) -> Router<CallHandlerState> {
    Router::new()
        .route("/call/webrtc", get(super::webrtc::webrtc_handler))
        .route("/call/sip", get(super::sip::sip_handler))
        .route("/call/lists", get(list_calls))
        .route("/call/kill/{id}", post(kill_call))
        .nest("/llm/v1", super::llmproxy::router())
        .route("/iceservers", get(super::webrtc::get_iceservers))
        .layer(axum::extract::Extension(useragent))
}

async fn list_calls(State(state): State<CallHandlerState>) -> Response {
    let calls = serde_json::json!({
        "calls": state.active_calls.lock().await.iter().map(|(id, call)| {
            serde_json::json!({
                "id": id,
                "call_type": call.call_type,
                "created_at": call.created_at.to_rfc3339(),
                "options": call.options,
            })
        }).collect::<Vec<_>>(),
    });
    Json(calls).into_response()
}

async fn kill_call(State(state): State<CallHandlerState>, Path(id): Path<String>) -> Response {
    if let Some(call) = state.active_calls.lock().await.remove(&id) {
        call.cancel_token.cancel();
        info!("Call {} killed", id);
    }
    Json(true).into_response()
}
