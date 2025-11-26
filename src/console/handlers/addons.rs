use crate::console::ConsoleState;
use axum::{
    extract::State,
    response::IntoResponse,
    routing::get,
    Router,
};
use std::sync::Arc;

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new().route("/addons", get(index))
}

pub async fn index(State(state): State<Arc<ConsoleState>>) -> impl IntoResponse {
    let addons = if let Some(app_state) = state.app_state() {
        app_state.addon_registry.list_addons()
    } else {
        vec![]
    };

    state.render("console/addons.html", serde_json::json!({
        "addons": addons,
        "page_title": "Addons",
        "nav_active": "addons"
    }))
}
