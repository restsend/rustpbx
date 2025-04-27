use crate::app::AppState;
use anyhow::Result;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use reqwest::header;
use serde::{Deserialize, Serialize};
use std::{env, time::Instant};
use tracing::{error, info};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAIConfig {
    pub endpoint: String,
    pub api_key: String,
}

impl Default for OpenAIConfig {
    fn default() -> Self {
        Self {
            endpoint: env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            api_key: env::var("OPENAI_API_KEY").unwrap_or_default(),
        }
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/{*path}", post(proxy_handler))
}

async fn proxy_handler(State(state): State<AppState>, req: Request<Body>) -> Response {
    match forward_request(&state, req).await {
        Ok(response) => response,
        Err(e) => {
            error!("Error forwarding request: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to proxy request: {}", e),
            )
                .into_response()
        }
    }
}

async fn forward_request(state: &AppState, req: Request<Body>) -> Result<Response> {
    // Get configuration from environment variables
    let config = OpenAIConfig::default();

    // Extract path
    let path = req.uri().path();
    let path_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(path);

    let endpoint = state
        .config
        .llmproxy
        .clone()
        .unwrap_or_else(|| config.endpoint.clone());

    let forward_url = format!("{}{}", endpoint, path_query);
    let start_time = Instant::now();
    info!("Forwarding request to: {}", forward_url);

    let headers = req.headers().clone();
    let client = reqwest::Client::new();

    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX).await?;
    let mut req_builder = client.post(&forward_url).body(body_bytes);

    for (name, value) in headers.iter() {
        if name != header::HOST {
            req_builder = req_builder.header(name, value);
        }
    }

    // Add API key if no Authorization header is present
    if !headers.contains_key(header::AUTHORIZATION) && !config.api_key.is_empty() {
        req_builder =
            req_builder.header(header::AUTHORIZATION, format!("Bearer {}", config.api_key));
    }

    let response = req_builder.send().await?;

    let status = response.status();
    let headers = response.headers().clone();
    let mut resp_builder = Response::builder().status(status);
    resp_builder = resp_builder.header("X-Accel-Buffering", "no");

    for (name, value) in headers.iter() {
        resp_builder = resp_builder.header(name, value);
    }
    info!(
        "llm_proxy: ttfb time: {:?} status: {}",
        start_time.elapsed(),
        status
    );
    // Create streaming body
    let stream = response.bytes_stream();
    let body = axum::body::Body::from_stream(stream);

    // Build complete response
    match resp_builder.body(body) {
        Ok(response) => Ok(response),
        Err(e) => Err(anyhow::anyhow!("Failed to build response: {}", e)),
    }
}
