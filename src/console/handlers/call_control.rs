use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::proxy_call::sip_session::SessionSnapshot;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

const DEFAULT_ACTIVE_CALL_LIMIT: usize = 50;
const MAX_ACTIVE_CALL_LIMIT: usize = 500;
#[cfg(feature = "commerce")]
const CLUSTER_FORWARD_TIMEOUT_SECS: u64 = 10;

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/calls/active", get(list_active_calls))
        .route("/calls/active/{session_id}", get(show_active_call))
        .route(
            "/calls/active/{session_id}/commands",
            post(dispatch_call_command),
        )
}

pub fn api_urls() -> Router<Arc<ConsoleState>> {
    urls()
}

#[derive(Default, Deserialize)]
pub struct ActiveCallListQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CallCommandPayload {
    Hangup {
        reason: Option<String>,
        code: Option<u16>,
        initiator: Option<String>,
    },
    #[serde(alias = "accept")]
    Accept {
        callee: Option<String>,
        sdp: Option<String>,
    },
    Transfer {
        target: String,
        #[serde(default)]
        attended: Option<bool>,
    },
    Hold {
        #[serde(default)]
        leg_id: Option<String>,
    },
    Unhold {
        #[serde(default)]
        leg_id: Option<String>,
    },
    SendDtmf {
        digits: String,
        #[serde(default)]
        leg_id: Option<String>,
    },
    Mute {
        track_id: String,
    },
    Unmute {
        track_id: String,
    },
    Play {
        source: ConsoleMediaSource,
        #[serde(default)]
        leg_id: Option<String>,
        #[serde(default)]
        interrupt_on_dtmf: bool,
        #[serde(default)]
        loop_playback: bool,
    },
    StopPlayback {
        #[serde(default)]
        leg_id: Option<String>,
    },
    StartRecording {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        format: Option<String>,
    },
    StopRecording,
    PauseRecording,
    ResumeRecording,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConsoleMediaSource {
    File { path: String },
    Url { url: String },
    Silence,
    Tone { frequency: u32, duration_ms: u32 },
}

pub async fn list_active_calls(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Query(query): Query<ActiveCallListQuery>,
) -> Response {
    list_active_calls_inner(&state, &query).await
}

pub async fn show_active_call(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    AxumPath(session_id): AxumPath<String>,
) -> Response {
    show_active_call_inner(&state, &session_id).await
}

pub async fn dispatch_call_command(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    AxumPath(session_id): AxumPath<String>,
    Json(payload): Json<CallCommandPayload>,
) -> Response {
    dispatch_call_command_inner(&state, &session_id, payload).await
}

pub async fn list_active_calls_inner(
    state: &Arc<ConsoleState>,
    query: &ActiveCallListQuery,
) -> Response {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_ACTIVE_CALL_LIMIT)
        .clamp(1, MAX_ACTIVE_CALL_LIMIT);

    let mut all_entries: Vec<serde_json::Value> = Vec::new();

    if let Some(server) = state.sip_server() {
        let registry = server.active_call_registry.clone();
        let entries = registry.list_recent(limit);
        for entry in entries {
            let session_id = entry.session_id.clone();
            all_entries.push(json!({
                "node": "local",
                "meta": entry,
                "state": snapshot_for(&registry, &session_id),
            }));
        }
    }

    #[cfg(feature = "commerce")]
    {
        let peer_entries = fetch_peer_calls(state, limit).await;
        all_entries.extend(peer_entries);
    }

    all_entries.sort_by(|a, b| {
        let ts_a = a
            .get("meta")
            .and_then(|m| m.get("started_at"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ts_b = b
            .get("meta")
            .and_then(|m| m.get("started_at"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        ts_b.cmp(ts_a)
    });
    all_entries.truncate(limit);

    Json(json!({ "data": all_entries })).into_response()
}

pub async fn show_active_call_inner(state: &Arc<ConsoleState>, session_id: &str) -> Response {
    if let Some(server) = state.sip_server() {
        let registry = server.active_call_registry.clone();

        if let Some(handle) = registry.get_handle(session_id) {
            return Json(json!({ "data": json!({
                "meta": registry.get(session_id),
                "state": handle.snapshot(),
            }) }))
            .into_response();
        }
    }

    #[cfg(feature = "commerce")]
    {
        if let Some(resp) = query_session_from_peers(state, session_id).await {
            return resp;
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({ "message": "Call not found" })),
    )
        .into_response()
}

pub async fn dispatch_call_command_inner(
    state: &Arc<ConsoleState>,
    session_id: &str,
    payload: CallCommandPayload,
) -> Response {
    if let Some(server) = state.sip_server() {
        let registry = server.active_call_registry.clone();

        if registry.get_handle(session_id).is_some() {
            use crate::call::runtime::dispatch_console_command;

            return match dispatch_console_command(&registry, session_id, payload) {
                Ok(result) => {
                    if result.success {
                        let mut resp = json!({ "message": "Command dispatched" });
                        if let Some(data) = result.data {
                            resp.as_object_mut().unwrap().insert("data".into(), data);
                        }
                        Json(resp).into_response()
                    } else {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "message": result.message })),
                        )
                            .into_response()
                    }
                }
                Err(e) => (
                    StatusCode::CONFLICT,
                    Json(json!({ "message": format!("Failed to deliver command: {}", e) })),
                )
                    .into_response(),
            };
        }
    }

    #[cfg(feature = "commerce")]
    {
        let resp = forward_command_to_peers(state, session_id, &payload).await;
        if resp.is_some() {
            return resp.unwrap();
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({ "message": "Call not found" })),
    )
        .into_response()
}

