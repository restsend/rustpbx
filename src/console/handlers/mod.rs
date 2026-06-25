use crate::console::{ConsoleState, ReloadTarget};
use axum::{Json, Router, extract::State, response::IntoResponse, routing::get};
use http::StatusCode;
use serde_json::json;
use std::sync::Arc;

pub mod addons;
pub mod call_control;
pub mod call_record;
pub mod dashboard;
pub mod diagnostics;
pub mod extension;
pub mod forms;
pub mod licenses;
pub mod locales;
pub mod metrics;
pub mod notifications;
pub mod presence;
pub mod routing;
pub mod setting;
pub mod sip_trunk;
pub mod sipflow;
pub mod user;
pub mod utils;

pub fn bad_request(message: impl Into<String>) -> axum::response::Response {
    crate::console::config_helpers::json_error(StatusCode::BAD_REQUEST, message)
}

#[allow(clippy::result_large_err)]
pub fn require_field(
    value: &Option<String>,
    field: &str,
) -> Result<String, axum::response::Response> {
    normalize_optional_string(value).ok_or_else(|| bad_request(format!("{} is required", field)))
}

pub fn normalize_optional_string(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

pub fn sanitize_optional_string(value: Option<String>) -> Option<String> {
    normalize_optional_string(&value)
}

pub fn router(state: Arc<ConsoleState>) -> Router {
    let base_path = state.base_path().to_string();
    let api_prefix = state.api_prefix().to_string();

    // Page routes (nested under base_path)
    let mut page_routes = Router::new()
        .merge(user::urls())
        .merge(extension::urls())
        .merge(sip_trunk::urls())
        .merge(setting::urls())
        .merge(routing::urls())
        .merge(call_record::urls())
        .merge(diagnostics::urls())
        .merge(call_control::urls())
        .merge(addons::urls())
        .merge(licenses::urls())
        .merge(sipflow::urls())
        .merge(notifications::urls())
        .merge(metrics::urls());

    // Addon page routes (collected at runtime via Addon trait hooks).
    if let Some(app_state) = state.app_state() {
        let config = app_state.config();
        for r in app_state
            .addon_registry
            .get_console_page_routes(&state, &config)
        {
            page_routes = page_routes.merge(r);
        }
    }

    // API routes were unified into src/api/mod.rs (single /api tree with
    // api_auth_middleware). See `api::router`.
    let api_routes = Router::new();

    Router::new()
        .route(&format!("{base_path}/"), get(self::dashboard::dashboard))
        .route(
            &format!("{base_path}/dashboard/data"),
            get(self::dashboard::dashboard_data),
        )
        .nest(&base_path, page_routes)
        .nest(&api_prefix, api_routes)
        // CSRF protection on all console mutations (POST/PUT/PATCH/DELETE).
        // Auth endpoints (login/register/reset/forgot/logout) are exempt
        // inside csrf_guard itself; they rely on SameSite=Lax.
        .layer(axum::middleware::from_fn(
            crate::console::middleware::csrf_guard,
        ))
        .with_state(state)
}

pub async fn pending_reloads_handler(State(state): State<Arc<ConsoleState>>) -> impl IntoResponse {
    let targets = state.pending_reload_targets();
    Json(json!({
        "pending": {
            "routes": targets.contains(&ReloadTarget::Routes),
            "trunks": targets.contains(&ReloadTarget::Trunks),
            "sbc_routes": targets.contains(&ReloadTarget::SbcRoutes),
            "sbc_trunks": targets.contains(&ReloadTarget::SbcTrunks),
            "queues": targets.contains(&ReloadTarget::Queues),
            "app": targets.contains(&ReloadTarget::App),
            "acl": targets.contains(&ReloadTarget::Acl),
        }
    }))
}

#[cfg(test)]
pub mod test_helpers {
    use crate::{config::ConsoleConfig, console::ConsoleState, models::migration::Migrator};
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use std::sync::Arc;

    pub async fn setup_state() -> Arc<ConsoleState> {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory");
        Migrator::up(&db, None).await.expect("run migrations");
        ConsoleState::initialize(db, ConsoleConfig::default())
            .await
            .expect("initialize console state")
    }

    pub fn superuser() -> crate::models::user::Model {
        let now = chrono::Utc::now();
        crate::models::user::Model {
            id: 1,
            email: "admin@rustpbx.com".into(),
            username: "admin".into(),
            password_hash: "hashed".into(),
            reset_token: None,
            reset_token_expires: None,
            last_login_at: None,
            last_login_ip: None,
            created_at: now,
            updated_at: now,
            is_active: true,
            is_staff: true,
            is_superuser: true,
            mfa_enabled: false,
            mfa_secret: None,
            auth_source: "local".into(),
        }
    }

    pub fn unprivileged_user() -> crate::models::user::Model {
        let now = chrono::Utc::now();
        crate::models::user::Model {
            id: 2,
            email: "user@rustpbx.com".into(),
            username: "user".into(),
            password_hash: "hashed".into(),
            reset_token: None,
            reset_token_expires: None,
            last_login_at: None,
            last_login_ip: None,
            created_at: now,
            updated_at: now,
            is_active: true,
            is_staff: false,
            is_superuser: false,
            mfa_enabled: false,
            mfa_secret: None,
            auth_source: "local".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::setup_state;
    use super::*;
    use axum::{body::to_bytes, extract::State, http::StatusCode};

    #[tokio::test]
    async fn pending_reloads_handler_false_initially() {
        let state = setup_state().await;
        let response = pending_reloads_handler(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["pending"]["routes"], false);
        assert_eq!(v["pending"]["trunks"], false);
    }

    #[tokio::test]
    async fn pending_reloads_handler_true_after_mark() {
        let state = setup_state().await;
        state.mark_pending_reload(ReloadTarget::Routes);

        let response = pending_reloads_handler(State(state)).await.into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["pending"]["routes"], true);
        assert_eq!(v["pending"]["trunks"], false);
    }
}
