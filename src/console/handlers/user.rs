use crate::{
    console::{
        ConsoleState,
        auth::{RESET_TOKEN_VALID_MINUTES, RegistrationPolicy},
        handlers::forms::{ForgotForm, LoginForm, LoginQuery, RegisterForm, ResetForm},
    },
    handler::middleware::clientaddr::ClientAddr,
};
use axum::{
    Router,
    extract::{Form, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header::SET_COOKIE},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use sea_orm::EntityTrait;
use serde_json::json;
use std::sync::Arc;
use tracing::{info, warn};

async fn load_mfa_user_or_redirect(
    state: &Arc<ConsoleState>,
    user_id: i64,
) -> Result<crate::models::user::Model, Response> {
    match crate::models::user::Entity::find_by_id(user_id)
        .one(&state.db)
        .await
    {
        Ok(Some(user)) => Ok(user),
        _ => Err(Redirect::to(&state.url_for("/login")).into_response()),
    }
}

fn is_secure_request(headers: &HeaderMap) -> bool {
    use crate::handler::middleware::clientaddr;
    if !clientaddr::forwarded_headers_trusted() {
        return false;
    }
    if let Some(proto) = headers.get("x-forwarded-proto")
        && let Ok(proto_str) = proto.to_str()
    {
        return proto_str.eq_ignore_ascii_case("https");
    }
    false
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/login", get(login_page).post(login_post))
        .route("/login/mfa", get(login_mfa_page).post(login_mfa_post))
        .route("/login/mfa/enroll-secret", get(login_mfa_enroll_secret))
        .route("/login/mfa/enroll", post(login_mfa_enroll_post))
        .route("/logout", get(logout))
        .route("/register", get(register_page).post(register_post))
        .route("/forgot", get(forgot_page).post(forgot_post))
        .route("/reset/{token}", get(reset_page).post(reset_post))
}

const SUPERUSER_NOTICE: &str =
    "You are creating the first administrator account. Please store this password securely.";

pub async fn login_page(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let policy = match state.registration_policy().await {
        Ok(policy) => policy,
        Err(err) => {
            warn!("failed to load registration policy: {}", err);
            RegistrationPolicy::default()
        }
    };

    let login_action = state.login_url(query.next.clone());
    let register_url = if policy.allowed {
        Some(state.register_url(query.next.clone()))
    } else {
        None
    };

    let demo_mode = state.config().demo_mode;

    state.render_with_headers(
        "console/login.html",
        json!({
            "login_action": login_action,
            "register_url": register_url,
            "registration_allowed": policy.allowed,
            "demo_mode": demo_mode,
            "error_message": null,
            "identifier": "",
            "next": query.next.clone(),
        }),
        &headers,
    )
}

pub async fn login_post(
    client_addr: ClientAddr,
    headers: HeaderMap,
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<LoginQuery>,
    Form(form): Form<LoginForm>,
) -> Response {
    let identifier = form.identifier.trim();
    let password = form.password.trim();
    let next = match form
        .next
        .clone()
        .and_then(|n| if n.trim().is_empty() { None } else { Some(n) })
        .or(query.next.clone())
    {
        Some(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    };
    let policy = match state.registration_policy().await {
        Ok(policy) => policy,
        Err(err) => {
            warn!("failed to load registration policy: {}", err);
            RegistrationPolicy::default()
        }
    };
    let register_url = if policy.allowed {
        Some(state.register_url(next.clone()))
    } else {
        None
    };
    let demo_mode = state.config().demo_mode;
    if identifier.is_empty() || password.is_empty() {
        return state.render_with_headers(
            "console/login.html",
            json!({
                "login_action": state.login_url(next.clone()),
                "register_url": register_url.clone(),
                "registration_allowed": policy.allowed,
                "demo_mode": demo_mode,
                "error_message": "Please provide both username/email and password",
                "identifier": identifier,
                "next": next.clone(),
            }),
            &headers,
        );
    }

    // Brute-force protection: lock out an IP / identifier that accumulates too
    // many consecutive failures before touching the password verifier.
    if state.login_is_locked(client_addr.ip().to_string().as_str(), identifier) {
        tracing::warn!(identifier = %identifier, %client_addr, "login temporarily locked");
        return state.render_with_headers(
            "console/login.html",
            json!({
                "login_action": state.login_url(next.clone()),
                "register_url": register_url,
                "registration_allowed": policy.allowed,
                "demo_mode": demo_mode,
                "error_message": "Too many failed attempts. Please try again later.",
                "identifier": identifier,
                "next": next,
            }),
            &headers,
        );
    }

    match state.authenticate(identifier, password).await {
        Ok(Some(user)) => {
            state.login_clear_failures(client_addr.ip().to_string().as_str(), identifier);
            // Check if MFA is required for this user
            if user.mfa_enabled {
                // Create MFA session and redirect to verification
                let redirect_target = state.url_for("/login/mfa");
                let mut response = Redirect::to(&redirect_target).into_response();
                if let Some(header) =
                    state.mfa_session_cookie_header(user.id, is_secure_request(&headers))
                {
                    response.headers_mut().append(SET_COOKIE, header);
                }
                return response;
            }

            if state.require_mfa() {
                let redirect_target = format!("{}?mode=enroll", state.url_for("/login/mfa"));
                let mut response = Redirect::to(&redirect_target).into_response();
                if let Some(header) =
                    state.mfa_session_cookie_header(user.id, is_secure_request(&headers))
                {
                    response.headers_mut().append(SET_COOKIE, header);
                }
                return response;
            }

            // No MFA required, complete login
            if let Err(err) = state.mark_login(&user, client_addr.ip().to_string()).await {
                warn!("failed to update last_login: {}", err);
            }
            let redirect_target = resolve_next_redirect(state.as_ref(), next.clone());
            let mut response = Redirect::to(&redirect_target).into_response();
            if let Some(header) = state.session_cookie_header(&user, is_secure_request(&headers))
            {
                response.headers_mut().append(SET_COOKIE, header);
            }
            response
        }
        Ok(None) => {
            state.login_record_failure(client_addr.ip().to_string().as_str(), identifier);
            state.render_with_headers(
                "console/login.html",
                json!({
                    "login_action": state.login_url(next.clone()),
                    "register_url": register_url,
                    "registration_allowed": policy.allowed,
                    "demo_mode": demo_mode,
                    "error_message": "Invalid credentials",
                    "identifier": identifier,
                    "next": next,
                }),
                &headers,
            )
        }
        Err(err) => {
            warn!("login error: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Sign-in failed: {}", err),
            )
                .into_response()
        }
    }
}

fn resolve_next_redirect(state: &ConsoleState, next: Option<String>) -> String {
    if let Some(raw) = next {
        let candidate = raw.trim();
        if candidate.starts_with('/') && !candidate.starts_with("//") && !candidate.contains("://")
        {
            if candidate == "/" {
                return state.url_for("/");
            }
            if state.base_path() != "/" && candidate.starts_with(state.base_path()) {
                return candidate.to_string();
            }
            return state.url_for(candidate);
        }
    }

    state.url_for("/")
}

pub async fn logout(
    headers: HeaderMap,
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<LoginQuery>,
) -> Response {
    // Validate `next` against the console base path — an unvalidated value here
    // would make GET /logout an open redirect.
    let next = resolve_next_redirect(state.as_ref(), query.next);
    let mut response = Redirect::to(&next).into_response();
    if let Some(header) = state.clear_session_cookie(is_secure_request(&headers)) {
        response.headers_mut().append(SET_COOKIE, header);
    }
    response
}

pub async fn register_page(State(state): State<Arc<ConsoleState>>, headers: HeaderMap) -> Response {
    let policy = match state.registration_policy().await {
        Ok(policy) => policy,
        Err(err) => {
            warn!("failed to load registration policy: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Unable to load registration page: {}", err),
            )
                .into_response();
        }
    };

    let superuser_notice = if policy.first_user {
        Some(SUPERUSER_NOTICE.to_string())
    } else {
        None
    };

    let mut response = state.render_with_headers(
        "console/register.html",
        json!({
            "register_action": state.url_for("/register"),
            "login_url": state.url_for("/login"),
            "error_message": null,
            "email": "",
            "username": "",
            "registration_closed": !policy.allowed,
            "superuser_notice": superuser_notice,
        }),
        &headers,
    );

    if !policy.allowed {
        *response.status_mut() = StatusCode::FORBIDDEN;
    }

    response
}

