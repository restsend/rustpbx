use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::models::{
    routing::{
        ActiveModel as RoutingActiveModel, Column as RoutingColumn, Entity as RoutingEntity,
        Model as RoutingModel,
    },
    sip_trunk::{Column as SipTrunkColumn, Entity as SipTrunkEntity, Model as SipTrunkModel},
};
use axum::{
    extract::{Form, Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use chrono::{NaiveDateTime, TimeZone, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait,
    QueryFilter, QueryOrder, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tracing::warn;

const DEFAULT_PRIORITY: i32 = 100;
const MIN_PRIORITY: i32 = 0;
const MAX_PRIORITY: i32 = 10_000;
const DEFAULT_DIRECTION: &str = "outbound";
const ALLOWED_DIRECTIONS: &[&str] = &["inbound", "outbound"];
const SELECTION_ALGORITHMS: &[&str] = &["rr", "weight", "hash"];

#[derive(Debug, Clone, Deserialize)]
pub struct RoutingForm {
    #[serde(default)]
    payload: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RouteInput {
    #[serde(default)]
    id: Option<i64>,
    name: Option<String>,
    description: Option<String>,
    owner: Option<String>,
    direction: Option<String>,
    priority: Option<i32>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(rename = "match")]
    matchers: Option<JsonMap<String, Value>>,
    #[serde(default)]
    rewrite: Option<JsonMap<String, Value>>,
    #[serde(default)]
    action: Option<RouteActionInput>,
    #[serde(default)]
    source_trunk: Option<String>,
    #[serde(default)]
    target_trunks: Option<Vec<String>>,
    #[serde(default)]
    notes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct RouteActionInput {
    select: Option<String>,
    hash_key: Option<String>,
    #[serde(default)]
    trunks: Vec<RouteTrunkInput>,
}

#[derive(Debug, Clone, Deserialize)]
struct RouteTrunkInput {
    name: Option<String>,
    weight: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouteDocument {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default = "default_direction_string")]
    direction: String,
    #[serde(default = "default_priority_value")]
    priority: i32,
    #[serde(default)]
    disabled: bool,
    #[serde(rename = "match", default)]
    matchers: JsonMap<String, Value>,
    #[serde(default)]
    rewrite: JsonMap<String, Value>,
    #[serde(default)]
    action: RouteActionDocument,
    #[serde(default)]
    source_trunk: Option<String>,
    #[serde(default)]
    target_trunks: Vec<String>,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouteActionDocument {
    select: String,
    #[serde(default)]
    hash_key: Option<String>,
    #[serde(default)]
    trunks: Vec<RouteTrunkDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouteTrunkDocument {
    name: String,
    weight: i32,
}

#[derive(Debug)]
struct RouteError {
    message: String,
}

impl RouteError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn message(&self) -> &str {
        &self.message
    }
}

impl RoutingForm {
    fn parse_document(&self) -> Result<RouteDocument, RouteError> {
        if self.payload.trim().is_empty() {
            return Err(RouteError::new("Route payload is required"));
        }

        let raw_value: Value = serde_json::from_str(&self.payload)
            .map_err(|err| RouteError::new(format!("Invalid route payload: {}", err)))?;
        let input: RouteInput = serde_json::from_value(raw_value)
            .map_err(|err| RouteError::new(format!("Invalid route payload: {}", err)))?;

        let mut doc = RouteDocument::from_input(input);
        doc.ensure_consistency();
        Ok(doc)
    }
}

impl Default for RouteActionDocument {
    fn default() -> Self {
        Self {
            select: SELECTION_ALGORITHMS[0].to_string(),
            hash_key: None,
            trunks: Vec::new(),
        }
    }
}

impl Default for RouteDocument {
    fn default() -> Self {
        Self {
            id: None,
            name: String::new(),
            description: None,
            owner: None,
            direction: DEFAULT_DIRECTION.to_string(),
            priority: DEFAULT_PRIORITY,
            disabled: false,
            matchers: JsonMap::new(),
            rewrite: JsonMap::new(),
            action: RouteActionDocument::default(),
            source_trunk: None,
            target_trunks: Vec::new(),
            notes: Vec::new(),
        }
    }
}

impl RouteActionDocument {
    fn from_input(input: Option<RouteActionInput>) -> Self {
        if let Some(raw) = input {
            let mut action = RouteActionDocument {
                select: normalize_selection(raw.select),
                hash_key: sanitize_optional_string(raw.hash_key),
                trunks: sanitize_trunks(raw.trunks),
            };
            if action.select != "hash" {
                action.hash_key = None;
            }
            action
        } else {
            RouteActionDocument::default()
        }
    }
}

impl RouteDocument {
    fn from_input(input: RouteInput) -> Self {
        let mut doc = RouteDocument {
            id: input.id,
            name: sanitize_optional_string(input.name).unwrap_or_default(),
            description: sanitize_optional_string(input.description),
            owner: sanitize_optional_string(input.owner),
            direction: sanitize_direction(input.direction),
            priority: clamp_priority(input.priority),
            disabled: input.disabled.unwrap_or(false),
            matchers: sanitize_map(input.matchers),
            rewrite: sanitize_map(input.rewrite),
            action: RouteActionDocument::from_input(input.action),
            source_trunk: sanitize_optional_string(input.source_trunk),
            target_trunks: sanitize_target_trunks(input.target_trunks),
            notes: sanitize_notes(input.notes),
        };
        doc.ensure_consistency();
        doc
    }

    fn from_model(model: &RoutingModel) -> Self {
        if let Some(meta) = model.metadata.clone() {
            if let Ok(mut doc) = serde_json::from_value::<RouteDocument>(meta) {
                doc.id = Some(model.id);
                doc.name = model.name.clone();
                doc.description = model.description.clone();
                doc.owner = model.owner.clone();
                doc.direction = model.direction.clone();
                doc.priority = model.priority;
                doc.disabled = !model.is_active;
                doc.action.select = normalize_selection(Some(doc.action.select.clone()));
                doc.action.hash_key = if doc.action.select == "hash" {
                    sanitize_optional_string(model.hash_key.clone()).or(doc.action.hash_key)
                } else {
                    None
                };
                if let Some(filters) = model.header_filters.clone() {
                    doc.matchers = value_to_map(filters);
                }
                if let Some(rewrites) = model.rewrite_rules.clone() {
                    doc.rewrite = value_to_map(rewrites);
                }
                let assignments = parse_trunk_assignments(model.target_trunks.clone());
                if !assignments.is_empty() {
                    doc.action.trunks = assignments;
                }
                if let Some(notes) = model.notes.clone() {
                    doc.notes = parse_notes_value(Some(notes));
                }
                doc.ensure_consistency();
                return doc;
            }
        }

        let mut doc = RouteDocument {
            id: Some(model.id),
            name: model.name.clone(),
            description: model.description.clone(),
            owner: model.owner.clone(),
            direction: model.direction.clone(),
            priority: model.priority,
            disabled: !model.is_active,
            matchers: model
                .header_filters
                .clone()
                .map(value_to_map)
                .unwrap_or_default(),
            rewrite: model
                .rewrite_rules
                .clone()
                .map(value_to_map)
                .unwrap_or_default(),
            action: RouteActionDocument {
                select: normalize_selection(Some(model.selection_strategy.clone())),
                hash_key: if model.selection_strategy.eq_ignore_ascii_case("hash") {
                    sanitize_optional_string(model.hash_key.clone())
                } else {
                    None
                },
                trunks: parse_trunk_assignments(model.target_trunks.clone()),
            },
            source_trunk: None,
            target_trunks: Vec::new(),
            notes: parse_notes_value(model.notes.clone()),
        };
        doc.ensure_consistency();
        doc
    }

    fn validate(&self) -> Result<(), RouteError> {
        if self.name.trim().is_empty() {
            return Err(RouteError::new("Route name is required"));
        }
        Ok(())
    }

    fn ensure_consistency(&mut self) {
        self.action.select = normalize_selection(Some(self.action.select.clone()));
        if self.action.select != "hash" {
            self.action.hash_key = None;
        }

        let mut dedup = HashSet::new();
        let mut targets = Vec::new();
        for name in std::mem::take(&mut self.target_trunks) {
            if let Some(clean) = sanitize_optional_string(Some(name)) {
                if dedup.insert(clean.clone()) {
                    targets.push(clean);
                }
            }
        }
        for trunk in &self.action.trunks {
            if dedup.insert(trunk.name.clone()) {
                targets.push(trunk.name.clone());
            }
        }
        self.target_trunks = targets;

        self.notes = self
            .notes
            .iter()
            .filter_map(|note| sanitize_optional_string(Some(note.clone())))
            .collect();
    }

    fn metadata_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    fn display_label(&self, fallback_id: Option<i64>) -> String {
        if !self.name.trim().is_empty() {
            return self.name.clone();
        }
        fallback_id
            .map(|id| format!("Route #{id}"))
            .unwrap_or_else(|| "New route".to_string())
    }

    fn apply_trunk_context(&mut self, model: &RoutingModel, trunks: &HashMap<i64, SipTrunkModel>) {
        if self.source_trunk.is_none() {
            if let Some(source_id) = model.source_trunk_id {
                if let Some(trunk) = trunks.get(&source_id) {
                    self.source_trunk = Some(trunk.name.clone());
                }
            }
        }

        if self.target_trunks.is_empty() {
            if let Some(default_id) = model.default_trunk_id {
                if let Some(trunk) = trunks.get(&default_id) {
                    if !self
                        .target_trunks
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&trunk.name))
                    {
                        self.target_trunks.push(trunk.name.clone());
                    }
                }
            }
            self.ensure_consistency();
        }
    }
}

fn default_direction_string() -> String {
    DEFAULT_DIRECTION.to_string()
}

fn default_priority_value() -> i32 {
    DEFAULT_PRIORITY
}

fn sanitize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn sanitize_direction(value: Option<String>) -> String {
    value
        .and_then(|v| {
            let normalized = v.trim().to_ascii_lowercase();
            if ALLOWED_DIRECTIONS.contains(&normalized.as_str()) {
                Some(normalized)
            } else {
                None
            }
        })
        .unwrap_or_else(|| DEFAULT_DIRECTION.to_string())
}

fn normalize_selection(value: Option<String>) -> String {
    value
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| SELECTION_ALGORITHMS.contains(&v.as_str()))
        .unwrap_or_else(|| SELECTION_ALGORITHMS[0].to_string())
}

fn clamp_priority(value: Option<i32>) -> i32 {
    value
        .unwrap_or(DEFAULT_PRIORITY)
        .clamp(MIN_PRIORITY, MAX_PRIORITY)
}

fn sanitize_map(map: Option<JsonMap<String, Value>>) -> JsonMap<String, Value> {
    let mut result = JsonMap::new();
    if let Some(entries) = map {
        for (key, value) in entries {
            let trimmed_key = key.trim();
            if trimmed_key.is_empty() {
                continue;
            }
            let sanitized_value = extract_string(value);
            if sanitized_value.is_empty() {
                continue;
            }
            result.insert(trimmed_key.to_string(), Value::String(sanitized_value));
        }
    }
    result
}

fn sanitize_trunks(items: Vec<RouteTrunkInput>) -> Vec<RouteTrunkDocument> {
    let mut result = Vec::new();
    for item in items {
        if let Some(name) = sanitize_optional_string(item.name) {
            let weight = item.weight.unwrap_or(0).clamp(0, MAX_PRIORITY);
            result.push(RouteTrunkDocument { name, weight });
        }
    }
    result
}

fn sanitize_target_trunks(targets: Option<Vec<String>>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for value in targets.unwrap_or_default() {
        if let Some(name) = sanitize_optional_string(Some(value)) {
            if seen.insert(name.clone()) {
                result.push(name);
            }
        }
    }
    result
}

fn sanitize_notes(notes: Option<Vec<String>>) -> Vec<String> {
    notes
        .unwrap_or_default()
        .into_iter()
        .filter_map(|note| sanitize_optional_string(Some(note)))
        .collect()
}

fn extract_string(value: Value) -> String {
    match value {
        Value::String(s) => s.trim().to_string(),
        Value::Number(num) => num.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => String::new(),
        other => serde_json::to_string(&other).unwrap_or_default(),
    }
}

fn value_to_map(value: Value) -> JsonMap<String, Value> {
    match value {
        Value::Object(map) => map
            .into_iter()
            .filter_map(|(key, value)| {
                let trimmed_key = key.trim();
                if trimmed_key.is_empty() {
                    return None;
                }
                let str_value = extract_string(value);
                if str_value.is_empty() {
                    return None;
                }
                Some((trimmed_key.to_string(), Value::String(str_value)))
            })
            .collect(),
        _ => JsonMap::new(),
    }
}

fn parse_trunk_assignments(value: Option<Value>) -> Vec<RouteTrunkDocument> {
    match value {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| {
                if let Value::Object(mut obj) = item {
                    let name = obj
                        .remove("name")
                        .and_then(|v| sanitize_optional_string(v.as_str().map(|s| s.to_string())));
                    let weight = obj
                        .remove("weight")
                        .and_then(|v| v.as_i64())
                        .map(|w| w.clamp(0, MAX_PRIORITY as i64) as i32)
                        .unwrap_or(0);
                    name.map(|n| RouteTrunkDocument { name: n, weight })
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_notes_value(value: Option<Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| sanitize_optional_string(item.as_str().map(|s| s.to_string())))
            .collect(),
        Some(Value::String(note)) => sanitize_optional_string(Some(note)).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn value_from_map(map: &JsonMap<String, Value>) -> Option<Value> {
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map.clone()))
    }
}

fn value_from_trunks(trunks: &[RouteTrunkDocument]) -> Option<Value> {
    if trunks.is_empty() {
        None
    } else {
        Some(json!(trunks))
    }
}

fn resolve_trunk_id(lookup: &HashMap<String, i64>, name: Option<&str>) -> Option<i64> {
    name.and_then(|raw| {
        lookup
            .get(raw)
            .copied()
            .or_else(|| lookup.get(&raw.to_ascii_lowercase()).copied())
    })
}

fn build_trunk_name_lookup(trunks: &[SipTrunkModel]) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    for trunk in trunks {
        map.insert(trunk.name.clone(), trunk.id);
        map.insert(trunk.name.to_ascii_lowercase(), trunk.id);
    }
    map
}

fn notes_value(notes: &[String]) -> Option<Value> {
    if notes.is_empty() {
        None
    } else {
        Some(json!(notes))
    }
}

fn format_timestamp(value: NaiveDateTime) -> String {
    Utc.from_utc_datetime(&value).to_rfc3339()
}

async fn load_trunks(db: &DatabaseConnection) -> Result<Vec<SipTrunkModel>, DbErr> {
    SipTrunkEntity::find()
        .order_by_asc(SipTrunkColumn::Name)
        .all(db)
        .await
}

fn build_trunk_options(trunks: &[SipTrunkModel]) -> Vec<Value> {
    trunks
        .iter()
        .map(|trunk| {
            json!({
                "id": trunk.id,
                "name": trunk.name,
                "display_name": trunk
                    .display_name
                    .clone()
                    .unwrap_or_else(|| trunk.name.clone()),
                "carrier": trunk.carrier.clone().unwrap_or_default(),
                "status": trunk.status,
                "direction": trunk.direction,
            })
        })
        .collect()
}

fn build_route_console_payload(
    state: &ConsoleState,
    doc: &RouteDocument,
    model: &RoutingModel,
) -> Value {
    let status = if doc.disabled { "paused" } else { "active" };
    json!({
        "id": model.id,
        "name": doc.name,
        "description": doc.description.clone().unwrap_or_default(),
        "owner": doc.owner.clone().unwrap_or_default(),
        "direction": doc.direction,
        "priority": doc.priority,
        "disabled": doc.disabled,
        "match": doc.matchers.clone(),
        "rewrite": doc.rewrite.clone(),
        "action": {
            "select": doc.action.select.clone(),
            "hash_key": doc.action.hash_key.clone(),
            "trunks": doc.action.trunks.clone(),
        },
        "source_trunk": doc.source_trunk.clone(),
        "target_trunks": doc.target_trunks.clone(),
        "notes": doc.notes.clone(),
        "created_at": format_timestamp(model.created_at),
        "last_modified": format_timestamp(model.updated_at),
        "last_deploy": model.last_deployed_at.map(format_timestamp),
        "status": status,
        "detail_url": state.url_for(&format!("/routing/{}", model.id)),
        "edit_url": state.url_for(&format!("/routing/{}", model.id)),
        "delete_url": state.url_for(&format!("/routing/{}/delete", model.id)),
    })
}

fn build_routes_summary(routes: &[RoutingModel]) -> Value {
    let total_routes = routes.len();
    let active_routes = routes.iter().filter(|route| route.is_active).count();
    let last_deploy = routes
        .iter()
        .filter_map(|route| route.last_deployed_at)
        .max()
        .map(format_timestamp);

    json!({
        "total_routes": total_routes,
        "active_routes": active_routes,
        "last_deploy": last_deploy,
    })
}

fn selection_algorithms() -> Vec<Value> {
    vec![
        json!({"value": "rr", "label": "Round robin"}),
        json!({"value": "weight", "label": "Weighted"}),
        json!({"value": "hash", "label": "Deterministic hash"}),
    ]
}

fn render_route_form(
    state: &ConsoleState,
    mode: &str,
    doc: &RouteDocument,
    trunks: &[SipTrunkModel],
    error_message: Option<String>,
    form_action: String,
) -> Response {
    let route_value = serde_json::to_value(doc).unwrap_or(Value::Null);
    let trunk_options = Value::Array(build_trunk_options(trunks));
    let selection_algorithms = Value::Array(selection_algorithms());
    let direction_options = json!(ALLOWED_DIRECTIONS);
    let status_options = json!([
        {"value": false, "label": "Active"},
        {"value": true, "label": "Paused"},
    ]);
    let submit_label = if mode == "edit" {
        "Save changes"
    } else {
        "Create routing rule"
    };
    let page_title = if mode == "edit" {
        format!("Edit routing · {}", doc.display_label(doc.id))
    } else {
        "Create routing rule".to_string()
    };

    state.render(
        "console/routing_form.html",
        json!({
            "nav_active": "routing",
            "mode": mode,
            "page_title": page_title,
            "submit_label": submit_label,
            "route_data": route_value,
            "trunk_options": trunk_options,
            "selection_algorithms": selection_algorithms,
            "direction_options": direction_options,
            "status_options": status_options,
            "form_action": form_action,
            "back_url": state.url_for("/routing"),
            "error_message": error_message,
        }),
    )
}

fn apply_document_to_active(
    active: &mut RoutingActiveModel,
    doc: &RouteDocument,
    trunk_lookup: &HashMap<String, i64>,
    now: NaiveDateTime,
) {
    active.name = Set(doc.name.clone());
    active.description = Set(doc.description.clone());
    active.owner = Set(doc.owner.clone());
    active.direction = Set(doc.direction.clone());
    active.priority = Set(doc.priority);
    active.is_active = Set(!doc.disabled);
    active.selection_strategy = Set(doc.action.select.clone());
    active.hash_key = Set(doc.action.hash_key.clone());
    active.header_filters = Set(value_from_map(&doc.matchers));
    active.rewrite_rules = Set(value_from_map(&doc.rewrite));
    active.target_trunks = Set(value_from_trunks(&doc.action.trunks));
    active.source_trunk_id = Set(resolve_trunk_id(trunk_lookup, doc.source_trunk.as_deref()));
    let default_target = doc.target_trunks.first().map(|name| name.as_str());
    active.default_trunk_id = Set(resolve_trunk_id(trunk_lookup, default_target));
    active.notes = Set(notes_value(&doc.notes));
    let metadata = doc.metadata_value();
    active.metadata = Set(if metadata.is_null() {
        None
    } else {
        Some(metadata)
    });
    active.updated_at = Set(now);
}

pub async fn page_routing(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let routes = match RoutingEntity::find()
        .order_by_asc(RoutingColumn::Priority)
        .order_by_asc(RoutingColumn::Name)
        .all(db)
        .await
    {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load routes: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load routing rules",
            )
                .into_response();
        }
    };

    let trunk_map: HashMap<i64, SipTrunkModel> = match load_trunks(db).await {
        Ok(trunks) => trunks.into_iter().map(|trunk| (trunk.id, trunk)).collect(),
        Err(err) => {
            warn!("failed to load trunks for routing list: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load routing rules",
            )
                .into_response();
        }
    };

    let mut payload_routes = Vec::with_capacity(routes.len());
    for model in &routes {
        let mut doc = RouteDocument::from_model(model);
        doc.apply_trunk_context(model, &trunk_map);
        payload_routes.push(build_route_console_payload(state.as_ref(), &doc, model));
    }

    let summary = build_routes_summary(&routes);

    state.render(
        "console/routing.html",
        json!({
            "nav_active": "routing",
            "routing_data": json!({
                "routes": payload_routes,
                "summary": summary,
            }),
            "create_url": state.url_for("/routing/new"),
        }),
    )
}

