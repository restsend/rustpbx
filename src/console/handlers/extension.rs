use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::models::{
    department::{
        ActiveModel as DepartmentActiveModel, Column as DepartmentColumn,
        Entity as DepartmentEntity, Model as DepartmentModel,
    },
    extension::{
        ActiveModel as ExtensionActiveModel, Column as ExtensionColumn, Entity as ExtensionEntity,
        Model as ExtensionModel,
    },
    extension_department::{
        ActiveModel as ExtensionDepartmentActiveModel, Column as ExtensionDepartmentColumn,
        Entity as ExtensionDepartmentEntity,
    },
};
use axum::{
    extract::{Form, Path as AxumPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use chrono::{NaiveDateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, DatabaseConnection,
    DatabaseTransaction, DbErr, EntityTrait, ModelTrait, PaginatorTrait, QueryFilter, QueryOrder,
    TransactionTrait,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tracing::error;

const DEFAULT_FORWARDING_TIMEOUT: i32 = 30;
const MIN_FORWARDING_TIMEOUT: i32 = 5;
const MAX_FORWARDING_TIMEOUT: i32 = 120;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExtensionListQuery {
    page: u32,
    per_page: u32,
}

impl Default for ExtensionListQuery {
    fn default() -> Self {
        Self {
            page: 1,
            per_page: 20,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExtensionForm {
    extension: String,
    display_name: Option<String>,
    department: Option<String>,
    email: Option<String>,
    login_allowed: Option<String>,
    voicemail_enabled: Option<String>,
    call_forwarding_mode: Option<String>,
    call_forwarding_destination: Option<String>,
    call_forwarding_timeout: Option<String>,
    sip_password: Option<String>,
    pin: Option<String>,
    caller_id_name: Option<String>,
    caller_id_number: Option<String>,
    outbound_caller_id: Option<String>,
    emergency_caller_id: Option<String>,
    notes: Option<String>,
}

impl Default for ExtensionForm {
    fn default() -> Self {
        Self {
            extension: String::new(),
            display_name: None,
            department: None,
            email: None,
            login_allowed: None,
            voicemail_enabled: None,
            call_forwarding_mode: None,
            call_forwarding_destination: None,
            call_forwarding_timeout: None,
            sip_password: None,
            pin: None,
            caller_id_name: None,
            caller_id_number: None,
            outbound_caller_id: None,
            emergency_caller_id: None,
            notes: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ExtensionDraft {
    extension: String,
    display_name: Option<String>,
    department: Option<String>,
    email: Option<String>,
    login_allowed: bool,
    voicemail_enabled: bool,
    call_forwarding_mode: String,
    call_forwarding_destination: Option<String>,
    call_forwarding_timeout: i32,
    sip_password: Option<String>,
    pin: Option<String>,
    caller_id_name: Option<String>,
    caller_id_number: Option<String>,
    outbound_caller_id: Option<String>,
    emergency_caller_id: Option<String>,
    notes: Option<String>,
}

impl ExtensionForm {
    fn into_draft(self) -> ExtensionDraft {
        ExtensionDraft {
            extension: self.extension.trim().to_string(),
            display_name: sanitize_string(&self.display_name),
            department: sanitize_string(&self.department),
            email: sanitize_string(&self.email),
            login_allowed: parse_bool_flag(self.login_allowed.as_ref()),
            voicemail_enabled: parse_bool_flag(self.voicemail_enabled.as_ref()),
            call_forwarding_mode: normalize_forwarding_mode(self.call_forwarding_mode.as_deref()),
            call_forwarding_destination: sanitize_forwarding_destination(
                self.call_forwarding_destination,
                self.call_forwarding_mode.as_deref(),
            ),
            call_forwarding_timeout: parse_forwarding_timeout(
                self.call_forwarding_timeout.as_ref(),
            ),
            sip_password: sanitize_string(&self.sip_password),
            pin: sanitize_string(&self.pin),
            caller_id_name: sanitize_string(&self.caller_id_name),
            caller_id_number: sanitize_string(&self.caller_id_number),
            outbound_caller_id: sanitize_string(&self.outbound_caller_id),
            emergency_caller_id: sanitize_string(&self.emergency_caller_id),
            notes: sanitize_string(&self.notes),
        }
    }
}

impl ExtensionDraft {
    fn to_value_for_create(&self) -> Value {
        json!({
            "id": Value::Null,
            "extension": self.extension,
            "display_name": self.display_name.clone().unwrap_or_default(),
            "department": self.department.clone().unwrap_or_default(),
            "email": self.email.clone().unwrap_or_default(),
            "login_allowed": self.login_allowed,
            "voicemail_enabled": self.voicemail_enabled,
            "sip_password": self.sip_password.clone().unwrap_or_default(),
            "pin": self.pin.clone().unwrap_or_default(),
            "caller_id_name": self.caller_id_name.clone().unwrap_or_default(),
            "caller_id_number": self.caller_id_number.clone().unwrap_or_default(),
            "outbound_caller_id": self.outbound_caller_id.clone().unwrap_or_default(),
            "emergency_caller_id": self.emergency_caller_id.clone().unwrap_or_default(),
            "call_forwarding_mode": self.call_forwarding_mode,
            "call_forwarding_destination": self
                .call_forwarding_destination
                .clone()
                .unwrap_or_default(),
            "call_forwarding_timeout": self.call_forwarding_timeout,
            "notes": self.notes.clone().unwrap_or_default(),
            "registered_at": Value::Null,
            "created_at": "",
            "registrar": Value::Null,
            "status": "",
        })
    }

    fn to_value_with_model(&self, model: &ExtensionModel) -> Value {
        json!({
            "id": model.id,
            "extension": self.extension.clone(),
            "display_name": self.display_name.clone().unwrap_or_default(),
            "department": self.department.clone().unwrap_or_default(),
            "email": self.email.clone().unwrap_or_default(),
            "login_allowed": self.login_allowed,
            "voicemail_enabled": self.voicemail_enabled,
            "sip_password": self.sip_password.clone().unwrap_or_default(),
            "pin": self.pin.clone().unwrap_or_default(),
            "caller_id_name": self.caller_id_name.clone().unwrap_or_default(),
            "caller_id_number": self.caller_id_number.clone().unwrap_or_default(),
            "outbound_caller_id": self.outbound_caller_id.clone().unwrap_or_default(),
            "emergency_caller_id": self.emergency_caller_id.clone().unwrap_or_default(),
            "call_forwarding_mode": self.call_forwarding_mode.clone(),
            "call_forwarding_destination": self
                .call_forwarding_destination
                .clone()
                .unwrap_or_default(),
            "call_forwarding_timeout": self.call_forwarding_timeout,
            "notes": self.notes.clone().unwrap_or_default(),
            "registered_at": format_optional_datetime(model.registered_at),
            "created_at": format_datetime(model.created_at),
            "registrar": model.registrar.clone().unwrap_or_default(),
            "status": model.status.clone().unwrap_or_default(),
        })
    }
}

pub async fn page_extensions(
    Query(query): Query<ExtensionListQuery>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let departments = match load_all_departments(db).await {
        Ok(list) => list,
        Err(err) => {
            error!("failed to load departments: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extensions",
            )
                .into_response();
        }
    };

    let per_page = query.per_page.clamp(5, 100);
    let page = query.page.max(1);

    let select = ExtensionEntity::find().order_by_asc(ExtensionColumn::Extension);
    let paginator = select.paginate(db, per_page as u64);

    let total_records = match paginator.num_items().await {
        Ok(count) => count as u64,
        Err(err) => {
            error!("failed to count extensions: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extensions",
            )
                .into_response();
        }
    };

    let per_page_u64 = per_page as u64;
    let total_pages = if total_records == 0 {
        1
    } else {
        ((total_records + per_page_u64 - 1) / per_page_u64).max(1)
    };
    let requested_page = (page as u64).max(1);
    let page_index = requested_page
        .saturating_sub(1)
        .min(total_pages.saturating_sub(1));

    let items = match paginator.fetch_page(page_index).await {
        Ok(list) => list,
        Err(err) => {
            error!("failed to fetch extensions: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extensions",
            )
                .into_response();
        }
    };

    let ids: Vec<i64> = items.iter().map(|item| item.id).collect();
    let department_map = match load_department_map(db, &ids).await {
        Ok(map) => map,
        Err(err) => {
            error!("failed to load extension departments: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extensions",
            )
                .into_response();
        }
    };

    let extensions: Vec<Value> = items
        .iter()
        .map(|model| {
            let departments = department_map.get(&model.id).cloned().unwrap_or_default();
            json!({
                "id": model.id.to_string(),
                "extension": model.extension,
                "display_name": model.display_name.clone(),
                "department": departments,
                "status": model.status.clone().unwrap_or_else(|| "Unknown".to_string()),
                "login_allowed": model.login_allowed,
                "call_forwarding_mode": model.call_forwarding_mode.clone(),
                "call_forwarding_destination": model.call_forwarding_destination.clone(),
                "created_at": format_datetime(model.created_at),
                "registered_at": format_optional_datetime(model.registered_at),
                "registrar": model.registrar.clone(),
                "detail_url": state.url_for(&format!("/extensions/{}", model.id)),
                "edit_url": state.url_for(&format!("/extensions/{}", model.id)),
                "toggle_url": state.url_for(&format!("/extensions/{}/toggle", model.id)),
                "delete_url": state.url_for(&format!("/extensions/{}/delete", model.id)),
            })
        })
        .collect();

    let current_page = (page_index as u32) + 1;
    let total_pages_u32 = total_pages as u32;
    let showing_from = if extensions.is_empty() {
        0
    } else {
        (page_index as u32) * per_page + 1
    };
    let showing_to = if extensions.is_empty() {
        0
    } else {
        showing_from + extensions.len() as u32 - 1
    };

    let department_options: Vec<Value> = departments
        .iter()
        .map(|dept| {
            let label = department_label(dept);
            json!({"value": label, "label": label})
        })
        .collect();

    let filters_context = json!({
        "available": [
            {
                "id": "department",
                "label": "Department",
                "type": "select",
                "options": department_options,
            },
            {
                "id": "login_allowed",
                "label": "Login allowed",
                "type": "select",
                "options": [
                    {"value": "true", "label": "Allowed"},
                    {"value": "false", "label": "Blocked"},
                ],
            },
            {
                "id": "call_forwarding",
                "label": "Call forwarding",
                "type": "select",
                "options": [
                    {"value": "none", "label": "None"},
                    {"value": "when_busy", "label": "When busy"},
                    {"value": "always", "label": "Always"},
                    {"value": "when_not_answered", "label": "When not answered"},
                ],
            },
        ],
        "active": [],
    });

    state.render(
        "console/extensions.html",
        json!({
            "nav_active": "extensions",
            "extensions": extensions,
            "pagination": {
                "current_page": current_page,
                "per_page": per_page,
                "total_records": total_records,
                "total_pages": total_pages_u32.max(1),
                "showing_from": showing_from,
                "showing_to": showing_to,
                "has_prev": current_page > 1,
                "has_next": current_page < total_pages_u32.max(1),
                "prev_page": if current_page > 1 { current_page - 1 } else { 1 },
                "next_page": if current_page < total_pages_u32.max(1) {
                    current_page + 1
                } else {
                    total_pages_u32.max(1)
                },
            },
            "filters": filters_context,
            "create_url": state.url_for("/extensions/new"),
        }),
    )
}

pub async fn page_extension_create(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let department_options = match load_department_labels(state.db()).await {
        Ok(values) => values,
        Err(err) => {
            error!("failed to load departments: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extension form",
            )
                .into_response();
        }
    };

    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "mode": "create",
            "page_title": "Create extension",
            "submit_label": "Create extension",
            "extension": blank_extension_payload(),
            "department_options": department_options,
            "form_action": state.url_for("/extensions"),
            "back_url": state.url_for("/extensions"),
            "login_states": [
                {"value": true, "label": "Allowed"},
                {"value": false, "label": "Blocked"},
            ],
            "error_message": Value::Null,
        }),
    )
}

pub async fn page_extension_detail(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let (model, departments) = match fetch_extension_with_departments(db, id).await {
        Ok(Some(result)) => result,
        Ok(None) => return (StatusCode::NOT_FOUND, "Extension not found").into_response(),
        Err(err) => {
            error!("failed to load extension {}: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extension",
            )
                .into_response();
        }
    };

    let mut department_options = match load_department_labels(db).await {
        Ok(labels) => labels,
        Err(err) => {
            error!("failed to load departments: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extension",
            )
                .into_response();
        }
    };

    if let Some(primary) = primary_department_label(&departments) {
        if !primary.is_empty() && !department_options.iter().any(|opt| opt == &primary) {
            department_options.insert(0, primary.clone());
        }
    }

    let payload = extension_to_value(&model, &departments);

    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "mode": "edit",
            "page_title": format!("Extension {}", model.extension),
            "submit_label": "Save changes",
            "extension": payload,
            "department_options": department_options,
            "form_action": state.url_for(&format!("/extensions/{}", id)),
            "back_url": state.url_for("/extensions"),
            "login_states": [
                {"value": true, "label": "Allowed"},
                {"value": false, "label": "Blocked"},
            ],
            "error_message": Value::Null,
        }),
    )
}

pub async fn create_extension(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<ExtensionForm>,
) -> Response {
    let draft = form.into_draft();
    if draft.extension.is_empty() {
        return render_create_error(&state, draft, "Extension number is required").await;
    }

    let db = state.db();
    let tx = match db.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            error!("failed to start transaction: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create extension",
            )
                .into_response();
        }
    };

    match ExtensionEntity::find()
        .filter(ExtensionColumn::Extension.eq(draft.extension.clone()))
        .one(&tx)
        .await
    {
        Ok(Some(_)) => {
            let _ = tx.rollback().await;
            return render_create_error(&state, draft, "Extension number already exists").await;
        }
        Ok(None) => {}
        Err(err) => {
            error!("failed to check extension uniqueness: {}", err);
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create extension",
            )
                .into_response();
        }
    }

    let now = Utc::now().naive_utc();
    let mut model: ExtensionActiveModel = Default::default();
    model.extension = Set(draft.extension.clone());
    model.display_name = Set(draft.display_name.clone());
    model.email = Set(draft.email.clone());
    model.login_allowed = Set(draft.login_allowed);
    model.voicemail_enabled = Set(draft.voicemail_enabled);
    model.sip_password = Set(draft.sip_password.clone());
    model.pin = Set(draft.pin.clone());
    model.caller_id_name = Set(draft.caller_id_name.clone());
    model.caller_id_number = Set(draft.caller_id_number.clone());
    model.outbound_caller_id = Set(draft.outbound_caller_id.clone());
    model.emergency_caller_id = Set(draft.emergency_caller_id.clone());
    model.call_forwarding_mode = Set(draft.call_forwarding_mode.clone());
    model.call_forwarding_destination = Set(draft.call_forwarding_destination.clone());
    model.call_forwarding_timeout = Set(Some(draft.call_forwarding_timeout));
    model.notes = Set(draft.notes.clone());
    model.created_at = Set(now);
    model.updated_at = Set(now);

    let inserted = match model.insert(&tx).await {
        Ok(record) => record,
        Err(err) => {
            error!("failed to insert extension: {}", err);
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create extension",
            )
                .into_response();
        }
    };

    if let Err(err) = assign_department(&tx, inserted.id, draft.department.as_deref()).await {
        error!("failed to assign department: {}", err);
        let _ = tx.rollback().await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create extension",
        )
            .into_response();
    }

    if let Err(err) = tx.commit().await {
        error!("failed to commit extension create: {}", err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create extension",
        )
            .into_response();
    }

    Redirect::to(&state.url_for("/extensions")).into_response()
}

