use crate::call::domain::LegId;
use crate::proxy::proxy_call::session_hooks::{
    CallSessionContext, CallSessionHook, IvrExecCompletion, SendInfoSpec,
};
use async_trait::async_trait;
use serde::Serialize;
use std::collections::HashMap;
use tracing::{info, warn};

/// Per-session state written before an IVR exec starts.
/// Only present when `ivr.exec` is the active flow.
#[derive(Clone)]
pub struct IvrExecState {
    /// Correlator that will be echoed back in the result.
    pub request_id: String,
    /// The leg that was put on hold (typically "callee"), or `None` if
    /// `hold_agent` was `false`.
    pub held_leg: Option<LegId>,
    /// The leg that initiated the exec (for routing the result INFO).
    pub initiator_leg: LegId,
    /// Optional webhook URL for posting the result.
    pub webhook_url: Option<String>,
    /// Application name (e.g. "ivr", "csat_survey").
    pub app_name: String,
    /// Opaque metadata from the caller, echoed back in the result.
    pub metadata: serde_json::Value,
}

/// Result collected by the IVR app on exit and stored in extensions.
#[derive(Clone)]
pub struct IvrExecResult {
    pub status: String,
    pub reason: String,
    pub routing_target: Option<String>,
    pub collected: HashMap<String, String>,
    pub trace: Vec<serde_json::Value>,
    pub duration_ms: u64,
    pub completion_time: String,
}

/// Payload sent via webhook POST or SIP INFO result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct IvrExecResultPayload {
    event: String,
    request_id: String,
    call_id: String,
    app: String,
    status: String,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    routing_target: Option<String>,
    duration_ms: u64,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    collected: HashMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    trace: Vec<serde_json::Value>,
    metadata: serde_json::Value,
    completion_time: String,
}

const RESULT_CT: &str = "application/vnd.rustpbx.result+json";

/// Hook that handles the `ivr.exec` flow: on app exit, automatically unholds
/// the callee leg and sends the IVR result back as a SIP INFO with
/// `application/vnd.rustpbx.result+json` content-type.
pub struct IvrExecHook;

