use super::middleware::clientaddr::ClientAddr;
use crate::app::AppState;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use reqwest;
use serde::{Deserialize, Serialize};
use std::{env, time::Instant};
use tracing::{error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<String>,
}

pub(crate) async fn get_iceservers(
    client_ip: ClientAddr,
    State(state): State<AppState>,
) -> Response {
    let rs_token = env::var("RESTSEND_TOKEN").unwrap_or_default();
    let default_ice_servers = state.config.ice_servers.as_ref();
    if rs_token.is_empty() {
        if let Some(ice_servers) = default_ice_servers {
            return Json(ice_servers).into_response();
        }
        return Json(vec![IceServer {
            urls: vec!["stun:restsend.com:3478".to_string()],
            username: None,
            credential: None,
        }])
        .into_response();
    }

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