pub async fn update_extension(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<ExtensionForm>,
) -> Response {
    let draft = form.into_draft();
    if draft.extension.is_empty() {
        return render_update_error(&state, id, draft, "Extension number is required").await;
    }

    let db = state.db();
    let (model, _departments) = match fetch_extension_with_departments(db, id).await {
        Ok(Some(result)) => result,
        Ok(None) => return (StatusCode::NOT_FOUND, "Extension not found").into_response(),
        Err(err) => {
            error!("failed to load extension {} for update: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to update extension",
            )
                .into_response();
        }
    };

    let tx = match db.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            error!("failed to start update transaction: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to update extension",
            )
                .into_response();
        }
    };

    match ExtensionEntity::find()
        .filter(ExtensionColumn::Extension.eq(draft.extension.clone()))
        .one(&tx)
        .await
    {
        Ok(Some(existing)) if existing.id != id => {
            let _ = tx.rollback().await;
            return render_update_error(&state, id, draft, "Extension number already exists").await;
        }
        Ok(_) => {}
        Err(err) => {
            error!("failed to check extension uniqueness: {}", err);
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to update extension",
            )
                .into_response();
        }
    }

    let mut active: ExtensionActiveModel = model.clone().into();
    active.extension = Set(draft.extension.clone());
    active.display_name = Set(draft.display_name.clone());
    active.email = Set(draft.email.clone());
    active.login_allowed = Set(draft.login_allowed);
    active.voicemail_enabled = Set(draft.voicemail_enabled);
    active.sip_password = Set(draft.sip_password.clone());
    active.pin = Set(draft.pin.clone());
    active.caller_id_name = Set(draft.caller_id_name.clone());
    active.caller_id_number = Set(draft.caller_id_number.clone());
    active.outbound_caller_id = Set(draft.outbound_caller_id.clone());
    active.emergency_caller_id = Set(draft.emergency_caller_id.clone());
    active.call_forwarding_mode = Set(draft.call_forwarding_mode.clone());
    active.call_forwarding_destination = Set(draft.call_forwarding_destination.clone());
    active.call_forwarding_timeout = Set(Some(draft.call_forwarding_timeout));
    active.notes = Set(draft.notes.clone());
    active.updated_at = Set(Utc::now().naive_utc());

    if let Err(err) = active.update(&tx).await {
        error!("failed to update extension {}: {}", id, err);
        let _ = tx.rollback().await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update extension",
        )
            .into_response();
    }

    if let Err(err) = assign_department(&tx, id, draft.department.as_deref()).await {
        error!("failed to update extension department {}: {}", id, err);
        let _ = tx.rollback().await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update extension",
        )
            .into_response();
    }

    if let Err(err) = tx.commit().await {
        error!("failed to commit extension update {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update extension",
        )
            .into_response();
    }

    Redirect::to(&state.url_for("/extensions")).into_response()
}

