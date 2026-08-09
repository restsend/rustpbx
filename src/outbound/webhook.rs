//! Sync webhook client for post-answer instructions.

use crate::http_util::HttpFetchOptions;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Payload POSTed to the sync webhook after the callee answers.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookPayload<'a> {
    pub call_id: &'a str,
    pub leg_id: Option<&'a str>,
    pub caller: &'a str,
    pub callee: &'a str,
    /// When the call was answered (post-answer payloads). `None` for failure
    /// notifications, which carry `failure_reason` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Machine-readable failure reason (e.g. `busy`, `no_answer`, `timeout`)
    /// on failure notifications. Absent on successful post-answer payloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    pub metadata: &'a HashMap<String, String>,
}

/// Response expected from the sync webhook.
#[derive(Debug, Clone, Deserialize)]
pub struct WebhookInstruction {
    /// What to do next with the answered call.
    pub action: WebhookActionType,
    /// Target for bridge (leg_id) / enqueue (queue) / app (app_name).
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebhookActionType {
    Bridge,
    Enqueue,
    App,
    Hangup,
}

/// Outcome of a sync webhook call.
pub enum WebhookOutcome {
    /// Webhook returned a valid instruction.
    Ok(WebhookInstruction),
    /// Webhook timed out or returned an error.
    Err(String),
}

/// POST the answer payload and wait for an instruction.
pub async fn call_sync_webhook(
    client: &reqwest::Client,
    url: &str,
    headers: &HashMap<String, String>,
    timeout: Duration,
    payload: &WebhookPayload<'_>,
) -> WebhookOutcome {
    let opts = HttpFetchOptions::new()
        .with_timeout(timeout)
        .with_headers(headers.clone());

    let req = client.post(url).json(payload);
    match crate::http_util::execute_request(req, &opts.headers, opts.timeout).await {
        Ok(resp) => match resp.json::<WebhookInstruction>().await {
            Ok(instr) => WebhookOutcome::Ok(instr),
            Err(e) => WebhookOutcome::Err(format!("invalid webhook response: {e}")),
        },
        Err(e) => WebhookOutcome::Err(format!("webhook request failed: {e}")),
    }
}


