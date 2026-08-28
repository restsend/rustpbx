//! Shared helpers for ivr.exec completion, return-app query building, and
//! `start_app` sub-flow orchestration.

use crate::call::app::ApplicationContext;
use crate::call::app::CallApp;
use crate::call::domain::ReturnAppSpec;
use crate::proxy::proxy_call::ivr_exec_hook::{IvrExecResult, IvrExecState};
use crate::proxy::proxy_call::session_hooks::SessionExtensions;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;

use super::common::effective_return_app;

/// Session var: return-app name written by [`store_pending_return`].
pub const PENDING_RETURN_APP_KEY: &str = "_pending_return_app";
/// Session var: JSON [`ReturnAppSpec::params`] for the pending return app.
pub const PENDING_RETURN_PARAMS_KEY: &str = "_pending_return_params";
/// Session var: IVR config file path used when this session's IVR was started.
pub const IVR_START_FILE_KEY: &str = "ivr_start_file";
/// Session var: last sub-app exit status (`completed`, `error`, …).
pub const SUB_APP_STATUS_KEY: &str = "sub_app_status";
/// Session var: name of the last sub-app that ran.
pub const SUB_APP_NAME_KEY: &str = "sub_app_name";

/// Record the IVR config file path so `start_app` return specs can resume IVR.
pub fn remember_ivr_start_file(ctx: &ApplicationContext, file: &str) {
    if !file.is_empty() {
        ctx.set_var(IVR_START_FILE_KEY, file);
    }
}

/// Publish standardized sub-app exit metadata for IVR variable substitution.
pub fn publish_sub_app_exit(ctx: &ApplicationContext, app_name: &str, status: &str) {
    publish_sub_app_exit_to_vars(&ctx.session_vars, app_name, status);
}

/// Publish sub-app exit metadata directly into a session-vars map.
pub fn publish_sub_app_exit_to_vars(
    vars: &dashmap::DashMap<String, String>,
    app_name: &str,
    status: &str,
) {
    vars.insert(SUB_APP_NAME_KEY.to_string(), app_name.to_string());
    vars.insert(SUB_APP_STATUS_KEY.to_string(), status.to_string());
}

/// Store a pending return target before chaining to a sub-app.
pub fn store_pending_return(
    ctx: &ApplicationContext,
    return_app: &str,
    return_target: Option<&str>,
    return_menu: Option<&str>,
) {
    let mut ivr_params = HashMap::new();
    if let Some(menu) = return_menu.filter(|s| !s.is_empty() && *s != "root") {
        ivr_params.insert("return_menu".to_string(), menu.to_string());
    } else if let Some(target) = return_target.filter(|s| !s.is_empty()) {
        ivr_params.insert("return_menu".to_string(), target.to_string());
    }

    let params = if return_app == "ivr" {
        let file = ctx.get_var(IVR_START_FILE_KEY).unwrap_or_else(|| {
            return_target
                .filter(|s| !s.is_empty())
                .map(|name| format!("config/ivr/{name}.toml"))
                .unwrap_or_else(|| "config/ivr/main.toml".to_string())
        });
        let mut app_params = serde_json::json!({ "file": file });
        if !ivr_params.is_empty() {
            app_params["ivr_params"] = serde_json::json!(ivr_params);
        }
        app_params
    } else {
        serde_json::json!({})
    };

    ctx.set_var(PENDING_RETURN_APP_KEY, return_app);
    ctx.set_var(
        PENDING_RETURN_PARAMS_KEY,
        serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string()),
    );
}

/// Take and clear a pending return spec, if any.
pub fn take_pending_return(ctx: &ApplicationContext) -> Option<ReturnAppSpec> {
    let app_name = ctx
        .session_vars
        .remove(PENDING_RETURN_APP_KEY)
        .map(|(_, v)| v)?;
    let params_raw = ctx
        .session_vars
        .remove(PENDING_RETURN_PARAMS_KEY)
        .map(|(_, v)| v)
        .unwrap_or_else(|| "{}".to_string());
    let params = serde_json::from_str(&params_raw).unwrap_or(serde_json::json!({}));
    Some(ReturnAppSpec { app_name, params })
}

/// Resolve a `start_app` action: optionally store return metadata, then build the sub-app.
pub async fn prepare_start_app(
    ctx: &ApplicationContext,
    app: &str,
    params: Option<serde_json::Value>,
    return_app: &Option<String>,
    return_target: &Option<String>,
    return_menu: &Option<String>,
) -> anyhow::Result<Box<dyn CallApp>> {
    if let Some(ret_app) = effective_return_app(return_app, return_target) {
        store_pending_return(
            ctx,
            ret_app,
            return_target.as_deref(),
            return_menu.as_deref(),
        );
    }

    let factory = ctx
        .app_factory
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("app factory not available for start_app"))?;

    let mut params = params;
    if app == "csat_survey" {
        let needs_merge = params
            .as_ref()
            .map(|p| p.is_null() || p.as_object().is_some_and(|o| o.is_empty()))
            .unwrap_or(true);
        if needs_merge {
            if let Some(raw) = ctx.get_var(super::builtin::CSAT_PARAMS_KEY) {
                if let Ok(merged) = serde_json::from_str::<serde_json::Value>(&raw) {
                    params = Some(merged);
                }
            }
        }
    }

    match factory.create_app(app, params, ctx).await {
        Ok(Some(sub_app)) => Ok(sub_app),
        Ok(None) => anyhow::bail!("unknown application: {app}"),
        Err(e) => Err(e),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::app::CallInfo;
    use std::sync::Arc;

    fn make_ctx() -> ApplicationContext {
        ApplicationContext::new(
            sea_orm::DatabaseConnection::default(),
            CallInfo {
                session_id: "sess-1".into(),
                caller: "1001".into(),
                callee: "1002".into(),
                direction: "inbound".into(),
                started_at: chrono::Utc::now(),
                sip_headers: Default::default(),
                route_name: None,
            },
            Arc::new(crate::config::Config::default()),
        )
    }

    #[test]
    fn store_and_take_pending_return_ivr() {
        let ctx = make_ctx();
        remember_ivr_start_file(&ctx, "config/ivr/main.toml");
        store_pending_return(&ctx, "ivr", Some("post_call"), None);
        let spec = take_pending_return(&ctx).expect("pending return");
        assert_eq!(spec.app_name, "ivr");
        assert_eq!(spec.params["file"], "config/ivr/main.toml");
        assert_eq!(spec.params["ivr_params"]["return_menu"], "post_call");
        assert!(take_pending_return(&ctx).is_none());
    }

    #[test]
    fn publish_sub_app_exit_vars() {
        let ctx = make_ctx();
        publish_sub_app_exit(&ctx, "csat_survey", "completed");
        assert_eq!(ctx.get_var(SUB_APP_NAME_KEY), Some("csat_survey".into()));
        assert_eq!(ctx.get_var(SUB_APP_STATUS_KEY), Some("completed".into()));
    }
}