pub async fn page_routing_create(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let trunks = match load_trunks(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load trunks for route create page: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load routing form",
            )
                .into_response();
        }
    };

    let doc = RouteDocument::default();
    render_route_form(
        state.as_ref(),
        "create",
        &doc,
        &trunks,
        None,
        state.url_for("/routing"),
    )
}

pub async fn page_routing_edit(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match RoutingEntity::find_by_id(id).one(db).await {
        Ok(Some(route)) => route,
        Ok(None) => return (StatusCode::NOT_FOUND, "Route not found").into_response(),
        Err(err) => {
            warn!("failed to load route {} for edit: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load routing rule",
            )
                .into_response();
        }
    };

    let trunks = match load_trunks(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load trunks for route edit page: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load routing form",
            )
                .into_response();
        }
    };

    let trunk_map: HashMap<i64, SipTrunkModel> = trunks
        .iter()
        .cloned()
        .map(|trunk| (trunk.id, trunk))
        .collect();

    let mut doc = RouteDocument::from_model(&model);
    doc.apply_trunk_context(&model, &trunk_map);

    render_route_form(
        state.as_ref(),
        "edit",
        &doc,
        &trunks,
        None,
        state.url_for(&format!("/routing/{}", id)),
    )
}

pub async fn create_routing(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<RoutingForm>,
) -> Response {
    let db = state.db();
    let trunks = match load_trunks(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load trunks for route create: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create routing rule",
            )
                .into_response();
        }
    };
    let form_action = state.url_for("/routing");

    if trunks.is_empty() {
        return render_route_form(
            state.as_ref(),
            "create",
            &RouteDocument::default(),
            &trunks,
            Some("Add at least one SIP trunk before creating a route".to_string()),
            form_action,
        );
    }

    let doc_result = form.parse_document();
    let doc = match doc_result {
        Ok(doc) => doc,
        Err(err) => {
            return render_route_form(
                state.as_ref(),
                "create",
                &RouteDocument::default(),
                &trunks,
                Some(err.message().to_string()),
                form_action,
            );
        }
    };

    if let Err(err) = doc.validate() {
        return render_route_form(
            state.as_ref(),
            "create",
            &doc,
            &trunks,
            Some(err.message().to_string()),
            form_action,
        );
    }

    let trunk_lookup = build_trunk_name_lookup(&trunks);

    let source_id = resolve_trunk_id(&trunk_lookup, doc.source_trunk.as_deref());
    if source_id.is_none() {
        let message = match doc.source_trunk.as_ref() {
            Some(name) if !name.is_empty() => {
                format!("Source trunk \"{}\" was not found", name)
            }
            _ => "Source trunk is required".to_string(),
        };
        return render_route_form(
            state.as_ref(),
            "create",
            &doc,
            &trunks,
            Some(message),
            form_action,
        );
    }

    let tx = match db.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            warn!("failed to start transaction for route create: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create routing rule",
            )
                .into_response();
        }
    };

    match RoutingEntity::find()
        .filter(RoutingColumn::Name.eq(doc.name.clone()))
        .one(&tx)
        .await
    {
        Ok(Some(_)) => {
            let _ = tx.rollback().await;
            return render_route_form(
                state.as_ref(),
                "create",
                &doc,
                &trunks,
                Some("Route name already exists".to_string()),
                form_action,
            );
        }
        Ok(None) => {}
        Err(err) => {
            warn!("failed to check route uniqueness: {}", err);
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create routing rule",
            )
                .into_response();
        }
    }

    let now = Utc::now().naive_utc();
    let mut active: RoutingActiveModel = Default::default();
    active.created_at = Set(now);
    apply_document_to_active(&mut active, &doc, &trunk_lookup, now);

    if let Err(err) = active.insert(&tx).await {
        warn!("failed to insert routing rule {}: {}", doc.name, err);
        let _ = tx.rollback().await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create routing rule",
        )
            .into_response();
    }

    if let Err(err) = tx.commit().await {
        warn!("failed to commit routing rule create {}: {}", doc.name, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create routing rule",
        )
            .into_response();
    }

    Redirect::to(&state.url_for("/routing")).into_response()
}

