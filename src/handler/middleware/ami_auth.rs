use crate::{app::AppState, handler::middleware::clientaddr::ClientAddr};
use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use tracing::warn;

pub async fn ami_auth_middleware(
    State(state): State<AppState>,
    client_ip: ClientAddr,
    request: Request,
    next: Next,
) -> Response {
    let is_allowed = state.config.ami.as_ref().map_or(false, |ami| {
        ami.is_allowed(client_ip.ip().to_string().as_str())
    });

    if !is_allowed {
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
