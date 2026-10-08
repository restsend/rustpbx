use crate::call::domain::LegId;
use crate::proxy::proxy_call::session_hooks::{
    CallSessionContext, CallSessionHook, IvrExecCompletion, SendInfoSpec, SessionExtensions,
};
use async_trait::async_trait;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

const HANDOFF_RESULT_WAIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Per-session state written before an IVR exec starts.
/// Only present when `ivr.exec` is the active flow.
#[derive(Clone)]
pub struct IvrExecState {
    /// Runtime generation bound after the application starts successfully.
    pub app_execution_id: Option<u64>,
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
    pub app_execution_id: Option<u64>,
    pub status: String,
    pub reason: String,
    pub routing_target: Option<String>,
    pub collected: HashMap<String, String>,
    pub trace: Vec<serde_json::Value>,
    pub duration_ms: u64,
    pub completion_time: String,
}

#[derive(Clone, Default)]
pub(crate) struct IvrExecHandoff {
    pub pending_app_execution_ids: HashMap<u64, u64>,
    pub completed_results: BTreeMap<u64, IvrExecResult>,
    pub next_segment_index: u64,
    pub result_ready: Arc<tokio::sync::Notify>,
}

/// Routing policy resources owned by one ivr.exec invocation.
#[derive(Clone)]
pub struct IvrExecRouteHints(
    std::sync::Arc<parking_lot::Mutex<Option<crate::config::DialplanHints>>>,
);

impl IvrExecRouteHints {
    pub fn new(hints: crate::config::DialplanHints) -> Self {
        Self(std::sync::Arc::new(parking_lot::Mutex::new(Some(hints))))
    }

    fn take(&self) -> Option<crate::config::DialplanHints> {
        self.0.lock().take()
    }
}

/// Suspend the current app segment without ending its logical `ivr.exec` flow.
/// The return app will bind its new execution id through the normal start path.
pub(crate) fn suspend_ivr_exec(extensions: &SessionExtensions) -> bool {
    let mut guard = extensions.write();
    let Some(app_execution_id) = guard
        .get::<IvrExecState>()
        .and_then(|state| state.app_execution_id)
    else {
        return false;
    };
    if let Some(state) = guard.get_mut::<IvrExecState>() {
        state.app_execution_id = None;
    }
    let completed_result = guard
        .remove::<IvrExecResult>()
        .filter(|result| result.app_execution_id == Some(app_execution_id));
    if guard.get::<IvrExecHandoff>().is_none() {
        guard.insert(IvrExecHandoff::default());
    }
    if let Some(handoff) = guard.get_mut::<IvrExecHandoff>() {
        let segment_index = handoff.next_segment_index;
        handoff.next_segment_index = handoff.next_segment_index.saturating_add(1);
        if let Some(result) = completed_result {
            handoff.completed_results.insert(segment_index, result);
        } else {
            handoff
                .pending_app_execution_ids
                .insert(app_execution_id, segment_index);
        }
    }
    true
}

/// Bind a resumed app generation to the logical `ivr.exec` invocation.
pub(crate) fn bind_ivr_exec(extensions: &SessionExtensions, app_execution_id: u64) -> bool {
    let mut guard = extensions.write();
    let Some(state) = guard
        .get_mut::<IvrExecState>()
        .filter(|state| state.app_execution_id.is_none())
    else {
        return false;
    };
    state.app_execution_id = Some(app_execution_id);
    true
}

/// Convert a suspended logical flow into a terminal failure without losing
/// values collected before the hand-off.
pub(crate) fn fail_suspended_ivr_exec(extensions: &SessionExtensions, reason: &str) -> bool {
    let mut guard = extensions.write();
    if !guard
        .get::<IvrExecState>()
        .is_some_and(|state| state.app_execution_id.is_none())
    {
        return false;
    }
    guard.insert(IvrExecResult {
        app_execution_id: None,
        status: "failed".to_string(),
        reason: reason.to_string(),
        routing_target: None,
        collected: HashMap::new(),
        trace: Vec::new(),
        duration_ms: 0,
        completion_time: chrono::Utc::now().to_rfc3339(),
    });
    true
}

pub(crate) async fn wait_for_pending_ivr_exec_results(extensions: &SessionExtensions) {
    let wait = async {
        loop {
            let result_ready = {
                let guard = extensions.read();
                let Some(handoff) = guard.get::<IvrExecHandoff>() else {
                    return;
                };
                if handoff.pending_app_execution_ids.is_empty() {
                    return;
                }
                handoff.result_ready.clone()
            };
            result_ready.notified().await;
        }
    };
    if tokio::time::timeout(HANDOFF_RESULT_WAIT_TIMEOUT, wait)
        .await
        .is_err()
    {
        warn!("Timed out waiting for suspended IVR result snapshots");
    }
}