#[async_trait]
impl CallSessionHook for IvrExecHook {
    async fn on_app_exited(
        &self,
        ctx: &CallSessionContext,
    ) -> Option<IvrExecCompletion> {
        let session_id = ctx.session_id.clone();

        // Read IvrExecState — if absent this is not an ivr.exec flow.
        let exec_state = {
            let guard = ctx.extensions.read();
            guard.get::<IvrExecState>().cloned()?
        };
        let app_name = exec_state.app_name;
        let request_id = exec_state.request_id;
        let metadata = exec_state.metadata;
        let webhook_url = exec_state.webhook_url;
        let unhold_leg = exec_state.held_leg;
        let initiator_leg = exec_state.initiator_leg;

        // Read result produced by the app.
        let result = {
            let guard = ctx.extensions.read();
            guard.get::<IvrExecResult>().cloned().map(|r| IvrExecResultPayload {
                event: "ivr_exec_completed".to_string(),
                request_id: request_id.clone(),
                call_id: session_id.clone(),
                app: app_name.clone(),
                status: r.status,
                reason: r.reason,
                routing_target: r.routing_target,
                duration_ms: r.duration_ms,
                collected: r.collected,
                trace: r.trace,
                metadata: metadata.clone(),
                completion_time: r.completion_time,
            })
        }
        .unwrap_or_else(|| IvrExecResultPayload {
            event: "ivr_exec_completed".to_string(),
            request_id: request_id.clone(),
            call_id: session_id.clone(),
            app: app_name.clone(),
            status: "completed".to_string(),
            reason: "app_exit".to_string(),
            routing_target: None,
            duration_ms: 0,
            collected: HashMap::new(),
            trace: vec![],
            metadata: metadata.clone(),
            completion_time: chrono::Utc::now().to_rfc3339(),
        });

        // Fire-and-forget webhook POST if URL is configured.
        if let Some(url) = webhook_url {
            let payload = result.clone();
            let session_id_clone = session_id.clone();
            tokio::spawn(async move {
                let http = reqwest::Client::new();
                match http
                    .post(&url)
                    .timeout(std::time::Duration::from_secs(10))
                    .json(&payload)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        info!(
                            call_id = %session_id_clone,
                            "IVR exec webhook pushed successfully"
                        );
                    }
                    Ok(resp) => {
                        warn!(
                            call_id = %session_id_clone,
                            status = %resp.status(),
                            "IVR exec webhook returned non-success status"
                        );
                    }
                    Err(e) => {
                        warn!(
                            call_id = %session_id_clone,
                            error = %e,
                            "IVR exec webhook push failed"
                        );
                    }
                }
            });
        }

        // 消费后清理 extensions，防止同一 session 后续 app 退出时重复触发。
        {
            let mut guard = ctx.extensions.write();
            guard.remove::<IvrExecState>();
            guard.remove::<IvrExecResult>();
        }

        let body = serde_json::to_vec(&result).unwrap_or_default();

        Some(IvrExecCompletion {
            unhold_leg,
            result_info: Some(SendInfoSpec {
                leg_id: initiator_leg,
                content_type: RESULT_CT.to_string(),
                body,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::domain::LegId;
    use crate::proxy::proxy_call::session_hooks::{CallSessionContext, SessionExtensions};

    fn make_ctx_with_exec_state() -> (CallSessionContext, SessionExtensions) {
        let ext = SessionExtensions::new();
        {
            let mut guard = ext.write();
            guard.insert(IvrExecState {
                request_id: "test-req".into(),
                held_leg: Some(LegId::from("callee")),
                initiator_leg: LegId::from("callee"),
                webhook_url: None,
                app_name: "ivr".into(),
                metadata: serde_json::Value::Null,
            });
            guard.insert(IvrExecResult {
                status: "transferred".into(),
                reason: "agent_transfer".into(),
                routing_target: Some("sip:agent@test".into()),
                collected: [("menu_choice".into(), "1".into())].into(),
                trace: vec![],
                duration_ms: 1234,
                completion_time: "2025-07-24T10:00:00Z".into(),
            });
        }
        let ctx = CallSessionContext {
            session_id: "test-session".into(),
            caller: "caller".into(),
            callee: "callee".into(),
            connected_callee: None,
            queue_name: None,
            direction: "inbound".into(),
            started_at: None,
            extensions: ext.clone(),
        };
        (ctx, ext)
    }

    #[tokio::test]
    async fn hook_no_op_without_state() {
        let hook = IvrExecHook;
        let ext = SessionExtensions::new();
        let ctx = CallSessionContext {
            session_id: "test-session".into(),
            caller: "caller".into(),
            callee: "callee".into(),
            connected_callee: None,
            queue_name: None,
            direction: "inbound".into(),
            started_at: None,
            extensions: ext,
        };
        let result = hook.on_app_exited(&ctx).await;
        assert!(result.is_none(), "hook should no-op without IvrExecState");
    }

    #[tokio::test]
    async fn hook_returns_completion_with_state() {
        let hook = IvrExecHook;
        let (ctx, _ext) = make_ctx_with_exec_state();

        let result = hook.on_app_exited(&ctx).await;
        assert!(result.is_some(), "hook should return completion");
        let completion = result.unwrap();

        // Should request unhold of callee.
        assert!(completion.unhold_leg.is_some());
        assert_eq!(completion.unhold_leg.unwrap().as_str(), "callee");

        // Should request result INFO with correct content-type.
        assert!(completion.result_info.is_some());
        let info = completion.result_info.unwrap();
        assert_eq!(info.content_type, "application/vnd.rustpbx.result+json");
        assert!(!info.body.is_empty(), "result body should not be empty");

        // Verify JSON body content.
        let payload: serde_json::Value =
            serde_json::from_slice(&info.body).expect("valid JSON");
        assert_eq!(payload["event"], "ivr_exec_completed");
        assert_eq!(payload["request_id"], "test-req");
        assert_eq!(payload["status"], "transferred");
        assert_eq!(payload["reason"], "agent_transfer");
        assert_eq!(payload["collected"]["menu_choice"], "1");
    }

    #[tokio::test]
    async fn hook_cleans_up_extensions() {
        let hook = IvrExecHook;
        let (ctx, ext) = make_ctx_with_exec_state();

        // Verify state exists before hook.
        assert!(ext.read().get::<IvrExecState>().is_some());
        assert!(ext.read().get::<IvrExecResult>().is_some());

        let _result = hook.on_app_exited(&ctx).await;

        // Verify state is removed after hook.
        assert!(ext.read().get::<IvrExecState>().is_none(),
            "IvrExecState should be removed after hook");
        assert!(ext.read().get::<IvrExecResult>().is_none(),
            "IvrExecResult should be removed after hook");
    }
}
