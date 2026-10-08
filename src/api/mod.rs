use crate::auth::DynTokenValidator;
use crate::console::ConsoleState;
use crate::console::middleware::ApiTokenAuth;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

/// Derive a stable synthetic principal id from the token material. Negative so
/// it can never collide with a real `rustpbx_users.id`; the API token scope map
/// on [`ConsoleState`] is keyed by this id.
fn synthetic_id_for_token(token: &str) -> i64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    let mut id: i64 = 0;
    for byte in digest.iter().take(8) {
        id = (id << 8) | *byte as i64;
    }
    id & i64::MAX | i64::MIN // force negative, keep 63 bits of entropy
}

pub fn router(state: Arc<ConsoleState>) -> Router {
    let mut token_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut unscoped_tokens = 0usize;
    if let Some(console_cfg) = &state.config().console {
        for t in &console_cfg.api_tokens {
            if t.scopes.is_empty() {
                unscoped_tokens += 1;
            }
            token_map
                .entry(t.token.clone())
                .or_default()
                .extend_from_slice(&t.scopes);
        }
    }
    if unscoped_tokens > 0 {
        tracing::warn!(
            count = unscoped_tokens,
            "[console.api_tokens] entries have no scopes and are treated as full \
             superuser API keys. Give each token an explicit `scopes` list \
             (e.g. scopes = [\"call.control\", \"recording\"]) to restrict it."
        );
    }

    let phone_auth = build_phone_auth(&state);

    let api_routes = Router::new()
        .merge(crate::console::handlers::call_control::api_urls())
        .merge(crate::console::handlers::sipflow::api_urls())
        .merge(crate::console::handlers::diagnostics::api_urls())
        .merge(crate::console::handlers::call_record::api_urls())
        .merge(crate::console::handlers::setting::api_urls())
        .merge(crate::console::handlers::routing::api_urls())
        .merge(crate::console::handlers::extension::api_urls())
        .merge(crate::console::handlers::sip_trunk::api_urls())
        .merge(crate::console::handlers::dashboard::api_urls());

    // Unified home for console-internal JSON endpoints (formerly nested inside
    // the console router). They all flow through the same api_auth_middleware.
    let mut api_routes = api_routes
        .route(
            "/pending-reloads",
            get(crate::console::handlers::pending_reloads_handler),
        )
        .merge(crate::console::handlers::locales::api_urls())
        .merge(crate::console::handlers::presence::api_urls())
        .merge(crate::console::handlers::notifications::api_urls())
        .merge(crate::console::handlers::metrics::api_urls())
        .merge(crate::console::handlers::reports::api_urls())
        .merge(crate::console::handlers::addons::api_urls());

    // Addon API routes (collected at runtime via Addon trait hooks), including
    // queue (`console_api_always_mounted`) and feature-gated addons.
    if let Some(app_state) = state.app_state() {
        let config = app_state.config();
        for r in app_state
            .addon_registry
            .get_console_api_routes(&state, &config)
        {
            api_routes = api_routes.merge(r);
        }
    }

    let api_routes = api_routes
        .layer(axum::middleware::from_fn(
            crate::console::middleware::csrf_guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            ApiAuthState {
                console: state.clone(),
                api_tokens: Arc::new(token_map),
                phone_auth,
            },
            api_auth_middleware,
        ));

    let api_prefix = state.api_prefix().to_string();
    Router::new()
        .nest(&api_prefix, api_routes)
        .with_state(state)
}

fn build_phone_auth(console: &Arc<ConsoleState>) -> Option<DynTokenValidator> {
    let app_state = console.app_state()?;
    let config = app_state.config();
    app_state
        .addon_registry
        .get_phone_auth_validator(console, &config)
}

#[derive(Clone)]
struct ApiAuthState {
    console: Arc<ConsoleState>,
    api_tokens: Arc<HashMap<String, Vec<String>>>,
    phone_auth: Option<DynTokenValidator>,
}

/// Permission set granted to phone/agent tokens. These tokens authenticate a
/// registered desk phone or agent client — they must not be superusers, but
/// need call control and call media access to function.
const PHONE_TOKEN_PERMISSIONS: &[&str] = &[
    "callcontrol:read",
    "callcontrol:command",
    "cdr:read",
    "sipflow:read",
    "presence:read",
    "dashboard:read",
];

