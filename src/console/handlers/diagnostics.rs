use crate::{
    call::{DialDirection, Location, RoutingState},
    config::{Config, ProxyConfig, RecordingPolicy, RouteResult, UserBackendConfig},
    console::{
        ConsoleState,
        handlers::{bad_request, normalize_optional_string},
        middleware::AuthRequired,
    },
    models::sip_trunk,
    proxy::{
        data::load_routes_from_db,
        routing::{
            self, SourceTrunk, TrunkDirection, build_source_trunk,
            matcher::{RouteResourceLookup, RouteTrace, match_invite_with_trace},
        },
        server::SipServerRef,
    },
};
use axum::{
    Router,
    extract::{Json, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use rand::random;
use rsip::{Header as SipHeader, Method, Scheme, Transport, Uri, Version};
use rsipstack::dialog::{
    client_dialog::ClientInviteDialog,
    dialog::{Dialog, DialogState},
    invitation::InviteOption,
    server_dialog::ServerInviteDialog,
};
use rsipstack::transport::SipAddr;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::{UdpSocket, lookup_host},
    time::timeout,
};

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/diagnostics", get(page_diagnostics))
        .route("/diagnostics/dialogs", get(list_dialogs))
        .route("/diagnostics/trunks/test", post(test_trunk))
        .route("/diagnostics/trunks/options", post(probe_trunk_options))
        .route("/diagnostics/routes/evaluate", post(route_evaluate))
        .route("/diagnostics/locator/lookup", post(locator_lookup))
        .route("/diagnostics/locator/clear", post(locator_clear))
}

const DEFAULT_OPTIONS_TIMEOUT_MS: u64 = 1_500;
const MIN_OPTIONS_TIMEOUT_MS: u64 = 100;
const MAX_OPTIONS_TIMEOUT_MS: u64 = 20_000;
const MAX_RESPONSE_PREVIEW_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeTransport {
    Udp,
    Tcp,
    Tls,
    Ws,
    Wss,
}

impl ProbeTransport {
    fn from_label(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "udp" => Ok(Self::Udp),
            "tcp" => Ok(Self::Tcp),
            "tls" => Ok(Self::Tls),
            "ws" => Ok(Self::Ws),
            "wss" => Ok(Self::Wss),
            other => Err(format!(
                "unsupported transport '{}': only udp is currently supported",
                other
            )),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            ProbeTransport::Udp => "udp",
            ProbeTransport::Tcp => "tcp",
            ProbeTransport::Tls => "tls",
            ProbeTransport::Ws => "ws",
            ProbeTransport::Wss => "wss",
        }
    }
}

impl From<Transport> for ProbeTransport {
    fn from(value: Transport) -> Self {
        match value {
            Transport::Udp => ProbeTransport::Udp,
            Transport::Tcp => ProbeTransport::Tcp,
            Transport::Tls => ProbeTransport::Tls,
            Transport::Ws => ProbeTransport::Ws,
            Transport::Wss => ProbeTransport::Wss,
            _ => ProbeTransport::Udp,
        }
    }
}

pub async fn page_diagnostics(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let bootstrap = diagnostics_bootstrap(&state).await;
    state.render(
        "console/diagnostics.html",
        json!({
            "nav_active": "diagnostics",
            "test_data": bootstrap,
            "addon_scripts": state.get_injected_scripts("/console/diagnostics"),
        }),
    )
}

async fn diagnostics_bootstrap(state: &Arc<ConsoleState>) -> JsonValue {
    let mut last_audit: Option<String> = None;
    let mut trunks: Vec<JsonValue> = Vec::new();
    let mut trunk_options: Vec<JsonValue> = Vec::new();
    let connection = diagnostics_connection_profile(state);

    if let Some(server) = state.sip_server() {
        let data_context = server.data_context.clone();
        let config_trunks = data_context.trunks_snapshot();
        let db_trunks = sip_trunk::Entity::find()
            .all(state.db())
            .await
            .unwrap_or_default();

        let mut db_by_name: HashMap<String, sip_trunk::Model> = HashMap::new();
        for model in db_trunks {
            db_by_name.insert(model.name.clone(), model);
        }

        let mut seen: HashSet<String> = HashSet::new();

        for (name, config) in config_trunks.iter() {
            let (label, status, last_test_at, direction, notes, limit, ingress_ips) =
                build_trunk_overview(name, config, db_by_name.get(name));

            let recording_value = config
                .recording
                .as_ref()
                .and_then(|policy| serde_json::to_value(policy).ok())
                .unwrap_or(JsonValue::Null);

            trunks.push(json!({
                "id": name,
                "label": label,
                "status": status,
                "latency_ms": 0,
                "packet_loss_percent": 0.0,
                "concurrency": {
                    "current": 0,
                    "limit": limit,
                },
                "last_test_at": last_test_at,
                "direction": direction,
                "egress": config.dest,
                "notes": notes,
                "ingress_ips": ingress_ips,
                "recording": recording_value,
            }));

            trunk_options.push(json!({
                "id": name,
                "label": label,
            }));

            seen.insert(name.clone());
        }

        for (name, model) in db_by_name.iter() {
            if seen.contains(name) {
                continue;
            }

            if let Some(config) = trunk_config_from_model(model) {
                let (label, status, last_test_at, direction, notes, limit, ingress_ips) =
                    build_trunk_overview(name, &config, Some(model));

                let recording_value = config
                    .recording
                    .as_ref()
                    .and_then(|policy| serde_json::to_value(policy).ok())
                    .unwrap_or(JsonValue::Null);

                trunks.push(json!({
                    "id": name,
                    "label": label,
                    "status": status,
                    "latency_ms": 0,
                    "packet_loss_percent": 0.0,
                    "concurrency": {
                        "current": 0,
                        "limit": limit,
                    },
                    "last_test_at": last_test_at,
                    "direction": direction,
                    "egress": config.dest,
                    "notes": notes,
                    "ingress_ips": ingress_ips,
                    "recording": recording_value,
                }));

                trunk_options.push(json!({
                    "id": name,
                    "label": label,
                }));
            }
        }

        last_audit = db_by_name
            .values()
            .filter_map(|model| model.last_health_check_at)
            .max()
            .map(|dt| dt.to_rfc3339());

        return json!({
            "last_audit": last_audit,
            "trunks": trunks,
            "routing_checks": [],
            "dialer": {
                "default_source": JsonValue::Null,
                "source_options": JsonValue::Array(vec![]),
                "destination_samples": JsonValue::Array(vec![]),
                "trunk_options": trunk_options,
            },
            "connection": connection,
        });
    }

    json!({
        "last_audit": last_audit,
        "trunks": trunks,
        "routing_checks": [],
        "dialer": {
            "default_source": JsonValue::Null,
            "source_options": JsonValue::Array(vec![]),
            "destination_samples": JsonValue::Array(vec![]),
            "trunk_options": trunk_options,
        },
        "connection": connection,
    })
}