pub async fn register_post(
    headers: HeaderMap,
    State(state): State<Arc<ConsoleState>>,
    Form(form): Form<RegisterForm>,
) -> Response {
    let policy = match state.registration_policy().await {
        Ok(policy) => policy,
        Err(err) => {
            warn!("failed to load registration policy: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Registration failed: {}", err),
            )
                .into_response();
        }
    };

    if !policy.allowed {
        let mut response = state.render_with_headers(
            "console/register.html",
            json!({
                "register_action": state.url_for("/register"),
                "login_url": state.url_for("/login"),
                "error_message": "User self-registration is disabled",
                "email": form.email.trim().to_lowercase(),
                "username": form.username.trim().to_string(),
                "registration_closed": true,
                "superuser_notice": None::<String>,
            }),
            &headers,
        );
        *response.status_mut() = StatusCode::FORBIDDEN;
        return response;
    }

    let email = form.email.trim().to_lowercase();
    let username = form.username.trim().to_string();
    let password = form.password.trim().to_string();
    let confirm = form.confirm_password.trim().to_string();
    let mut error_message = None;

    if !email.contains('@') {
        error_message = Some("Please enter a valid email address".to_string());
    } else if username.len() < 3 {
        error_message = Some("Username must be at least 3 characters".to_string());
    } else if password.len() < 8 {
        error_message = Some("Password must be at least 8 characters".to_string());
    } else if password != confirm {
        error_message = Some("Passwords do not match".to_string());
    }

    if error_message.is_none() {
        // Single generic message for both cases: distinguishing them lets an
        // attacker enumerate registered emails / usernames.
        match state.email_exists(&email).await {
            Ok(true) => {
                error_message = Some("Email or username is not available".to_string())
            }
            Ok(false) => {}
            Err(err) => {
                warn!("failed to check email uniqueness: {}", err);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Registration failed: {}", err),
                )
                    .into_response();
            }
        }
    }

    if error_message.is_none() {
        match state.username_exists(&username).await {
            Ok(true) => {
                error_message = Some("Email or username is not available".to_string())
            }
            Ok(false) => {}
            Err(err) => {
                warn!("failed to check username uniqueness: {}", err);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Registration failed: {}", err),
                )
                    .into_response();
            }
        }
    }

    if let Some(error) = error_message {
        return state.render_with_headers(
            "console/register.html",
            json!({
                "register_action": state.url_for("/register"),
                "login_url": state.url_for("/login"),
                "error_message": error,
                "email": email,
                "username": username,
                "registration_closed": false,
                "superuser_notice": if policy.first_user {
                    Some(SUPERUSER_NOTICE.to_string())
                } else {
                    None
                },
            }),
            &headers,
        );
    }

    match state.create_user(&email, &username, &password).await {
        Ok(user) => {
            if policy.first_user {
                info!("created initial superuser account: {}", user.username);
            }
            let mut response = Redirect::to(&state.url_for("/")).into_response();
            if let Some(header) = state.session_cookie_header(&user, is_secure_request(&headers))
            {
                response.headers_mut().append(SET_COOKIE, header);
            }
            response
        }
        Err(err) => {
            warn!("failed to create user: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Registration failed: {}", err),
            )
                .into_response()
        }
    }
}

