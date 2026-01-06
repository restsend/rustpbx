use crate::addons::queue::models::{
    Column as QueueColumn, Entity as QueueEntity, Model as QueueModel,
};
use crate::addons::queue::services::utils as queue_utils;
use crate::console::handlers::{bad_request, forms};
use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::models::{
    routing::{
        ActiveModel as RoutingActiveModel, Column as RoutingColumn, Entity as RoutingEntity,
        Model as RoutingModel, RoutingDirection, RoutingSelectionStrategy,
    },
    sip_trunk::{Column as SipTrunkColumn, Entity as SipTrunkEntity, Model as SipTrunkModel},
};
use axum::{
    Router,
    extract::{Json, Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, DatabaseConnection, DbErr,
    EntityTrait, Iterable, PaginatorTrait, QueryFilter, QueryOrder, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tracing::warn;

const DEFAULT_PRIORITY: i32 = 100;
const MAX_PRIORITY: i32 = 10_000;
const DEFAULT_DIRECTION: RoutingDirection = RoutingDirection::Outbound;
const DEFAULT_SELECTION: RoutingSelectionStrategy = RoutingSelectionStrategy::RoundRobin;

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route(
            "/routing",
            get(page_routing).post(query_routing).put(create_routing),
        )
        .route("/routing/new", get(page_routing_create))
        .route(
            "/routing/{id}",
            get(page_routing_edit)
                .patch(update_routing)
                .delete(delete_routing),
        )
        .route("/routing/{id}/clone", post(clone_routing))
        .route("/routing/{id}/toggle", post(toggle_routing))
        .route("/routing/{id}/data", get(route_detail_data))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RouteDocument {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default = "default_direction_value")]
    direction: RoutingDirection,
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
    notes: Vec<String>,
}

