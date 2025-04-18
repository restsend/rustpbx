use super::{
    call::{ActiveCallType, CallParams},
    middleware::clientip::ClientIp,
};
use crate::{
    app::AppState,
    handler::call::{handle_call, CallHandlerState},
};
use axum::{
    extract::{Query, State, WebSocketUpgrade},
    response::{IntoResponse, Response},
    Json,
};
use reqwest;
use serde::{Deserialize, Serialize};
use std::{env, time::Instant};
use tracing::{error, info};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<String>,
}

pub(crate) async fn get_iceservers(client_ip: ClientIp) -> Response {
    let rs_token = env::var("RESTSEND_TOKEN").unwrap_or_default();
    let default_stun_server =
        env::var("STUN_SERVER").unwrap_or_else(|_| "stun:restsend.com:3478".to_string());

    let default_ice_servers = vec![IceServer {
        urls: vec![default_stun_server.clone()],
        username: None,
        credential: None,
    }];

    if rs_token.is_empty() {
        return Json(default_ice_servers).into_response();
    }

    let start_time = Instant::now();
    let user_id = ""; // TODO: Get user ID from state if needed
    let timeout = std::time::Duration::from_secs(5);
    let url = format!(
        "https://restsend.com/api/iceservers?token={}&user={}&client={}",
        rs_token, user_id, client_ip
    );

    // Create a reqwest client with proper timeout
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(e) => {
            error!("voiceserver: failed to create HTTP client: {}", e);
            return Json(default_ice_servers).into_response();
        }
    };

    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(e) => {
            error!("voiceserver: alloc ice servers failed: {}", e);
            return Json(default_ice_servers).into_response();
        }
    };

    if !response.status().is_success() {
        error!(
            "voiceserver: ice servers request failed with status: {}",
            response.status()
        );
        return Json(default_ice_servers).into_response();
    }

    // Parse the response JSON
    match response.json::<Vec<IceServer>>().await {
        Ok(ice_servers) => {
            info!(
                "voiceserver: get ice servers - duration: {:?}, count: {}, userId: {}, clientIP: {}",
                start_time.elapsed(),
                ice_servers.len(),
                user_id,
                client_ip
            );
            Json(ice_servers).into_response()
        }
        Err(e) => {
            error!("voiceserver: decode ice servers failed: {}", e);
            Json(default_ice_servers).into_response()
        }
    }
}

pub async fn webrtc_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    let session_id = params.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let state_clone = state.clone();
    ws.on_upgrade(|socket| async move {
        info!("webrtc call: {session_id}");
        let start_time = Instant::now();
        match handle_call(ActiveCallType::Webrtc, session_id.clone(), socket, state).await {
            Ok(_) => (),
            Err(e) => {
                error!("Error handling WebRTC connection: {}", e);
            }
        }
        let mut active_calls = state_clone.active_calls.lock().await;
        match active_calls.remove(&session_id) {
            Some(call) => {
                info!(
                    "webrtc call: hangup, duration {}s",
                    start_time.elapsed().as_secs_f32()
                );
                call.cancel_token.cancel();
            }
            None => {
                error!("webrtc call: call not found");
            }
        }
    })
}
