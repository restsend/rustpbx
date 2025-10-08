use crate::console::{ConsoleState, auth::extract_session_cookie};
use askama::Template;
use axum::{
    Router,
    extract::{Form, Path as AxumPath, State},
    http::{HeaderMap, StatusCode, header::SET_COOKIE},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Deserialize, Default, Clone)]
struct LoginForm {
    identifier: String,
    password: String,
}

#[derive(Deserialize, Default, Clone)]
struct RegisterForm {
    email: String,
    username: String,
    password: String,
    confirm_password: String,
    invite_code: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
struct ForgotForm {
    email: String,
}

#[derive(Deserialize, Default, Clone)]
struct ResetForm {
    password: String,
    confirm_password: String,
}

struct HtmlTemplate<T>(T);

impl<T> IntoResponse for HtmlTemplate<T>
where
    T: Template,
{
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => {
                warn!("failed to render template: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Template rendering failed",
                )
                    .into_response()
            }
        }
    }
}

#[derive(Template)]
#[template(path = "console/login.html")]
struct LoginTemplate {
    login_action: String,
    register_url: String,
    forgot_url: String,
    error_message: Option<String>,
    identifier: String,
}

#[derive(Template)]
#[template(path = "console/register.html")]
struct RegisterTemplate {
    register_action: String,
    login_url: String,
    invite_required: bool,
    error_message: Option<String>,
    email: String,
    username: String,
    invite_value: String,
}

#[derive(Template)]
#[template(path = "console/forgot.html")]
struct ForgotTemplate {
    forgot_action: String,
    login_url: String,
    info_message: Option<String>,
    error_message: Option<String>,
    reset_link: Option<String>,
}

#[derive(Template)]
#[allow(dead_code)]
#[template(path = "console/reset.html")]
struct ResetTemplate {
    reset_action: String,
    login_url: String,
    token: String,
    error_message: Option<String>,
}

#[derive(Template)]
#[template(path = "console/dashboard.html")]
struct DashboardTemplate {
    logout_url: String,
    username: String,
    email: String,
}

pub fn router(state: Arc<ConsoleState>) -> Router {
    let base_path = state.base_path().to_string();
    let routes = Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_page).post(login_post))
        .route("/logout", get(logout))
        .route("/register", get(register_page).post(register_post))
        .route("/forgot", get(forgot_page).post(forgot_post))
        .route("/reset/{token}", get(reset_page).post(reset_post))
        .with_state(state);

    Router::new().nest(&base_path, routes)
}

async fn dashboard(State(state): State<Arc<ConsoleState>>, headers: HeaderMap) -> Response {
    let session_cookie = extract_session_cookie(&headers);
    match state.current_user(session_cookie.as_deref()).await {
        Ok(Some(user)) => HtmlTemplate(DashboardTemplate {
            logout_url: state.url_for("/logout"),
            username: user.username,
            email: user.email,
        })
        .into_response(),
        Ok(None) => Redirect::to(&state.url_for("/login")).into_response(),
        Err(err) => {
            warn!("failed to load dashboard: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to load console").into_response()
        }
    }
}

async fn login_page(State(state): State<Arc<ConsoleState>>, headers: HeaderMap) -> Response {
    let session_cookie = extract_session_cookie(&headers);
    match state.current_user(session_cookie.as_deref()).await {
        Ok(Some(_)) => Redirect::to(&state.url_for("/")).into_response(),
        Ok(None) => HtmlTemplate(LoginTemplate {
            login_action: state.url_for("/login"),
            register_url: state.url_for("/register"),
            forgot_url: state.url_for("/forgot"),
            error_message: None,
            identifier: String::new(),
        })
        .into_response(),
        Err(err) => {
            warn!("failed to render login page: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load sign-in page",
            )
                .into_response()
        }
    }
}

async fn login_post(
    State(state): State<Arc<ConsoleState>>,
    Form(form): Form<LoginForm>,
) -> Response {
    let identifier = form.identifier.trim();
    let password = form.password.trim();
    if identifier.is_empty() || password.is_empty() {
        return HtmlTemplate(LoginTemplate {
            login_action: state.url_for("/login"),
            register_url: state.url_for("/register"),
            forgot_url: state.url_for("/forgot"),
            error_message: Some("Please provide both username/email and password".to_string()),
            identifier: identifier.to_string(),
        })
        .into_response();
    }

    match state.authenticate(identifier, password).await {
        Ok(Some(user)) => {
            if let Err(err) = state.mark_login(&user).await {
                warn!("failed to update last_login: {}", err);
            }
            let mut response = Redirect::to(&state.url_for("/")).into_response();
            if let Some(header) = state.session_cookie_header(user.id) {
                response.headers_mut().append(SET_COOKIE, header);
            }
            response
        }
        Ok(None) => HtmlTemplate(LoginTemplate {
            login_action: state.url_for("/login"),
            register_url: state.url_for("/register"),
            forgot_url: state.url_for("/forgot"),
            error_message: Some("Invalid credentials".to_string()),
            identifier: identifier.to_string(),
        })
        .into_response(),
        Err(err) => {
            warn!("login error: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed").into_response()
        }
    }
}

async fn logout(State(state): State<Arc<ConsoleState>>) -> Response {
    let mut response = Redirect::to(&state.url_for("/login")).into_response();
    if let Some(header) = state.clear_session_cookie() {
        response.headers_mut().append(SET_COOKIE, header);
    }
    response
}