impl Default for RouteDocument {
    fn default() -> Self {
        Self {
            id: None,
            name: String::new(),
            description: None,
            owner: None,
            direction: DEFAULT_DIRECTION,
            priority: DEFAULT_PRIORITY,
            disabled: false,
            matchers: JsonMap::new(),
            rewrite: JsonMap::new(),
            action: RouteActionDocument::default(),
            source_trunk: None,
            notes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RouteTargetKind {
    SipTrunk,
    Queue,
}

impl Default for RouteTargetKind {
    fn default() -> Self {
        Self::SipTrunk
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RouteActionDocument {
    select: RoutingSelectionStrategy,
    #[serde(default)]
    hash_key: Option<String>,
    #[serde(default)]
    trunks: Vec<RouteTrunkDocument>,
    #[serde(default)]
    target_type: RouteTargetKind,
    #[serde(default)]
    queue_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RouteTrunkDocument {
    name: String,
    weight: i32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct QueryRoutingFilters {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    selection: Option<String>,
    #[serde(default)]
    owner: Option<String>,
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

impl Default for RouteActionDocument {
    fn default() -> Self {
        Self {
            select: DEFAULT_SELECTION,
            hash_key: None,
            trunks: Vec::new(),
            target_type: RouteTargetKind::SipTrunk,
            queue_file: None,
        }
    }
}
impl RouteDocument {
    fn from_model(model: &RoutingModel) -> Self {
        if let Some(meta) = model.metadata.clone() {
            if let Ok(mut doc) = serde_json::from_value::<RouteDocument>(meta) {
                doc.id = Some(model.id);
                doc.name = model.name.clone();
                doc.description = model.description.clone();
                doc.owner = model.owner.clone();
                doc.direction = model.direction;
                doc.priority = model.priority;
                doc.disabled = !model.is_active;
                doc.action.select = model.selection_strategy;
                doc.action.hash_key = if doc.action.select == RoutingSelectionStrategy::Hash {
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
            direction: model.direction,
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
                select: model.selection_strategy,
                hash_key: if model.selection_strategy == RoutingSelectionStrategy::Hash {
                    sanitize_optional_string(model.hash_key.clone())
                } else {
                    None
                },
                trunks: parse_trunk_assignments(model.target_trunks.clone()),
                ..RouteActionDocument::default()
            },
            source_trunk: None,
            notes: parse_notes_value(model.notes.clone()),
        };
        doc.ensure_consistency();
        doc
    }

    fn validate(&self) -> Result<(), RouteError> {
        if self.name.trim().is_empty() {
            return Err(RouteError::new("Route name is required"));
        }
        match self.action.target_type {
            RouteTargetKind::SipTrunk => {}
            RouteTargetKind::Queue => {
                let has_queue_file = self
                    .action
                    .queue_file
                    .as_ref()
                    .and_then(|value| sanitize_optional_string(Some(value.clone())));
                if has_queue_file.is_none() {
                    return Err(RouteError::new(
                        "Queue destination requires a queue file reference",
                    ));
                }
            }
        }
        Ok(())
    }

    fn ensure_consistency(&mut self) {
        if self.action.select != RoutingSelectionStrategy::Hash {
            self.action.hash_key = None;
        }

        self.action.queue_file = sanitize_optional_string(self.action.queue_file.take());

        match self.action.target_type {
            RouteTargetKind::SipTrunk => {
                self.action.queue_file = None;
            }
            RouteTargetKind::Queue => {
                self.action.trunks.clear();
                self.action.select = DEFAULT_SELECTION;
                self.action.hash_key = None;
            }
        }

        let mut dedup = HashSet::new();
        let mut unique_trunks = Vec::new();

        for trunk in std::mem::take(&mut self.action.trunks) {
            if dedup.insert(trunk.name.clone()) {
                unique_trunks.push(trunk);
            }
        }
        self.action.trunks = unique_trunks;

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

        if !matches!(self.action.target_type, RouteTargetKind::SipTrunk) {
            return;
        }

        if self.action.trunks.is_empty() {
            if let Some(default_id) = model.default_trunk_id {
                if let Some(trunk) = trunks.get(&default_id) {
                    if !self
                        .action
                        .trunks
                        .iter()
                        .any(|t| t.name.eq_ignore_ascii_case(&trunk.name))
                    {
                        self.action.trunks.push(RouteTrunkDocument {
                            name: trunk.name.clone(),
                            weight: DEFAULT_PRIORITY,
                        });
                    }
                }
            }
            self.ensure_consistency();
        }
    }
}

fn default_direction_value() -> RoutingDirection {
    DEFAULT_DIRECTION
}

fn default_priority_value() -> i32 {
    DEFAULT_PRIORITY
}

fn sanitize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn normalize_selection(value: Option<String>) -> RoutingSelectionStrategy {
    value
        .and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
            "rr" | "round_robin" | "round-robin" => Some(RoutingSelectionStrategy::RoundRobin),
            "weight" | "weighted" => Some(RoutingSelectionStrategy::Weighted),
            "hash" => Some(RoutingSelectionStrategy::Hash),
            _ => None,
        })
        .unwrap_or(DEFAULT_SELECTION)
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

async fn generate_clone_name(db: &DatabaseConnection, original: &str) -> Result<String, DbErr> {
    let trimmed = original.trim();
    let base = if trimmed.is_empty() {
        "Route copy".to_string()
    } else {
        format!("{trimmed} (copy)")
    };

    let mut candidate = base.clone();
    let mut suffix = 2;
    loop {
        let existing = RoutingEntity::find()
            .filter(RoutingColumn::Name.eq(candidate.clone()))
            .one(db)
            .await?;
        if existing.is_none() {
            return Ok(candidate);
        }
        candidate = format!("{base} {suffix}");
        suffix += 1;
    }
}

async fn load_trunks(db: &DatabaseConnection) -> Result<Vec<SipTrunkModel>, DbErr> {
    SipTrunkEntity::find()
        .order_by_asc(SipTrunkColumn::Name)
        .all(db)
        .await
}

async fn load_queues(db: &DatabaseConnection) -> Result<Vec<QueueModel>, DbErr> {
    QueueEntity::find()
        .order_by_asc(QueueColumn::Name)
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

fn build_queue_options(queues: &[QueueModel]) -> Vec<Value> {
    queues
        .iter()
        .filter_map(|queue| match queue_utils::convert_queue_model(queue.clone()) {
            Ok(entry) => Some(json!({
                "id": queue.id,
                "name": entry.name,
                "description": entry.description,
                "is_active": queue.is_active,
                "updated_at": queue.updated_at.to_rfc3339(),
                "file_name": entry.file_name(),
            })),
            Err(err) => {
                warn!(queue = %queue.name, error = %err, "failed to convert queue for routing options");
                None
            }
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
            "select": doc.action.select,
            "hash_key": doc.action.hash_key.clone(),
            "trunks": doc.action.trunks.clone(),
            "target_type": doc.action.target_type,
            "queue_file": doc.action.queue_file.clone(),
        },
        "source_trunk": doc.source_trunk.clone(),
        "target_trunks": doc.action.trunks.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
        "notes": doc.notes.clone(),
        "created_at": model.created_at.to_rfc3339(),
        "last_modified": model.updated_at.to_rfc3339(),
        "last_deploy": model.last_deployed_at.map(|t|t.to_rfc3339()),
        "status": status,
        "detail_url": state.url_for(&format!("/routing/{}", model.id)),
        "edit_url": state.url_for(&format!("/routing/{}", model.id)),
        "delete_url": state.url_for(&format!("/routing/{}", model.id)),
        "toggle_url": state.url_for(&format!("/routing/{}/toggle", model.id)),
        "clone_url": state.url_for(&format!("/routing/{}/clone", model.id)),
    })
}

fn build_routes_summary(routes: &[RoutingModel]) -> Value {
    let total_routes = routes.len();
    let active_routes = routes.iter().filter(|route| route.is_active).count();
    let last_deploy = routes
        .iter()
        .filter_map(|route| route.last_deployed_at)
        .max()
        .map(|t| t.to_rfc3339());

    json!({
        "total_routes": total_routes,
        "active_routes": active_routes,
        "last_deploy": last_deploy,
    })
}

fn selection_algorithms() -> Vec<Value> {
    RoutingSelectionStrategy::iter()
        .map(|strategy| {
            let label = match strategy {
                RoutingSelectionStrategy::RoundRobin => "Round robin",
                RoutingSelectionStrategy::Weighted => "Weighted",
                RoutingSelectionStrategy::Hash => "Deterministic hash",
            };
            json!({
                "value": strategy.as_str(),
                "label": label,
            })
        })
        .collect()
}

fn selection_algorithms_value() -> Value {
    Value::Array(selection_algorithms())
}

fn direction_options_value() -> Value {
    json!(
        RoutingDirection::iter()
            .map(|direction| direction.as_str())
            .collect::<Vec<_>>()
    )
}

fn status_options_value() -> Value {
    json!([
        {"value": false, "label": "Active"},
        {"value": true, "label": "Paused"},
    ])
}

fn render_route_form(
    state: &ConsoleState,
    mode: &str,
    doc: &RouteDocument,
    trunks: &[SipTrunkModel],
    queues: &[QueueModel],
    error_message: Option<String>,
    form_action: String,
) -> Response {
    let route_value = serde_json::to_value(doc).unwrap_or(Value::Null);
    let trunk_options = Value::Array(build_trunk_options(trunks));
    let queue_options = Value::Array(build_queue_options(queues));
    let selection_algorithms = selection_algorithms_value();
    let direction_options = direction_options_value();
    let status_options = status_options_value();
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

    let script_path = if mode == "edit" {
        if let Some(id) = doc.id {
            format!("/console/routing/{}", id)
        } else {
            "/console/routing/edit".to_string()
        }
    } else {
        "/console/routing/new".to_string()
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
            "queue_options": queue_options,
            "selection_algorithms": selection_algorithms,
            "direction_options": direction_options,
            "status_options": status_options,
            "form_action": form_action,
            "back_url": state.url_for("/routing"),
            "error_message": error_message,
            "addon_scripts": state.get_injected_scripts(&script_path),
        }),
    )
}

fn apply_document_to_active(
    active: &mut RoutingActiveModel,
    doc: &RouteDocument,
    trunk_lookup: &HashMap<String, i64>,
    now: DateTime<Utc>,
) {
    active.name = Set(doc.name.clone());
    active.description = Set(doc.description.clone());
    active.owner = Set(doc.owner.clone());
    active.direction = Set(doc.direction);
    active.priority = Set(doc.priority);
    active.is_active = Set(!doc.disabled);
    active.selection_strategy = Set(doc.action.select);
    active.hash_key = Set(doc.action.hash_key.clone());
    active.header_filters = Set(value_from_map(&doc.matchers));
    active.rewrite_rules = Set(value_from_map(&doc.rewrite));
    active.target_trunks = Set(value_from_trunks(&doc.action.trunks));
    active.source_trunk_id = Set(resolve_trunk_id(trunk_lookup, doc.source_trunk.as_deref()));
    let default_target = doc.action.trunks.first().map(|t| t.name.as_str());
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
    let trunk_options = match load_trunks(db).await {
        Ok(trunks) => Value::Array(build_trunk_options(&trunks)),
        Err(err) => {
            warn!("failed to load trunks for routing filters: {}", err);
            Value::Array(vec![])
        }
    };

    state.render(
        "console/routing.html",
        json!({
            "nav_active": "routing",
            "filters": {
                "trunks": trunk_options,
                "selection_algorithms": selection_algorithms_value(),
                "direction_options": direction_options_value(),
                "status_options": status_options_value(),
            },
            "create_url": state.url_for("/routing/new"),
        }),
    )
}

pub(crate) async fn query_routing(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<forms::ListQuery<QueryRoutingFilters>>,
) -> Response {
    let db = state.db();
    let mut selector = RoutingEntity::find()
        .order_by_asc(RoutingColumn::Priority)
        .order_by_asc(RoutingColumn::Name);

    if let Some(filters) = &payload.filters {
        if let Some(ref raw_q) = filters.q {
            let trimmed = raw_q.trim();
            if !trimmed.is_empty() {
                let mut condition = Condition::any();
                condition = condition.add(RoutingColumn::Name.contains(trimmed));
                condition = condition.add(RoutingColumn::Description.contains(trimmed));
                condition = condition.add(RoutingColumn::Owner.contains(trimmed));
                selector = selector.filter(condition);
            }
        }

        if let Some(ref raw_direction) = filters.direction {
            let direction = match raw_direction.trim().to_ascii_lowercase().as_str() {
                "inbound" => Some(RoutingDirection::Inbound),
                "outbound" => Some(RoutingDirection::Outbound),
                _ => None,
            };
            if let Some(direction) = direction {
                selector = selector.filter(RoutingColumn::Direction.eq(direction));
            }
        }

        if let Some(ref raw_status) = filters.status {
            match raw_status.trim().to_ascii_lowercase().as_str() {
                "active" => selector = selector.filter(RoutingColumn::IsActive.eq(true)),
                "paused" | "disabled" => {
                    selector = selector.filter(RoutingColumn::IsActive.eq(false))
                }
                _ => {}
            }
        }

        if let Some(ref raw_selection) = filters.selection {
            let strategy = normalize_selection(Some(raw_selection.clone()));
            selector = selector.filter(RoutingColumn::SelectionStrategy.eq(strategy));
        }

        if let Some(ref owner) = filters.owner {
            let trimmed = owner.trim();
            if !trimmed.is_empty() {
                selector = selector.filter(RoutingColumn::Owner.contains(trimmed));
            }
        }
    }

    let trunks = match load_trunks(db).await {
        Ok(trunks) => trunks,
        Err(err) => {
            warn!("failed to load trunks for routing query: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load routing data"})),
            )
                .into_response();
        }
    };
    let trunk_map: HashMap<i64, SipTrunkModel> =
        trunks.iter().cloned().map(|t| (t.id, t)).collect();

    let summary_routes = match selector.clone().all(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load routes for summary: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load routing data"})),
            )
                .into_response();
        }
    };

    let (_, per_page) = payload.normalize();
    let paginator = selector.clone().paginate(db, per_page);
    let pagination = match forms::paginate(paginator, &payload).await {
        Ok(pagination) => pagination,
        Err(err) => {
            warn!("failed to paginate routing list: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load routing data"})),
            )
                .into_response();
        }
    };

    let items: Vec<Value> = pagination
        .items
        .into_iter()
        .map(|model| {
            let mut doc = RouteDocument::from_model(&model);
            doc.apply_trunk_context(&model, &trunk_map);
            build_route_console_payload(state.as_ref(), &doc, &model)
        })
        .collect();

    Json(json!({
        "page": pagination.current_page,
        "per_page": pagination.per_page,
        "total_pages": pagination.total_pages,
        "total_items": pagination.total_items,
        "items": items,
        "summary": build_routes_summary(&summary_routes),
        "filters": {
            "trunks": build_trunk_options(&trunks),
            "selection_algorithms": selection_algorithms(),
            "direction_options": direction_options_value(),
            "status_options": status_options_value(),
        },
    }))
    .into_response()
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
    let queues = match load_queues(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load queues for route create page: {}", err);
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
        &queues,
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

    let queues = match load_queues(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load queues for route edit page: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load routing form",
            )
                .into_response();
        }
    };

    let mut doc = RouteDocument::from_model(&model);
    doc.apply_trunk_context(&model, &trunk_map);

    render_route_form(
        state.as_ref(),
        "edit",
        &doc,
        &trunks,
        &queues,
        None,
        state.url_for(&format!("/routing/{}", id)),
    )
}