fn diagnostics_connection_profile(state: &Arc<ConsoleState>) -> JsonValue {
    let mut realm = "localhost".to_string();
    let mut host = realm.clone();
    let mut transports: Vec<JsonValue> = Vec::new();
    let mut accounts: Vec<JsonValue> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut expires: Option<u32> = None;

    if let Some(app) = state.app_state() {
        let config = app.config().clone();
        let proxy_cfg = &config.proxy;
        realm = resolve_default_realm(proxy_cfg);
        host = resolve_preferred_host(Some(config.as_ref()), proxy_cfg, &realm);
        expires = proxy_cfg.registrar_expires;
        transports = build_transport_entries(&host, &realm, proxy_cfg);
        let (account_entries, backend_notes) = collect_account_entries(&realm, proxy_cfg);
        accounts = account_entries;
        notes.extend(backend_notes);
    } else if let Some(server) = state.sip_server() {
        let proxy_cfg = &server.proxy_config;
        realm = resolve_default_realm(proxy_cfg);
        host = resolve_preferred_host(None, proxy_cfg, &realm);
        expires = proxy_cfg.registrar_expires;
        transports = build_transport_entries(&host, &realm, proxy_cfg);
        let (account_entries, backend_notes) = collect_account_entries(&realm, proxy_cfg);
        accounts = account_entries;
        notes.extend(backend_notes);
        notes.push("Rendered from live proxy configuration.".to_string());
    } else {
        notes.push("SIP server is not currently running; showing defaults.".to_string());
    }

    if accounts.is_empty() {
        notes.push("No plaintext credentials available; create an extension or memory backend user for quick testing.".to_string());
    }

    notes.sort();
    notes.dedup();

    json!({
        "host": host,
        "realm": realm,
        "transports": transports,
        "accounts": accounts,
        "notes": notes,
        "expires": expires,
    })
}

fn resolve_default_realm(proxy_cfg: &ProxyConfig) -> String {
    if let Some(realms) = proxy_cfg.realms.as_ref() {
        for realm in realms {
            let candidate = realm.trim();
            if !candidate.is_empty() && candidate != "*" {
                return candidate.to_string();
            }
        }
    }
    ProxyConfig::normalize_realm("").to_string()
}

fn resolve_preferred_host(config: Option<&Config>, proxy_cfg: &ProxyConfig, realm: &str) -> String {
    if let Some(cfg) = config {
        if let Some(external) = cfg
            .external_ip
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            return external.to_string();
        }
    }

    let addr = proxy_cfg.addr.trim();
    if addr.is_empty() || addr == "0.0.0.0" || addr == "::" || addr == "[::]" || addr == "*" {
        realm.to_string()
    } else {
        addr.to_string()
    }
}

fn format_host_for_uri(host: &str) -> String {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        "localhost".to_string()
    } else if trimmed.contains(':') && !trimmed.starts_with('[') {
        format!("[{}]", trimmed)
    } else {
        trimmed.to_string()
    }
}

fn normalize_ws_path(path: Option<&str>) -> String {
    let raw = path.unwrap_or("/ws").trim();
    if raw.is_empty() {
        "/ws".to_string()
    } else if raw.starts_with('/') {
        raw.to_string()
    } else {
        format!("/{}", raw)
    }
}

fn build_sip_example(scheme: &str, host: &str, port: u16, default_port: u16) -> String {
    if port == default_port {
        format!("{}:{}", scheme, host)
    } else {
        format!("{}:{}:{}", scheme, host, port)
    }
}

fn build_transport_entries(host: &str, realm: &str, proxy_cfg: &ProxyConfig) -> Vec<JsonValue> {
    let mut transports = Vec::new();
    let host_uri = format_host_for_uri(host);
    let realm_uri = format_host_for_uri(realm);

    if let Some(port) = proxy_cfg.udp_port {
        transports.push(json!({
            "protocol": "udp",
            "label": "SIP UDP",
            "address": format!("{}:{}", host_uri, port),
            "port": port,
            "example_uri": build_sip_example("sip", &realm_uri, port, 5060),
        }));
    }

    if let Some(port) = proxy_cfg.tcp_port {
        transports.push(json!({
            "protocol": "tcp",
            "label": "SIP TCP",
            "address": format!("{}:{}", host_uri, port),
            "port": port,
            "example_uri": build_sip_example("sip", &realm_uri, port, 5060),
        }));
    }

    if let Some(port) = proxy_cfg.tls_port {
        transports.push(json!({
            "protocol": "tls",
            "label": "SIP TLS",
            "address": format!("{}:{}", host_uri, port),
            "port": port,
            "example_uri": build_sip_example("sips", &realm_uri, port, 5061),
        }));
    }

    if let Some(port) = proxy_cfg.ws_port {
        let path = normalize_ws_path(proxy_cfg.ws_handler.as_deref());
        transports.push(json!({
            "protocol": "ws",
            "label": "WebSocket (RFC 7118)",
            "address": format!("ws://{}:{}{}", host_uri, port, path),
            "port": port,
            "path": path,
            "example_uri": format!("ws://{}:{}{}", realm, port, path),
        }));
    }

    transports
}

fn collect_account_entries(
    default_realm: &str,
    proxy_cfg: &ProxyConfig,
) -> (Vec<JsonValue>, Vec<String>) {
    let mut accounts = Vec::new();
    let mut notes = Vec::new();

    for backend in proxy_cfg.user_backends.iter() {
        match backend {
            UserBackendConfig::Memory { users } => {
                if let Some(users) = users {
                    for user in users.iter().filter(|u| u.enabled).take(1) {
                        let realm = user
                            .realm
                            .as_ref()
                            .map(|value| value.trim())
                            .filter(|value| !value.is_empty())
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| default_realm.to_string());
                        accounts.push(json!({
                            "username": user.username,
                            "password": user.password,
                            "realm": realm,
                            "uri": format!("sip:{}@{}", user.username, realm),
                            "source": "memory",
                            "source_label": "Memory backend",
                        }));
                    }

                    if users.len() > 1 {
                        notes.push(format!(
                            "Memory backend defines {} users; showing first 1.",
                            users.len()
                        ));
                    }
                }
            }
            UserBackendConfig::Extension { .. } => notes.push(
                "Extension backend enabled — manage credentials under Console → Extensions."
                    .to_string(),
            ),
            UserBackendConfig::Http { url, .. } => notes.push(format!(
                "HTTP backend at {} supplies authentication credentials.",
                url
            )),
            UserBackendConfig::Plain { path } => {
                notes.push(format!("Plaintext backend uses credential file: {}.", path))
            }
            UserBackendConfig::Database { url, .. } => notes.push(format!(
                "Database backend {} is configured for authentication.",
                url.clone().unwrap_or_default(),
            )),
        }
    }

    (accounts, notes)
}