pub async fn delete_extension(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    match ExtensionEntity::delete_by_id(id).exec(db).await {
        Ok(result) => {
            if result.rows_affected == 0 {
                (StatusCode::NOT_FOUND, "Extension not found").into_response()
            } else {
                Redirect::to(&state.url_for("/extensions")).into_response()
            }
        }
        Err(err) => {
            error!("failed to delete extension {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to delete extension",
            )
                .into_response()
        }
    }
}

pub async fn toggle_extension_login(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match ExtensionEntity::find_by_id(id).one(db).await {
        Ok(Some(result)) => result,
        Ok(None) => return (StatusCode::NOT_FOUND, "Extension not found").into_response(),
        Err(err) => {
            error!("failed to load extension {} for toggle: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to toggle extension",
            )
                .into_response();
        }
    };

    let mut active: ExtensionActiveModel = model.into();
    active.login_allowed = Set(!active.login_allowed.unwrap());
    active.updated_at = Set(Utc::now().naive_utc());

    if let Err(err) = active.update(db).await {
        error!("failed to toggle login for extension {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to toggle extension",
        )
            .into_response();
    }

    Redirect::to(&state.url_for("/extensions")).into_response()
}

async fn render_create_error(
    state: &Arc<ConsoleState>,
    draft: ExtensionDraft,
    message: &str,
) -> Response {
    let department_options = match load_department_labels(state.db()).await {
        Ok(values) => values,
        Err(err) => {
            error!("failed to load departments: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extension form",
            )
                .into_response();
        }
    };

    let mut options = department_options;
    if let Some(dept) = draft.department.as_ref() {
        if !dept.is_empty() && !options.iter().any(|label| label == dept) {
            options.insert(0, dept.clone());
        }
    }

    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "mode": "create",
            "page_title": "Create extension",
            "submit_label": "Create extension",
            "extension": draft.to_value_for_create(),
            "department_options": options,
            "form_action": state.url_for("/extensions"),
            "back_url": state.url_for("/extensions"),
            "login_states": [
                {"value": true, "label": "Allowed"},
                {"value": false, "label": "Blocked"},
            ],
            "error_message": message,
        }),
    )
}

