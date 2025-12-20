use crate::{app::AppState, handler::middleware::clientaddr::ClientAddr};
use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use tracing::warn;

#[cfg(feature = "console")]
use crate::console::middleware::extract_session_cookie;

pub async fn ami_auth_middleware(
    State(state): State<AppState>,
    client_ip: ClientAddr,
    request: Request,
    next: Next,
) -> Response {
    #[allow(unused_mut)]
    let mut allowed = state.config().ami.as_ref().map_or(false, |ami| {
        ami.is_allowed(client_ip.ip().to_string().as_str())
    });

    #[cfg(feature = "console")]
    if !allowed {
        // Let authenticated console superusers bypass AMI IP checks.
        if let Some(console_state) = &state.console {
            if let Some(cookie_value) = extract_session_cookie(request.headers()) {
                match console_state.current_user(Some(&cookie_value)).await {
                    Ok(Some(user)) => {
                        if user.is_superuser {
                            allowed = true;
                        }
                    }
                    Ok(None) => {
                        // Session cookie present but no active user; fall back to IP checks
                    }
                    Err(err) => {
                        warn!(%client_ip, error = %err, "Failed to resolve console user for AMI access");
                    }
                }
            }
        }
    }

    if !allowed {
        warn!(
            %client_ip,
            "AMI access denied for client"
        );

        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "Access denied",
                "message": "You don't have permission to access AMI interfaces"
            })),
        )
            .into_response();
    }

    next.run(request).await
}