pub async fn route_detail_data(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match RoutingEntity::find_by_id(id).one(db).await {
        Ok(Some(route)) => route,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Routing rule not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load route {} detail data: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load routing data"})),
            )
                .into_response();
        }
    };

    let trunks = match load_trunks(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load trunks for route detail {}: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load routing data"})),
            )
                .into_response();
        }
    };

    let queues = match load_queues(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load queues for route detail {}: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load routing data"})),
            )
                .into_response();
        }
    };

    let trunk_map: HashMap<i64, SipTrunkModel> =
        trunks.iter().cloned().map(|t| (t.id, t)).collect();
    let mut doc = RouteDocument::from_model(&model);
    doc.apply_trunk_context(&model, &trunk_map);

    Json(json!({
        "route": doc,
        "trunk_options": build_trunk_options(&trunks),
        "queue_options": build_queue_options(&queues),
        "selection_algorithms": selection_algorithms(),
        "direction_options": direction_options_value(),
        "status_options": status_options_value(),
        "meta": {
            "update_url": state.url_for(&format!("/routing/{}", id)),
            "delete_url": state.url_for(&format!("/routing/{}", id)),
            "detail_url": state.url_for(&format!("/routing/{}", id)),
            "toggle_url": state.url_for(&format!("/routing/{}/toggle", id)),
            "clone_url": state.url_for(&format!("/routing/{}/clone", id)),
        }
    }))
    .into_response()
}

