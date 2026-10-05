use crate::console::ConsoleState;
use crate::console::middleware::AuthRequired;
use crate::models::user::Model as UserModel;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use tracing::warn;

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/account/security", get(account_security_page))
        .route("/mfa/status", get(mfa_status))
        .route("/mfa/setup", get(mfa_setup))
        .route("/mfa/enable", post(mfa_enable))
        .route("/mfa/disable", post(mfa_disable))
}

pub(crate) fn enrollment_payload(user: &UserModel) -> Option<Value> {
    use totp_rs::{Algorithm, Secret, TOTP};

    let secret_bytes = Secret::generate_secret().to_bytes().ok()?;
    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret_bytes,
        Some(crate::config::BRAND_NAME.to_string()),
        user.email.clone(),
    )
    .ok()?;
    Some(json!({
        "secret": totp.get_secret_base32(),
        "otpauth_uri": totp.get_url(),
        "qr_png_base64": totp.get_qr_base64().ok()?,
    }))
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"status": "error", "message": message})),
    )
        .into_response()
}

pub async fn account_security_page(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let current_user = state.build_current_user_ctx(&user).await;
    state.render_with_headers(
        "console/account_security.html",
        json!({
            "nav_active": "account_security",
            "current_user": current_user,
            "mfa_enabled": user.mfa_enabled,
            "required": state.require_mfa(),
        }),
        &headers,
    )
}

pub async fn mfa_status(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
) -> Response {
    axum::Json(json!({
        "enabled": user.mfa_enabled,
        "required": state.require_mfa(),
    }))
    .into_response()
}

pub async fn mfa_setup(
    State(_state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
) -> Response {
    match enrollment_payload(&user) {
        Some(payload) => axum::Json(payload).into_response(),
        None => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to generate MFA secret",
        ),
    }
}

#[derive(Deserialize, Default, Clone)]
pub struct MfaEnablePayload {
    pub secret: String,
    pub code: String,
}

pub async fn mfa_enable(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Json(payload): axum::extract::Json<MfaEnablePayload>,
) -> Response {
    let secret = payload.secret.trim();
    let code = payload.code.trim();
    if secret.is_empty() || code.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Secret and code are required");
    }
    match state.enable_mfa(&user, secret, code).await {
        Ok(true) => {
            state.mfa_clear_failures(user.id);
            axum::Json(json!({"status": "ok"})).into_response()
        }
        Ok(false) => json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid verification code",
        ),
        Err(err) => {
            warn!("failed to enable MFA for {}: {}", user.username, err);
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to enable two-factor authentication",
            )
        }
    }
}

#[derive(Deserialize, Default, Clone)]
pub struct MfaDisablePayload {
    pub password: String,
}

