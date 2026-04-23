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

pub struct WholesaleScope {
    pub user: crate::models::user::Model,
    pub agent_id: Option<i64>,
}

impl<S> FromRequestParts<S> for WholesaleScope
where
    Arc<ConsoleState>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let AuthRequired(user) = AuthRequired::from_request_parts(parts, state).await?;
        let console_state = Arc::<ConsoleState>::from_ref(state);
        let agent_id = if user.is_superuser {
            None
        } else {
            resolve_agent_id(&console_state, user.id).await
        };
        Ok(WholesaleScope { user, agent_id })
    }
}

async fn resolve_agent_id(state: &ConsoleState, user_id: i64) -> Option<i64> {
    #[cfg(feature = "addon-wholesale")]
    use crate::addons::wholesale::models::wholesale_agent::{Column, Entity};
    #[cfg(feature = "addon-wholesale")]
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    #[cfg(not(feature = "addon-wholesale"))]
    {
        let _ = (state, user_id);
        None
    }

    #[cfg(feature = "addon-wholesale")]
    Entity::find()
        .filter(Column::UserId.eq(user_id))
        .one(state.db())
        .await
        .ok()
        .flatten()
        .map(|a| a.id)
}

pub fn extract_session_cookie(headers: &HeaderMap) -> Option<String> {
    if let Some(auth_header) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(auth_str) = auth_header.to_str()
            && let Some(token) = auth_str.strip_prefix("Bearer ") {
                return Some(token.trim().to_string());
            }

    for cookie_header in headers.get_all(COOKIE) {
        if let Ok(s) = cookie_header.to_str() {
            let found = s.split(';').find_map(|pair| {
                let mut parts = pair.trim().splitn(2, '=');
                let key = parts.next()?.trim();
                if key == super::auth::SESSION_COOKIE_NAME {
                    Some(parts.next().unwrap_or("").trim().to_string())
                } else {
                    None
                }
            });
            if found.is_some() {
                return found;
            }
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConsoleConfig;
    use crate::models::migration::Migrator;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    #[cfg(feature = "addon-wholesale")]
    use chrono::Utc;
    #[cfg(feature = "addon-wholesale")]
    use crate::addons::wholesale::models::wholesale_agent;
    #[cfg(feature = "addon-wholesale")]
    use sea_orm::{ActiveValue::Set, EntityTrait};

    async fn setup_state() -> Arc<ConsoleState> {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory");
        Migrator::up(&db, None).await.expect("run migrations");
        #[cfg(feature = "addon-wholesale")]
        {
            use crate::addons::wholesale::migration::Migrator as WholesaleMigrator;
            WholesaleMigrator::up(&db, None).await.expect("run wholesale migrations");
        }
        ConsoleState::initialize(
            Arc::new(crate::callrecord::DefaultCallRecordFormatter::default()),
            db,
            ConsoleConfig::default(),
        )
        .await
        .expect("initialize console state")
    }

    #[tokio::test]
    async fn superuser_has_no_agent_id() {
        let state = setup_state().await;
        let agent_id = resolve_agent_id(&state, 999).await;
        assert!(agent_id.is_none());
    }

    #[tokio::test]
    #[cfg(feature = "addon-wholesale")]
    async fn user_with_agent_record_resolves_agent_id() {
        let state = setup_state().await;
        let now = Utc::now();
        let inserted = wholesale_agent::Entity::insert(wholesale_agent::ActiveModel {
            user_id: Set(42),
            name: Set("Agent Test".to_string()),
            max_discount_bps: Set(500),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec_with_returning(state.db())
        .await
        .expect("insert agent");

        let agent_id = resolve_agent_id(&state, 42).await;
        assert_eq!(agent_id, Some(inserted.id));
    }

    #[tokio::test]
    #[cfg(not(feature = "addon-wholesale"))]
    async fn resolve_agent_id_returns_none_without_wholesale_feature() {
        let state = setup_state().await;
        let agent_id = resolve_agent_id(&state, 42).await;
        assert!(agent_id.is_none());
    }

    #[tokio::test]
    async fn user_without_agent_record_returns_none() {
        let state = setup_state().await;
        let agent_id = resolve_agent_id(&state, 12345).await;
        assert!(agent_id.is_none());
    }
}