fn build_trunk_overview(
    name: &str,
    config: &routing::TrunkConfig,
    model: Option<&sip_trunk::Model>,
) -> (
    String,
    String,
    Option<String>,
    Vec<String>,
    String,
    u32,
    Vec<String>,
) {
    if let Some(model) = model {
        let label = model
            .display_name
            .clone()
            .unwrap_or_else(|| name.to_string());
        let status = model.status.as_str().to_string();
        let last_test_at = model.last_health_check_at.map(|dt| dt.to_rfc3339());
        let direction = match model.direction {
            sip_trunk::SipTrunkDirection::Inbound => vec!["inbound".to_string()],
            sip_trunk::SipTrunkDirection::Outbound => vec!["outbound".to_string()],
            sip_trunk::SipTrunkDirection::Bidirectional => {
                vec!["inbound".to_string(), "outbound".to_string()]
            }
        };
        let notes = model.description.clone().unwrap_or_default();
        let limit = model
            .max_concurrent
            .and_then(|v| (v > 0).then_some(v as u32))
            .or_else(|| config.max_calls)
            .unwrap_or(0);
        let ingress_ips = if !config.inbound_hosts.is_empty() {
            config.inbound_hosts.clone()
        } else {
            model
                .allowed_ips
                .as_ref()
                .map(json_value_to_string_list)
                .unwrap_or_default()
        };

        (
            label,
            status,
            last_test_at,
            direction,
            notes,
            limit,
            ingress_ips,
        )
    } else {
        let direction = match config.direction {
            Some(TrunkDirection::Inbound) => vec!["inbound".to_string()],
            Some(TrunkDirection::Outbound) => vec!["outbound".to_string()],
            Some(TrunkDirection::Bidirectional) => {
                vec!["inbound".to_string(), "outbound".to_string()]
            }
            None => Vec::new(),
        };
        let limit = config.max_calls.unwrap_or(0);
        (
            name.to_string(),
            "unknown".to_string(),
            None,
            direction,
            String::new(),
            limit,
            config.inbound_hosts.clone(),
        )
    }
}

fn json_value_to_string_list(value: &JsonValue) -> Vec<String> {
    match value {
        JsonValue::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(|s| s.to_string()))
            .collect(),
        JsonValue::String(item) => vec![item.clone()],
        _ => Vec::new(),
    }
}

fn trunk_config_from_model(model: &sip_trunk::Model) -> Option<routing::TrunkConfig> {
    let dest = model
        .sip_server
        .clone()
        .and_then(|value| normalize_non_empty(&value))
        .or_else(|| {
            model
                .outbound_proxy
                .clone()
                .and_then(|value| normalize_non_empty(&value))
        })?;

    let backup_dest = model
        .outbound_proxy
        .as_ref()
        .and_then(|value| normalize_non_empty(value))
        .filter(|value| value != &dest);

    let mut inbound_hosts = model
        .allowed_ips
        .as_ref()
        .map(json_value_to_string_list)
        .unwrap_or_default();

    if let Some(host) = extract_host_from_uri(&dest) {
        push_unique_string(&mut inbound_hosts, host);
    }

    if let Some(backup) = backup_dest.as_ref() {
        if let Some(host) = extract_host_from_uri(backup) {
            push_unique_string(&mut inbound_hosts, host);
        }
    }

    let recording = model
        .metadata
        .as_ref()
        .and_then(recording_policy_from_metadata);

    Some(routing::TrunkConfig {
        dest,
        backup_dest,
        username: model.auth_username.clone(),
        password: model.auth_password.clone(),
        codec: Vec::new(),
        disabled: Some(!model.is_active),
        max_calls: model.max_concurrent.map(|value| value as u32),
        max_cps: model.max_cps.map(|value| value as u32),
        weight: None,
        transport: Some(model.sip_transport.as_str().to_string()),
        id: Some(model.id),
        direction: Some(model.direction.into()),
        inbound_hosts,
        recording,
        incoming_from_user_prefix: model.incoming_from_user_prefix.clone(),
        incoming_to_user_prefix: model.incoming_to_user_prefix.clone(),
        origin: routing::ConfigOrigin::embedded(),
        country: None,
        policy: None,
    })
}

fn extract_host_from_uri(value: &str) -> Option<String> {
    Uri::try_from(value)
        .ok()
        .map(|parsed| parsed.host_with_port.host.to_string())
}

fn push_unique_string(list: &mut Vec<String>, value: String) {
    if value.is_empty() {
        return;
    }
    if !list.iter().any(|existing| existing == &value) {
        list.push(value);
    }
}

fn recording_policy_from_metadata(value: &JsonValue) -> Option<RecordingPolicy> {
    value
        .get("recording")
        .and_then(|entry| serde_json::from_value::<RecordingPolicy>(entry.clone()).ok())
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
        if let Some(dialog) = dialog_layer.get_dialog_with(&id) {
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

async fn test_trunk(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<TrunkTestPayload>,
) -> impl IntoResponse {
    let trunk_name = payload.trunk.trim();
    if trunk_name.is_empty() {
        return bad_request("trunk is required");
    }

    let address = payload.address.trim();
    if address.is_empty() {
        return bad_request("address is required");
    }

    let Some(server) = state.sip_server() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "SIP server unavailable" })),
        )
            .into_response();
    };

    let data_context = server.data_context.clone();
    let trunks = data_context.trunks_snapshot();
    let Some(trunk) = trunks.get(trunk_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": format!("trunk '{}' not found", trunk_name) })),
        )
            .into_response();
    };

    let resolved_ips = match resolve_candidate_ips(address).await {
        Ok(ips) if !ips.is_empty() => ips,
        Ok(_) => return bad_request("address did not resolve to any IP addresses"),
        Err(err) => return bad_request(err),
    };

    let mut evaluations = Vec::new();
    let mut overall_match = false;

    for ip in &resolved_ips {
        let mut matched_sources = Vec::new();

        for (idx, pattern) in trunk.inbound_hosts.iter().enumerate() {
            if routing::candidate_matches_ip(pattern, ip).await {
                matched_sources.push(format!("inbound_hosts[{}]", idx));
            }
        }

        if routing::candidate_matches_ip(&trunk.dest, ip).await {
            matched_sources.push("dest".to_string());
        }

        if let Some(backup) = &trunk.backup_dest {
            if routing::candidate_matches_ip(backup, ip).await {
                matched_sources.push("backup_dest".to_string());
            }
        }

        let matched = !matched_sources.is_empty();
        overall_match |= matched;
        evaluations.push(TrunkIpEvaluation {
            ip: ip.to_string(),
            matched,
            matched_sources,
        });
    }

    let direction = trunk.direction;
    let allows_inbound = direction
        .map(|d| d.allows(&DialDirection::Inbound))
        .unwrap_or(true);
    let allows_outbound = direction
        .map(|d| d.allows(&DialDirection::Outbound))
        .unwrap_or(true);

    Json(TrunkTestResponse {
        trunk: trunk_name.to_string(),
        address: address.to_string(),
        evaluated_at: Utc::now().to_rfc3339(),
        resolved_ips: resolved_ips.iter().map(ToString::to_string).collect(),
        ip_results: evaluations,
        overall_match,
        direction: trunk.direction,
        allows_inbound,
        allows_outbound,
    })
    .into_response()
}

