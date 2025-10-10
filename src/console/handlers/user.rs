use crate::{
    console::{
        ConsoleState,
        handlers::forms::{ForgotForm, LoginForm, LoginQuery, RegisterForm, ResetForm},
    },
    handler::middleware::clientaddr::ClientAddr,
};
use axum::{
    extract::{Form, Path as AxumPath, Query, State},
    http::{StatusCode, header::SET_COOKIE},
    response::{IntoResponse, Redirect, Response},
};
use serde_json::json;
use std::sync::Arc;
use tracing::{info, warn};

pub async fn login_page(
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<LoginQuery>,
) -> Response {
    state.render(
        "console/login.html",
        json!({
            "login_action": state.login_url(query.next.clone()),
            "register_url": state.register_url(query.next),
            "error_message": null,
            "identifier": "",
        }),
    )
}

pub async fn login_post(
    client_addr: ClientAddr,
    State(state): State<Arc<ConsoleState>>,
    Form(form): Form<LoginForm>,
) -> Response {
    let identifier = form.identifier.trim();
    let password = form.password.trim();
    let next = form.next.clone();
    if identifier.is_empty() || password.is_empty() {
        return state.render(
            "console/login.html",
            json!({
                "login_action": state.login_url(next.clone()),
                "register_url": state.register_url(next),
                "error_message": "Please provide both username/email and password",
                "identifier": identifier,
            }),
        );
    }

    match state.authenticate(identifier, password).await {
        Ok(Some(user)) => {
            if let Err(err) = state.mark_login(&user, client_addr.ip().to_string()).await {
                warn!("failed to update last_login: {}", err);
            }
            let mut response = Redirect::to(&state.url_for("/")).into_response();
            if let Some(header) = state.session_cookie_header(user.id) {
                response.headers_mut().append(SET_COOKIE, header);
            }
            response
        }
        Ok(None) => state.render(
            "console/login.html",
            json!({
                "login_action": state.login_url(next.clone()),
                "register_url": state.register_url(next),
                "error_message": "Invalid credentials",
                "identifier": identifier,
            }),
        ),
        Err(err) => {
            warn!("login error: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed").into_response()
        }
    }
}

pub async fn logout(
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<LoginQuery>,
) -> Response {
    let next = query.next.unwrap_or_else(|| state.url_for("/"));
    let mut response = Redirect::to(&next).into_response();
    if let Some(header) = state.clear_session_cookie() {
        response.headers_mut().append(SET_COOKIE, header);
    }
    response
}

pub async fn register_page(State(state): State<Arc<ConsoleState>>) -> Response {
    state.render(
        "console/register.html",
        json!({
            "register_action": state.url_for("/register"),
            "login_url": state.url_for("/login"),
            "error_message": null,
            "email": "",
            "username": "",
        }),
    )
}

pub async fn register_post(
    State(state): State<Arc<ConsoleState>>,
    Form(form): Form<RegisterForm>,
) -> Response {
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
            }),
        );
    }

    match state.create_user(&email, &username, &password).await {
        Ok(user) => {
            let mut response = Redirect::to(&state.url_for("/")).into_response();
            if let Some(header) = state.session_cookie_header(user.id) {
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
                    if let Some(header) = state.session_cookie_header(updated_user.id) {
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
