use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde_json::json;
use std::sync::Arc;

use crate::console::ConsoleState;
use crate::console::middleware::AuthRequired;

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/security/bans", get(bans_page))
        .route("/security/bans/{ip}/release", post(release_ban))
}

fn ban_store(state: &ConsoleState) -> Option<Arc<crate::security::BanStore>> {
    state
        .app_state()
        .and_then(|app| app.sip_server().inner.bans.clone())
}

async fn bans_page(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let current_user = state.build_current_user_ctx(&user).await;
    let bans = match ban_store(&state) {
        Some(store) => store.list_active().await.unwrap_or_default(),
        None => Vec::new(),
    };

    state.render_with_headers(
        "console/security_bans.html",
        json!({
            "nav_active": "security",
            "bans": bans,
            "current_user": current_user,
        }),
        &headers,
    )
}

async fn release_ban(
    State(state): State<Arc<ConsoleState>>,
    Path(ip): Path<String>,
    AuthRequired(user): AuthRequired,
) -> Response {
    let actor = user.username.clone();
    if let Some(store) = ban_store(&state) {
        tracing::info!(%ip, actor, "releasing ban via console");
        if let Err(e) = store.release(&ip, &actor).await {
            tracing::error!(%ip, error = %e, "failed to release ban");
        }
    }
    Redirect::to(&format!("{}/security/bans", state.base_path())).into_response()
}