async fn render_update_error(
    state: &Arc<ConsoleState>,
    id: i64,
    draft: ExtensionDraft,
    message: &str,
) -> Response {
    let db = state.db();
    let (model, departments) = match fetch_extension_with_departments(db, id).await {
        Ok(Some(result)) => result,
        Ok(None) => return (StatusCode::NOT_FOUND, "Extension not found").into_response(),
        Err(err) => {
            error!("failed to reload extension {}: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extension",
            )
                .into_response();
        }
    };

    let mut department_options = match load_department_labels(db).await {
        Ok(labels) => labels,
        Err(err) => {
            error!("failed to load departments: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load extension",
            )
                .into_response();
        }
    };

    let mut draft = draft;
    if draft.department.is_none() {
        draft.department = primary_department_label(&departments);
    }

    if let Some(dept) = draft.department.as_ref() {
        if !dept.is_empty() && !department_options.iter().any(|label| label == dept) {
            department_options.insert(0, dept.clone());
        }
    } else if let Some(primary) = primary_department_label(&departments) {
        if !primary.is_empty() && !department_options.iter().any(|label| label == &primary) {
            department_options.insert(0, primary.clone());
        }
    }

    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "mode": "edit",
            "page_title": format!("Extension {}", model.extension),
            "submit_label": "Save changes",
            "extension": draft.to_value_with_model(&model),
            "department_options": department_options,
            "form_action": state.url_for(&format!("/extensions/{}", id)),
            "back_url": state.url_for("/extensions"),
            "login_states": [
                {"value": true, "label": "Allowed"},
                {"value": false, "label": "Blocked"},
            ],
            "error_message": message,
        }),
    )
}

