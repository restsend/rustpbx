use crate::{
    call::Location,
    console::{ConsoleState, handlers::bad_request, middleware::AuthRequired},
};
use axum::{
    Router,
    extract::{Json, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use rsip::Uri;
use rsipstack::dialog::{
    client_dialog::ClientInviteDialog,
    dialog::{Dialog, DialogState},
    server_dialog::ServerInviteDialog,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{borrow::Cow, sync::Arc};

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/diagnostics", get(page_diagnostics))
        .route("/diagnostics/dialogs", get(list_dialogs))
        .route("/diagnostics/locator/lookup", post(locator_lookup))
        .route("/diagnostics/locator/clear", post(locator_clear))
}

pub async fn page_diagnostics(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "console/diagnostics.html",
        json!({
            "nav_active": "diagnostics",
            "test_data": {
                "last_audit": "2025-10-10T02:40:00Z",
                "trunks": [
                    {
                        "id": "macrovox-primary",
                        "label": "Macrovox Primary",
                        "status": "healthy",
                        "latency_ms": 32,
                        "packet_loss_percent": 0.2,
                        "concurrency": { "current": 42, "limit": 80 },
                        "last_test_at": "2025-10-09T23:55:00Z",
                        "ingress_ips": ["203.0.113.14", "203.0.113.15"],
                        "egress": "sip:macrovox.sip.rustpbx.net",
                        "direction": ["outbound", "inbound"],
                        "notes": "Primary carrier with elastic CPS"
                    },
                    {
                        "id": "bluewave-backup",
                        "label": "BlueWave Backup",
                        "status": "warning",
                        "latency_ms": 58,
                        "packet_loss_percent": 1.4,
                        "concurrency": { "current": 12, "limit": 60 },
                        "last_test_at": "2025-10-10T00:12:00Z",
                        "ingress_ips": ["198.51.100.22"],
                        "egress": "sip:backup.bluewave.net",
                        "direction": ["outbound"],
                        "notes": "Burstable trunk used during peak hours"
                    },
                    {
                        "id": "internal-webrtc",
                        "label": "Internal WebRTC",
                        "status": "lab",
                        "latency_ms": 12,
                        "packet_loss_percent": 0.0,
                        "concurrency": { "current": 18, "limit": 120 },
                        "last_test_at": "2025-10-08T17:34:00Z",
                        "ingress_ips": ["10.10.10.0/24"],
                        "egress": "webrtc:internal.cluster.local",
                        "direction": ["internal"],
                        "notes": "Lab trunk for browser endpoints"
                    }
                ],
                "routing_checks": [
                    {
                        "id": "check-001",
                        "input": "+14155550123",
                        "direction": "outbound",
                        "matched_route": "US-Long-Distance",
                        "selected_trunk": "macrovox-primary",
                        "rewrites": ["strip_prefix:+1"],
                        "result": "ok",
                        "latency_ms": 35
                    },
                    {
                        "id": "check-002",
                        "input": "4008",
                        "direction": "internal",
                        "matched_route": "Support-IVR",
                        "selected_trunk": "internal-webrtc",
                        "rewrites": ["append_context:support"],
                        "result": "ok",
                        "latency_ms": 8
                    },
                    {
                        "id": "check-003",
                        "input": "+442079460000",
                        "direction": "outbound",
                        "matched_route": "EMEA-Offnet",
                        "selected_trunk": "bluewave-backup",
                        "rewrites": ["prefix:0044"],
                        "result": "warning",
                        "latency_ms": 72
                    }
                ],
                "recent_tests": [
                    {
                        "timestamp": "2025-10-10T02:10:00Z",
                        "type": "trunk",
                        "subject": "macrovox-primary",
                        "status": "pass",
                        "details": "OPTIONS ping ok · 34 ms round-trip"
                    },
                    {
                        "timestamp": "2025-10-09T23:50:00Z",
                        "type": "routing",
                        "subject": "+61370101234",
                        "status": "warning",
                        "details": "Fallback to BlueWave due to Macrovox concurrency cap"
                    },
                    {
                        "timestamp": "2025-10-09T21:15:00Z",
                        "type": "call",
                        "subject": "ext-201 → +14155559876",
                        "status": "pass",
                        "details": "31 s media loopback · MOS 4.3"
                    }
                ],
                "dialer": {
                    "default_source": "ext-101",
                    "source_options": ["ext-101", "ext-201", "qa-bot"],
                    "destination_samples": [
                        { "label": "San Francisco DID", "value": "+14155551212" },
                        { "label": "Support IVR", "value": "4008" },
                        { "label": "Echo test", "value": "*43" }
                    ],
                    "trunk_options": [
                        { "id": "macrovox-primary", "label": "Macrovox Primary" },
                        { "id": "bluewave-backup", "label": "BlueWave Backup" },
                        { "id": "internal-webrtc", "label": "Internal WebRTC" }
                    ]
                }
            }
        }),
    )
}

pub async fn list_dialogs(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Query(params): Query<DialogListQuery>,
) -> impl IntoResponse {
    let Some(server) = state.sip_server() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "SIP server unavailable" })),
        )
            .into_response();
    };

    let dialog_layer = server.dialog_layer.clone();
    const MAX_DIALOGS: usize = 20;
    let limit = params.limit.unwrap_or(MAX_DIALOGS).max(1).min(MAX_DIALOGS);
    let call_id_filter = params
        .call_id
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());

    let mut items = Vec::new();
    let mut has_more = false;

    for id in dialog_layer.all_dialog_ids() {
        if let Some(dialog) = dialog_layer.get_dialog(&id) {
            if let Some(summary) = summarize_dialog(&dialog) {
                if let Some(ref call_id) = call_id_filter {
                    if summary.call_id != *call_id {
                        continue;
                    }
                }

                if items.len() >= limit {
                    has_more = true;
                    break;
                }

                items.push(summary);
            }
        }
    }

    Json(DialogListResponse {
        generated_at: Utc::now().to_rfc3339(),
        total: items.len(),
        has_more,
        items,
    })
    .into_response()
}

