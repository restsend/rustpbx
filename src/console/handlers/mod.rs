use crate::console::ConsoleState;
use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;

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
        // PBX pages
        // .route(
        //     "/extensions",
        //     get(self::extension::page_extensions).post(self::extension::create_extension),
        // )
        // .route(
        //     "/extensions/new",
        //     get(self::extension::page_extension_create),
        // )
        // .route(
        //     "/extensions/{id}",
        //     get(self::extension::page_extension_detail).post(self::extension::update_extension),
        // )
        // .route(
        //     "/extensions/{id}/delete",
        //     post(self::extension::delete_extension),
        // )
        // .route(
        //     "/extensions/{id}/toggle",
        //     post(self::extension::toggle_extension_login),
        // )
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
        .route("/sip-trunk", get(self::sip_trunk::page_sip_trunk))
        .route(
            "/sip-trunk/{id}",
            get(self::sip_trunk::page_sip_trunk_detail),
        )
        .route("/call-records", get(self::call_record::page_call_records))
        .route(
            "/call-records/{id}",
            get(self::call_record::page_call_record_detail),
        )
        .route("/diagnostics", get(self::diagnostics::page_diagnostics))
        .route("/settings", get(self::setting::page_settings));

    Router::new()
        .route(&format!("{base_path}/"), get(self::dashboard::dashboard))
        .nest(&base_path, routes)
        .with_state(state)
}