async fn probe_trunk_options(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<OptionsProbePayload>,
) -> impl IntoResponse {
    let trunk_name = payload.trunk.trim();
    if trunk_name.is_empty() {
        return bad_request("trunk is required");
    }

    let Some(server) = state.sip_server() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "SIP server unavailable" })),
        )
            .into_response();
    };

    let data_context = server.data_context.clone();
    let trunks = data_context.trunks_snapshot();
    let Some(trunk) = trunks.get(trunk_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": format!("trunk '{}' not found", trunk_name) })),
        )
            .into_response();
    };

    let target_raw = payload
        .address
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .unwrap_or_else(|| trunk.dest.clone());

    if target_raw.trim().is_empty() {
        return bad_request("trunk destination is empty");
    }

    let normalized_target = if target_raw.starts_with("sip:") || target_raw.starts_with("sips:") {
        target_raw.clone()
    } else {
        format!("sip:{}", target_raw)
    };

    let target_uri: Uri = match Uri::try_from(normalized_target.as_str()) {
        Ok(uri) => uri,
        Err(err) => {
            return bad_request(format!("invalid SIP URI '{}': {}", target_raw, err));
        }
    };

    let transport = match determine_transport(payload.transport.as_deref(), &target_uri) {
        Ok(value) => value,
        Err(err) => return bad_request(err),
    };

    let timeout_ms = payload
        .timeout_ms
        .unwrap_or(DEFAULT_OPTIONS_TIMEOUT_MS)
        .clamp(MIN_OPTIONS_TIMEOUT_MS, MAX_OPTIONS_TIMEOUT_MS);
    let timeout_duration = Duration::from_millis(timeout_ms);

    let host = target_uri.host_with_port.host.to_string();
    if host.is_empty() {
        return bad_request("target host is empty");
    }

    let port = target_uri
        .host_with_port
        .port
        .map(|p| p.into())
        .unwrap_or_else(|| default_port_for_transport(transport));

    let lookup_target = format!("{}:{}", host, port);
    let resolved = match lookup_host(lookup_target).await {
        Ok(items) => items.collect::<Vec<_>>(),
        Err(err) => {
            return bad_request(format!("failed to resolve destination '{}': {}", host, err));
        }
    };

    if resolved.is_empty() {
        return bad_request("destination did not resolve to any IP addresses");
    }

    let mut destinations = resolved;
    destinations.sort_unstable();
    destinations.dedup();

    let default_host = default_host_for_probe(&server, &target_uri);
    let ua_label = server
        .proxy_config
        .useragent
        .clone()
        .unwrap_or_else(|| format!("RustPBX diagnostics/{}", env!("CARGO_PKG_VERSION")));

    let mut attempts = Vec::new();
    let mut success = false;

    for destination in destinations {
        let attempt = match transport {
            ProbeTransport::Udp => {
                let bind_ip = select_bind_ip(&server.proxy_config.addr, &destination);
                send_options_udp(
                    bind_ip,
                    destination,
                    &target_uri,
                    &default_host,
                    Some(ua_label.as_str()),
                    timeout_duration,
                )
                .await
            }
            _ => OptionsProbeAttempt {
                destination: destination.to_string(),
                transport: transport.as_str().to_string(),
                success: false,
                latency_ms: None,
                status_code: None,
                reason: None,
                server: None,
                error: Some(format!(
                    "transport '{}' is not supported for diagnostics probes yet",
                    transport.as_str()
                )),
                raw_response: None,
            },
        };

        success |= attempt.success;
        attempts.push(attempt);
    }

    let response = OptionsProbeResponse {
        trunk: trunk_name.to_string(),
        target_uri: target_uri.to_string(),
        evaluated_at: Utc::now().to_rfc3339(),
        transport: transport.as_str().to_string(),
        attempts,
        success,
    };

    Json(response).into_response()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvaluationDataset {
    Runtime,
    Database,
}

impl EvaluationDataset {
    fn from_label(input: &str) -> Option<Self> {
        match input {
            "runtime" | "active" | "live" | "formal" | "deployed" => Some(Self::Runtime),
            "database" | "db" | "pending" => Some(Self::Database),
            _ => None,
        }
    }
}