pub async fn update_routing(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<RoutingForm>,
) -> Response {
    let db = state.db();

    let model = match RoutingEntity::find_by_id(id).one(db).await {
        Ok(Some(route)) => route,
        Ok(None) => return (StatusCode::NOT_FOUND, "Route not found").into_response(),
        Err(err) => {
            warn!("failed to load route {} for update: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to update routing rule",
            )
                .into_response();
        }
    };

    let trunks = match load_trunks(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load trunks for route update: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to update routing rule",
            )
                .into_response();
        }
    };
    let trunk_map_by_id: HashMap<i64, SipTrunkModel> = trunks
        .iter()
        .cloned()
        .map(|trunk| (trunk.id, trunk))
        .collect();
    let trunk_lookup = build_trunk_name_lookup(&trunks);
    let form_action = state.url_for(&format!("/routing/{}", id));

    let doc_result = form.parse_document();
    let doc = match doc_result {
        Ok(mut doc) => {
            doc.id = Some(id);
            doc.ensure_consistency();
            doc
        }
        Err(err) => {
            let mut existing_doc = RouteDocument::from_model(&model);
            existing_doc.apply_trunk_context(&model, &trunk_map_by_id);
            return render_route_form(
                state.as_ref(),
                "edit",
                &existing_doc,
                &trunks,
                Some(err.message().to_string()),
                form_action,
            );
        }
    };

    if let Err(err) = doc.validate() {
        return render_route_form(
            state.as_ref(),
            "edit",
            &doc,
            &trunks,
            Some(err.message().to_string()),
            form_action,
        );
    }

    if trunks.is_empty() {
        return render_route_form(
            state.as_ref(),
            "edit",
            &doc,
            &trunks,
            Some("Add at least one SIP trunk before editing routes".to_string()),
            form_action,
        );
    }

    if resolve_trunk_id(&trunk_lookup, doc.source_trunk.as_deref()).is_none() {
        let message = match doc.source_trunk.as_ref() {
            Some(name) if !name.is_empty() => {
                format!("Source trunk \"{}\" was not found", name)
            }
            _ => "Source trunk is required".to_string(),
        };
        return render_route_form(
            state.as_ref(),
            "edit",
            &doc,
            &trunks,
            Some(message),
            form_action,
        );
    }

    let tx = match db.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            warn!(
                "failed to start transaction for route update {}: {}",
                id, err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to update routing rule",
            )
                .into_response();
        }
    };

    match RoutingEntity::find()
        .filter(RoutingColumn::Name.eq(doc.name.clone()))
        .one(&tx)
        .await
    {
        Ok(Some(existing)) if existing.id != id => {
            let _ = tx.rollback().await;
            return render_route_form(
                state.as_ref(),
                "edit",
                &doc,
                &trunks,
                Some("Route name already exists".to_string()),
                form_action,
            );
        }
        Ok(_) => {}
        Err(err) => {
            warn!(
                "failed to check route uniqueness for update {}: {}",
                id, err
            );
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to update routing rule",
            )
                .into_response();
        }
    }

    let mut active: RoutingActiveModel = model.clone().into();
    let now = Utc::now().naive_utc();
    apply_document_to_active(&mut active, &doc, &trunk_lookup, now);

    if let Err(err) = active.update(&tx).await {
        warn!("failed to update routing rule {}: {}", id, err);
        let _ = tx.rollback().await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update routing rule",
        )
            .into_response();
    }

    if let Err(err) = tx.commit().await {
        warn!("failed to commit routing rule update {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update routing rule",
        )
            .into_response();
    }

    Redirect::to(&state.url_for("/routing")).into_response()
}

pub async fn delete_routing(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    match RoutingEntity::delete_by_id(id).exec(db).await {
        Ok(result) => {
            if result.rows_affected == 0 {
                (StatusCode::NOT_FOUND, "Route not found").into_response()
            } else {
                Redirect::to(&state.url_for("/routing")).into_response()
            }
        }
        Err(err) => {
            warn!("failed to delete routing rule {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to delete routing rule",
            )
                .into_response()
        }
    }
}