pub async fn clone_routing(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match RoutingEntity::find_by_id(id).one(db).await {
        Ok(Some(route)) => route,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Route not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load route {} for clone: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to clone routing rule"})),
            )
                .into_response();
        }
    };

    let mut doc = RouteDocument::from_model(&model);
    doc.id = None;

    let name = match generate_clone_name(db, &doc.name).await {
        Ok(name) => name,
        Err(err) => {
            warn!("failed to generate clone name for route {}: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to clone routing rule"})),
            )
                .into_response();
        }
    };
    doc.name = name;
    doc.ensure_consistency();

    if let Err(err) = doc.validate() {
        return bad_request(err.message().to_string());
    }

    let trunks = match load_trunks(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load trunks for route clone {}: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to clone routing rule"})),
            )
                .into_response();
        }
    };
    let trunk_lookup = build_trunk_name_lookup(&trunks);

    if let Err(err) = load_queues(db).await {
        warn!("failed to load queues for route clone {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": "Failed to clone routing rule"})),
        )
            .into_response();
    }

    doc.source_trunk = sanitize_optional_string(doc.source_trunk.take());
    if let Some(ref name) = doc.source_trunk {
        if resolve_trunk_id(&trunk_lookup, Some(name.as_str())).is_none() {
            return bad_request(format!("Source trunk \"{}\" was not found", name));
        }
    }

    let tx = match db.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            warn!(
                "failed to start transaction for route clone {}: {}",
                id, err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to clone routing rule"})),
            )
                .into_response();
        }
    };

    let now = Utc::now();
    let mut active: RoutingActiveModel = Default::default();
    active.created_at = Set(now);
    apply_document_to_active(&mut active, &doc, &trunk_lookup, now);

    let new_model = match active.insert(&tx).await {
        Ok(model) => model,
        Err(err) => {
            warn!("failed to insert cloned routing rule from {}: {}", id, err);
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to clone routing rule"})),
            )
                .into_response();
        }
    };

    if let Err(err) = tx.commit().await {
        warn!("failed to commit cloned routing rule from {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": "Failed to clone routing rule"})),
        )
            .into_response();
    }

    Json(json!({"status": "ok", "id": new_model.id, "name": new_model.name})).into_response()
}

