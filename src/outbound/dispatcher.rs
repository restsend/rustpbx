//! Post-answer dispatcher — executes the configured `on_answer` action after
//! the callee picks up.
//!
//! All actions delegate to `RwiCommandProcessor` (RWI commands). The resulting
//! RWI events (`call_bridged`, `queue_joined`, etc.) flow back through the
//! gateway event tap and are delivered to the SSE consumer as-is — the
//! dispatcher does NOT construct any SSE events.

use crate::outbound::request::{FallbackAction, OnAnswer, WebhookAction};
use crate::outbound::webhook::{WebhookActionType, WebhookPayload, call_sync_webhook};
use crate::rwi::processor::RwiCommandProcessor;
use crate::rwi::session::RwiCommandPayload;
use tracing::warn;

/// Outcome of dispatching the post-answer action.
/// `detail` is for logging only — not sent as an SSE event.
pub struct DispatchOutcome {
    pub success: bool,
    pub detail: String,
}

/// Execute the `on_answer` action.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch(
    processor: &RwiCommandProcessor,
    http_client: &reqwest::Client,
    call_id: &str,
    callee: &str,
    caller: &str,
    metadata: &std::collections::HashMap<String, String>,
    on_answer: &OnAnswer,
    webhook_timeout: std::time::Duration,
) -> DispatchOutcome {
    // Inner non-recursive dispatcher. The webhook path may recurse once.
    Box::pin(dispatch_inner(
        processor,
        http_client,
        call_id,
        callee,
        caller,
        metadata,
        on_answer,
        webhook_timeout,
        MAX_WEBHOOK_DEPTH,
    ))
    .await
}

/// Maximum recursion depth for webhook-returned actions (webhook → action →
/// webhook → ...). Bounds the fallback chain so a misbehaving webhook cannot
/// cause unbounded recursion.
pub const MAX_WEBHOOK_DEPTH: u8 = 3;

