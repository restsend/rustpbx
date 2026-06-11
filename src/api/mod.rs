use crate::auth::DynTokenValidator;
use crate::console::middleware::ApiTokenAuth;
use crate::console::ConsoleState;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

pub fn router(state: Arc<ConsoleState>) -> Router {
    let mut token_map: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(console_cfg) = &state.config().console {
        for t in &console_cfg.api_tokens {
            token_map
                .entry(t.token.clone())
                .or_default()
                .extend_from_slice(&t.scopes);
        }
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
        .merge(crate::console::handlers::dashboard::api_urls())
        .merge(
            crate::addons::queue::console::handlers::api_urls()
                .with_state(state.clone()),
        );

    let api_routes = api_routes.layer(axum::middleware::from_fn_with_state(
        ApiAuthState {
            console: state.clone(),
            api_tokens: Arc::new(token_map),
            phone_auth,
        },
        api_auth_middleware,
    ));

    let api_prefix = state.api_prefix().to_string();
    Router::new().nest(&api_prefix, api_routes).with_state(state)
}

fn build_phone_auth(console: &Arc<ConsoleState>) -> Option<DynTokenValidator> {
    let app_state = console.app_state()?;

    #[cfg(feature = "addon-cc")]
    {
        let cc_state = app_state.get_addon_state::<crate::addons::cc::CcAddonState>()?;
        Some(Arc::new(cc_state.phone_auth.clone()) as DynTokenValidator)
    }

    #[cfg(not(feature = "addon-cc"))]
    {
        let _ = app_state;
        None
    }
}

#[derive(Clone)]
struct ApiAuthState {
    console: Arc<ConsoleState>,
    api_tokens: Arc<HashMap<String, Vec<String>>>,
    phone_auth: Option<DynTokenValidator>,
}

async fn api_auth_middleware(
    axum::extract::State(auth_state): axum::extract::State<ApiAuthState>,
    mut req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    let headers = req.headers().clone();

    if let Some(bearer) = extract_bearer_token(&headers) {
        if auth_state.api_tokens.contains_key(&bearer) {
            let user = make_synthetic_user();
            req.extensions_mut().insert(ApiTokenAuth(user));
            return next.run(req).await;
        }

        if let Some(ref validator) = auth_state.phone_auth {
            if let Some(_agent_id) = validator.validate_token(&bearer) {
                let user = make_synthetic_user();
                req.extensions_mut().insert(ApiTokenAuth(user));
                return next.run(req).await;
            }
        }

        if let Ok(Some(user)) = auth_state.console.current_user(Some(&bearer)).await {
            req.extensions_mut().insert(ApiTokenAuth(user));
            return next.run(req).await;
        }

        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "message": "invalid or expired token" })),
        )
            .into_response();
    }

    if let Some(session_token) =
        crate::console::middleware::extract_session_cookie(&headers)
    {
        if let Ok(Some(user)) = auth_state
            .console
            .current_user(Some(&session_token))
            .await
        {
            req.extensions_mut().insert(ApiTokenAuth(user));
            return next.run(req).await;
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "message": "authentication required" })),
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

fn make_synthetic_user() -> crate::models::user::Model {
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
        is_staff: true,
        is_superuser: true,
        mfa_enabled: false,
        mfa_secret: None,
        auth_source: "api-token".to_string(),
    }
}