async fn route_evaluate(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<RouteEvaluationPayload>,
) -> impl IntoResponse {
    let Some(server) = state.sip_server() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "SIP server unavailable" })),
        )
            .into_response();
    };

    let callee_input = payload.callee.trim();
    if callee_input.is_empty() {
        return bad_request("callee is required");
    }

    let direction_label = payload
        .direction
        .as_ref()
        .map(|d| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "outbound".to_string());

    let direction = match direction_label.as_str() {
        "inbound" => DialDirection::Inbound,
        "internal" => DialDirection::Internal,
        "outbound" => DialDirection::Outbound,
        other => {
            return bad_request(format!(
                "unsupported direction '{}': use inbound/outbound/internal",
                other
            ));
        }
    };

    let dataset_label = payload
        .dataset
        .as_ref()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "database".to_string());

    let dataset = match EvaluationDataset::from_label(&dataset_label) {
        Some(kind) => kind,
        None => {
            return bad_request(format!(
                "unsupported dataset '{}': use runtime or database",
                dataset_label
            ));
        }
    };

    let default_host = server
        .proxy_config
        .realms
        .as_ref()
        .and_then(|realms| realms.first())
        .map(|s| s.as_str())
        .unwrap_or("localhost");

    let default_caller_value = default_caller_for(&direction, default_host);
    let caller_input = normalize_optional_string(&payload.caller).unwrap_or(default_caller_value);
    let request_uri_input = normalize_optional_string(&payload.request_uri);

    let caller_uri_str = build_sip_uri(&caller_input, default_host);
    let callee_uri_str = build_sip_uri(callee_input, default_host);
    let request_uri_str = request_uri_input
        .as_deref()
        .map(|value| build_sip_uri(value, default_host))
        .unwrap_or_else(|| callee_uri_str.clone());

    let caller_uri: rsip::Uri = match caller_uri_str.try_into() {
        Ok(uri) => uri,
        Err(err) => return bad_request(format!("invalid caller uri: {}", err)),
    };
    let callee_uri: rsip::Uri = match callee_uri_str.try_into() {
        Ok(uri) => uri,
        Err(err) => return bad_request(format!("invalid callee uri: {}", err)),
    };
    let request_uri: rsip::Uri = match request_uri_str.try_into() {
        Ok(uri) => uri,
        Err(err) => return bad_request(format!("invalid request uri: {}", err)),
    };

    let mut invite_option = InviteOption::default();
    invite_option.caller = caller_uri.clone();
    invite_option.callee = callee_uri.clone();
    invite_option.contact = caller_uri.clone();

    let custom_headers = payload
        .headers
        .as_ref()
        .map(|headers| headers_to_vec(headers))
        .unwrap_or_default();
    if !custom_headers.is_empty() {
        invite_option.headers = Some(custom_headers.clone());
    }

    let request =
        match build_diagnostics_request(&caller_uri, &callee_uri, &request_uri, custom_headers) {
            Ok(req) => req,
            Err(err) => return bad_request(err),
        };

    let data_context = server.data_context.clone();

    let (trunks_snapshot, routes_snapshot) = match dataset {
        EvaluationDataset::Runtime => {
            let trunks = data_context.trunks_snapshot();
            let routes = data_context.routes_snapshot();
            (trunks, routes)
        }
        EvaluationDataset::Database => {
            let db = state.db();
            let trunk_models = match sip_trunk::Entity::find().all(db).await {
                Ok(models) => models,
                Err(err) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "message": format!("failed to load trunks from database: {}", err),
                        })),
                    )
                        .into_response();
                }
            };

            let mut trunk_lookup: HashMap<i64, String> = HashMap::new();
            let mut trunks: HashMap<String, routing::TrunkConfig> = HashMap::new();
            for model in trunk_models {
                if let Some(config) = trunk_config_from_model(&model) {
                    if let Some(id) = config.id {
                        trunk_lookup.insert(id, model.name.clone());
                    }
                    trunks.insert(model.name.clone(), config);
                }
            }

            let routes = match load_routes_from_db(db, &trunk_lookup).await {
                Ok(routes) => routes,
                Err(err) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "message": format!("failed to load routes from database: {}", err),
                        })),
                    )
                        .into_response();
                }
            };

            (trunks, routes)
        }
    };

    let source_ip_input = normalize_optional_string(&payload.source_ip);

    let mut detected_trunk_from_ip = None;
    if let Some(ref source_ip) = source_ip_input {
        match source_ip.parse::<IpAddr>() {
            Ok(addr) => {
                detected_trunk_from_ip = detect_trunk_by_ip(&trunks_snapshot, &addr).await;
            }
            Err(_) => return bad_request("invalid source_ip"),
        }
    }

    let mut source_trunk_name = normalize_optional_string(&payload.source_trunk);
    if source_trunk_name.is_none() {
        source_trunk_name = detected_trunk_from_ip.clone();
    }

    let mut source_trunk_value: Option<SourceTrunk> = None;
    if let Some(name) = source_trunk_name.as_ref() {
        let Some(config) = trunks_snapshot.get(name) else {
            return bad_request(format!("source trunk '{}' not found", name));
        };
        source_trunk_value = build_source_trunk(name.clone(), config, &direction);
        if source_trunk_value.is_none() {
            return bad_request(format!(
                "trunk '{}' does not allow {:?} calls",
                name, direction
            ));
        }
    }

    let mut trace = RouteTrace::default();
    let resource_lookup = data_context.as_ref() as &dyn RouteResourceLookup;
    let original_option = invite_option.clone();
    let result = match match_invite_with_trace(
        if trunks_snapshot.is_empty() {
            None
        } else {
            Some(&trunks_snapshot)
        },
        if routes_snapshot.is_empty() {
            None
        } else {
            Some(&routes_snapshot)
        },
        Some(resource_lookup),
        invite_option,
        &request,
        source_trunk_value.as_ref(),
        Arc::new(RoutingState::new()),
        &direction,
        &mut trace,
    )
    .await
    {
        Ok(res) => res,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": format!("routing evaluation failed: {}", err) })),
            )
                .into_response();
        }
    };

    let (outcome, caller_render, callee_render, request_render, rewrites) = match result {
        RouteResult::Forward(option, _) => {
            let rewrites = collect_rewrite_diff(&original_option, &option);
            (
                RouteOutcomeView::Forward(RouteForwardOutcome {
                    destination: option.destination.map(|d| d.addr.to_string()),
                    headers: option
                        .headers
                        .unwrap_or_default()
                        .into_iter()
                        .map(|h| h.to_string())
                        .collect(),
                    credential: option.credential.map(|cred| CredentialView {
                        username: cred.username,
                        realm: cred.realm.map(|r| r.to_string()),
                    }),
                }),
                option.caller.to_string(),
                option.callee.to_string(),
                request.uri.to_string(),
                rewrites,
            )
        }
        RouteResult::Queue { option, queue, .. } => {
            let rewrites = collect_rewrite_diff(&original_option, &option);
            let forward = RouteForwardOutcome {
                destination: option.destination.map(|d| d.addr.to_string()),
                headers: option
                    .headers
                    .unwrap_or_default()
                    .into_iter()
                    .map(|h| h.to_string())
                    .collect(),
                credential: option.credential.map(|cred| CredentialView {
                    username: cred.username,
                    realm: cred.realm.map(|r| r.to_string()),
                }),
            };
            (
                RouteOutcomeView::Queue(RouteQueueOutcome {
                    queue: QueuePlanView::from(&queue),
                    forward,
                }),
                option.caller.to_string(),
                option.callee.to_string(),
                request.uri.to_string(),
                rewrites,
            )
        }
        RouteResult::NotHandled(option, _) => {
            let rewrites = collect_rewrite_diff(&original_option, &option);
            (
                RouteOutcomeView::NotHandled,
                option.caller.to_string(),
                option.callee.to_string(),
                request.uri.to_string(),
                rewrites,
            )
        }
        RouteResult::Abort(code, reason) => (
            RouteOutcomeView::Abort(RouteAbortOutcome {
                code: code.into(),
                reason: reason.clone(),
            }),
            original_option.caller.to_string(),
            original_option.callee.to_string(),
            request.uri.to_string(),
            Vec::new(),
        ),
    };

    Json(RouteEvaluationResponse {
        evaluated_at: Utc::now().to_rfc3339(),
        direction: direction_label,
        caller: caller_render,
        callee: callee_render,
        request_uri: request_render,
        source_trunk: source_trunk_name,
        detected_trunk: detected_trunk_from_ip,
        source_ip: source_ip_input,
        matched_rule: trace.matched_rule,
        selected_trunk: trace.selected_trunk,
        used_default_route: trace.used_default_route,
        rewrite_operations: trace.rewrite_operations,
        rewrites,
        outcome,
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

#[derive(Deserialize)]
struct OptionsProbePayload {
    trunk: String,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    transport: Option<String>,
}

#[derive(Serialize)]
struct OptionsProbeResponse {
    trunk: String,
    target_uri: String,
    evaluated_at: String,
    transport: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attempts: Vec<OptionsProbeAttempt>,
    success: bool,
}

#[derive(Serialize)]
struct OptionsProbeAttempt {
    destination: String,
    transport: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_response: Option<String>,
}

#[derive(Deserialize)]
struct TrunkTestPayload {
    trunk: String,
    address: String,
}

#[derive(Serialize)]
struct TrunkTestResponse {
    trunk: String,
    address: String,
    evaluated_at: String,
    resolved_ips: Vec<String>,
    ip_results: Vec<TrunkIpEvaluation>,
    overall_match: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<TrunkDirection>,
    allows_inbound: bool,
    allows_outbound: bool,
}

#[derive(Serialize)]
struct TrunkIpEvaluation {
    ip: String,
    matched: bool,
    matched_sources: Vec<String>,
}

#[derive(Deserialize)]
struct RouteEvaluationPayload {
    callee: String,
    caller: Option<String>,
    direction: Option<String>,
    dataset: Option<String>,
    source_trunk: Option<String>,
    source_ip: Option<String>,
    request_uri: Option<String>,
    headers: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
struct RouteEvaluationResponse {
    evaluated_at: String,
    direction: String,
    caller: String,
    callee: String,
    request_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_trunk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detected_trunk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_trunk: Option<String>,
    used_default_route: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rewrite_operations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rewrites: Vec<RewriteDiff>,
    outcome: RouteOutcomeView,
}

async fn detect_trunk_by_ip(
    trunks: &HashMap<String, routing::TrunkConfig>,
    addr: &IpAddr,
) -> Option<String> {
    for (name, config) in trunks.iter() {
        let candidate = name.clone();
        if config.matches_inbound_ip(addr).await {
            return Some(candidate);
        }
    }
    None
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RouteOutcomeView {
    Forward(RouteForwardOutcome),
    Queue(RouteQueueOutcome),
    NotHandled,
    Abort(RouteAbortOutcome),
}

#[derive(Serialize)]
struct RouteForwardOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    headers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<CredentialView>,
}

#[derive(Serialize)]
struct CredentialView {
    username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    realm: Option<String>,
}

#[derive(Serialize)]
struct RouteQueueOutcome {
    queue: QueuePlanView,
    forward: RouteForwardOutcome,
}

#[derive(Serialize)]
struct QueuePlanView {
    accept_immediately: bool,
    passthrough_ringback: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hold_audio: Option<String>,
    loop_playback: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback: Option<String>,
}

impl From<&crate::call::QueuePlan> for QueuePlanView {
    fn from(plan: &crate::call::QueuePlan) -> Self {
        let hold_audio = plan.hold.as_ref().and_then(|hold| hold.audio_file.clone());
        let loop_playback = plan
            .hold
            .as_ref()
            .map(|hold| hold.loop_playback)
            .unwrap_or(true);
        let fallback = plan.fallback.as_ref().map(|action| match action {
            crate::call::QueueFallbackAction::Failure(_) => "failure".to_string(),
            crate::call::QueueFallbackAction::Redirect { target } => {
                format!("redirect: {}", target)
            }
            crate::call::QueueFallbackAction::Queue { name } => {
                format!("queue: {}", name)
            }
        });
        Self {
            accept_immediately: plan.accept_immediately,
            passthrough_ringback: plan.passthrough_ringback,
            hold_audio,
            loop_playback,
            fallback,
        }
    }
}

#[derive(Serialize)]
struct RouteAbortOutcome {
    code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct RewriteDiff {
    field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<String>,
}

fn headers_to_vec(headers: &HashMap<String, String>) -> Vec<SipHeader> {
    let mut entries: Vec<_> = headers.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .into_iter()
        .map(|(name, value)| SipHeader::Other(name.clone(), value.clone()))
        .collect()
}

fn build_diagnostics_request(
    caller: &rsip::Uri,
    callee: &rsip::Uri,
    request_uri: &rsip::Uri,
    mut headers: Vec<SipHeader>,
) -> Result<rsip::Request, String> {
    let branch = format!("z9hG4bK{}", random_hex(10));
    let from_tag = random_hex(8);
    let call_id = format!("diag-{}", random_hex(12));

    let via = format!(
        "SIP/2.0/UDP diagnostics.console.local:5060;branch={}",
        branch
    );

    let mut base_headers = vec![
        SipHeader::Via(
            via.try_into()
                .map_err(|_| "invalid via header".to_string())?,
        ),
        SipHeader::From(
            format!("Diagnostics <{}>;tag={}", caller, from_tag)
                .try_into()
                .map_err(|_| "invalid from header".to_string())?,
        ),
        SipHeader::To(
            format!("<{}>", callee)
                .try_into()
                .map_err(|_| "invalid to header".to_string())?,
        ),
        SipHeader::CallId(call_id.into()),
        SipHeader::CSeq(
            format!("1 {}", Method::Invite)
                .try_into()
                .map_err(|_| "invalid cseq header".to_string())?,
        ),
        SipHeader::MaxForwards(70.into()),
        SipHeader::ContentLength(0.into()),
    ];

    base_headers.append(&mut headers);

    Ok(rsip::Request {
        method: Method::Invite,
        uri: request_uri.clone(),
        version: Version::V2,
        headers: base_headers.into(),
        body: Vec::new(),
    })
}

fn default_caller_for(direction: &DialDirection, default_host: &str) -> String {
    match direction {
        DialDirection::Inbound => format!("incoming@{}", default_host),
        DialDirection::Internal => format!("diagnostics@{}", default_host),
        DialDirection::Outbound => format!("diagnostics@{}", default_host),
    }
}

fn build_sip_uri(value: &str, default_host: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return format!("sip:anonymous@{}", default_host);
    }

    if let Some(rest) = trimmed.strip_prefix("sip:") {
        if rest.contains('@') {
            format!("sip:{}", rest)
        } else {
            format!("sip:{}@{}", rest, default_host)
        }
    } else if trimmed.contains('@') {
        format!("sip:{}", trimmed)
    } else {
        format!("sip:{}@{}", trimmed, default_host)
    }
}

async fn resolve_candidate_ips(input: &str) -> Result<Vec<IpAddr>, String> {
    let cleaned = input.trim().trim_matches(|c| c == '<' || c == '>');
    if cleaned.is_empty() {
        return Err("address is empty".to_string());
    }

    if let Ok(ip) = cleaned.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }

    if let Ok(socket) = cleaned.parse::<SocketAddr>() {
        return Ok(vec![socket.ip()]);
    }

    if let Some((host, port)) = extract_sip_host(cleaned) {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }

        match lookup_host((host.as_str(), port)).await {
            Ok(resolved) => {
                let mut ips: Vec<IpAddr> = resolved.map(|addr| addr.ip()).collect();
                ips.sort();
                ips.dedup();
                return Ok(ips);
            }
            Err(err) => {
                return Err(format!("failed to resolve '{}': {}", input, err));
            }
        }
    }

    let lookup_target = if cleaned.contains(':') {
        cleaned.to_string()
    } else {
        format!("{}:0", cleaned)
    };

    match lookup_host(lookup_target.as_str()).await {
        Ok(resolved) => {
            let mut ips: Vec<IpAddr> = resolved.map(|addr| addr.ip()).collect();
            ips.sort();
            ips.dedup();
            Ok(ips)
        }
        Err(err) => Err(format!("failed to resolve '{}': {}", input, err)),
    }
}

fn extract_sip_host(candidate: &str) -> Option<(String, u16)> {
    // Accept inputs that may be formatted as SIP URIs and surface the host/port for DNS resolution.
    if let Ok(uri) = rsip::Uri::try_from(candidate) {
        let host = uri.host_with_port.host.to_string();
        if host.is_empty() {
            return None;
        }
        let port = uri
            .host_with_port
            .port
            .map(|value| value.into())
            .unwrap_or(0);
        return Some((host, port));
    }

    if let Some(rest) = candidate.strip_prefix("sip:") {
        let host = rest
            .split(|c| c == ';' || c == '?')
            .next()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())?;

        if let Ok(socket) = host.parse::<SocketAddr>() {
            return Some((socket.ip().to_string(), socket.port()));
        }

        return Some((host.to_string(), 0));
    }

    None
}

