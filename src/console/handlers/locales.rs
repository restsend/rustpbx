use crate::console::ConsoleState;
use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::sync::Arc;

pub fn api_urls() -> Router<Arc<ConsoleState>> {
    Router::new().route("/locales/{lang}", get(get_locale_js))
}

async fn get_locale_js(
    State(state): State<Arc<ConsoleState>>,
    Path(lang): Path<String>,
) -> Response {
    let lang = lang.strip_suffix(".js").unwrap_or(&lang);
    let json = state.i18n.get_translations_json(lang);
    let js = format!("window.__i18n_t = {};", json);

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=604800, immutable"),
        ],
        js,
    )
        .into_response()
}