pub async fn toggle_routing(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match RoutingEntity::find_by_id(id).one(db).await {
        Ok(Some(route)) => route,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Route not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load route {} for toggle: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to toggle routing rule"})),
            )
                .into_response();
        }
    };

    let mut doc = RouteDocument::from_model(&model);
    doc.disabled = !doc.disabled;
    doc.ensure_consistency();

    let new_is_active = !model.is_active;
    let mut active: RoutingActiveModel = model.into();
    active.is_active = Set(new_is_active);
    active.updated_at = Set(Utc::now());
    let metadata = doc.metadata_value();
    active.metadata = Set(if metadata.is_null() {
        None
    } else {
        Some(metadata)
    });

    match active.update(db).await {
        Ok(updated) => {
            let disabled = !updated.is_active;
            Json(json!({
                "status": "ok",
                "id": id,
                "disabled": disabled,
                "is_active": updated.is_active,
            }))
            .into_response()
        }
        Err(err) => {
            warn!("failed to toggle routing rule {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to toggle routing rule"})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn create_routing(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(mut doc): Json<RouteDocument>,
) -> Response {
    let db = state.db();
    doc.id = None;
    doc.ensure_consistency();

    if let Err(err) = doc.validate() {
        return bad_request(err.message().to_string());
    }

    let trunks = match load_trunks(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load trunks for route create: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to create routing rule"})),
            )
                .into_response();
        }
    };

    let trunk_lookup = build_trunk_name_lookup(&trunks);
    doc.source_trunk = sanitize_optional_string(doc.source_trunk.take());
    if let Some(ref name) = doc.source_trunk {
        if resolve_trunk_id(&trunk_lookup, Some(name.as_str())).is_none() {
            return bad_request(format!("Source trunk \"{}\" was not found", name));
        }
    }

    let tx = match db.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            warn!("failed to start transaction for route create: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to create routing rule"})),
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
            return bad_request("Route name already exists");
        }
        Ok(None) => {}
        Err(err) => {
            warn!("failed to check route uniqueness: {}", err);
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to create routing rule"})),
            )
                .into_response();
        }
    }

    let now = Utc::now();
    let mut active: RoutingActiveModel = Default::default();
    active.created_at = Set(now);
    apply_document_to_active(&mut active, &doc, &trunk_lookup, now);

    let model = match active.insert(&tx).await {
        Ok(model) => model,
        Err(err) => {
            warn!("failed to insert routing rule {}: {}", doc.name, err);
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to create routing rule"})),
            )
                .into_response();
        }
    };

    if let Err(err) = tx.commit().await {
        warn!("failed to commit routing rule create {}: {}", doc.name, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": "Failed to create routing rule"})),
        )
            .into_response();
    }

    Json(json!({"status": "ok", "id": model.id})).into_response()
}

