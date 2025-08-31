use super::middleware::clientaddr::ClientAddr;
use crate::{app::AppState, config::IceServer};
use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use reqwest;
use std::time::Instant;
use tracing::{info, warn};

pub(crate) async fn get_iceservers(
    client_ip: ClientAddr,
    State(state): State<AppState>,
) -> Response {
    let default_ice_servers = state.config.ice_servers.as_ref();
    if let Some(ice_servers) = default_ice_servers {
        return Json(ice_servers).into_response();
    }
    let mut ice_servers = vec![IceServer {
        urls: vec!["stun:stun.l.google.com:19302".to_string()],
        ..Default::default()
    }];
    if state.config.restsend_token.is_none() {
        return Json(ice_servers).into_response();
    }
    let rs_token = state.config.restsend_token.as_deref().unwrap_or_else(|| "");
    let start_time = Instant::now();
    let user_id = ""; // TODO: Get user ID from state if needed
    let timeout = std::time::Duration::from_secs(5);
    let url = format!(
        "https://restsend.com/api/iceservers?token={}&user={}&client={}&turn_only=true",
        rs_token,
        user_id,
        client_ip.ip().to_string()
    );

    // Create a reqwest client with proper timeout
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(e) => {
            warn!(
                user_id,
                %client_ip,
                "failed to create HTTP client: {}", e);
            return Json(ice_servers).into_response();
        }
    };

    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(e) => {
            warn!(
                user_id,
                %client_ip,
                "alloc ice servers failed: {}", e
            );
            return Json(ice_servers).into_response();
        }
    };

    if !response.status().is_success() {
        warn!(
                user_id,
                %client_ip,
            "ice servers request failed with status: {}",
            response.status()
        );
        return Json(ice_servers).into_response();
    }
    let body = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!(
                user_id,
                %client_ip, "read ice servers response body failed: {}", e
            );
            return Json(ice_servers).into_response();
        }
    };
    match serde_json::from_slice::<Vec<IceServer>>(&body) {
        Ok(trun_servers) => {
            info!(
                user_id,
                %client_ip,
                "get ice servers - duration: {:?} len: {}",
                start_time.elapsed(),
                trun_servers.len());
            ice_servers.extend(trun_servers);
            Json(ice_servers).into_response()
        }
        Err(e) => {
            warn!(
                user_id,
                %client_ip,
                body = String::from_utf8_lossy(&body).to_string(),
                "decode ice servers failed: {}",
                e,
            );
            Json(ice_servers).into_response()
        }
    }
}