async fn fetch_extension_with_departments(
    db: &DatabaseConnection,
    id: i64,
) -> Result<Option<(ExtensionModel, Vec<DepartmentModel>)>, DbErr> {
    if let Some(model) = ExtensionEntity::find_by_id(id).one(db).await? {
        let departments = model.find_related(DepartmentEntity).all(db).await?;
        Ok(Some((model, departments)))
    } else {
        Ok(None)
    }
}

async fn load_all_departments(db: &DatabaseConnection) -> Result<Vec<DepartmentModel>, DbErr> {
    DepartmentEntity::find()
        .order_by_asc(DepartmentColumn::DisplayLabel)
        .order_by_asc(DepartmentColumn::Name)
        .all(db)
        .await
}

async fn load_department_labels(db: &DatabaseConnection) -> Result<Vec<String>, DbErr> {
    let departments = load_all_departments(db).await?;
    Ok(departments
        .into_iter()
        .map(|dept| department_label(&dept))
        .collect())
}

async fn load_department_map(
    db: &DatabaseConnection,
    ids: &[i64],
) -> Result<HashMap<i64, Vec<String>>, DbErr> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows = ExtensionDepartmentEntity::find()
        .filter(ExtensionDepartmentColumn::ExtensionId.is_in(ids.iter().cloned()))
        .find_also_related(DepartmentEntity)
        .all(db)
        .await?;

    let mut map: HashMap<i64, Vec<String>> = HashMap::new();
    for (link, maybe_dept) in rows {
        if let Some(dept) = maybe_dept {
            map.entry(link.extension_id)
                .or_default()
                .push(department_label(&dept));
        }
    }
    Ok(map)
}

