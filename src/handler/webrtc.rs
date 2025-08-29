use super::middleware::clientaddr::ClientAddr;
use crate::app::AppState;
use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use reqwest;
use std::time::Instant;
use tracing::{info, warn};
use webrtc::ice_transport::ice_server::RTCIceServer;

pub(crate) async fn get_iceservers(
    client_ip: ClientAddr,
    State(state): State<AppState>,
) -> Response {
    let default_ice_servers = state.config.ice_servers.as_ref();
    if let Some(ice_servers) = default_ice_servers {
        return Json(ice_servers).into_response();
    }
    if state.config.restsend_token.is_none() {
        return Json(vec![RTCIceServer {
            urls: vec!["stun:restsend.com:3478".to_string()],
            ..Default::default()
        }])
        .into_response();
    }
    let rs_token = state.config.restsend_token.as_deref().unwrap_or_else(|| "");
    let start_time = Instant::now();
    let user_id = ""; // TODO: Get user ID from state if needed
    let timeout = std::time::Duration::from_secs(5);
    let url = format!(
        "https://restsend.com/api/iceservers?token={}&user={}&client={}",
        rs_token,
        user_id,
        client_ip.ip().to_string()
    );

    // Create a reqwest client with proper timeout
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(e) => {
            warn!("failed to create HTTP client: {}", e);
            return Json(default_ice_servers).into_response();
        }
    };

    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(e) => {
            warn!("alloc ice servers failed: {}", e);
            return Json(default_ice_servers).into_response();
        }
    };

    if !response.status().is_success() {
        warn!(
            "ice servers request failed with status: {}",
            response.status()
        );
        return Json(default_ice_servers).into_response();
    }

    // Parse the response JSON
    match response.json::<Vec<RTCIceServer>>().await {
        Ok(ice_servers) => {
            info!(
                "get ice servers - duration: {:?}, count: {}, userId: {}, clientIP: {}",
                start_time.elapsed(),
                ice_servers.len(),
                user_id,
                client_ip
            );
            Json(ice_servers).into_response()
        }
        Err(e) => {
            warn!("decode ice servers failed: {}", e);
            Json(default_ice_servers).into_response()
        }
    }
}