pub(crate) async fn update_routing(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(mut doc): Json<RouteDocument>,
) -> Response {
    let db = state.db();

    let model = match RoutingEntity::find_by_id(id).one(db).await {
        Ok(Some(route)) => route,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Route not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load route {} for update: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to update routing rule"})),
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
                Json(json!({"message": "Failed to update routing rule"})),
            )
                .into_response();
        }
    };

    let trunk_lookup = build_trunk_name_lookup(&trunks);

    doc.id = Some(id);
    doc.ensure_consistency();

    if let Err(err) = doc.validate() {
        return bad_request(err.message().to_string());
    }

    doc.source_trunk = sanitize_optional_string(doc.source_trunk.take());
    if let Some(ref name) = doc.source_trunk {
        if resolve_trunk_id(&trunk_lookup, Some(name.as_str())).is_none() {
            return bad_request(format!("Source trunk \"{}\" was not found", name));
        }
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
                Json(json!({"message": "Failed to update routing rule"})),
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
            return bad_request("Route name already exists");
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
                Json(json!({"message": "Failed to update routing rule"})),
            )
                .into_response();
        }
    }

    let mut active: RoutingActiveModel = model.clone().into();
    let now = Utc::now();
    apply_document_to_active(&mut active, &doc, &trunk_lookup, now);

    if let Err(err) = active.update(&tx).await {
        warn!("failed to update routing rule {}: {}", id, err);
        let _ = tx.rollback().await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": "Failed to update routing rule"})),
        )
            .into_response();
    }

    if let Err(err) = tx.commit().await {
        warn!("failed to commit routing rule update {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": "Failed to update routing rule"})),
        )
            .into_response();
    }

    Json(json!({"status": "ok", "id": id})).into_response()
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
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"message": "Route not found"})),
                )
                    .into_response()
            } else {
                Json(json!({"status": "ok", "rows_affected": result.rows_affected})).into_response()
            }
        }
        Err(err) => {
            warn!("failed to delete routing rule {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to delete routing rule"})),
            )
                .into_response()
        }
    }
}
