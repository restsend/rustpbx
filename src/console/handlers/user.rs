use crate::{
    console::{
        ConsoleState,
        auth::RegistrationPolicy,
        handlers::forms::{ForgotForm, LoginForm, LoginQuery, RegisterForm, ResetForm},
    },
    handler::middleware::clientaddr::ClientAddr,
};
use axum::{
    Router,
    extract::{Form, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header::SET_COOKIE},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use serde_json::json;
use std::sync::Arc;
use tracing::{info, warn};

fn is_secure_request(headers: &HeaderMap) -> bool {
    if let Some(proto) = headers.get("x-forwarded-proto") {
        if let Ok(proto_str) = proto.to_str() {
            return proto_str.eq_ignore_ascii_case("https");
        }
    }
    false
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/login", get(login_page).post(login_post))
        .route("/logout", get(logout))
        .route("/register", get(register_page).post(register_post))
        .route("/forgot", get(forgot_page).post(forgot_post))
        .route("/reset/{token}", get(reset_page).post(reset_post))
}

const SUPERUSER_NOTICE: &str =
    "You are creating the first administrator account. Please store this password securely.";

pub async fn login_page(
    State(state): State<Arc<ConsoleState>>,
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

    state.render(
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
        return state.render(
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
        );
    }

    match state.authenticate(identifier, password).await {
        Ok(Some(user)) => {
            if let Err(err) = state.mark_login(&user, client_addr.ip().to_string()).await {
                warn!("failed to update last_login: {}", err);
            }
            let redirect_target = resolve_next_redirect(state.as_ref(), next.clone());
            let mut response = Redirect::to(&redirect_target).into_response();
            if let Some(header) = state.session_cookie_header(user.id, is_secure_request(&headers))
            {
                response.headers_mut().append(SET_COOKIE, header);
            }
            response
        }
        Ok(None) => state.render(
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
        ),
        Err(err) => {
            warn!("login error: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed").into_response()
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
    let next = query.next.unwrap_or_else(|| state.url_for("/"));
    let mut response = Redirect::to(&next).into_response();
    if let Some(header) = state.clear_session_cookie(is_secure_request(&headers)) {
        response.headers_mut().append(SET_COOKIE, header);
    }
    response
}

pub async fn register_page(State(state): State<Arc<ConsoleState>>) -> Response {
    let policy = match state.registration_policy().await {
        Ok(policy) => policy,
        Err(err) => {
            warn!("failed to load registration policy: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to load registration page",
            )
                .into_response();
        }
    };

    let superuser_notice = if policy.first_user {
        Some(SUPERUSER_NOTICE.to_string())
    } else {
        None
    };

    let mut response = state.render(
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
            return (StatusCode::INTERNAL_SERVER_ERROR, "Registration failed").into_response();
        }
    };

    if !policy.allowed {
        let mut response = state.render(
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
        match state.email_exists(&email).await {
            Ok(true) => error_message = Some("Email is already registered".to_string()),
            Ok(false) => {}
            Err(err) => {
                warn!("failed to check email uniqueness: {}", err);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Registration failed").into_response();
            }
        }
    }

    if error_message.is_none() {
        match state.username_exists(&username).await {
            Ok(true) => error_message = Some("Username is already taken".to_string()),
            Ok(false) => {}
            Err(err) => {
                warn!("failed to check username uniqueness: {}", err);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Registration failed").into_response();
            }
        }
    }

    if let Some(error) = error_message {
        return state.render(
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
        );
    }

    match state.create_user(&email, &username, &password).await {
        Ok(user) => {
            if policy.first_user {
                info!("created initial superuser account: {}", user.username);
            }
            let mut response = Redirect::to(&state.url_for("/")).into_response();
            if let Some(header) = state.session_cookie_header(user.id, is_secure_request(&headers))
            {
                response.headers_mut().append(SET_COOKIE, header);
            }
            response
        }
        Err(err) => {
            warn!("failed to create user: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Registration failed").into_response()
        }
    }
}

pub async fn forgot_page(State(state): State<Arc<ConsoleState>>) -> Response {
    state.render(
        "console/forgot.html",
        json!({
            "info_message": null,
            "error_message": null,
            "reset_link": null,
        }),
    )
}

pub async fn forgot_post(
    State(state): State<Arc<ConsoleState>>,
    Form(form): Form<ForgotForm>,
) -> Response {
    let email = form.email.trim().to_lowercase();

    if email.is_empty() {
        return state.render(
            "console/forgot.html",
            json!({
                "info_message": null,
                "error_message": "Please enter your registered email address",
                "reset_link": null,
            }),
        );
    }

    let mut reset_link = None;
    match state.find_user_by_email(&email).await {
        Ok(Some(user)) => match state.upsert_reset_token(&user).await {
            Ok((token, _)) => {
                let link = state.url_for(&format!("/reset/{}", token));
                info!("password reset link generated for {}: {}", email, link);
                reset_link = Some(link);
            }
            Err(err) => {
                warn!("failed to save reset token: {}", err);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Unable to process request",
                )
                    .into_response();
            }
        },
        Ok(None) => {}
        Err(err) => {
            warn!("failed to handle forgot password: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to process request",
            )
                .into_response();
        }
    }

    state.render(
        "console/forgot.html",
        json!({
            "forgot_action": state.url_for("/forgot"),
            "info_message": "If the account exists, we've sent a reset link",
            "error_message": null,
            "reset_link": reset_link,
        }),
    )
}

pub async fn reset_page(
    State(state): State<Arc<ConsoleState>>,
    AxumPath(token): AxumPath<String>,
) -> Response {
    match state.find_by_reset_token(&token).await {
        Ok(Some(user)) => {
            if user.token_expired() {
                state.render(
                    "console/forgot.html",
                    json!({
                        "forgot_action": state.url_for("/forgot"),
                        "info_message": null,
                        "error_message": "Reset link has expired. Please request a new one.",
                        "reset_link": null,
                    }),
                )
            } else {
                state.render(
                    "console/reset.html",
                    json!({
                        "reset_action": state.url_for(&format!("/reset/{}", token)),
                        "token": token,
                        "error_message": null,
                    }),
                )
            }
        }
        Ok(None) => state.render(
            "console/forgot.html",
            json!({
                "forgot_action": state.url_for("/forgot"),
                "info_message": null,
                "error_message": "Reset link is invalid",
                "reset_link": null,
            }),
        ),
        Err(err) => {
            warn!("failed to verify reset token: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to process reset request",
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
                return state.render(
                    "console/forgot.html",
                    json!({
                        "forgot_action": state.url_for("/forgot"),
                        "info_message": null,
                        "error_message": "Reset link has expired. Please request a new one.",
                        "reset_link": null,
                    }),
                );
            }
            let password = form.password.trim();
            let confirm = form.confirm_password.trim();
            if password.len() < 8 {
                return state.render(
                    "console/reset.html",
                    json!({
                        "reset_action": state.url_for(&format!("/reset/{}", token)),
                        "token": token,
                        "error_message": "Password must be at least 8 characters",
                    }),
                );
            }
            if password != confirm {
                return state.render(
                    "console/reset.html",
                    json!({
                        "reset_action": state.url_for(&format!("/reset/{}", token)),
                        "token": token,
                        "error_message": "Passwords do not match",
                    }),
                );
            }

            match state.update_password(&user, password).await {
                Ok(updated_user) => {
                    let mut response = Redirect::to(&state.url_for("/")).into_response();
                    if let Some(header) =
                        state.session_cookie_header(updated_user.id, is_secure_request(&headers))
                    {
                        response.headers_mut().append(SET_COOKIE, header);
                    }
                    response
                }
                Err(err) => {
                    warn!("failed to update password: {}", err);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to reset password",
                    )
                        .into_response()
                }
            }
        }
        Ok(None) => state.render(
            "console/forgot.html",
            json!({
                "forgot_action": state.url_for("/forgot"),
                "info_message": null,
                "error_message": "Reset link is invalid",
                "reset_link": null,
            }),
        ),
        Err(err) => {
            warn!("failed to reset password: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to reset password",
            )
                .into_response()
        }
    }
}
