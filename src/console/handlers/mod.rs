use crate::console::ConsoleState;
use axum::{Router, routing::get};
use std::sync::Arc;

pub mod call_record;
pub mod dashboard;
pub mod extension;
pub mod forms;
pub mod routing;
pub mod setting;
pub mod sip_trunk;
pub mod testing;
pub mod user;

pub fn router(state: Arc<ConsoleState>) -> Router {
    let base_path = state.base_path().to_string();
    let routes = Router::new()
        .route(
            "/login",
            get(self::user::login_page).post(self::user::login_post),
        )
        .route("/logout", get(self::user::logout))
        .route(
            "/register",
            get(self::user::register_page).post(self::user::register_post),
        )
        .route(
            "/forgot",
            get(self::user::forgot_page).post(self::user::forgot_post),
        )
        .route(
            "/reset/{token}",
            get(self::user::reset_page).post(self::user::reset_post),
        )
        // PBX pages
        .route("/extensions", get(self::extension::page_extensions))
        .route(
            "/extensions/new",
            get(self::extension::page_extension_create),
        )
        .route(
            "/extensions/{id}",
            get(self::extension::page_extension_detail),
        )
        .route("/routing", get(self::routing::page_routing))
        .route("/sip", get(self::sip_trunk::page_sip))
        .route("/call-records", get(self::call_record::page_call_records))
        .route("/test", get(self::testing::page_testing))
        .route("/settings", get(self::setting::page_settings));

    Router::new()
        .route(&format!("{base_path}/"), get(self::dashboard::dashboard))
        .nest(&base_path, routes)
        .with_state(state)
}
