use crate::app::AppState;
use axum::{Router, routing::get};

pub fn router(app_state: AppState) -> Router<AppState> {
    Router::new()
        .route("/health", get(super::ami::health_handler))
        .nest("/ami/v1", super::ami::router(app_state))
}