pub async fn forgot_page(State(state): State<Arc<ConsoleState>>, headers: HeaderMap) -> Response {
    state.render_with_headers(
        "console/forgot.html",
        json!({
            "info_message": null,
            "error_message": null,
            "reset_link": null,
        }),
        &headers,
    )
}

pub async fn forgot_post(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    Form(form): Form<ForgotForm>,
) -> Response {
    let email = form.email.trim().to_lowercase();

    if email.is_empty() {
        return state.render_with_headers(
            "console/forgot.html",
            json!({
                "info_message": null,
                "error_message": "Please enter your registered email address",
                "reset_link": null,
            }),
            &headers,
        );
    }

    let mut reset_link = None;
    match state.find_user_by_email(&email).await {
        Ok(Some(user)) => match state.upsert_reset_token(&user).await {
            Ok((token, _)) => {
                let link = state.url_for(&format!("/reset/{}", token));
                // SECURITY: the reset link is a bearer credential for the
                // account. It must only ever travel to the mailbox owner. Log
                // the event without the token, and only render the link when
                // the operator has explicitly enabled the development escape
                // hatch (`expose_reset_link_dev = true` in [console]) on a
                // non-production instance.
                info!(
                    "password reset requested for {} (token generated, expires in {} min)",
                    email,
                    RESET_TOKEN_VALID_MINUTES
                );
                if state.expose_reset_link_dev() {
                    tracing::debug!(target: "rustpbx::dev", "dev reset link: {}", link);
                    reset_link = Some(link);
                }
            }
            Err(err) => {
                warn!("failed to save reset token: {}", err);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Unable to process request: {}", err),
                )
                    .into_response();
            }
        },
        Ok(None) => {}
        Err(err) => {
            warn!("failed to handle forgot password: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Unable to process request: {}", err),
            )
                .into_response();
        }
    }

    state.render_with_headers(
        "console/forgot.html",
        json!({
            "forgot_action": state.url_for("/forgot"),
            // Deliberately identical whether or not the address exists, so the
            // endpoint cannot be used to enumerate registered accounts.
            "info_message": "If the account exists, a password reset link has been generated. Contact your administrator if you did not receive it.",
            "error_message": null,
            "reset_link": reset_link,
        }),
        &headers,
    )
}

