use crate::console::ConsoleState;
use crate::proxy::presence::PresenceState;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Serialize;
use std::sync::Arc;

/// API routes (nested under api_prefix)
///
/// Read-only by design: a POST here used to write the SIP presence mirror
/// directly, bypassing the CC agent state machine and desyncing BLF/monitor
/// state until the next CC `project_presence` overwrote it. CC agent
/// presence must go through `POST /cc/agents/{agent_id}/status`.
pub fn api_urls() -> Router<Arc<ConsoleState>> {
    Router::new().route("/presence/{extension}", get(get_presence))
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