async fn assign_department(
    tx: &DatabaseTransaction,
    extension_id: i64,
    department_label: Option<&str>,
) -> Result<(), DbErr> {
    ExtensionDepartmentEntity::delete_many()
        .filter(ExtensionDepartmentColumn::ExtensionId.eq(extension_id))
        .exec(tx)
        .await?;

    let Some(label) = department_label.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }) else {
        return Ok(());
    };

    let department = ensure_department(tx, label).await?;

    let mut link: ExtensionDepartmentActiveModel = Default::default();
    link.extension_id = Set(extension_id);
    link.department_id = Set(department.id);
    link.created_at = Set(Utc::now().naive_utc());
    link.insert(tx).await.map(|_| ())
}

async fn ensure_department(
    tx: &DatabaseTransaction,
    label: &str,
) -> Result<DepartmentModel, DbErr> {
    let condition = Condition::any()
        .add(DepartmentColumn::Name.eq(label))
        .add(DepartmentColumn::DisplayLabel.eq(label));

    if let Some(existing) = DepartmentEntity::find().filter(condition).one(tx).await? {
        return Ok(existing);
    }

    let now = Utc::now().naive_utc();
    let mut model: DepartmentActiveModel = Default::default();
    model.name = Set(label.to_string());
    model.display_label = Set(Some(label.to_string()));
    model.slug = Set(None);
    model.description = Set(None);
    model.color = Set(None);
    model.manager_contact = Set(None);
    model.created_at = Set(now);
    model.updated_at = Set(now);
    model.metadata = Set(None);
    model.insert(tx).await
}