fn determine_transport(override_label: Option<&str>, uri: &Uri) -> Result<ProbeTransport, String> {
    if let Some(label) = override_label {
        return ProbeTransport::from_label(label);
    }

    if let Some(param_transport) = uri.params.iter().find_map(|param| match param {
        rsip::Param::Transport(t) => Some(ProbeTransport::from(*t)),
        _ => None,
    }) {
        return Ok(param_transport);
    }

    if uri.scheme == Some(Scheme::Sips) {
        return Ok(ProbeTransport::Tls);
    }

    Ok(ProbeTransport::Udp)
}

fn default_port_for_transport(transport: ProbeTransport) -> u16 {
    match transport {
        ProbeTransport::Udp | ProbeTransport::Tcp => 5060,
        ProbeTransport::Tls => 5061,
        ProbeTransport::Ws => 80,
        ProbeTransport::Wss => 443,
    }
}

fn default_host_for_probe(server: &SipServerRef, uri: &Uri) -> String {
    server
        .proxy_config
        .realms
        .as_ref()
        .and_then(|realms| {
            realms
                .iter()
                .find(|realm| !realm.trim().is_empty())
                .map(|realm| realm.trim().to_string())
        })
        .filter(|realm| !realm.is_empty())
        .unwrap_or_else(|| uri.host_with_port.host.to_string())
}