pub async fn mfa_disable(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Json(payload): axum::extract::Json<MfaDisablePayload>,
) -> Response {
    if payload.password.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Password is required");
    }
    if !ConsoleState::verify_user_password(&user, &payload.password) {
        return json_error(StatusCode::UNPROCESSABLE_ENTITY, "Incorrect password");
    }
    match state.disable_mfa(&user).await {
        Ok(_) => axum::Json(json!({"status": "ok"})).into_response(),
        Err(err) => {
            warn!("failed to disable MFA for {}: {}", user.username, err);
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to disable two-factor authentication",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addons::Addon;
    use crate::app::AppStateBuilder;
    use crate::config::Config;
    use crate::console::handlers::test_helpers::{setup_state, superuser};
    use crate::models::migration::Migrator;
    use axum::body::to_bytes;
    use base64::Engine as _;
    use sea_orm::Database;
    use sea_orm::EntityTrait;
    use sea_orm_migration::MigratorTrait;
    use std::path::PathBuf;
    use std::sync::Arc as StdArc;
    use tower::ServiceExt;

    struct CommercialTestAddon;

    #[async_trait::async_trait]
    impl Addon for CommercialTestAddon {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn id(&self) -> &'static str {
            "mfa_test_commercial"
        }
        fn name(&self) -> &'static str {
            "MfaTestCommercial"
        }
        fn category(&self) -> crate::addons::AddonCategory {
            crate::addons::AddonCategory::Commercial
        }
        fn router(&self, _state: crate::app::AppState) -> Option<axum::Router> {
            None
        }
        async fn initialize(&self, _state: crate::app::AppState) -> anyhow::Result<()> {
            Ok(())
        }
    }

    async fn commercial_state(
        require_mfa: bool,
    ) -> (
        Arc<ConsoleState>,
        StdArc<crate::app::AppStateInner>,
        PathBuf,
    ) {
        let dir = std::env::temp_dir().join(format!(
            "rustpbx_mfa_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config = Config {
            database_url: format!("sqlite://{}/app.db?mode=rwc", dir.display()),
            ..Default::default()
        };
        let addon: StdArc<dyn Addon> = StdArc::new(CommercialTestAddon);
        let app = AppStateBuilder::new()
            .with_config(config)
            .with_skip_sip_bind()
            .with_extra_addons(StdArc::new(vec![addon]))
            .build()
            .await
            .expect("build app state");
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory");
        Migrator::up(&db, None).await.expect("run migrations");
        let console_config = crate::config::ConsoleConfig {
            require_mfa,
            ..Default::default()
        };
        let state = ConsoleState::initialize(db, console_config, None)
            .await
            .expect("initialize console state");
        state.set_app_state(Some(StdArc::downgrade(&app)));
        (state, app, dir)
    }

    fn generate_code(secret: &str, email: &str) -> String {
        use totp_rs::{Algorithm, Secret, TOTP};
        let bytes = Secret::Encoded(secret.to_string())
            .to_bytes()
            .expect("secret bytes");
        let totp = TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            bytes,
            Some(crate::config::BRAND_NAME.to_string()),
            email.to_string(),
        )
        .expect("totp");
        totp.generate_current().expect("code")
    }

    async fn body_json(response: Response) -> Value {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        serde_json::from_slice(&body).expect("parse json")
    }

    #[tokio::test]
    async fn mfa_status_reports_enabled_and_required() {
        let (state, app, dir) = commercial_state(false).await;
        let user = superuser();
        let response = mfa_status(State(state.clone()), AuthRequired(user.clone())).await;
        let payload = body_json(response).await;
        assert_eq!(payload["enabled"], false);
        assert_eq!(payload["required"], false);
        app.token().cancel();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn mfa_status_reflects_require_mfa_config() {
        let (state, app, dir) = commercial_state(true).await;
        let user = superuser();
        let response = mfa_status(State(state), AuthRequired(user)).await;
        let payload = body_json(response).await;
        assert_eq!(payload["required"], true);
        app.token().cancel();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn mfa_setup_returns_secret_uri_and_qr() {
        let (state, app, dir) = commercial_state(false).await;
        let user = superuser();
        let response = mfa_setup(State(state), AuthRequired(user)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body_json(response).await;
        let secret = payload["secret"].as_str().expect("secret");
        assert!(!secret.is_empty());
        let uri = payload["otpauth_uri"].as_str().expect("uri");
        assert!(uri.starts_with("otpauth://totp/"));
        assert!(uri.contains(&format!("secret={}", secret)));
        assert!(uri.contains(&format!("issuer={}", crate::config::BRAND_NAME)));
        let qr = payload["qr_png_base64"].as_str().expect("qr");
        assert!(!qr.is_empty());
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(qr)
            .expect("base64 qr");
        assert_eq!(&decoded[..4], b"\x89PNG");
        app.token().cancel();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn mfa_enable_with_current_code_persists() {
        let (state, app, dir) = commercial_state(false).await;
        let user = state
            .create_user("enable-mfa@rustpbx.com", "enablemfa", "password123")
            .await
            .expect("seed user");
        let payload = enrollment_payload(&user).expect("enrollment payload");
        let secret = payload["secret"].as_str().unwrap().to_string();
        let code = generate_code(&secret, &user.email);
        let response = mfa_enable(
            State(state.clone()),
            AuthRequired(user.clone()),
            axum::extract::Json(MfaEnablePayload {
                secret: secret.clone(),
                code,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["status"], "ok");
        let reloaded = crate::models::user::Entity::find_by_id(user.id)
            .one(state.db())
            .await
            .expect("query user")
            .expect("user exists");
        assert!(reloaded.mfa_enabled);
        assert_eq!(reloaded.mfa_secret.as_deref(), Some(secret.as_str()));
        app.token().cancel();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn mfa_enable_rejects_wrong_code_with_422() {
        let (state, app, dir) = commercial_state(false).await;
        let user = state
            .create_user("wrong-code@rustpbx.com", "wrongcode", "password123")
            .await
            .expect("seed user");
        let payload = enrollment_payload(&user).expect("enrollment payload");
        let secret = payload["secret"].as_str().unwrap().to_string();
        let response = mfa_enable(
            State(state.clone()),
            AuthRequired(user.clone()),
            axum::extract::Json(MfaEnablePayload {
                secret,
                code: "000000".to_string(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let reloaded = crate::models::user::Entity::find_by_id(user.id)
            .one(state.db())
            .await
            .expect("query user")
            .expect("user exists");
        assert!(!reloaded.mfa_enabled);
        app.token().cancel();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn mfa_disable_requires_matching_password() {
        let (state, app, dir) = commercial_state(false).await;
        let user = state
            .create_user("disable-mfa@rustpbx.com", "disablemfa", "password123")
            .await
            .expect("seed user");
        let payload = enrollment_payload(&user).expect("enrollment payload");
        let secret = payload["secret"].as_str().unwrap().to_string();
        let code = generate_code(&secret, &user.email);
        state
            .enable_mfa(&user, &secret, &code)
            .await
            .expect("enable mfa");
        state
            .update_password(&user, "password123")
            .await
            .expect("set known password");

        let wrong = mfa_disable(
            State(state.clone()),
            AuthRequired(user.clone()),
            axum::extract::Json(MfaDisablePayload {
                password: "not-the-password".into(),
            }),
        )
        .await;
        assert_eq!(wrong.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let good = mfa_disable(
            State(state.clone()),
            AuthRequired(user.clone()),
            axum::extract::Json(MfaDisablePayload {
                password: "password123".into(),
            }),
        )
        .await;
        assert_eq!(good.status(), StatusCode::OK);
        let reloaded = crate::models::user::Entity::find_by_id(user.id)
            .one(state.db())
            .await
            .expect("query user")
            .expect("user exists");
        assert!(!reloaded.mfa_enabled);
        assert!(reloaded.mfa_secret.is_none());
        app.token().cancel();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn mfa_routes_gated_by_commercial_addon() {
        let state = setup_state().await;
        let app_router = crate::console::handlers::router(state.clone());
        let request = axum::http::Request::builder()
            .uri(format!("{}/mfa/status", state.base_path()))
            .body(axum::body::Body::empty())
            .expect("request");
        let response = app_router.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let (state, app, dir) = commercial_state(false).await;
        state
            .create_user("gate-admin@rustpbx.com", "gate-admin", "password123")
            .await
            .expect("seed user");
        let app_router = crate::console::handlers::router(state.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(format!("{}/login", state.base_path()))
            .header(
                axum::http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(axum::body::Body::from(
                "identifier=gate-admin&password=password123",
            ))
            .expect("request");
        let response = app_router.clone().oneshot(request).await.expect("response");
        assert!(response.status().is_redirection());
        let cookie = response
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .map(|part| part.trim())
            .find(|part| part.starts_with("rustpbx_session="))
            .expect("session cookie")
            .to_string();
        let request = axum::http::Request::builder()
            .uri(format!("{}/mfa/status", state.base_path()))
            .header(axum::http::header::COOKIE, cookie)
            .body(axum::body::Body::empty())
            .expect("request");
        let response = app_router.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body_json(response).await;
        assert_eq!(payload["enabled"], false);
        assert_eq!(payload["required"], false);
        app.token().cancel();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn account_security_page_renders_when_commercial() {
        let (state, app, dir) = commercial_state(false).await;
        let user = state
            .create_user("sec@rustpbx.com", "secuser", "password123")
            .await
            .expect("seed user");
        let response =
            account_security_page(State(state.clone()), HeaderMap::new(), AuthRequired(user)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let html = String::from_utf8_lossy(&body).into_owned();
        assert!(html.contains("account-security"), "expected mfa card");
        app.token().cancel();
        std::fs::remove_dir_all(dir).ok();
    }
}
