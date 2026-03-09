use crate::console::ConsoleState;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use std::sync::Arc;

/// Register license routes — commerce builds add a `/licenses/verify` POST endpoint.
pub fn urls() -> Router<Arc<ConsoleState>> {
    #[cfg(feature = "commerce")]
    return commerce::urls();

    #[cfg(not(feature = "commerce"))]
    Router::new().route("/licenses", get(index))
}

pub async fn index(State(state): State<Arc<ConsoleState>>) -> impl IntoResponse {
    let bp = state.base_path().to_string();
    // Community build: redirect straight to addons (no license tab).
    #[cfg(not(feature = "commerce"))]
    return axum::response::Redirect::to(&format!("{bp}/addons"));
    #[cfg(feature = "commerce")]
    axum::response::Redirect::to(&format!("{bp}/addons#licenses"))
}

/// Core license-key verification logic — commerce builds only.
#[cfg(feature = "commerce")]
pub async fn verify_license_key(_state: Arc<ConsoleState>, license_key: String) -> Response {
    use super::addons::{
        get_config_path, json_error, load_document, parse_config_from_str, persist_document,
    };
    use toml_edit::{Item, Table};

    let license_key = license_key.trim().to_string();
    if license_key.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "License key cannot be empty.").into_response();
    }

    let info = match crate::license::verify_license(&license_key).await {
        Ok(i) => i,
        Err(e) => {
            return json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("License verification failed: {e}"),
            )
            .into_response();
        }
    };

    if !info.valid {
        let reason_msg = match info.reject_reason.as_deref() {
            Some("ip_limit_exceeded") => {
                "This license key has reached its maximum IP usage limit. Please contact support to increase the limit or free up an IP."
            }
            Some("email_mismatch") => {
                "This key is restricted to a specific authorized email. Make sure [licenses] email in your config matches the key's authorized email."
            }
            Some("email_required") => {
                "This key requires an authorized email. Set [licenses] email in your config.toml."
            }
            Some("license_revoked") => "This license key has been revoked.",
            Some("license_expired") => "This license key has expired.",
            Some("key_not_found") => {
                "License key not found. Please double-check the key and try again."
            }
            _ => "License key is not valid.",
        };
        return json_error(StatusCode::UNPROCESSABLE_ENTITY, reason_msg).into_response();
    }
    if crate::license::is_expired(&info) {
        return json_error(StatusCode::UNPROCESSABLE_ENTITY, "License key has expired.")
            .into_response();
    }

    let config_path = match get_config_path(&_state) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let mut doc = match load_document(&config_path) {
        Ok(d) => d,
        Err(resp) => return resp,
    };

    // Determine target addon IDs from the license scope.
    let (target_addon_ids, key_name) = match &info.scope {
        None => {
            let ids = if let Some(app_state) = _state.app_state() {
                app_state
                    .addon_registry
                    .list_addons(app_state.clone())
                    .into_iter()
                    .filter(|a| a.category == crate::addons::AddonCategory::Commercial)
                    .map(|a| a.id.clone())
                    .collect::<Vec<_>>()
            } else {
                vec![]
            };
            (ids, "global".to_string())
        }
        Some(scopes) => {
            let name = scopes
                .first()
                .cloned()
                .unwrap_or_else(|| "license".to_string());
            (scopes.clone(), name)
        }
    };

    if !doc.contains_key("licenses") || !doc["licenses"].is_table() {
        doc["licenses"] = Item::Table(Table::new());
    }
    {
        let licenses_table = doc["licenses"]
            .as_table_mut()
            .expect("[licenses] is a table");
        if !licenses_table.contains_key("addons") || !licenses_table["addons"].is_table() {
            licenses_table["addons"] = Item::Table(Table::new());
        }
        if let Some(t) = licenses_table["addons"].as_table_mut() {
            for addon_id in &target_addon_ids {
                t[addon_id.as_str()] = toml_edit::value(key_name.clone());
            }
        }
        if !licenses_table.contains_key("keys") || !licenses_table["keys"].is_table() {
            licenses_table["keys"] = Item::Table(Table::new());
        }
        if let Some(t) = licenses_table["keys"].as_table_mut() {
            t[key_name.as_str()] = toml_edit::value(license_key.clone());
        }
    }

    let doc_text = doc.to_string();
    if let Err(resp) = parse_config_from_str(&doc_text) {
        return resp;
    }
    if let Err(resp) = persist_document(&config_path, doc_text) {
        return resp;
    }

    let plan = if info.plan.is_empty() {
        "unknown".to_string()
    } else {
        info.plan
    };
    let expiry = info.expiry.map(|d| d.format("%Y-%m-%d").to_string());
    let scope = info.scope;

    // Update in-memory license status immediately so the UI reflects the new
    // state without requiring a server restart.
    let new_status = crate::license::LicenseStatus {
        key_name: key_name.clone(),
        valid: true,
        expired: false,
        expiry: expiry.clone(),
        plan: plan.clone(),
        is_trial: false,
        scope: scope.clone(),
    };
    crate::license::update_license_status(&target_addon_ids, new_status);

    axum::Json(serde_json::json!({
        "success": true,
        "message": "License key verified and saved successfully.",
        "plan": plan,
        "expiry": expiry,
        "scope": scope,
    }))
    .into_response()
}