pub async fn reset_page(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
) -> Response {
    match state.find_by_reset_token(&token).await {
        Ok(Some(user)) => {
            if user.token_expired() {
                state.render_with_headers(
                    "console/forgot.html",
                    json!({
                        "forgot_action": state.url_for("/forgot"),
                        "info_message": null,
                        "error_message": "Reset link has expired. Please request a new one.",
                        "reset_link": null,
                    }),
                    &headers,
                )
            } else {
                state.render_with_headers(
                    "console/reset.html",
                    json!({
                        "reset_action": state.url_for(&format!("/reset/{}", token)),
                        "token": token,
                        "error_message": null,
                    }),
                    &headers,
                )
            }
        }
        Ok(None) => state.render_with_headers(
            "console/forgot.html",
            json!({
                "forgot_action": state.url_for("/forgot"),
                "info_message": null,
                "error_message": "Reset link is invalid",
                "reset_link": null,
            }),
            &headers,
        ),
        Err(err) => {
            warn!("failed to verify reset token: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Unable to process reset request: {}", err),
            )
                .into_response()
        }
    }
}

pub async fn reset_post(
    headers: HeaderMap,
    State(state): State<Arc<ConsoleState>>,
    AxumPath(token): AxumPath<String>,
    Form(form): Form<ResetForm>,
) -> Response {
    match state.find_by_reset_token(&token).await {
        Ok(Some(user)) => {
            if user.token_expired() {
                return state.render_with_headers(
                    "console/forgot.html",
                    json!({
                        "forgot_action": state.url_for("/forgot"),
                        "info_message": null,
                        "error_message": "Reset link has expired. Please request a new one.",
                        "reset_link": null,
                    }),
                    &headers,
                );
            }
            let password = form.password.trim();
            let confirm = form.confirm_password.trim();
            if password.len() < 8 {
                return state.render_with_headers(
                    "console/reset.html",
                    json!({
                        "reset_action": state.url_for(&format!("/reset/{}", token)),
                        "token": token,
                        "error_message": "Password must be at least 8 characters",
                    }),
                    &headers,
                );
            }
            if password != confirm {
                return state.render_with_headers(
                    "console/reset.html",
                    json!({
                        "reset_action": state.url_for(&format!("/reset/{}", token)),
                        "token": token,
                        "error_message": "Passwords do not match",
                    }),
                    &headers,
                );
            }

            match state.update_password(&user, password).await {
                Ok(updated_user) => {
                    let mut response = Redirect::to(&state.url_for("/")).into_response();
                    if let Some(header) =
                        state.session_cookie_header(&updated_user, is_secure_request(&headers))
                    {
                        response.headers_mut().append(SET_COOKIE, header);
                    }
                    response
                }
                Err(err) => {
                    warn!("failed to update password: {}", err);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to reset password: {}", err),
                    )
                        .into_response()
                }
            }
        }
        Ok(None) => state.render_with_headers(
            "console/forgot.html",
            json!({
                "forgot_action": state.url_for("/forgot"),
                "info_message": null,
                "error_message": "Reset link is invalid",
                "reset_link": null,
            }),
            &headers,
        ),
        Err(err) => {
            warn!("failed to reset password: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to reset password: {}", err),
            )
                .into_response()
        }
    }
}