pub(crate) fn combined_ivr_exec_result(
    extensions: &SessionExtensions,
    app_execution_id: Option<u64>,
) -> Option<IvrExecResult> {
    let guard = extensions.read();
    let mut result = guard
        .get::<IvrExecResult>()
        .cloned()
        .filter(|result| result.app_execution_id == app_execution_id);
    let completed_results = guard
        .get::<IvrExecHandoff>()
        .map(|handoff| &handoff.completed_results);

    if result.is_none() {
        result = completed_results
            .and_then(|results| {
                results
                    .values()
                    .rev()
                    .find(|result| result.app_execution_id == app_execution_id)
                    .cloned()
            });
    }
    let mut result = result?;
    if let Some(results) = completed_results {
        for previous in results.values().rev() {
            if previous.app_execution_id == result.app_execution_id {
                continue;
            }
            for (name, value) in &previous.collected {
                result
                    .collected
                    .entry(name.clone())
                    .or_insert_with(|| value.clone());
            }
            let mut trace = previous.trace.clone();
            trace.extend(result.trace);
            result.trace = trace;
            result.duration_ms = previous.duration_ms.saturating_add(result.duration_ms);
        }
    }
    Some(result)
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
    /// Always `true` — the payload is only produced on flow termination
    /// (including caller hangup mid-flow, whose `status`/`reason` carry
    /// `user_hangup`). Present so consumers can filter uniformly on `end`.
    end: bool,
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
        app_execution_id: Option<u64>,
    ) -> Option<IvrExecCompletion> {
        let session_id = ctx.session_id.clone();

        // Read IvrExecState — if absent this is not an ivr.exec flow.
        let exec_state = {
            let guard = ctx.extensions.read();
            guard.get::<IvrExecState>().cloned()?
        };
        if exec_state.app_execution_id != app_execution_id {
            return None;
        }
        let app_name = exec_state.app_name;
        let request_id = exec_state.request_id;
        let metadata = exec_state.metadata;
        let webhook_url = exec_state.webhook_url;
        let unhold_leg = exec_state.held_leg;
        let initiator_leg = exec_state.initiator_leg;

        wait_for_pending_ivr_exec_results(&ctx.extensions).await;

        // Read result produced by the app.
        let result = combined_ivr_exec_result(&ctx.extensions, app_execution_id)
                .map(|r| IvrExecResultPayload {
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
                    end: true,
                })
        .unwrap_or_else(|| IvrExecResultPayload {
            event: "ivr_exec_completed".to_string(),
            request_id: request_id.clone(),
            call_id: session_id.clone(),
            app: app_name.clone(),
            status: "failed".to_string(),
            reason: "app_exit_without_result".to_string(),
            routing_target: None,
            duration_ms: 0,
            collected: HashMap::new(),
            trace: vec![],
            metadata: metadata.clone(),
            completion_time: chrono::Utc::now().to_rfc3339(),
            end: true,
        });

        // Fire-and-forget webhook POST if URL is configured.
        if let Some(url) = webhook_url {
            // SSRF guard: the URL arrives via SIP INFO payload parameters, so
            // any in-call client can supply it. Refuse non-public targets
            // (loopback / private / link-local, including after DNS).
            if !crate::utils::is_url_ssrf_safe_async(&url).await {
                warn!(
                    call_id = %session_id,
                    "IVR exec webhook refused: URL does not point at a public host"
                );
            } else {
            let payload = result.clone();
            let session_id_clone = session_id.clone();
            tokio::spawn(async move {
                let http = crate::http_util::shared_keepalive_client();
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
        }

        // Clean up extensions after consumption to prevent re-triggering
        // when a subsequent app exits in the same session.
        let route_hints = {
            let mut guard = ctx.extensions.write();
            guard.remove::<IvrExecState>();
            guard.remove::<IvrExecResult>();
            guard.remove::<IvrExecHandoff>();
            guard
                .remove::<IvrExecRouteHints>()
                .and_then(|value| value.take())
        };

        let body = serde_json::to_vec(&result).unwrap_or_default();

        Some(IvrExecCompletion {
            unhold_leg,
            route_hints,
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
                app_execution_id: Some(1),
                request_id: "test-req".into(),
                held_leg: Some(LegId::from("callee")),
                initiator_leg: LegId::from("callee"),
                webhook_url: None,
                app_name: "ivr".into(),
                metadata: serde_json::Value::Null,
            });
            guard.insert(IvrExecResult {
                app_execution_id: Some(1),
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
            root_session_id: None,
            caller: "caller".into(),
            callee: "callee".into(),
            connected_callee: None,
            queue_name: None,
            skill_group_id: None,
            transferred: false,
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
            root_session_id: None,
            caller: "caller".into(),
            callee: "callee".into(),
            connected_callee: None,
            queue_name: None,
            skill_group_id: None,
            transferred: false,
            direction: "inbound".into(),
            started_at: None,
            extensions: ext,
        };
        let result = hook.on_app_exited(&ctx, None).await;
        assert!(result.is_none(), "hook should no-op without IvrExecState");
    }

    #[tokio::test]
    async fn hook_returns_completion_with_state() {
        let hook = IvrExecHook;
        let (ctx, _ext) = make_ctx_with_exec_state();

        let result = hook.on_app_exited(&ctx, Some(1)).await;
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
        let payload: serde_json::Value = serde_json::from_slice(&info.body).expect("valid JSON");
        assert_eq!(payload["event"], "ivr_exec_completed");
        assert_eq!(payload["request_id"], "test-req");
        assert_eq!(payload["status"], "transferred");
        assert_eq!(payload["reason"], "agent_transfer");
        assert_eq!(payload["collected"]["menu_choice"], "1");
    }

    #[tokio::test]
    async fn hook_reports_failed_when_the_app_exits_without_a_result() {
        let hook = IvrExecHook;
        let (ctx, ext) = make_ctx_with_exec_state();
        ext.write().remove::<IvrExecResult>();

        let completion = hook
            .on_app_exited(&ctx, Some(1))
            .await
            .expect("matching app exit must produce a terminal completion");
        let info = completion
            .result_info
            .expect("terminal completion must include result INFO");
        let payload: serde_json::Value =
            serde_json::from_slice(&info.body).expect("valid result payload");

        assert_eq!(payload["status"], "failed");
        assert_eq!(payload["reason"], "app_exit_without_result");
        assert!(ext.read().get::<IvrExecState>().is_none());
    }

    #[tokio::test]
    async fn pending_ivr_exec_result_is_included_before_terminal_completion() {
        let hook = IvrExecHook;
        let (ctx, ext) = make_ctx_with_exec_state();
        ext.write().remove::<IvrExecResult>();
        assert!(suspend_ivr_exec(&ext));
        assert!(bind_ivr_exec(&ext, 2));
        crate::call::app::ivr::exec::write_ivr_exec_result(
            &ext,
            2,
            crate::call::app::ivr::exec::build_ivr_exec_result(
                "completed",
                "normal",
                None,
                [("successor".to_string(), "done".to_string())]
                    .into_iter()
                    .collect(),
                20,
            ),
        );

        let completion_task = tokio::spawn(async move {
            hook.on_app_exited(&ctx, Some(2)).await
        });
        tokio::pin!(completion_task);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                completion_task.as_mut()
            )
            .await
            .is_err(),
            "terminal completion must wait for the pending segment snapshot"
        );

        crate::call::app::ivr::exec::write_ivr_exec_result(
            &ext,
            1,
            crate::call::app::ivr::exec::build_ivr_exec_result(
                "transferred",
                "transfer_to_ivr",
                None,
                [("previous".to_string(), "saved".to_string())]
                    .into_iter()
                    .collect(),
                10,
            ),
        );
        let completion = completion_task
            .await
            .expect("completion task must finish")
            .expect("matching app exit must complete");
        let payload: serde_json::Value = serde_json::from_slice(
            &completion.result_info.expect("result INFO").body,
        )
        .expect("valid result payload");
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["collected"]["previous"], "saved");
        assert_eq!(payload["collected"]["successor"], "done");
        assert_eq!(payload["duration_ms"], 30);
    }

    #[tokio::test]
    async fn hook_cleans_up_extensions() {
        let hook = IvrExecHook;
        let (ctx, ext) = make_ctx_with_exec_state();

        // Verify state exists before hook.
        assert!(ext.read().get::<IvrExecState>().is_some());
        assert!(ext.read().get::<IvrExecResult>().is_some());

        let _result = hook.on_app_exited(&ctx, Some(1)).await;

        // Verify state is removed after hook.
        assert!(
            ext.read().get::<IvrExecState>().is_none(),
            "IvrExecState should be removed after hook"
        );
        assert!(
            ext.read().get::<IvrExecResult>().is_none(),
            "IvrExecResult should be removed after hook"
        );
    }

    #[tokio::test]
    async fn stale_app_exit_cannot_consume_current_invocation() {
        let hook = IvrExecHook;
        let (ctx, ext) = make_ctx_with_exec_state();

        let result = hook.on_app_exited(&ctx, Some(0)).await;

        assert!(result.is_none());
        assert!(ext.read().get::<IvrExecState>().is_some());
        assert!(ext.read().get::<IvrExecResult>().is_some());
}

    #[tokio::test]
    async fn route_capacity_remains_owned_until_invocation_cleanup() {
        let hook = IvrExecHook;
        let (ctx, ext) = make_ctx_with_exec_state();
        let limiter = std::sync::Arc::new(
            crate::call::concurrent_call_limiter::ConcurrentCallLimiter::new(1),
        );
        let permit = limiter
            .try_acquire()
            .expect("route capacity should be available");
        let lease = crate::call::concurrent_call_limiter::ConcurrentCallLease::default();
        lease.push(permit);
        ext.write()
            .insert(IvrExecRouteHints::new(crate::config::DialplanHints {
                concurrent_call_lease: lease,
                ..Default::default()
            }));
        assert_eq!(limiter.current(), 1);

        let completion = hook
            .on_app_exited(&ctx, Some(1))
            .await
            .expect("matching app exit should own invocation cleanup");

        assert_eq!(
            limiter.current(),
            1,
            "cleanup owns the route lease until applied"
        );
        drop(completion);
        assert_eq!(
            limiter.current(),
            0,
            "route capacity must be released after cleanup"
        );
    }
}
