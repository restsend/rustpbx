use crate::console::ConsoleState;
use axum::response::{Html, IntoResponse, Redirect};
use axum::{
    extract::{FromRef, FromRequestParts},
    http::{HeaderMap, header::COOKIE, request::Parts},
    response::Response,
};
use minijinja::Environment;
use std::sync::Arc;
use tracing::warn;

pub struct AuthRequired(pub crate::models::user::Model);

impl<S> FromRequestParts<S> for AuthRequired
where
    Arc<ConsoleState>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let state = Arc::<ConsoleState>::from_ref(state);
        let next = Some(parts.uri.path().to_string());
        let session_cookie = extract_session_cookie(&parts.headers)
            .ok_or_else(|| Redirect::to(&state.login_url(next.clone())).into_response())?;

        match state.current_user(Some(&session_cookie)).await {
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
    headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie_str| {
            cookie_str.split(';').find_map(|pair| {
                let mut parts = pair.trim().splitn(2, '=');
                let key = parts.next()?.trim();
                if key == super::auth::SESSION_COOKIE_NAME {
                    Some(parts.next().unwrap_or("").trim().to_string())
                } else {
                    None
                }
            })
        })
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