/// Notify the configured `on_failure` webhook that the call failed before
/// answer. Fire-and-forget: the outcome is logged, never propagated (the SSE
/// stream is already closing). Uses a failure payload (`answered_at` omitted,
/// `failure_reason` set).
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_failure(
    http_client: &reqwest::Client,
    call_id: &str,
    callee: &str,
    caller: &str,
    metadata: &std::collections::HashMap<String, String>,
    on_failure: &crate::outbound::request::OnFailure,
    reason: &str,
    webhook_timeout: std::time::Duration,
) {
    let timeout = on_failure
        .webhook
        .timeout_secs
        .map(std::time::Duration::from_secs)
        .unwrap_or(webhook_timeout);
    let payload = WebhookPayload {
        call_id,
        leg_id: None,
        caller,
        callee,
        answered_at: None,
        failure_reason: Some(reason.to_string()),
        metadata,
    };
    match call_sync_webhook(
        http_client,
        &on_failure.webhook.url,
        &on_failure.webhook.headers,
        timeout,
        &payload,
    )
    .await
    {
        crate::outbound::webhook::WebhookOutcome::Ok(_) => {
            warn!(%call_id, reason, "failure webhook acknowledged");
        }
        crate::outbound::webhook::WebhookOutcome::Err(msg) => {
            warn!(%call_id, %msg, reason, "failure webhook failed");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_inner(
    processor: &RwiCommandProcessor,
    http_client: &reqwest::Client,
    call_id: &str,
    callee: &str,
    caller: &str,
    metadata: &std::collections::HashMap<String, String>,
    on_answer: &OnAnswer,
    webhook_timeout: std::time::Duration,
    depth: u8,
) -> DispatchOutcome {
    match on_answer {
        OnAnswer::ExecuteFlow => DispatchOutcome {
            success: true,
            detail: "execute_flow".to_string(),
        },

        OnAnswer::App {
            app_name,
            app_params,
        } => dispatch_app(processor, call_id, app_name, app_params).await,

        OnAnswer::BridgeToLeg { leg_id } => dispatch_bridge(processor, call_id, leg_id).await,

        OnAnswer::Enqueue {
            queue, priority, ..
        } => dispatch_enqueue(processor, call_id, queue, *priority).await,

        OnAnswer::Webhook(action) => {
            dispatch_webhook(
                processor,
                http_client,
                call_id,
                callee,
                caller,
                metadata,
                action,
                webhook_timeout,
                depth,
            )
            .await
        }
    }
}

async fn dispatch_app(
    processor: &RwiCommandProcessor,
    call_id: &str,
    app_name: &str,
    app_params: &std::collections::HashMap<String, String>,
) -> DispatchOutcome {
    let params = if app_params.is_empty() {
        None
    } else {
        Some(serde_json::to_value(app_params).unwrap_or_default())
    };
    dispatch_app_value(processor, call_id, app_name, params).await
}

async fn dispatch_app_value(
    processor: &RwiCommandProcessor,
    call_id: &str,
    app_name: &str,
    params: Option<serde_json::Value>,
) -> DispatchOutcome {
    let cmd = RwiCommandPayload::AppStart {
        call_id: call_id.to_string(),
        app_name: app_name.to_string(),
        params,
    };
    match processor.process_command(cmd).await {
        Ok(_) => DispatchOutcome {
            success: true,
            detail: format!("app:{}", app_name),
        },
        Err(e) => {
            warn!(%call_id, %app_name, error = %e, "app start failed");
            DispatchOutcome {
                success: false,
                detail: format!("app start failed: {}", e),
            }
        }
    }
}

async fn dispatch_bridge(
    processor: &RwiCommandProcessor,
    call_id: &str,
    target_leg: &str,
) -> DispatchOutcome {
    let cmd = RwiCommandPayload::Bridge {
        call_id: String::new(),
        leg_a: call_id.to_string(),
        leg_b: target_leg.to_string(),
    };
    match processor.process_command(cmd).await {
        Ok(_) => DispatchOutcome {
            success: true,
            detail: format!("bridged to {}", target_leg),
        },
        Err(e) => {
            warn!(%call_id, %target_leg, error = %e, "bridge_to_leg failed");
            DispatchOutcome {
                success: false,
                detail: format!("bridge failed: {}", e),
            }
        }
    }
}

/// Queue-app params for `OnAnswer::Enqueue`. JSON-native (NOT a stringified
/// map): the priority must stay a JSON number or `QueuePlan::from_app_params`
/// would drop it.
fn enqueue_app_params(queue: &str, priority: Option<u32>) -> Option<serde_json::Value> {
    let mut params = serde_json::Map::new();
    params.insert(
        "queue".to_string(),
        serde_json::Value::String(queue.to_string()),
    );
    if let Some(priority) = priority {
        params.insert("priority".to_string(), serde_json::Value::from(priority));
    }
    (!params.is_empty()).then(|| serde_json::Value::Object(params))
}

async fn dispatch_enqueue(
    processor: &RwiCommandProcessor,
    call_id: &str,
    queue: &str,
    priority: Option<u32>,
) -> DispatchOutcome {
    // Enqueue is implemented as `AppStart("queue", {queue, priority})`: the
    // session-side queue start resolves skill-groups / registered agents and
    // launches the queue app with real agent ringing (the bare `QueueEnqueue`
    // command is bookkeeping-only — it never starts the app).
    let params = enqueue_app_params(queue, priority);
    dispatch_app_value(processor, call_id, "queue", params).await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_webhook(
    processor: &RwiCommandProcessor,
    http_client: &reqwest::Client,
    call_id: &str,
    callee: &str,
    caller: &str,
    metadata: &std::collections::HashMap<String, String>,
    action: &WebhookAction,
    default_timeout: std::time::Duration,
    depth: u8,
) -> DispatchOutcome {
    // Depth guard: a webhook that keeps returning another webhook action is
    // capped (its response action is ignored, treated as execute-flow).
    if depth == 0 {
        warn!(%call_id, "webhook recursion depth exceeded; treating as execute_flow");
        return DispatchOutcome {
            success: true,
            detail: "execute_flow (webhook depth exceeded)".to_string(),
        };
    }

    let timeout = action
        .timeout_secs
        .map(std::time::Duration::from_secs)
        .unwrap_or(default_timeout);

    let payload = WebhookPayload {
        call_id,
        leg_id: None,
        caller,
        callee,
        answered_at: Some(chrono::Utc::now()),
        failure_reason: None,
        metadata,
    };

    match call_sync_webhook(http_client, &action.url, &action.headers, timeout, &payload).await {
        crate::outbound::webhook::WebhookOutcome::Ok(instr) => {
            let resolved = resolve_webhook_instruction(&instr);
            // Recurse via `dispatch_inner` (boxed — async recursion) with a
            // decremented depth so the webhook→action→webhook chain is bounded.
            Box::pin(dispatch_inner(
                processor,
                http_client,
                call_id,
                callee,
                caller,
                metadata,
                &resolved,
                default_timeout,
                depth.saturating_sub(1),
            ))
            .await
        }
        crate::outbound::webhook::WebhookOutcome::Err(msg) => {
            warn!(%call_id, %msg, "sync webhook failed, using fallback");
            match resolve_fallback(&action.fallback) {
                Some(action) => {
                    Box::pin(dispatch_inner(
                        processor,
                        http_client,
                        call_id,
                        callee,
                        caller,
                        metadata,
                        &action,
                        default_timeout,
                        depth.saturating_sub(1),
                    ))
                    .await
                }
                None => DispatchOutcome {
                    success: false,
                    detail: format!("webhook fallback hangup: {}", msg),
                },
            }
        }
    }
}

/// Convert the webhook instruction into an `OnAnswer`.
fn resolve_webhook_instruction(instr: &crate::outbound::webhook::WebhookInstruction) -> OnAnswer {
    match instr.action {
        WebhookActionType::Bridge => OnAnswer::BridgeToLeg {
            leg_id: instr.target.clone().unwrap_or_default(),
        },
        WebhookActionType::Enqueue => OnAnswer::Enqueue {
            queue: instr.target.clone().unwrap_or_default(),
            priority: None,
        },
        WebhookActionType::App => OnAnswer::App {
            app_name: instr.target.clone().unwrap_or_default(),
            app_params: std::collections::HashMap::new(),
        },
        WebhookActionType::Hangup => OnAnswer::ExecuteFlow,
    }
}

/// Resolve a `FallbackAction` to an `OnAnswer` (or `None` for `Hangup`).
fn resolve_fallback(fallback: &FallbackAction) -> Option<OnAnswer> {
    match fallback {
        FallbackAction::Hangup => None,
        FallbackAction::Bridge { leg_id } => Some(OnAnswer::BridgeToLeg {
            leg_id: leg_id.clone(),
        }),
        FallbackAction::Enqueue { queue } => Some(OnAnswer::Enqueue {
            queue: queue.clone(),
            priority: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_params_keep_priority_numeric() {
        let params = enqueue_app_params("sales", Some(3)).expect("params present");
        assert_eq!(
            params,
            serde_json::json!({"queue": "sales", "priority": 3}),
            "priority must be a JSON number — stringifying drops it in QueuePlan::from_app_params"
        );
        // Round-trip through the session-side parser (string map path).
        let stringified = serde_json::json!({"queue": "sales", "priority": "3"});
        assert_eq!(
            stringified.get("priority").and_then(|v| v.as_u64()),
            None,
            "documents the regression: as_u64() on a JSON string is None"
        );
    }

    #[test]
    fn enqueue_params_without_priority_only_carry_queue() {
        let params = enqueue_app_params("sales", None).expect("params present");
        assert_eq!(params, serde_json::json!({"queue": "sales"}));
    }
}