async fn api_auth_middleware(
    axum::extract::State(auth_state): axum::extract::State<ApiAuthState>,
    mut req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    let headers = req.headers().clone();

    if let Some(bearer) = extract_bearer_token(&headers) {
        if let Some(scopes) = auth_state.api_tokens.get(bearer.as_str()) {
            let user = if scopes.is_empty() {
                // Unscoped token: explicit opt-in to full access (warned about
                // at startup).
                make_synthetic_user(true)
            } else {
                let perms = ConsoleState::scopes_to_permissions(scopes);
                let synthetic_id = synthetic_id_for_token(&bearer);
                auth_state
                    .console
                    .register_api_token_permissions(synthetic_id, perms);
                make_scoped_synthetic_user(synthetic_id)
            };
            req.extensions_mut().insert(ApiTokenAuth(user));
            return next.run(req).await;
        }

        if let Some(ref validator) = auth_state.phone_auth {
            if let Some(_agent_id) = validator.validate_token(&bearer) {
                let mut perms = std::collections::HashSet::new();
                for p in PHONE_TOKEN_PERMISSIONS {
                    perms.insert(p.to_string());
                }
                let synthetic_id = synthetic_id_for_token(&bearer);
                auth_state
                    .console
                    .register_api_token_permissions(synthetic_id, perms);
                req.extensions_mut()
                    .insert(ApiTokenAuth(make_scoped_synthetic_user(synthetic_id)));
                return next.run(req).await;
            }
        }

        // SSO broker tokens (enterprise JWT passthrough or rustpbx-minted).
        // Unmatched bearers still fall through to the session check below.
        #[cfg(feature = "commerce")]
        if let Some(user) =
            crate::auth::sso::resolve_user_for_bearer(&auth_state.console, &bearer).await
        {
            req.extensions_mut().insert(ApiTokenAuth(user));
            return next.run(req).await;
        }

        if let Ok(Some(user)) = auth_state.console.current_user(Some(&bearer)).await {
            req.extensions_mut().insert(ApiTokenAuth(user));
            return next.run(req).await;
        }

        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "status": "error", "message": "invalid or expired token" })),
        )
            .into_response();
    }

    if let Some(session_token) = crate::console::middleware::extract_session_cookie(&headers) {
        if let Ok(Some(user)) = auth_state.console.current_user(Some(&session_token)).await {
            req.extensions_mut().insert(ApiTokenAuth(user));
            return next.run(req).await;
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "status": "error", "message": "authentication required" })),
    )
        .into_response()
}

fn extract_bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
}

fn make_synthetic_user(superuser: bool) -> crate::models::user::Model {
    use chrono::Utc;
    crate::models::user::Model {
        id: 0,
        email: "api-token@system".to_string(),
        username: "api-token".to_string(),
        password_hash: String::new(),
        reset_token: None,
        reset_token_expires: None,
        last_login_at: None,
        last_login_ip: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        is_active: true,
        is_staff: superuser,
        is_superuser: superuser,
        mfa_enabled: false,
        mfa_secret: None,
        session_epoch: 0,
        auth_source: "api-token".to_string(),
    }
}

/// A synthetic principal for a scoped token / phone token: authenticated, but
/// restricted to the permissions registered for its synthetic id.
fn make_scoped_synthetic_user(synthetic_id: i64) -> crate::models::user::Model {
    let mut user = make_synthetic_user(false);
    user.id = synthetic_id;
    user.username = "api-token".to_string();
    user.auth_source = "api-token-scoped".to_string();
    user
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_ids_are_negative_and_stable() {
        let a = synthetic_id_for_token("token-a");
        let b = synthetic_id_for_token("token-a");
        let c = synthetic_id_for_token("token-b");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a < 0);
    }

    #[test]
    fn unscoped_is_superusers_only_via_explicit_flag() {
        assert!(make_synthetic_user(true).is_superuser);
        assert!(!make_synthetic_user(false).is_superuser);
        assert!(make_scoped_synthetic_user(-42).id < 0);
        assert!(!make_scoped_synthetic_user(-42).is_superuser);
    }
}
