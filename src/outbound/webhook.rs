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
    ///
    /// Serialized as RFC 3339 in the server's local timezone (not UTC `Z`) so
    /// every outgoing webhook timestamp shares one shape — see
    /// [`crate::utils::rfc3339_local`].
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_answered_at"
    )]
    pub answered_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Machine-readable failure reason (e.g. `busy`, `no_answer`, `timeout`)
    /// on failure notifications. Absent on successful post-answer payloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    pub metadata: &'a HashMap<String, String>,
}

/// `serialize_with` helper for [`WebhookPayload::answered_at`]: renders the
/// answer time as RFC 3339 in the server's local timezone.
fn serialize_answered_at<S>(
    dt: &Option<chrono::DateTime<chrono::Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match dt {
        Some(dt) => serializer.serialize_str(&crate::utils::rfc3339_local(*dt)),
        None => serializer.serialize_none(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(answered_at: Option<chrono::DateTime<chrono::Utc>>) -> serde_json::Value {
        let metadata = HashMap::new();
        serde_json::to_value(WebhookPayload {
            call_id: "call-1",
            leg_id: None,
            caller: "1001",
            callee: "2000",
            answered_at,
            failure_reason: None,
            metadata: &metadata,
        })
        .expect("serialize webhook payload")
    }

    /// `answered_at` is an RFC 3339 string in the server's local timezone
    /// (e.g. `+08:00`), matching every other outgoing webhook timestamp.
    #[test]
    fn answered_at_serializes_as_local_rfc3339() {
        let value = payload(Some(chrono::Utc::now()));
        let ts = value["answered_at"]
            .as_str()
            .expect("answered_at must serialize as a string");
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap_or_else(|e| panic!("answered_at {ts:?} is not RFC3339: {e}"));
    }

    /// Failure notifications omit `answered_at` entirely.
    #[test]
    fn answered_at_omitted_when_none() {
        let value = payload(None);
        assert!(value.get("answered_at").is_none());
    }
}
