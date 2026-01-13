use crate::config::Config;
use crate::console::ConsoleState;
use axum::{
    Router,
    extract::{Json, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::sync::Arc;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/addons", get(index))
        .route("/addons/toggle", post(toggle_addon))
        .route("/addons/{id}", get(detail))
}

pub async fn index(State(state): State<Arc<ConsoleState>>) -> impl IntoResponse {
    let addons = if let Some(app_state) = state.app_state() {
        // Try to load config from disk to get the latest state
        let config = if let Some(path) = &app_state.config_path {
            match fs::read_to_string(path) {
                Ok(content) => toml::from_str::<Config>(&content)
                    .unwrap_or_else(|_| (**app_state.config()).clone()),
                Err(_) => (**app_state.config()).clone(),
            }
        } else {
            (**app_state.config()).clone()
        };

        let mut list = app_state.addon_registry.list_addons(app_state.clone());
        for addon in &mut list {
            let enabled_in_disk = app_state.addon_registry.is_enabled(&addon.id, &config);
            let enabled_in_mem = app_state
                .addon_registry
                .is_enabled(&addon.id, app_state.config());

            addon.enabled = enabled_in_disk;
            addon.restart_required = enabled_in_disk != enabled_in_mem;

            if addon.category == crate::addons::AddonCategory::Commercial {
                if let Some(addon_config) = config.addons.get(&addon.id) {
                    if let Some(key) = addon_config.get("license_key") {
                        if let Ok(info) = crate::license::verify_license(key).await {
                            addon.license_status = Some(if info.valid {
                                "Valid".to_string()
                            } else {
                                "Invalid".to_string()
                            });
                            addon.license_expiry =
                                info.expiry.map(|d| d.format("%Y-%m-%d").to_string());
                        }
                    }
                }
            }
        }
        list
    } else {
        vec![]
    };

    state.render(
        "console/addons.html",
        serde_json::json!({
            "addons": addons,
            "page_title": "Addons",
            "nav_active": "addons"
        }),
    )
}

#[derive(Deserialize)]
pub struct ToggleAddonPayload {
    id: String,
    enabled: bool,
}

pub async fn toggle_addon(
    State(state): State<Arc<ConsoleState>>,
    Json(payload): Json<ToggleAddonPayload>,
) -> Response {
    let config_path = match get_config_path(&state) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let mut doc = match load_document(&config_path) {
        Ok(doc) => doc,
        Err(resp) => return resp,
    };

    let proxy_table = ensure_table_mut(&mut doc, "proxy");

    // Ensure addons array exists
    if !proxy_table.contains_key("addons") || !proxy_table["addons"].is_array() {
        proxy_table["addons"] = Item::Value(Value::Array(Array::new()));
    }

    let addons_array = proxy_table["addons"].as_array_mut().expect("array");

    if payload.enabled {
        // Add if not exists
        let mut exists = false;
        for i in 0..addons_array.len() {
            if let Some(s) = addons_array.get(i).and_then(|v| v.as_str()) {
                if s == payload.id {
                    exists = true;
                    break;
                }
            }
        }
        if !exists {
            addons_array.push(payload.id);
        }
    } else {
        // Remove if exists
        let mut index_to_remove = None;
        for i in 0..addons_array.len() {
            if let Some(s) = addons_array.get(i).and_then(|v| v.as_str()) {
                if s == payload.id {
                    index_to_remove = Some(i);
                    break;
                }
            }
        }
        if let Some(idx) = index_to_remove {
            addons_array.remove(idx);
        }
    }

    let doc_text = doc.to_string();

    // Validate config before saving
    if let Err(resp) = parse_config_from_str(&doc_text) {
        return resp;
    }

    if let Err(resp) = persist_document(&config_path, doc_text) {
        return resp;
    }

    Json(json!({
        "success": true,
        "requires_restart": true,
        "message": "Addon state updated. Restart RustPBX to apply changes."
    }))
    .into_response()
}

pub async fn detail(
    State(state): State<Arc<ConsoleState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let addon = if let Some(app_state) = state.app_state() {
        let list = app_state.addon_registry.list_addons(app_state.clone());
        list.into_iter().find(|a| a.id == id)
    } else {
        None
    };

    if let Some(mut addon) = addon {
        if let Some(app_state) = state.app_state() {
            let config = if let Some(path) = &app_state.config_path {
                match fs::read_to_string(path) {
                    Ok(content) => toml::from_str::<Config>(&content)
                        .unwrap_or_else(|_| (**app_state.config()).clone()),
                    Err(_) => (**app_state.config()).clone(),
                }
            } else {
                (**app_state.config()).clone()
            };

            let enabled_in_disk = app_state.addon_registry.is_enabled(&addon.id, &config);
            let enabled_in_mem = app_state
                .addon_registry
                .is_enabled(&addon.id, app_state.config());
            addon.enabled = enabled_in_disk;
            addon.restart_required = enabled_in_disk != enabled_in_mem;

            if addon.category == crate::addons::AddonCategory::Commercial {
                if let Some(addon_config) = config.addons.get(&addon.id) {
                    if let Some(key) = addon_config.get("license_key") {
                        if let Ok(info) = crate::license::verify_license(key).await {
                            addon.license_status = Some(if info.valid {
                                "Valid".to_string()
                            } else {
                                "Invalid".to_string()
                            });
                            addon.license_expiry =
                                info.expiry.map(|d| d.format("%Y-%m-%d").to_string());
                        }
                    }
                }
            }
        }

        state.render(
            "console/addon_detail.html",
            serde_json::json!({
                "addon": addon,
                "page_title": format!("Addon: {}", addon.name),
                "nav_active": "addons"
            }),
        )
    } else {
        (StatusCode::NOT_FOUND, "Addon not found").into_response()
    }
}

fn get_config_path(state: &ConsoleState) -> Result<String, Response> {
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

fn load_document(path: &str) -> Result<DocumentMut, Response> {
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

fn persist_document(path: &str, contents: String) -> Result<(), Response> {
    fs::write(path, contents).map_err(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to write configuration file: {}", err),
        )
    })
}

fn parse_config_from_str(contents: &str) -> Result<Config, Response> {
    toml::from_str::<Config>(contents).map_err(|err| {
        json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Configuration validation failed: {}", err),
        )
    })
}

fn ensure_table_mut<'doc>(doc: &'doc mut DocumentMut, key: &str) -> &'doc mut Table {
    if !doc[key].is_table() {
        doc[key] = Item::Table(Table::new());
    }
    doc[key].as_table_mut().expect("table")
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({
            "success": false,
            "message": message.into(),
        })),
    )
        .into_response()
}