fn select_bind_ip(config_addr: &str, destination: &SocketAddr) -> IpAddr {
    if let Ok(ip) = config_addr.parse::<IpAddr>() {
        if !ip.is_unspecified() && ip.is_ipv4() == destination.is_ipv4() {
            return ip;
        }
    }

    if destination.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    }
}

async fn send_options_udp(
    bind_ip: IpAddr,
    destination: SocketAddr,
    target_uri: &Uri,
    default_host: &str,
    user_agent: Option<&str>,
    timeout_duration: Duration,
) -> OptionsProbeAttempt {
    let mut attempt = OptionsProbeAttempt {
        destination: destination.to_string(),
        transport: ProbeTransport::Udp.as_str().to_string(),
        success: false,
        latency_ms: None,
        status_code: None,
        reason: None,
        server: None,
        error: None,
        raw_response: None,
    };

    let socket = match UdpSocket::bind(SocketAddr::new(bind_ip, 0)).await {
        Ok(socket) => socket,
        Err(err) => {
            attempt.error = Some(format!("failed to bind UDP socket: {}", err));
            return attempt;
        }
    };

    if let Err(err) = socket.connect(destination).await {
        attempt.error = Some(format!("failed to connect to {}: {}", destination, err));
        return attempt;
    }

    let local_addr = match socket.local_addr() {
        Ok(addr) => addr,
        Err(err) => {
            attempt.error = Some(format!("failed to read local socket address: {}", err));
            return attempt;
        }
    };

    let request = build_options_request(local_addr, target_uri, default_host, user_agent);

    if let Err(err) = socket.send(request.as_bytes()).await {
        attempt.error = Some(format!("failed to send OPTIONS request: {}", err));
        return attempt;
    }

    let started = Instant::now();
    let mut buffer = vec![0u8; 4096];

    match timeout(timeout_duration, socket.recv(&mut buffer)).await {
        Ok(Ok(received)) => {
            let elapsed = started.elapsed();
            attempt.latency_ms = Some(elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
            let raw = String::from_utf8_lossy(&buffer[..received]).to_string();
            let summary = summarize_sip_response(&raw);
            attempt.status_code = summary.status_code;
            attempt.reason = summary.reason;
            attempt.server = summary.server;
            attempt.raw_response = summary.preview;
            attempt.success = summary
                .status_code
                .map(|code| (200..300).contains(&code))
                .unwrap_or(false);
        }
        Ok(Err(err)) => {
            attempt.error = Some(format!("failed to receive response: {}", err));
        }
        Err(_) => {
            attempt.error = Some(format!(
                "timed out waiting for response after {} ms",
                timeout_duration.as_millis()
            ));
        }
    }

    attempt
}

fn build_options_request(
    local_addr: SocketAddr,
    target_uri: &Uri,
    default_host: &str,
    user_agent: Option<&str>,
) -> String {
    let branch = format!("z9hG4bK{}", random_hex(10));
    let from_tag = random_hex(8);
    let call_id = format!("diag-{}", random_hex(12));

    let from_uri = format!("sip:diagnostics@{}", default_host);
    let contact_uri = format!("sip:diagnostics@{}:{}", local_addr.ip(), local_addr.port());
    let request_uri = target_uri.to_string();
    let to_uri = target_uri.to_string();

    let mut lines = vec![
        format!("OPTIONS {} SIP/2.0", request_uri),
        format!(
            "Via: SIP/2.0/UDP {}:{};branch={};rport",
            local_addr.ip(),
            local_addr.port(),
            branch
        ),
        "Max-Forwards: 70".to_string(),
        format!("From: \"Diagnostics\" <{}>;tag={}", from_uri, from_tag),
        format!("To: <{}>", to_uri),
        format!("Call-ID: {}", call_id),
        "CSeq: 1 OPTIONS".to_string(),
        format!("Contact: <{}>", contact_uri),
        "Accept: application/sdp".to_string(),
        "Allow: INVITE, ACK, CANCEL, OPTIONS, BYE".to_string(),
    ];

    if let Some(agent) = user_agent {
        lines.push(format!("User-Agent: {}", agent));
    }

    lines.push("Content-Length: 0".to_string());
    lines.push(String::new());
    lines.push(String::new());
    lines.join("\r\n")
}

struct SipResponseSummary {
    status_code: Option<u16>,
    reason: Option<String>,
    server: Option<String>,
    preview: Option<String>,
}

fn summarize_sip_response(raw: &str) -> SipResponseSummary {
    let mut summary = SipResponseSummary {
        status_code: None,
        reason: None,
        server: None,
        preview: None,
    };

    if let Some(first_line) = raw.lines().next() {
        let mut parts = first_line.split_whitespace();
        if matches!(parts.next(), Some(proto) if proto.eq_ignore_ascii_case("SIP/2.0")) {
            if let Some(code_part) = parts.next() {
                if let Ok(code) = code_part.parse::<u16>() {
                    summary.status_code = Some(code);
                }
            }
            let reason = parts.collect::<Vec<_>>().join(" ");
            if !reason.is_empty() {
                summary.reason = Some(reason);
            }
        }
    }

    for line in raw.lines() {
        if line.to_ascii_lowercase().starts_with("server:") {
            if let Some(rest) = line.splitn(2, ':').nth(1) {
                let value = rest.trim();
                if !value.is_empty() {
                    summary.server = Some(value.to_string());
                }
            }
            break;
        }
    }

    if !raw.trim().is_empty() {
        summary.preview = Some(truncate(raw.trim(), MAX_RESPONSE_PREVIEW_CHARS));
    }

    summary
}

fn collect_rewrite_diff(before: &InviteOption, after: &InviteOption) -> Vec<RewriteDiff> {
    let mut diffs = Vec::new();

    record_uri_diff("caller", &before.caller, &after.caller, &mut diffs);
    record_uri_diff("callee", &before.callee, &after.callee, &mut diffs);
    record_uri_diff("contact", &before.contact, &after.contact, &mut diffs);
    record_option_string_diff(
        "content_type",
        &before.content_type,
        &after.content_type,
        &mut diffs,
    );
    record_destination_diff(
        "destination",
        before.destination.as_ref(),
        after.destination.as_ref(),
        &mut diffs,
    );
    record_headers_diff("headers", &before.headers, &after.headers, &mut diffs);

    diffs
}

fn record_uri_diff(
    field: &str,
    before: &rsip::Uri,
    after: &rsip::Uri,
    diffs: &mut Vec<RewriteDiff>,
) {
    let before_str = before.to_string();
    let after_str = after.to_string();
    if before_str != after_str {
        diffs.push(RewriteDiff {
            field: field.to_string(),
            before: Some(before_str),
            after: Some(after_str),
        });
    }
}

fn record_option_string_diff(
    field: &str,
    before: &Option<String>,
    after: &Option<String>,
    diffs: &mut Vec<RewriteDiff>,
) {
    if before == after {
        return;
    }
    diffs.push(RewriteDiff {
        field: field.to_string(),
        before: before.clone(),
        after: after.clone(),
    });
}

fn record_destination_diff(
    field: &str,
    before: Option<&SipAddr>,
    after: Option<&SipAddr>,
    diffs: &mut Vec<RewriteDiff>,
) {
    let before_str = before.map(|dest| dest.addr.to_string());
    let after_str = after.map(|dest| dest.addr.to_string());
    if before_str != after_str {
        diffs.push(RewriteDiff {
            field: field.to_string(),
            before: before_str,
            after: after_str,
        });
    }
}

fn record_headers_diff(
    field: &str,
    before: &Option<Vec<SipHeader>>,
    after: &Option<Vec<SipHeader>>,
    diffs: &mut Vec<RewriteDiff>,
) {
    let before_render = headers_to_string_list(before);
    let after_render = headers_to_string_list(after);
    if before_render != after_render {
        diffs.push(RewriteDiff {
            field: field.to_string(),
            before: before_render,
            after: after_render,
        });
    }
}

fn headers_to_string_list(headers: &Option<Vec<SipHeader>>) -> Option<String> {
    headers.as_ref().map(|items| {
        let mut values: Vec<String> = items.iter().map(|h| h.to_string()).collect();
        values.sort();
        values.join("\n")
    })
}

fn random_hex(length: usize) -> String {
    let mut output = String::with_capacity(length);
    while output.len() < length {
        output.push_str(&format!("{:x}", random::<u64>()));
    }
    output.truncate(length);
    output
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
        _ => None,
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
        from_tag: id.local_tag.clone(),
        to_tag: id.remote_tag.clone(),
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
        from_tag: id.local_tag.clone(),
        to_tag: id.remote_tag.clone(),
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
        DialogState::Updated(_, req, _) => Some(DialogStateSnapshot {
            label: "Updated".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Notify(_, req, _) => Some(DialogStateSnapshot {
            label: "Notify".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Refer(_, req, _) => Some(DialogStateSnapshot {
            label: "Refer".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Message(_, req, _) => Some(DialogStateSnapshot {
            label: "Message".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Info(_, req, _) => Some(DialogStateSnapshot {
            label: "Info".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Options(_, req, _) => Some(DialogStateSnapshot {
            label: "Options".to_string(),
            detail: Some(req.method.to_string()),
            answer: decode_body(&req.body),
        }),
        DialogState::Publish(_, req, _) => Some(DialogStateSnapshot {
            label: "Publish".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::IpAddr, time::Duration};
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn resolve_candidate_ips_accepts_sip_uri() {
        let ips = resolve_candidate_ips("sip:127.0.0.1:5060").await.unwrap();
        assert_eq!(ips, vec![IpAddr::from([127, 0, 0, 1])]);
    }

    #[tokio::test]
    async fn resolve_candidate_ips_accepts_bracketed_uri() {
        let ips = resolve_candidate_ips("<sip:127.0.0.1>").await.unwrap();
        assert_eq!(ips, vec![IpAddr::from([127, 0, 0, 1])]);
    }

    #[tokio::test]
    async fn send_options_udp_marks_success() {
        let server_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_socket.local_addr().unwrap();

        tokio::spawn(async move {
            let mut buffer = [0u8; 2048];
            if let Ok((_, peer)) = server_socket.recv_from(&mut buffer).await {
                let response = b"SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n";
                let _ = server_socket.send_to(response, peer).await;
            }
        });

        let target_uri: Uri = Uri::try_from("sip:test@127.0.0.1").unwrap();
        let attempt = send_options_udp(
            IpAddr::from([127, 0, 0, 1]),
            server_addr,
            &target_uri,
            "localhost",
            Some("Test-UA"),
            Duration::from_millis(500),
        )
        .await;

        assert!(attempt.success);
        assert_eq!(attempt.status_code, Some(200));
        assert!(attempt.error.is_none());
        assert!(
            attempt
                .raw_response
                .as_ref()
                .map(|raw| raw.starts_with("SIP/2.0 200"))
                .unwrap_or(false)
        );
    }
}
