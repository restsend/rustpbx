use crate::config::Config;
use crate::console::ConsoleState;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::fs;
use toml_edit::{DocumentMut, Item, Table};

pub(crate) fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({
            "status": "error",
            "message": message.into(),
        })),
    )
        .into_response()
}

#[allow(dead_code)]
pub(crate) fn permission_denied() -> Response {
    json_error(StatusCode::FORBIDDEN, "Permission denied")
}

#[allow(dead_code)]
pub(crate) fn not_found(label: impl Into<String>) -> Response {
    json_error(StatusCode::NOT_FOUND, format!("{} not found", label.into()))
}

#[allow(dead_code)]
pub(crate) fn bad_request(msg: impl Into<String>) -> Response {
    json_error(StatusCode::BAD_REQUEST, msg)
}

#[allow(dead_code)]
pub(crate) fn internal_error(msg: impl Into<String>) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, msg)
}

#[allow(dead_code)]
pub(crate) fn ok_json(data: serde_json::Value) -> Response {
    (StatusCode::OK, Json(data)).into_response()
}

// ── Unified envelope helpers (`{status, message?, data?}`) ──────────
// Prefer these for new JSON handlers; old `ok_json`/inline `json!` callers
// keep working unchanged and are migrated page-by-page in stage C/D.

#[allow(dead_code)]
pub(crate) fn api_ok(data: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "status": "ok", "data": data })),
    )
        .into_response()
}

#[allow(dead_code)]
pub(crate) fn api_ok_message(message: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "status": "ok", "message": message.into() })),
    )
        .into_response()
}

#[allow(dead_code)]
pub(crate) fn api_created(data: serde_json::Value) -> Response {
    (
        StatusCode::CREATED,
        Json(json!({ "status": "ok", "data": data })),
    )
        .into_response()
}

/// Look up an entity by primary key.  Returns the model on success,
/// `not_found(label)` on `Ok(None)`, or `internal_error(...)` on `Err`.
#[allow(unused_macros)]
macro_rules! find_or_404 {
    ($entity:ty, $id:expr, $db:expr, $label:literal) => {{
        use sea_orm::EntityTrait;
        match <$entity>::find_by_id($id).one($db).await {
            Ok(Some(model)) => model,
            Ok(None) => {
                return $crate::console::config_helpers::not_found($label).into_response()
            }
            Err(err) => {
                tracing::warn!(id = %$id, ?err, "failed to load {}", $label);
                return $crate::console::config_helpers::internal_error(format!(
                    "Failed to load {}: {}",
                    $label, err
                ))
                .into_response();
            }
        }
    }};
}
#[allow(unused_imports)]
pub(crate) use find_or_404;

#[allow(clippy::result_large_err)]
pub(crate) fn get_config_path(state: &ConsoleState) -> Result<String, Response> {
    let Some(app_state) = state.app_state() else {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Application state is unavailable.",
        ));
    };

    let Some(path) = app_state.config_path.clone() else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "Configuration file path is unknown. Start the service with --conf to enable editing.",
        ));
    };
    Ok(path)
}

#[allow(clippy::result_large_err)]
pub(crate) fn load_document(path: &str) -> Result<DocumentMut, Response> {
    let contents = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read configuration file: {}", err),
            ));
        }
    };

    contents.parse::<DocumentMut>().map_err(|err| {
        json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Configuration file is not valid TOML: {}", err),
        )
    })
}

#[allow(clippy::result_large_err)]
pub(crate) fn persist_document(path: &str, contents: String) -> Result<(), Response> {
    fs::write(path, contents).map_err(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to write configuration file: {}", err),
        )
    })
}

#[allow(clippy::result_large_err)]
pub(crate) fn parse_config_from_str(contents: &str) -> Result<Config, Response> {
    toml::from_str::<Config>(contents)
        .map(|mut cfg| {
            cfg.ensure_recording_defaults();
            cfg
        })
        .map_err(|err| {
            json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Configuration validation failed: {}", err),
            )
        })
}

pub(crate) fn ensure_table_mut<'doc>(doc: &'doc mut DocumentMut, key: &str) -> &'doc mut Table {
    let needs_init = doc
        .as_table()
        .get(key)
        .map(|item| !item.is_table())
        .unwrap_or(true);

    if needs_init {
        doc.insert(key, Item::Table(Table::new()));
    }

    doc.as_table_mut()
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .expect("table")
}