async fn register_page(State(state): State<Arc<ConsoleState>>, headers: HeaderMap) -> Response {
    let session_cookie = extract_session_cookie(&headers);
    match state.current_user(session_cookie.as_deref()).await {
        Ok(Some(_)) => Redirect::to(&state.url_for("/")).into_response(),
        Ok(None) => HtmlTemplate(RegisterTemplate {
            register_action: state.url_for("/register"),
            login_url: state.url_for("/login"),
            invite_required: state.invite_code().is_some(),
            error_message: None,
            email: String::new(),
            username: String::new(),
            invite_value: String::new(),
        })
        .into_response(),
        Err(err) => {
            warn!("failed to load register page: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load sign-up page",
            )
                .into_response()
        }
    }
}

async fn register_post(
    State(state): State<Arc<ConsoleState>>,
    Form(form): Form<RegisterForm>,
) -> Response {
    let email = form.email.trim().to_lowercase();
    let username = form.username.trim().to_string();
    let password = form.password.trim().to_string();
    let confirm = form.confirm_password.trim().to_string();
    let invite = form.invite_code.as_ref().map(|s| s.trim().to_string());

    let invite_required = state.invite_code().is_some();
    let mut error_message = None;

    if !email.contains('@') {
        error_message = Some("Please enter a valid email address".to_string());
    } else if username.len() < 3 {
        error_message = Some("Username must be at least 3 characters".to_string());
    } else if password.len() < 8 {
        error_message = Some("Password must be at least 8 characters".to_string());
    } else if password != confirm {
        error_message = Some("Passwords do not match".to_string());
    } else if invite_required && state.invite_code() != invite.as_deref() {
        error_message = Some("Invalid invite code".to_string());
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
        let invite_value = invite.unwrap_or_default();
        return HtmlTemplate(RegisterTemplate {
            register_action: state.url_for("/register"),
            login_url: state.url_for("/login"),
            invite_required,
            error_message: Some(error),
            email: email.clone(),
            username: username.clone(),
            invite_value,
        })
        .into_response();
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

async fn forgot_page(State(state): State<Arc<ConsoleState>>) -> Response {
    HtmlTemplate(ForgotTemplate {
        forgot_action: state.url_for("/forgot"),
        login_url: state.url_for("/login"),
        info_message: None,
        error_message: None,
        reset_link: None,
    })
    .into_response()
}

async fn forgot_post(
    State(state): State<Arc<ConsoleState>>,
    Form(form): Form<ForgotForm>,
) -> Response {
    let email = form.email.trim().to_lowercase();

    if email.is_empty() {
        return HtmlTemplate(ForgotTemplate {
            forgot_action: state.url_for("/forgot"),
            login_url: state.url_for("/login"),
            info_message: None,
            error_message: Some("Please enter your registered email address".to_string()),
            reset_link: None,
        })
        .into_response();
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

    HtmlTemplate(ForgotTemplate {
        forgot_action: state.url_for("/forgot"),
        login_url: state.url_for("/login"),
        info_message: Some("If the account exists, we've sent a reset link".to_string()),
        error_message: None,
        reset_link,
    })
    .into_response()
}

async fn reset_page(
    State(state): State<Arc<ConsoleState>>,
    AxumPath(token): AxumPath<String>,
) -> Response {
    match state.find_by_reset_token(&token).await {
        Ok(Some(user)) => {
            if user.token_expired() {
                HtmlTemplate(ForgotTemplate {
                    forgot_action: state.url_for("/forgot"),
                    login_url: state.url_for("/login"),
                    info_message: None,
                    error_message: Some(
                        "Reset link has expired. Please request a new one.".to_string(),
                    ),
                    reset_link: None,
                })
                .into_response()
            } else {
                HtmlTemplate(ResetTemplate {
                    reset_action: state.url_for(&format!("/reset/{}", token)),
                    login_url: state.url_for("/login"),
                    token,
                    error_message: None,
                })
                .into_response()
            }
        }
        Ok(None) => HtmlTemplate(ForgotTemplate {
            forgot_action: state.url_for("/forgot"),
            login_url: state.url_for("/login"),
            info_message: None,
            error_message: Some("Reset link is invalid".to_string()),
            reset_link: None,
        })
        .into_response(),
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

async fn reset_post(
    State(state): State<Arc<ConsoleState>>,
    AxumPath(token): AxumPath<String>,
    Form(form): Form<ResetForm>,
) -> Response {
    match state.find_by_reset_token(&token).await {
        Ok(Some(user)) => {
            if user.token_expired() {
                return HtmlTemplate(ForgotTemplate {
                    forgot_action: state.url_for("/forgot"),
                    login_url: state.url_for("/login"),
                    info_message: None,
                    error_message: Some(
                        "Reset link has expired. Please request a new one.".to_string(),
                    ),
                    reset_link: None,
                })
                .into_response();
            }
            let password = form.password.trim();
            let confirm = form.confirm_password.trim();
            if password.len() < 8 {
                return HtmlTemplate(ResetTemplate {
                    reset_action: state.url_for(&format!("/reset/{}", token)),
                    login_url: state.url_for("/login"),
                    token,
                    error_message: Some("Password must be at least 8 characters".to_string()),
                })
                .into_response();
            }
            if password != confirm {
                return HtmlTemplate(ResetTemplate {
                    reset_action: state.url_for(&format!("/reset/{}", token)),
                    login_url: state.url_for("/login"),
                    token,
                    error_message: Some("Passwords do not match".to_string()),
                })
                .into_response();
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
        Ok(None) => HtmlTemplate(ForgotTemplate {
            forgot_action: state.url_for("/forgot"),
            login_url: state.url_for("/login"),
            info_message: None,
            error_message: Some("Reset link is invalid".to_string()),
            reset_link: None,
        })
        .into_response(),
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