// MFA Login Handlers

fn mfa_cookie_user(state: &Arc<ConsoleState>, headers: &HeaderMap) -> Option<i64> {
    let mfa_cookie =
        crate::console::i18n::get_cookie(headers, crate::console::auth::MFA_SESSION_COOKIE_NAME);
    state.verify_mfa_session_token(mfa_cookie.as_deref())
}

enum MfaPageError {
    InvalidCode,
    Locked(u64),
}

fn render_mfa_page(
    state: &Arc<ConsoleState>,
    headers: &HeaderMap,
    enroll: bool,
    error: Option<MfaPageError>,
) -> Response {
    let (error_kind, lockout_seconds) = match error {
        Some(MfaPageError::InvalidCode) => ("invalid_code", None::<u64>),
        Some(MfaPageError::Locked(secs)) => ("locked", Some(secs)),
        None => ("", None),
    };
    state.render_with_headers(
        "console/login_mfa.html",
        json!({
            "login_action": state.url_for("/login/mfa"),
            "enroll_action": state.url_for("/login/mfa/enroll"),
            "enroll_secret_url": state.url_for("/login/mfa/enroll-secret"),
            "enroll": enroll,
            "required": state.require_mfa(),
            "error_kind": error_kind,
            "lockout_seconds": lockout_seconds,
        }),
        headers,
    )
}

pub async fn login_mfa_page(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let user_id = match mfa_cookie_user(&state, &headers) {
        Some(id) => id,
        None => {
            // No valid MFA session, redirect to login
            return Redirect::to(&state.url_for("/login")).into_response();
        }
    };

    let enroll = query.mode.as_deref() == Some("enroll");
    match load_mfa_user_or_redirect(&state, user_id).await {
        Ok(_user) => render_mfa_page(&state, &headers, enroll, None),
        Err(resp) => resp,
    }
}

pub async fn login_mfa_post(
    client_addr: ClientAddr,
    headers: HeaderMap,
    State(state): State<Arc<ConsoleState>>,
    Form(form): Form<crate::console::handlers::forms::MfaForm>,
) -> Response {
    let user_id = match mfa_cookie_user(&state, &headers) {
        Some(id) => id,
        None => {
            return Redirect::to(&state.url_for("/login")).into_response();
        }
    };

    let user = match load_mfa_user_or_redirect(&state, user_id).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };

    if state.mfa_is_locked(user.id) {
        let remaining = state.mfa_lockout_remaining_secs(user.id).unwrap_or(0);
        return render_mfa_page(&state, &headers, false, Some(MfaPageError::Locked(remaining)));
    }

    // Verify MFA code
    if !ConsoleState::verify_mfa_code(&user, &form.code) {
        state.mfa_record_failure(user.id);
        state.report_mfa_attempt(
            &user.username,
            Some(client_addr.ip().to_string()),
            crate::addons::AuthAttemptOutcome::BadCredentials,
        );
        return render_mfa_page(&state, &headers, false, Some(MfaPageError::InvalidCode));
    }

    state.mfa_clear_failures(user.id);
    state.report_mfa_attempt(
        &user.username,
        Some(client_addr.ip().to_string()),
        crate::addons::AuthAttemptOutcome::Success,
    );

    // MFA verified, complete login
    if let Err(err) = state.mark_login(&user, client_addr.ip().to_string()).await {
        warn!("failed to update last_login: {}", err);
    }

    // Clear MFA session and create full session
    let redirect_target = state.url_for("/");
    let mut response = Redirect::to(&redirect_target).into_response();

    // Add session cookie
    if let Some(header) = state.session_cookie_header(&user, is_secure_request(&headers)) {
        response.headers_mut().append(SET_COOKIE, header);
    }

    // Clear MFA session cookie
    if let Some(header) = state.clear_mfa_session_cookie(is_secure_request(&headers)) {
        response.headers_mut().append(SET_COOKIE, header);
    }

    response
}

