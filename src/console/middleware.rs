use crate::console::ConsoleState;
use crate::console::i18n::get_cookie;
use axum::response::{Html, IntoResponse, Redirect};
use axum::{
    extract::{FromRef, FromRequestParts},
    http::{HeaderMap, HeaderValue, request::Parts},
    response::Response,
};
use minijinja::Environment;
use std::sync::Arc;
use tracing::warn;

pub struct AuthRequired(pub crate::models::user::Model);

#[derive(Clone)]
pub struct ApiTokenAuth(pub crate::models::user::Model);

impl<S> FromRequestParts<S> for AuthRequired
where
    Arc<ConsoleState>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        if let Some(api_auth) = parts.extensions.get::<ApiTokenAuth>() {
            return Ok(AuthRequired(api_auth.0.clone()));
        }

        let state = Arc::<ConsoleState>::from_ref(state);
        let next = Some(parts.uri.path().to_string());
        let session_token = extract_session_cookie(&parts.headers);

        let session_token = session_token
            .ok_or_else(|| Redirect::to(&state.login_url(next.clone())).into_response())?;

        match state.current_user(Some(&session_token)).await {
            Ok(user) => {
                if let Some(user) = user {
                    Ok(AuthRequired(user))
                } else {
                    Err(Redirect::to(&state.login_url(next)).into_response())
                }
            }
            Err(err) => {
                warn!("failed to load current user: {}", err);
                Err(Redirect::to(&state.login_url(next)).into_response())
            }
        }
    }
}

pub fn extract_session_cookie(headers: &HeaderMap) -> Option<String> {
    if let Some(auth_header) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(auth_str) = auth_header.to_str()
        && let Some(token) = auth_str.strip_prefix("Bearer ")
    {
        return Some(token.trim().to_string());
    }

    get_cookie(headers, super::auth::SESSION_COOKIE_NAME)
}

// ── CSRF protection (double-submit cookie) ──────────────────────────
//
// Strategy:
//   • Page routes (under base_path) get `seed_csrf_cookie` — only ever sets
//     the csrf_token cookie so api.js can read it; never validates.
//   • API routes (under api_prefix) get `csrf_guard` — validates the
//     X-CSRF-Token header against the cookie on unsafe methods.
//   • Bearer-token requests (Authorization header) are exempt — they are
//     token-authenticated, not cookie-authenticated, so CSRF doesn't apply.
//   • Auth form POSTs (login/register/reset) live under base_path and are
//     protected by the SameSite=Lax attribute on the session cookie.

pub const CSRF_COOKIE_NAME: &str = "csrf_token";
const CSRF_HEADER_NAME: &str = "x-csrf-token";

fn generate_csrf_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn ensure_csrf_cookie(headers: &HeaderMap, response: &mut Response) {
    if get_cookie(headers, CSRF_COOKIE_NAME).is_some() {
        return;
    }
    let token = generate_csrf_token();
    let cookie = format!("{}={}; Path=/; SameSite=Lax", CSRF_COOKIE_NAME, token);
    if let Ok(val) = HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, val);
    }
}

/// Attach a csrf_token cookie to the response if none is present. No validation.
pub async fn seed_csrf_cookie(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let headers = request.headers().clone();
    let mut response = next.run(request).await;
    ensure_csrf_cookie(&headers, &mut response);
    response
}

/// Validate the CSRF token on unsafe methods; seed cookie on every response.
pub async fn csrf_guard(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let method = request.method().clone();
    let headers = request.headers().clone();

    let is_safe = matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE");
    let has_authorization = headers.contains_key(axum::http::header::AUTHORIZATION);

    // Exempt auth endpoints (login/register/reset/forgot/logout/mfa) — these
    // are traditional form POSTs protected by SameSite=Lax, not CSRF tokens.
    let path = request.uri().path();
    let is_auth = path.ends_with("/login")
        || path.ends_with("/register")
        || path.ends_with("/forgot")
        || path.ends_with("/logout")
        || path.contains("/reset/")
        || path.ends_with("/login/mfa");

    if !is_safe && !has_authorization && !is_auth {
        let cookie_token = get_cookie(&headers, CSRF_COOKIE_NAME);
        let header_token = headers
            .get(CSRF_HEADER_NAME)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string());

        let valid = match (cookie_token, header_token) {
            (Some(c), Some(h)) => constant_time_eq(c.as_bytes(), h.as_bytes()),
            _ => false,
        };

        if !valid {
            return (
                axum::http::StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "status": "error",
                    "message": "CSRF token missing or invalid"
                })),
            )
                .into_response();
        }
    }

    let mut response = next.run(request).await;
    ensure_csrf_cookie(&headers, &mut response);
    response
}

/// Constant-time byte slice comparison to avoid timing oracles.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub struct RenderTemplate<'a> {
    pub tmpl_env: &'a Environment<'a>,
    pub template_name: &'a str,
    pub context: &'a serde_json::Value,
}

impl IntoResponse for RenderTemplate<'_> {
    fn into_response(self) -> Response {
        match self.tmpl_env.get_template(self.template_name) {
            Ok(tmpl) => match tmpl.render(self.context) {
                Ok(body) => Html(body).into_response(),
                Err(err) => {
                    warn!(
                        "failed to render template {}: {:?}",
                        self.template_name, err
                    );
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Internal Server Error: {}", err),
                    )
                        .into_response()
                }
            },
            Err(err) => {
                warn!("failed to get template {}: {}", self.template_name, err);
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Internal Server Error: {}", err),
                )
                    .into_response()
            }
        }
    }
}