/// Non-commerce stub so `addons.rs` can still reference the symbol without errors.
#[cfg(not(feature = "commerce"))]
pub async fn verify_license_key(_state: Arc<ConsoleState>, _license_key: String) -> Response {
    use super::addons::json_error;
    json_error(
        StatusCode::NOT_FOUND,
        "License management is not available in this build.",
    )
    .into_response()
}

/// Per-key row shown on the licenses overview page (commerce only).
#[cfg(feature = "commerce")]
#[derive(Debug, Serialize)]
pub(crate) struct LicenseRow {
    pub key_name: String,
    pub addon_ids: Vec<String>,
    pub status: String,
    pub plan: String,
    pub expiry: Option<String>,
    pub scope: Option<Vec<String>>,
    pub is_trial: bool,
}

/// Placeholder type so non-commerce code can write `Vec<LicenseRow>` without errors.
#[cfg(not(feature = "commerce"))]
#[derive(Debug, Serialize)]
pub(crate) struct LicenseRow;

#[cfg(feature = "commerce")]
pub(crate) fn build_license_rows(state: &ConsoleState) -> Vec<LicenseRow> {
    use std::collections::HashMap;

    let app_state = match state.app_state() {
        Some(s) => s,
        None => return vec![],
    };
    let config = app_state.config();
    let license_cfg = match &config.licenses {
        Some(l) => l,
        None => return vec![],
    };

    let mut key_to_addons: HashMap<String, Vec<String>> = HashMap::new();
    for (addon_id, key_name) in &license_cfg.addons {
        key_to_addons
            .entry(key_name.clone())
            .or_default()
            .push(addon_id.clone());
    }

    let mut rows: Vec<LicenseRow> = Vec::new();

    for (key_name, addon_ids) in &key_to_addons {
        let status_opt = addon_ids
            .iter()
            .find_map(|id| crate::license::get_license_status(id));

        let (status, plan, expiry, scope, is_trial) = match status_opt {
            Some(s) => {
                let label = if s.is_trial {
                    if s.valid {
                        format!("Trial ({})", s.plan)
                    } else {
                        "Trial Expired".to_string()
                    }
                } else if s.expired {
                    "Expired".to_string()
                } else if s.valid {
                    "Valid".to_string()
                } else {
                    "Invalid".to_string()
                };
                (
                    label,
                    s.plan.clone(),
                    s.expiry.clone(),
                    s.scope.clone(),
                    s.is_trial,
                )
            }
            None => ("Not Verified".to_string(), String::new(), None, None, false),
        };

        let mut sorted_addons = addon_ids.clone();
        sorted_addons.sort();

        rows.push(LicenseRow {
            key_name: key_name.clone(),
            addon_ids: sorted_addons,
            status,
            plan,
            expiry,
            scope,
            is_trial,
        });
    }

    rows.sort_by(|a, b| a.key_name.cmp(&b.key_name));
    rows
}

// Commerce-specific route registration lives in its own inline module so the
// Axum router types (Json, post) are not imported at the top level.
#[cfg(feature = "commerce")]
mod commerce {
    use super::*;
    use axum::{Json, extract::Json as AxJson, routing::post};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct VerifyLicensePayload {
        license_key: String,
    }

    async fn verify_license(
        State(state): State<Arc<ConsoleState>>,
        AxJson(payload): AxJson<VerifyLicensePayload>,
    ) -> Response {
        super::verify_license_key(state, payload.license_key).await
    }

    pub fn urls() -> Router<Arc<ConsoleState>> {
        let _ = Json::<()>;
        Router::new()
            .route("/licenses", get(super::index))
            .route("/licenses/verify", post(verify_license))
    }
}