pub async fn login_mfa_enroll_secret(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
) -> Response {
    let user_id = match mfa_cookie_user(&state, &headers) {
        Some(id) => id,
        None => {
            return crate::console::config_helpers::json_error(
                axum::http::StatusCode::UNAUTHORIZED,
                "MFA session required",
            );
        }
    };

    let user = match load_mfa_user_or_redirect(&state, user_id).await {
        Ok(user) => user,
        Err(_) => {
            return crate::console::config_helpers::json_error(
                axum::http::StatusCode::UNAUTHORIZED,
                "MFA session required",
            );
        }
    };

    // Enrollment is only for accounts without MFA yet. An account that already
    // has TOTP enabled must verify its existing code — handing out fresh
    // secrets here would let a password-only attacker replace the second
    // factor.
    if user.mfa_enabled {
        return crate::console::config_helpers::json_error(
            axum::http::StatusCode::FORBIDDEN,
            "MFA is already enabled for this account",
        );
    }

    match super::mfa::enrollment_payload(&user) {
        Some(payload) => axum::Json(payload).into_response(),
        None => crate::console::config_helpers::json_error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to generate MFA secret",
        ),
    }
}

pub async fn login_mfa_enroll_post(
    client_addr: ClientAddr,
    headers: HeaderMap,
    State(state): State<Arc<ConsoleState>>,
    Form(form): Form<crate::console::handlers::forms::MfaEnrollForm>,
) -> Response {
    let user_id = match mfa_cookie_user(&state, &headers) {
        Some(id) => id,
        None => {
            return Redirect::to(&state.url_for("/login")).into_response();
        }
    };

    let user = match load_mfa_user_or_redirect(&state, user_id).await {
        Ok(user) => user,
        Err(resp) => return resp,
    };

    // Hard stop for accounts that already have MFA: accepting a client-supplied
    // secret here would overwrite the victim's TOTP secret and complete the
    // login without the real second factor (MFA bypass). The account owner can
    // disable MFA from the security page with their password instead.
    if user.mfa_enabled {
        let redirect_target = state.url_for("/login/mfa");
        return Redirect::to(&redirect_target).into_response();
    }

    if state.mfa_is_locked(user.id) {
        let remaining = state.mfa_lockout_remaining_secs(user.id).unwrap_or(0);
        return render_mfa_page(&state, &headers, true, Some(MfaPageError::Locked(remaining)));
    }

    match state.enable_mfa(&user, form.secret.trim(), form.code.trim()).await {
        Ok(true) => {
            state.mfa_clear_failures(user.id);
            state.report_mfa_attempt(
                &user.username,
                Some(client_addr.ip().to_string()),
                crate::addons::AuthAttemptOutcome::Success,
            );

            if let Err(err) = state.mark_login(&user, client_addr.ip().to_string()).await {
                warn!("failed to update last_login: {}", err);
            }

            let mut response = Redirect::to(&state.url_for("/")).into_response();
            if let Some(header) = state.session_cookie_header(&user, is_secure_request(&headers))
            {
                response.headers_mut().append(SET_COOKIE, header);
            }
            if let Some(header) = state.clear_mfa_session_cookie(is_secure_request(&headers)) {
                response.headers_mut().append(SET_COOKIE, header);
            }
            response
        }
        Ok(false) => {
            state.mfa_record_failure(user.id);
            state.report_mfa_attempt(
                &user.username,
                Some(client_addr.ip().to_string()),
                crate::addons::AuthAttemptOutcome::BadCredentials,
            );
            render_mfa_page(&state, &headers, true, Some(MfaPageError::InvalidCode))
        }
        Err(err) => {
            warn!("failed to enable MFA for {}: {}", user.username, err);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Enrollment failed: {}", err),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConsoleConfig;
    use crate::console::handlers::test_helpers::setup_state;
    use crate::models::migration::Migrator;
    use axum::body::Body;
    use axum::http::header;
    use axum::http::Request;
    use axum::http::StatusCode;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use tower::ServiceExt;

    fn enrollment_secret() -> String {
        use totp_rs::Secret;
        Secret::generate_secret().to_encoded().to_string()
    }

    fn current_totp_code(secret: &str, email: &str) -> String {
        use totp_rs::{Algorithm, Secret, TOTP};
        let bytes = Secret::Encoded(secret.to_string())
            .to_bytes()
            .expect("valid base32 secret");
        let totp = TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            bytes,
            Some(crate::config::BRAND_NAME.to_string()),
            email.to_string(),
        )
        .expect("build totp");
        totp.generate_current().expect("generate code")
    }

    async fn state_with_config(config: ConsoleConfig) -> Arc<ConsoleState> {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory");
        Migrator::up(&db, None).await.expect("run migrations");
        ConsoleState::initialize(db, config, None)
            .await
            .expect("initialize console state")
    }

    fn cookie_from(response: &Response, name: &str) -> Option<String> {
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with(&format!("{}=", name)))
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            })
    }

    fn set_cookie_header(response: &Response, name: &str) -> Option<String> {
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with(&format!("{}=", name)))
            .map(|value| value.to_string())
    }

    async fn post_form(
        app: &axum::Router,
        uri: String,
        cookie: Option<&str>,
        body: String,
    ) -> Response {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        app.clone()
            .oneshot(builder.body(Body::from(body)).expect("build request"))
            .await
            .expect("send request")
    }

    async fn get_uri(app: &axum::Router, uri: String, cookie: Option<&str>) -> Response {
        let mut builder = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        app.clone()
            .oneshot(builder.body(Body::empty()).expect("build request"))
            .await
            .expect("send request")
    }

    #[tokio::test]
    async fn login_with_mfa_full_flow() {
        let state = setup_state().await;
        let user = state
            .create_user("mfa-e2e@rustpbx.com", "mfae2e", "password123")
            .await
            .expect("seed user");
        let secret = enrollment_secret();
        let code = current_totp_code(&secret, &user.email);
        assert!(
            state
                .enable_mfa(&user, &secret, &code)
                .await
                .expect("enable mfa")
        );

        let app = crate::console::handlers::router(state.clone());
        let base = state.base_path().to_string();

        let response = post_form(
            &app,
            format!("{}/login", base),
            None,
            "identifier=mfae2e&password=password123".to_string(),
        )
        .await;
        assert!(response.status().is_redirection());
        assert!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.ends_with("/login/mfa"))
                .unwrap_or(false)
        );
        let mfa_cookie = cookie_from(&response, "rustpbx_mfa").expect("mfa cookie issued");

        let wrong = post_form(
            &app,
            format!("{}/login/mfa", base),
            Some(&mfa_cookie),
            "code=000000".to_string(),
        )
        .await;
        assert_eq!(wrong.status(), StatusCode::OK);
        let body = axum::body::to_bytes(wrong.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = String::from_utf8_lossy(&body).into_owned();
        assert!(html.contains("Invalid verification code"));

        let good = post_form(
            &app,
            format!("{}/login/mfa", base),
            Some(&mfa_cookie),
            format!("code={}", code),
        )
        .await;
        assert!(good.status().is_redirection());
        let session_cookie = cookie_from(&good, "rustpbx_session").expect("session cookie issued");
        let cleared = set_cookie_header(&good, "rustpbx_mfa").expect("mfa cookie cleared");
        assert!(cleared.contains("Max-Age=0"), "mfa cookie should be cleared");

        let dashboard = get_uri(&app, format!("{}/", base), Some(&session_cookie)).await;
        assert_eq!(dashboard.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn forced_mfa_enrollment_flow_completes_login() {
        let state = state_with_config(ConsoleConfig {
            require_mfa: true,
            ..Default::default()
        })
        .await;
        let user = state
            .create_user("enroll-e2e@rustpbx.com", "enrolle2e", "password123")
            .await
            .expect("seed user");

        let app = crate::console::handlers::router(state.clone());
        let base = state.base_path().to_string();

        let response = post_form(
            &app,
            format!("{}/login", base),
            None,
            "identifier=enrolle2e&password=password123".to_string(),
        )
        .await;
        assert!(response.status().is_redirection());
        assert!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.ends_with("/login/mfa?mode=enroll"))
                .unwrap_or(false)
        );
        let mfa_cookie = cookie_from(&response, "rustpbx_mfa").expect("mfa cookie issued");
        assert!(
            cookie_from(&response, "rustpbx_session").is_none(),
            "no session cookie before enrollment"
        );

        let page = get_uri(
            &app,
            format!("{}/login/mfa?mode=enroll", base),
            Some(&mfa_cookie),
        )
        .await;
        assert_eq!(page.status(), StatusCode::OK);
        let body = axum::body::to_bytes(page.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = String::from_utf8_lossy(&body).into_owned();
        assert!(html.contains("mfa-qr"));
        assert!(html.contains("mfa-secret-input"));
        assert!(html.contains("enroll"));

        let secret_response =
            get_uri(&app, format!("{}/login/mfa/enroll-secret", base), Some(&mfa_cookie)).await;
        assert_eq!(secret_response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(secret_response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("parse json");
        let secret = payload["secret"].as_str().expect("secret").to_string();
        assert!(!secret.is_empty());
        assert!(payload["otpauth_uri"].as_str().unwrap_or_default().starts_with("otpauth://"));
        assert!(!payload["qr_png_base64"].as_str().unwrap_or_default().is_empty());

        let code = current_totp_code(&secret, &user.email);
        let enroll = post_form(
            &app,
            format!("{}/login/mfa/enroll", base),
            Some(&mfa_cookie),
            format!(
                "secret={}&code={}",
                urlencoding::encode(&secret),
                code
            ),
        )
        .await;
        assert!(enroll.status().is_redirection());
        assert!(
            enroll
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|v| v == format!("{}/", base))
                .unwrap_or(false)
        );
        let session_cookie = cookie_from(&enroll, "rustpbx_session").expect("session cookie issued");
        let cleared = set_cookie_header(&enroll, "rustpbx_mfa").expect("mfa cookie cleared");
        assert!(cleared.contains("Max-Age=0"), "mfa cookie cleared via Max-Age=0");

        let reloaded = state
            .find_user_by_email(&user.email)
            .await
            .expect("reload user")
            .expect("user exists");
        assert!(reloaded.mfa_enabled);

        let dashboard = get_uri(&app, format!("{}/", base), Some(&session_cookie)).await;
        assert_eq!(dashboard.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn mfa_step_locks_after_repeated_failures() {
        let state = setup_state().await;
        let user = state
            .create_user("locked-e2e@rustpbx.com", "lockede2e", "password123")
            .await
            .expect("seed user");
        let secret = enrollment_secret();
        let code = current_totp_code(&secret, &user.email);
        state
            .enable_mfa(&user, &secret, &code)
            .await
            .expect("enable mfa");

        let app = crate::console::handlers::router(state.clone());
        let base = state.base_path().to_string();

        let response = post_form(
            &app,
            format!("{}/login", base),
            None,
            "identifier=lockede2e&password=password123".to_string(),
        )
        .await;
        let mfa_cookie = cookie_from(&response, "rustpbx_mfa").expect("mfa cookie issued");

        for attempt in 0..crate::console::MFA_MAX_ATTEMPTS {
            let response = post_form(
                &app,
                format!("{}/login/mfa", base),
                Some(&mfa_cookie),
                "code=000000".to_string(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "attempt {}", attempt);
        }

        let locked = post_form(
            &app,
            format!("{}/login/mfa", base),
            Some(&mfa_cookie),
            format!("code={}", code),
        )
        .await;
        assert_eq!(locked.status(), StatusCode::OK);
        let body = axum::body::to_bytes(locked.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = String::from_utf8_lossy(&body).into_owned();
        assert!(
            html.contains("Too many failed attempts"),
            "expected lockout message"
        );
    }

    #[tokio::test]
    async fn enroll_requires_mfa_session_cookie() {
        let state = setup_state().await;
        let app = crate::console::handlers::router(state.clone());
        let base = state.base_path().to_string();

        let response = get_uri(&app, format!("{}/login/mfa/enroll-secret", base), None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = post_form(
            &app,
            format!("{}/login/mfa/enroll", base),
            None,
            "secret=ABC&code=123456".to_string(),
        )
        .await;
        assert!(response.status().is_redirection());
    }
}