fn blank_extension_payload() -> Value {
    json!({
        "id": Value::Null,
        "extension": "",
        "display_name": "",
        "department": "",
        "email": "",
        "login_allowed": true,
        "voicemail_enabled": false,
        "sip_password": "",
        "pin": "",
        "caller_id_name": "",
        "caller_id_number": "",
        "outbound_caller_id": "",
        "emergency_caller_id": "",
        "call_forwarding_mode": "none",
        "call_forwarding_destination": "",
        "call_forwarding_timeout": DEFAULT_FORWARDING_TIMEOUT,
        "notes": "",
        "registered_at": Value::Null,
        "created_at": "",
        "registrar": Value::Null,
        "status": "",
    })
}

fn extension_to_value(model: &ExtensionModel, departments: &[DepartmentModel]) -> Value {
    json!({
        "id": model.id,
        "extension": model.extension.clone(),
        "display_name": model.display_name.clone().unwrap_or_default(),
        "department": primary_department_label(departments).unwrap_or_default(),
        "email": model.email.clone().unwrap_or_default(),
        "login_allowed": model.login_allowed,
        "voicemail_enabled": model.voicemail_enabled,
        "sip_password": model.sip_password.clone().unwrap_or_default(),
        "pin": model.pin.clone().unwrap_or_default(),
        "caller_id_name": model.caller_id_name.clone().unwrap_or_default(),
        "caller_id_number": model.caller_id_number.clone().unwrap_or_default(),
        "outbound_caller_id": model.outbound_caller_id.clone().unwrap_or_default(),
        "emergency_caller_id": model.emergency_caller_id.clone().unwrap_or_default(),
        "call_forwarding_mode": model.call_forwarding_mode.clone(),
        "call_forwarding_destination": model
            .call_forwarding_destination
            .clone()
            .unwrap_or_default(),
        "call_forwarding_timeout": model
            .call_forwarding_timeout
            .unwrap_or(DEFAULT_FORWARDING_TIMEOUT),
        "notes": model.notes.clone().unwrap_or_default(),
        "registered_at": format_optional_datetime(model.registered_at),
        "created_at": format_datetime(model.created_at),
        "registrar": model.registrar.clone().unwrap_or_default(),
        "status": model.status.clone().unwrap_or_default(),
    })
}

fn primary_department_label(departments: &[DepartmentModel]) -> Option<String> {
    departments.first().map(|dept| department_label(dept))
}

fn department_label(model: &DepartmentModel) -> String {
    model
        .display_label
        .clone()
        .filter(|label| !label.trim().is_empty())
        .unwrap_or_else(|| model.name.clone())
}

fn sanitize_string(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn parse_bool_flag(value: Option<&String>) -> bool {
    value
        .map(|s| s.trim().to_ascii_lowercase())
        .map(|s| matches!(s.as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(false)
}

fn parse_forwarding_timeout(value: Option<&String>) -> i32 {
    value
        .and_then(|input| input.trim().parse::<i32>().ok())
        .map(|timeout| timeout.clamp(MIN_FORWARDING_TIMEOUT, MAX_FORWARDING_TIMEOUT))
        .unwrap_or(DEFAULT_FORWARDING_TIMEOUT)
}

fn normalize_forwarding_mode(value: Option<&str>) -> String {
    match value.unwrap_or("none") {
        "always" | "when_busy" | "when_not_answered" => value.unwrap().to_string(),
        _ => "none".to_string(),
    }
}

fn sanitize_forwarding_destination(value: Option<String>, mode: Option<&str>) -> Option<String> {
    if normalize_forwarding_mode(mode).as_str() == "none" {
        return None;
    }
    sanitize_string(&value)
}

fn format_datetime(value: NaiveDateTime) -> String {
    value.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn format_optional_datetime(value: Option<NaiveDateTime>) -> Option<String> {
    value.map(|dt| format_datetime(dt))
}
