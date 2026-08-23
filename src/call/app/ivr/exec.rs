//! Shared helpers for ivr.exec completion and return-app query building.

use crate::proxy::proxy_call::ivr_exec_hook::{IvrExecResult, IvrExecState};
use crate::proxy::proxy_call::session_hooks::SessionExtensions;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;

use super::common::effective_return_app;

/// Append `return_app` / `return_target` (and optional `return_menu`) to a query string.
pub fn append_return_app_query(
    query: &mut String,
    return_app: &Option<String>,
    return_target: &Option<String>,
    return_menu: Option<&str>,
) {
    if let Some(app) = effective_return_app(return_app, return_target) {
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str(&format!("return_app={}", urlencoding::encode(app)));
        if let Some(rt) = return_target.as_deref().filter(|s| !s.is_empty()) {
            query.push_str(&format!("&return_target={}", urlencoding::encode(rt)));
        }
        if let Some(menu) = return_menu.filter(|s| !s.is_empty() && *s != "root") {
            query.push_str(&format!("&return_menu={}", urlencoding::encode(menu)));
        }
    }
}

/// Append `return_app` / `return_target` to a URI path (adds `?` or `&` as needed).
pub fn append_return_app_to_uri(
    uri: &mut String,
    return_app: &Option<String>,
    return_target: &Option<String>,
) {
    if let Some(app) = effective_return_app(return_app, return_target) {
        let sep = if uri.contains('?') { "&" } else { "?" };
        uri.push_str(&format!("{sep}return_app={}", urlencoding::encode(app)));
        if let Some(rt) = return_target.as_deref().filter(|s| !s.is_empty()) {
            uri.push_str(&format!("&return_target={}", urlencoding::encode(rt)));
        }
    }
}

/// Write an [`IvrExecResult`] when the session was started via `ivr.exec`.
pub fn write_ivr_exec_result(extensions: &SessionExtensions, result: IvrExecResult) {
    if extensions.read().get::<IvrExecState>().is_some() {
        extensions.write().insert(result);
    }
}

/// Publish IVR end-reason keys consumed by session hangup / CDR enrichment.
pub fn publish_ivr_end_reason(
    runtime_vars: Option<&Arc<DashMap<String, String>>>,
    end_reason: &str,
    ivr_name: &str,
) {
    if let Some(vars) = runtime_vars {
        vars.insert("ivr_end_reason".to_string(), end_reason.to_string());
        vars.insert("ivr_status".to_string(), end_reason.to_string());
        vars.insert("ivr_name".to_string(), ivr_name.to_string());
    }
}

/// Build an [`IvrExecResult`] payload for ivr.exec completion.
pub fn build_ivr_exec_result(
    status: &str,
    reason: &str,
    routing_target: Option<String>,
    collected: HashMap<String, String>,
    duration_ms: u64,
) -> IvrExecResult {
    IvrExecResult {
        status: status.to_string(),
        reason: reason.to_string(),
        routing_target,
        collected,
        trace: vec![],
        duration_ms,
        completion_time: chrono::Utc::now().to_rfc3339(),
    }
}