fn snapshot_for(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: &str,
) -> Option<SessionSnapshot> {
    registry
        .get_handle(session_id)
        .and_then(|handle| handle.snapshot())
}

#[cfg(feature = "commerce")]
fn get_cluster_peers(state: &ConsoleState) -> Option<Vec<crate::config::ClusterPeer>> {
    state
        .app_state()
        .and_then(|app| app.config().cluster.as_ref().map(|c| c.peers.clone()))
}

#[cfg(feature = "commerce")]
fn get_ami_path(state: &ConsoleState) -> String {
    state
        .app_state()
        .map(|app| {
            app.config()
                .proxy
                .ami_path
                .clone()
                .unwrap_or_else(|| "/ami/v1".to_string())
        })
        .unwrap_or_else(|| "/ami/v1".to_string())
}

#[cfg(feature = "commerce")]
async fn forward_command_to_peers(
    state: &ConsoleState,
    session_id: &str,
    payload: &CallCommandPayload,
) -> Option<Response> {
    let peers = get_cluster_peers(state)?;
    if peers.is_empty() {
        return None;
    }

    let ami_path = get_ami_path(state);
    let body = json!({
        "session_id": session_id,
        "payload": payload,
    });

    let client = reqwest::Client::new();
    let mut handles: Vec<tokio::task::JoinHandle<Option<Response>>> = Vec::new();

    for peer in &peers {
        let url = format!(
            "http://{}:{}{}/cluster/dispatch_command",
            peer.addr, peer.ami_port, ami_path
        );
        let client = client.clone();
        let body = body.clone();

        handles.push(tokio::spawn(async move {
            let opts = crate::http_util::HttpFetchOptions::new()
                .with_timeout(std::time::Duration::from_secs(CLUSTER_FORWARD_TIMEOUT_SECS));
            let req = client.post(&url).json(&body);
            match crate::http_util::execute_request(req, &opts.headers, opts.timeout).await {
                Ok(resp) => {
                    let status = resp.status();
                    if status == StatusCode::NOT_FOUND {
                        return None;
                    }
                    let body_text = resp.text().await.ok()?;
                    Some((status, Json(body_text)).into_response())
                }
                Err(_) => None,
            }
        }));
    }

    for handle in handles {
        match handle.await {
            Ok(Some(resp)) => return Some(resp),
            Ok(None) => continue,
            Err(_) => continue,
        }
    }

    None
}

#[cfg(feature = "commerce")]
async fn query_session_from_peers(state: &ConsoleState, session_id: &str) -> Option<Response> {
    let peers = get_cluster_peers(state)?;
    if peers.is_empty() {
        return None;
    }

    let ami_path = get_ami_path(state);
    let client = reqwest::Client::new();
    let mut handles: Vec<tokio::task::JoinHandle<Option<Response>>> = Vec::new();

    for peer in &peers {
        let url = format!(
            "http://{}:{}{}/cluster/show_session/{}",
            peer.addr, peer.ami_port, ami_path, session_id
        );
        let client = client.clone();

        handles.push(tokio::spawn(async move {
            let opts = crate::http_util::HttpFetchOptions::new()
                .with_timeout(std::time::Duration::from_secs(CLUSTER_FORWARD_TIMEOUT_SECS));
            let req = client.get(&url);
            match crate::http_util::execute_request(req, &opts.headers, opts.timeout).await {
                Ok(resp) => {
                    let status = resp.status();
                    if status == StatusCode::NOT_FOUND {
                        return None;
                    }
                    let body_text = resp.text().await.ok()?;
                    Some((status, Json(body_text)).into_response())
                }
                Err(_) => None,
            }
        }));
    }

    for handle in handles {
        match handle.await {
            Ok(Some(resp)) => return Some(resp),
            Ok(None) => continue,
            Err(_) => continue,
        }
    }

    None
}

#[cfg(feature = "commerce")]
async fn fetch_peer_calls(state: &ConsoleState, limit: usize) -> Vec<serde_json::Value> {
    let peers = match get_cluster_peers(state) {
        Some(p) if !p.is_empty() => p,
        _ => return Vec::new(),
    };

    let ami_path = get_ami_path(state);
    let client = reqwest::Client::new();
    let mut handles: Vec<tokio::task::JoinHandle<Vec<serde_json::Value>>> = Vec::new();

    for peer in &peers {
        let url = format!(
            "http://{}:{}{}/cluster/list_calls?limit={}",
            peer.addr, peer.ami_port, ami_path, limit
        );
        let client = client.clone();
        let peer_label = format!("{}:{}", peer.addr, peer.sip_port);

        handles.push(tokio::spawn(async move {
            let opts = crate::http_util::HttpFetchOptions::new()
                .with_timeout(std::time::Duration::from_secs(CLUSTER_FORWARD_TIMEOUT_SECS));
            let req = client.get(&url);
            match crate::http_util::execute_request(req, &opts.headers, opts.timeout).await {
                Ok(resp) => {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
                            return data
                                .iter()
                                .map(|item| {
                                    let mut item = item.clone();
                                    if let Some(obj) = item.as_object_mut() {
                                        obj.insert(
                                            "node".to_string(),
                                            serde_json::Value::String(peer_label.clone()),
                                        );
                                    }
                                    item
                                })
                                .collect();
                        }
                    }
                    Vec::new()
                }
                Err(_) => Vec::new(),
            }
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        if let Ok(entries) = handle.await {
            results.extend(entries);
        }
    }
    results
}
