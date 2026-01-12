use crate::console::ConsoleState;
use crate::proxy::presence::{PresenceState, PresenceStatus};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new().route(
        "/api/presence/{extension}",
        get(get_presence).post(set_presence),
    )
}

#[derive(Serialize)]
pub struct PresenceResponse {
    pub success: bool,
    pub data: Option<PresenceState>,
}

pub async fn get_presence(
    State(state): State<Arc<ConsoleState>>,
    Path(extension): Path<String>,
) -> Json<PresenceResponse> {
    if let Some(sip_server) = state.get_sip_server() {
        let presence = sip_server.presence_manager.get_state(&extension);
        Json(PresenceResponse {
            success: true,
            data: Some(presence),
        })
    } else {
        Json(PresenceResponse {
            success: false,
            data: None,
        })
    }
}

#[derive(Deserialize)]
pub struct SetPresenceRequest {
    pub status: PresenceStatus,
    pub note: Option<String>,
    pub activity: Option<String>,
}

pub async fn set_presence(
    State(state): State<Arc<ConsoleState>>,
    Path(extension): Path<String>,
    Json(payload): Json<SetPresenceRequest>,
) -> Json<PresenceResponse> {
    if let Some(sip_server) = state.get_sip_server() {
        let new_state = PresenceState {
            status: payload.status,
            note: payload.note,
            activity: payload.activity,
            last_updated: chrono::Utc::now().timestamp(),
        };
        sip_server
            .presence_manager
            .update_state(&extension, new_state.clone())
            .await;
        Json(PresenceResponse {
            success: true,
            data: Some(new_state),
        })
    } else {
        Json(PresenceResponse {
            success: false,
            data: None,
        })
    }
}
