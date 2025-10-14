use crate::console::ConsoleState;
use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;

pub mod bill_template;
pub mod call_record;
pub mod dashboard;
pub mod diagnostics;
pub mod extension;
pub mod forms;
pub mod routing;
pub mod setting;
pub mod sip_trunk;
pub mod user;

pub fn router(state: Arc<ConsoleState>) -> Router {
    let base_path = state.base_path().to_string();
    let routes = Router::new()
        .merge(user::urls())
        .merge(extension::urls())
        .merge(bill_template::urls())
        .merge(sip_trunk::urls())
        .merge(setting::urls())
        .route(
            "/routing",
            get(self::routing::page_routing).post(self::routing::create_routing),
        )
        .route("/routing/new", get(self::routing::page_routing_create))
        .route(
            "/routing/{id}",
            get(self::routing::page_routing_edit).post(self::routing::update_routing),
        )
        .route("/routing/{id}/delete", post(self::routing::delete_routing))
        .route("/call-records", get(self::call_record::page_call_records))
        .route(
            "/call-records/{id}",
            get(self::call_record::page_call_record_detail),
        )
        .route("/diagnostics", get(self::diagnostics::page_diagnostics));

    Router::new()
        .route(&format!("{base_path}/"), get(self::dashboard::dashboard))
        .nest(&base_path, routes)
        .with_state(state)
}