async fn locator_lookup(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<LocatorPayload>,
) -> impl IntoResponse {
    if payload
        .user
        .as_ref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
        && payload
            .uri
            .as_ref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
    {
        return bad_request("user or uri is required");
    }

    let Some(server) = state.sip_server() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "SIP server unavailable" })),
        )
            .into_response();
    };
    let locator = server.locator.clone();

    let uri = match resolve_target_uri(&payload) {
        Ok(u) => u,
        Err(resp) => return resp,
    };

    let records = match locator.lookup(&uri).await {
        Ok(items) => items,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": format!("locator lookup failed: {}", err) })),
            )
                .into_response();
        }
    };

    let response = LocatorLookupResponse {
        generated_at: Utc::now().to_rfc3339(),
        total: records.len(),
        query: LocatorQueryEcho {
            user: payload.user.clone().filter(|s| !s.trim().is_empty()),
            uri: uri.to_string(),
        },
        records: records.into_iter().map(location_to_view).collect(),
    };

    Json(response).into_response()
}

async fn locator_clear(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<LocatorPayload>,
) -> impl IntoResponse {
    let Some(server) = state.sip_server() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "SIP server unavailable" })),
        )
            .into_response();
    };

    let locator = server.locator.clone();
    let uri = match resolve_target_uri(&payload) {
        Ok(u) => u,
        Err(resp) => return resp,
    };

    let Some((username, realm)) = extract_user_realm(&payload, &uri) else {
        return bad_request("user is required to clear registration");
    };

    let before = locator.lookup(&uri).await.unwrap_or_default();
    if let Err(err) = locator.unregister(&username, realm.as_deref()).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "message": format!("locator clear failed: {}", err) })),
        )
            .into_response();
    }
    let after = locator.lookup(&uri).await.unwrap_or_default();

    Json(LocatorClearResponse {
        removed: !before.is_empty(),
        remaining: after.len(),
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct LocatorPayload {
    user: Option<String>,
    uri: Option<String>,
}

#[derive(Serialize)]
struct DialogListResponse {
    generated_at: String,
    total: usize,
    has_more: bool,
    items: Vec<DialogSummary>,
}

#[derive(Default, Deserialize)]
pub struct DialogListQuery {
    call_id: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct DialogSummary {
    id: String,
    call_id: String,
    from_tag: String,
    to_tag: String,
    role: String,
    state: String,
    state_detail: Option<String>,
    from_display: String,
    to_display: String,
    remote_contact: Option<String>,
    offer: Option<String>,
    answer: Option<String>,
}

#[derive(Serialize)]
struct LocatorLookupResponse {
    generated_at: String,
    total: usize,
    query: LocatorQueryEcho,
    records: Vec<LocatorRecordView>,
}

#[derive(Serialize)]
struct LocatorQueryEcho {
    user: Option<String>,
    uri: String,
}

#[derive(Serialize)]
struct LocatorRecordView {
    binding_key: String,
    aor: String,
    destination: Option<String>,
    expires: u32,
    supports_webrtc: bool,
    contact: Option<String>,
    registered_aor: Option<String>,
    gruu: Option<String>,
    temp_gruu: Option<String>,
    instance_id: Option<String>,
    transport: Option<String>,
    path: Vec<String>,
    service_route: Vec<String>,
    contact_params: Option<std::collections::HashMap<String, String>>,
    age_seconds: Option<u64>,
    user_agent: Option<String>,
}

#[derive(Serialize)]
struct LocatorClearResponse {
    removed: bool,
    remaining: usize,
}

struct DialogStateSnapshot {
    label: String,
    detail: Option<String>,
    answer: Option<String>,
}

fn summarize_dialog(dialog: &Dialog) -> Option<DialogSummary> {
    let id = dialog.id();
    let from_display = dialog.from().to_string();
    let to_display = dialog.to().to_string();
    let remote_contact = dialog.remote_contact().map(|uri| uri.to_string());

    match dialog {
        Dialog::ServerInvite(server) => {
            summarize_server_dialog(server, id, from_display, to_display, remote_contact)
        }
        Dialog::ClientInvite(client) => {
            summarize_client_dialog(client, id, from_display, to_display, remote_contact)
        }
    }
}

fn summarize_server_dialog(
    server: &ServerInviteDialog,
    id: rsipstack::dialog::DialogId,
    from_display: String,
    to_display: String,
    remote_contact: Option<String>,
) -> Option<DialogSummary> {
    let state = server.state();
    let snapshot = summarize_state(&state)?;
    let offer = decode_body(server.initial_request().body.as_slice());

    Some(DialogSummary {
        id: id.to_string(),
        call_id: id.call_id.clone(),
        from_tag: id.from_tag.clone(),
        to_tag: id.to_tag.clone(),
        role: "server".to_string(),
        state: snapshot.label,
        state_detail: snapshot.detail,
        from_display,
        to_display,
        remote_contact,
        offer,
        answer: snapshot.answer,
    })
}

fn summarize_client_dialog(
    client: &ClientInviteDialog,
    id: rsipstack::dialog::DialogId,
    from_display: String,
    to_display: String,
    remote_contact: Option<String>,
) -> Option<DialogSummary> {
    let state = client.state();
    let snapshot = summarize_state(&state)?;

    Some(DialogSummary {
        id: id.to_string(),
        call_id: id.call_id.clone(),
        from_tag: id.from_tag.clone(),
        to_tag: id.to_tag.clone(),
        role: "client".to_string(),
        state: snapshot.label,
        state_detail: snapshot.detail,
        from_display,
        to_display,
        remote_contact,
        offer: None,
        answer: snapshot.answer,
    })
}

fn summarize_state(state: &DialogState) -> Option<DialogStateSnapshot> {
    match state {
        DialogState::Calling(_) => Some(DialogStateSnapshot {
            label: "Calling".to_string(),
            detail: None,
            answer: None,
        }),
        DialogState::Trying(_) => Some(DialogStateSnapshot {
            label: "Trying".to_string(),
            detail: None,
            answer: None,
        }),
        DialogState::Early(_, resp) => Some(DialogStateSnapshot {
            label: "Early".to_string(),
            detail: Some(resp.status_code.to_string()),
            answer: decode_body(resp.body()),
        }),
        DialogState::WaitAck(_, resp) => Some(DialogStateSnapshot {
            label: "WaitAck".to_string(),
            detail: Some(resp.status_code.to_string()),
            answer: decode_body(resp.body()),
        }),
        DialogState::Confirmed(_, resp) => Some(DialogStateSnapshot {
            label: "Confirmed".to_string(),
            detail: Some(resp.status_code.to_string()),
            answer: decode_body(resp.body()),
        }),
        DialogState::Updated(_, req) => Some(DialogStateSnapshot {
            label: "Updated".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Notify(_, req) => Some(DialogStateSnapshot {
            label: "Notify".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Info(_, req) => Some(DialogStateSnapshot {
            label: "Info".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Options(_, req) => Some(DialogStateSnapshot {
            label: "Options".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Terminated(_, _) => None,
    }
}

fn decode_body(body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let text: Cow<'_, str> = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(truncate(trimmed, 4096))
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "…"
}

fn resolve_target_uri(payload: &LocatorPayload) -> Result<Uri, Response> {
    if let Some(uri) = payload.uri.as_ref().and_then(|s| normalize_non_empty(s)) {
        return parse_uri(&uri);
    }
    if let Some(user) = payload.user.as_ref().and_then(|s| normalize_non_empty(s)) {
        let formatted = if user.contains('@') {
            format!("sip:{}", user)
        } else {
            format!("sip:{}@localhost", user)
        };
        return parse_uri(&formatted);
    }
    Err(bad_request("user or uri is required"))
}

fn normalize_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_uri(value: &str) -> Result<Uri, Response> {
    Uri::try_from(value).map_err(|err| bad_request(&format!("invalid uri: {}", err)))
}

fn extract_user_realm(payload: &LocatorPayload, uri: &Uri) -> Option<(String, Option<String>)> {
    if let Some(user_raw) = payload.user.as_ref().and_then(|s| normalize_non_empty(s)) {
        let (user, realm) = split_user_realm(&user_raw);
        if realm.is_some() {
            return Some((user, realm));
        }
        let realm = Some(uri.host().to_string());
        return Some((user, realm));
    }

    let username = uri.user()?.to_string();
    if username.trim().is_empty() {
        return None;
    }
    let realm = Some(uri.host().to_string());
    Some((username, realm))
}

fn split_user_realm(input: &str) -> (String, Option<String>) {
    if let Some((user, realm)) = input.split_once('@') {
        (user.to_string(), Some(realm.to_string()))
    } else {
        (input.to_string(), None)
    }
}

fn location_to_view(location: Location) -> LocatorRecordView {
    let path = location
        .path
        .as_ref()
        .into_iter()
        .flat_map(|vec| vec.iter())
        .map(|uri| uri.to_string())
        .collect::<Vec<_>>();
    let service_route = location
        .service_route
        .as_ref()
        .into_iter()
        .flat_map(|vec| vec.iter())
        .map(|uri| uri.to_string())
        .collect::<Vec<_>>();

    LocatorRecordView {
        binding_key: location.binding_key(),
        aor: location.aor.to_string(),
        destination: location.destination.map(|dest| dest.to_string()),
        expires: location.expires,
        supports_webrtc: location.supports_webrtc,
        contact: location.contact_raw,
        registered_aor: location.registered_aor.map(|uri| uri.to_string()),
        gruu: location.gruu,
        temp_gruu: location.temp_gruu,
        instance_id: location.instance_id,
        transport: location.transport.map(|t| t.to_string()),
        path,
        service_route,
        contact_params: location.contact_params,
        age_seconds: location
            .last_modified
            .map(|instant| instant.elapsed().as_secs()),
        user_agent: location.user_agent,
    }
}
