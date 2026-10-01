use crate::call::app::ivr::config::{ActionNode, EntryAction, IvrProviderConfig};
use crate::call::domain::TransferOutcome;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, error};

// ── Provider Trait ───────────────────────────────────────────────────────────

#[async_trait]
pub trait ActionProvider: Send + Sync {
    /// Provider identifier for debug tracing
    fn name(&self) -> &str {
        "unknown"
    }

    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode>;

    /// Request a recovery action after a node failed to execute.
    ///
    /// HTTP providers `POST {url}/fail`. Default: return `Err` so the caller
    /// can escalate to session-level IVR fallback.
    async fn fail_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        let _ = ctx;
        Err(anyhow::anyhow!("provider does not support /fail"))
    }

    async fn on_session_start(&self, ctx: &SessionContext) -> anyhow::Result<()> {
        let _ = ctx;
        Ok(())
    }

    async fn on_session_end(
        &self,
        reason: &SessionEndReason,
        _session_id: &str,
    ) -> anyhow::Result<()> {
        let _ = reason;
        Ok(())
    }

    async fn on_session_end_context(
        &self,
        reason: &SessionEndReason,
        context: &SessionContext,
    ) -> anyhow::Result<()> {
        self.on_session_end(reason, &context.session_id).await
    }

    /// Called when a DtmfMenu resolves a DTMF key locally (no round‑trip to
    /// the provider).  Fire‑and‑forget notification so the provider stays
    /// informed about which keys were pressed and what action was taken.
    async fn on_local_dtmf_match(&self, _digit: &str, _action: &ActionNode) {}
}

// ── Context ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContext {
    pub session_id: String,
    pub app_execution_id: u64,
    pub caller: String,
    pub callee: String,
    pub direction: String,
    pub tenant_id: Option<String>,
    pub ivr_id: Option<String>,
    pub variables: HashMap<String, String>,
    /// All SIP headers from the original INVITE request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sip_headers: Option<HashMap<String, String>>,
    /// Name of the matched route that sent this call into the IVR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_name: Option<String>,
    /// Arbitrary passthrough data set by the caller / external system.
    /// The provider receives this and can use it for correlation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_data: Option<serde_json::Value>,
    /// Whether this session was re-entered from a transfer (JumpIvr /
    /// agent / queue transfer-back). Values: `"ivr"`, `"agent"`, `"queue"`.
    /// When `"ivr"`, the `source_ivr` / `source_node` variables carry the
    /// originating IVR short code and node id; when `"queue"` (queue return),
    /// `source_queue` carries the queue name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transferred_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderContext {
    pub session_id: String,
    pub app_execution_id: u64,
    pub caller: String,
    pub callee: String,
    pub direction: String,
    pub tenant_id: Option<String>,
    pub ivr_id: Option<String>,
    pub variables: HashMap<String, String>,
    /// All SIP headers from the original INVITE request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sip_headers: Option<HashMap<String, String>>,
    pub event: Option<ProviderEvent>,
    /// Name of the matched route that sent this call into the IVR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_name: Option<String>,
    /// Passthrough data — the provider can set `custom_data` in its response
    /// and it will be echoed back in every subsequent ProviderContext.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_data: Option<serde_json::Value>,
    /// Step timing: ISO-8601 timestamp when this step started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_start_time: Option<String>,
    /// Step timing: ISO-8601 timestamp when this step ended (set before sending).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_end_time: Option<String>,
    /// Step timing: wall-clock duration of the previous step in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_duration_ms: Option<u64>,
    /// Monotonic step index (0 for SessionStart, incremented thereafter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_index: Option<u32>,
    /// Whether this session was re-entered from a transfer (JumpIvr /
    /// agent / queue transfer-back). Values: `"ivr"`, `"agent"`, `"queue"`,
    /// or `None` for a fresh entry. When `"ivr"`, the variables map also
    /// carries `source_ivr` (originating IVR short code) and `source_node`
    /// (originating node id); when `"queue"` (queue return), it carries
    /// `source_queue` (queue name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transferred_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    SessionStart,
    /// Continuation of a suspended flow (voip_bridge / queue return with no
    /// buffered digits). This is NOT a fresh session: the flow already emitted
    /// `session_start` at its true first entry, so the provider must resume
    /// from its stored per-session position instead of re-entering the flow
    /// (re-entering is the "caller hears the menu again" regression).
    ///
    /// `resume_from_step_id` is the step the flow was suspended on when a
    /// voip_bridge owned the media (`bridge_step_ctx`), so a stateless
    /// provider can still route to the successor. It is `None` for returns
    /// that carry no bridge context (queue return, JumpIvr) — consumers must
    /// not rely on its presence.
    Resume {
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_from_step_id: Option<String>,
    },
    AudioComplete {
        interrupted: bool,
    },
    Dtmf {
        digit: String,
    },
    DtmfTimeout,
    ApiResponse {
        status: u16,
        body: serde_json::Value,
    },
    PhoneCollected {
        number: String,
    },
    RecordingComplete {
        url: String,
        duration_secs: u64,
    },
    /// Mid-call `record_start` completed (recording is active).
    RecordingStarted {
        segment_type: String,
        segment_id: String,
    },
    /// Mid-call `record_stop` completed (segment finalized asynchronously).
    RecordingStopped {
        #[serde(default)]
        reason: Option<String>,
    },
    InputVoice {
        text: String,
        confidence: f32,
    },
    Error {
        reason: String,
    },
    /// Node execution failed; posted to `POST {url}/fail` (not `/step`).
    Fail {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        failed_step_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        failed_step_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        failed_action: Option<String>,
    },
    DtmfMenuInvalid {
        digit: String,
    },
    DtmfMenuTimeout,
    TransferResult {
        outcome: TransferOutcome,
    },
}

/// Why an IVR session ended.
///
/// Sent to the external provider via `POST {url}/end` as structured JSON so
/// the provider knows exactly how the call left the IVR.
///
/// # JSON wire format
///
/// Each variant serializes as `{"reason": "<tag>", "detail": "..."}`:
///
/// | Variant | `reason` | `detail` |
/// |---------|----------|----------|
/// | `Normal` | `"normal"` | `null` |
/// | `Transfer("2001")` | `"transfer"` | `"2001"` |
/// | `TransferToQueue("support")` | `"transfer_to_queue"` | `"support"` |
/// | `TransferToIvr("main")` | `"transfer_to_ivr"` | `"main"` |
/// | `Hangup` | `"hangup"` | `null` |
/// | `UserHangup` | `"user_hangup"` | `null` |
/// | `Timeout` | `"timeout"` | `null` |
/// | `Error("...")` | `"error"` | `"..."` |
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionEndReason {
    /// Machine-readable tag identifying the end reason.
    pub reason: SessionEndTag,
    /// Human-readable detail (target number, error message, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Machine-readable tag for [`SessionEndReason`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndTag {
    /// IVR completed normally (all steps finished, no transfer).
    Normal,
    /// Call transferred to an agent or extension.
    Transfer,
    /// Call sent to an ACD queue.
    TransferToQueue,
    /// Call jumped to another IVR.
    TransferToIvr,
    /// System (PBX) initiated hangup.
    Hangup,
    /// User / remote party hung up.
    UserHangup,
    /// IVR timed out (DTMF timeout / max retries exceeded with no fallback).
    Timeout,
    /// Error during IVR execution.
    Error,
}

// ── RetryConfig ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub timeout_ms: u64,
    pub retry_delay_ms: u64,
    pub fallback_action: Option<ActionNode>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            timeout_ms: 1000,
            retry_delay_ms: 100,
            fallback_action: Some(ActionNode {
                action: EntryAction::Hangup {
                    prompt: Some("sounds/error.wav".into()),
                    prompt_text: None,
                    prompt_voice: None,
                    delay_before_ms: 0,
                    delay_after_ms: 0,
                },
                ignore_prompt_dtmf: false,
                wait_for_result: false,
                next: None,
                step_id: None,
                step_name: None,
                extra: None,
                result_variables: HashMap::new(),
            }),
        }
    }
}

impl From<&IvrProviderConfig> for RetryConfig {
    fn from(config: &IvrProviderConfig) -> Self {
        Self {
            max_retries: config.max_retries,
            timeout_ms: config.timeout_secs.saturating_mul(1000),
            retry_delay_ms: config.retry_delay_ms,
            fallback_action: config.fallback_action.clone(),
        }
    }
}

// ── StepProvider (HTTP) ──────────────────────────────────────────────────────

/// Wire format used for the executor's `Resume` first event (voip_bridge /
/// queue return with no buffered digits).
///
/// - `Resume` (default): POST `{"type":"resume","resume_from_step_id":…}`.
///   If the remote rejects it (legacy `/step` endpoint), the endpoint is
///   automatically downgraded to `SessionStart` for the process lifetime and
///   the request is re-sent — see `next_action`.
/// - `SessionStart`: never emit `resume` on the wire; resumed instances POST
///   the legacy `session_start` event (the trace trigger still says
///   `resume`). Escape hatch for providers that hard-fail on unknown event
///   types; the `ivr_status=resuming` variable + `ivr_resume_from_step_id`
///   variable still disambiguate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeEventMode {
    Resume,
    SessionStart,
}

impl ResumeEventMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "resume" => Some(Self::Resume),
            "session_start" | "sessionstart" => Some(Self::SessionStart),
            _ => None,
        }
    }
}

/// Endpoints that rejected the `resume` event (auto-downgrade fuse). Keyed
/// by `/step` URL for the process lifetime: one legacy endpoint must not
/// cost every subsequent resumed call a failed round-trip, while distinct
/// endpoints (multi-tenant) keep their own verdict.
static RESUME_DOWNGRADED_ENDPOINTS: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

fn resume_downgraded_for(url: &str) -> bool {
    match RESUME_DOWNGRADED_ENDPOINTS.lock() {
        Ok(guard) => guard.as_ref().is_some_and(|set| set.contains(url)),
        Err(_) => false,
    }
}

fn mark_resume_downgraded(url: &str) {
    if let Ok(mut guard) = RESUME_DOWNGRADED_ENDPOINTS.lock() {
        guard.get_or_insert_with(Default::default).insert(url.to_string());
    }
}

#[cfg(test)]
pub(crate) fn reset_resume_downgrades_for_test() {
    if let Ok(mut guard) = RESUME_DOWNGRADED_ENDPOINTS.lock() {
        *guard = None;
    }
}

pub struct StepProvider {
    url: String,
    headers: HashMap<String, String>,
    http_client: reqwest::Client,
    retry: RetryConfig,
    /// When true, exhausted `/step` retries return `Err` instead of
    /// `retry.fallback_action` so the executor can jump to session IVR fallback.
    prefer_ivr_fallback: bool,
    /// Wire format for the executor's `Resume` event (see [`ResumeEventMode`]).
    resume_event_mode: ResumeEventMode,
}

impl StepProvider {
    pub fn new(url: impl Into<String>, http_client: reqwest::Client) -> Self {
        Self {
            url: url.into(),
            headers: HashMap::new(),
            http_client,
            retry: RetryConfig::default(),
            prefer_ivr_fallback: false,
            resume_event_mode: ResumeEventMode::Resume,
        }
    }

    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_prefer_ivr_fallback(mut self, prefer: bool) -> Self {
        self.prefer_ivr_fallback = prefer;
        self
    }

    pub fn with_resume_event_mode(mut self, mode: ResumeEventMode) -> Self {
        self.resume_event_mode = mode;
        self
    }

    /// Add a single HTTP header.
    pub fn add_header(&mut self, key: &str, value: &str) {
        self.headers.insert(key.to_string(), value.to_string());
    }

    fn endpoint_url(&self, suffix: Option<&str>) -> String {
        let base = self.url.trim().trim_end_matches('/');
        match suffix {
            Some(suffix) if !suffix.is_empty() => {
                if let Ok(mut url) = reqwest::Url::parse(base) {
                    let path = match url.path().trim_end_matches('/') {
                        "" => format!("/{suffix}"),
                        path => format!("{path}/{suffix}"),
                    };
                    url.set_path(&path);
                    url.to_string()
                } else {
                    format!("{base}/{suffix}")
                }
            }
            _ => base.to_string(),
        }
    }

    async fn post_action_node(
        &self,
        url: &str,
        ctx: &ProviderContext,
        label: &str,
    ) -> anyhow::Result<ActionNode> {
        self.post_action_node_with_retry(url, ctx, label, &self.retry)
            .await
    }

    async fn post_action_node_with_retry(
        &self,
        url: &str,
        ctx: &ProviderContext,
        label: &str,
        retry: &RetryConfig,
    ) -> anyhow::Result<ActionNode> {
        let mut last_err = anyhow::anyhow!("no retry attempted");
        // Serialize once — the same bytes go into the debug log and the body.
        let body_str = serde_json::to_string(ctx).unwrap_or_default();
        for attempt in 0..retry.max_retries {
            let start = std::time::Instant::now();
            debug!(
                url = %url,
                method = "POST",
                body = %body_str,
                attempt = attempt,
                "{label} request"
            );
            let req = self
                .http_client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body_str.clone());
            match crate::http_util::execute_request(
                req,
                &self.headers,
                Some(Duration::from_millis(retry.timeout_ms)),
            )
            .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let elapsed = start.elapsed();
                    let body = crate::http_util::read_body_with_timeout(
                        Duration::from_millis(retry.timeout_ms),
                        resp.text(),
                    )
                    .await
                    .unwrap_or_default();
                    debug!(
                        url = %url,
                        status = %status,
                        duration_ms = %elapsed.as_millis(),
                        response_body = %body,
                        "{label} response"
                    );
                    return serde_json::from_str(&body)
                        .map_err(|e| anyhow::anyhow!("failed to parse ActionNode: {}", e));
                }
                Err(e) => {
                    let elapsed = start.elapsed();
                    last_err = e;
                    error!(
                        url = %url,
                        error = %last_err,
                        duration_ms = %elapsed.as_millis(),
                        "{label} error"
                    );
                }
            }
            if attempt < retry.max_retries - 1 {
                tokio::time::sleep(Duration::from_millis(retry.retry_delay_ms)).await;
            }
        }
        Err(last_err)
    }
}

#[async_trait]
impl ActionProvider for StepProvider {
    fn name(&self) -> &str {
        "step"
    }

    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        let url = self.endpoint_url(None);

        // ── Resume wire-format adaptation + auto-downgrade fuse ────────────
        // The executor emits `ProviderEvent::Resume` for suspended-flow
        // continuations. Per config / observed endpoint capability it is
        // either sent as `{"type":"resume"}` or adapted to the legacy
        // `session_start` event. The executor renders the ivr_step_trace
        // trigger from its own event, so the trace says `resume` either way.
        let mut ctx = ctx;
        let mut probing_resume = false;
        if matches!(ctx.event, Some(ProviderEvent::Resume { .. })) {
            if self.resume_event_mode == ResumeEventMode::SessionStart
                || resume_downgraded_for(&url)
            {
                ctx.event = Some(ProviderEvent::SessionStart);
            } else {
                probing_resume = true;
            }
        }

        let result = if probing_resume {
            // Probe with a SINGLE attempt: a legacy endpoint must not cost
            // the caller the full retry budget of dead air before the fuse
            // blows.
            let probe_retry = RetryConfig {
                max_retries: 1,
                ..self.retry.clone()
            };
            match self
                .post_action_node_with_retry(
                    &url,
                    &ctx,
                    "StepProvider next_action (resume probe)",
                    &probe_retry,
                )
                .await
            {
                Ok(node) => return Ok(node),
                Err(probe_err) => {
                    // Fuse blown for this endpoint (process lifetime).
                    mark_resume_downgraded(&url);
                    error!(
                        url = %url,
                        error = %probe_err,
                        event = "ivr_resume_downgraded",
                        "endpoint rejected the resume event; re-sending as session_start and downgrading this endpoint"
                    );
                    let mut retry_ctx = ctx.clone();
                    retry_ctx.event = Some(ProviderEvent::SessionStart);
                    self.post_action_node_with_retry(
                        &url,
                        &retry_ctx,
                        "StepProvider next_action (resume downgraded)",
                        &self.retry,
                    )
                    .await
                }
            }
        } else {
            self.post_action_node_with_retry(&url, &ctx, "StepProvider next_action", &self.retry)
                .await
        };

        match result {
            Ok(node) => Ok(node),
            Err(last_err) => {
                // Prefer session-level IVR fallback when configured.
                if self.prefer_ivr_fallback {
                    return Err(last_err);
                }
                match &self.retry.fallback_action {
                    Some(node) => Ok(node.clone()),
                    None => Err(last_err),
                }
            }
        }
    }

    async fn fail_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        let url = self.endpoint_url(Some("fail"));
        // Do not apply retry.fallback_action — escalate to IVR fallback instead.
        self.post_action_node(&url, &ctx, "StepProvider fail_action")
            .await
    }

    async fn on_session_start(&self, ctx: &SessionContext) -> anyhow::Result<()> {
        let url = self.endpoint_url(Some("start"));
        let start = std::time::Instant::now();
        let req = self.http_client.post(&url).json(ctx);
        if let Err(e) = crate::http_util::execute_request(req, &self.headers, None).await {
            error!(
                url = %url,
                error = %e,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_session_start failed"
            );
        } else {
            debug!(
                url = %url,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_session_start response"
            );
        }
        Ok(())
    }

    async fn on_session_end_context(
        &self,
        reason: &SessionEndReason,
        context: &SessionContext,
    ) -> anyhow::Result<()> {
        let url = self.endpoint_url(Some("end"));
        let body = serde_json::json!({
            "session_id": context.session_id,
            "app_execution_id": context.app_execution_id,
            "reason": reason.reason,
            "detail": reason.detail,
        });
        let start = std::time::Instant::now();
        let req = self.http_client.post(&url).json(&body);
        if let Err(e) = crate::http_util::execute_request(req, &self.headers, None).await {
            error!(
                url = %url,
                error = %e,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_session_end failed"
            );
        }
        Ok(())
    }

    async fn on_local_dtmf_match(&self, digit: &str, action: &ActionNode) {
        let url = self.endpoint_url(Some("dtmf-match"));
        let body = serde_json::json!({ "digit": digit, "action": action });
        let start = std::time::Instant::now();
        let req = self.http_client.post(&url).json(&body);
        if let Err(e) = crate::http_util::execute_request(req, &self.headers, None).await {
            error!(
                url = %url,
                error = %e,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_local_dtmf_match failed"
            );
        } else {
            debug!(
                url = %url,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_local_dtmf_match response"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_result_uses_minimal_wire_contract() {
        let event = ProviderEvent::TransferResult {
            outcome: TransferOutcome::NotConnected,
        };

        assert_eq!(
            serde_json::to_value(event).unwrap(),
            serde_json::json!({"type": "transfer_result", "outcome": "not_connected"})
        );
        assert_eq!(
            serde_json::to_value(ProviderEvent::TransferResult {
                outcome: TransferOutcome::Busy,
            })
            .unwrap(),
            serde_json::json!({"type": "transfer_result", "outcome": "busy"})
        );
        assert_eq!(
            serde_json::to_value(ProviderEvent::TransferResult {
                outcome: TransferOutcome::NoAnswer,
            })
            .unwrap(),
            serde_json::json!({"type": "transfer_result", "outcome": "no_answer"})
        );
    }

    #[test]
    fn test_step_provider_endpoint_url_trims_whitespace_and_slash() {
        let provider =
            StepProvider::new(" http://127.0.0.1:28080/ivr/step/ ", reqwest::Client::new());

        assert_eq!(
            provider.endpoint_url(None),
            "http://127.0.0.1:28080/ivr/step"
        );
        assert_eq!(
            provider.endpoint_url(Some("start")),
            "http://127.0.0.1:28080/ivr/step/start"
        );
        assert_eq!(
            provider.endpoint_url(Some("end")),
            "http://127.0.0.1:28080/ivr/step/end"
        );
        assert_eq!(
            provider.endpoint_url(Some("fail")),
            "http://127.0.0.1:28080/ivr/step/fail"
        );
    }

    #[test]
    fn fail_event_serializes_type_fail() {
        let event = ProviderEvent::Fail {
            reason: "transfer failed".into(),
            failed_step_id: Some("n1".into()),
            failed_step_name: Some("xfer".into()),
            failed_action: Some("Transfer".into()),
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            serde_json::json!({
                "type": "fail",
                "reason": "transfer failed",
                "failed_step_id": "n1",
                "failed_step_name": "xfer",
                "failed_action": "Transfer",
            })
        );
    }

    #[test]
    fn test_session_end_reason_serializes_snake_case() {
        let r = SessionEndReason {
            reason: SessionEndTag::TransferToQueue,
            detail: Some("support".into()),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["reason"], "transfer_to_queue");
        assert_eq!(json["detail"], "support");
    }

    #[test]
    fn test_session_end_tag_serializes_snake_case() {
        let json = serde_json::to_string(&SessionEndTag::TransferToQueue).unwrap();
        assert_eq!(json, "\"transfer_to_queue\"");

        let json = serde_json::to_string(&SessionEndTag::UserHangup).unwrap();
        assert_eq!(json, "\"user_hangup\"");

        let json = serde_json::to_string(&SessionEndTag::Timeout).unwrap();
        assert_eq!(json, "\"timeout\"");
    }

    #[test]
    fn test_session_end_reason_json_roundtrip() {
        let original = SessionEndReason {
            reason: SessionEndTag::Transfer,
            detail: Some("2001".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("\"reason\":\"transfer\""));
        assert!(json.contains("\"detail\":\"2001\""));

        let parsed: SessionEndReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.reason, SessionEndTag::Transfer);
        assert_eq!(parsed.detail.as_deref(), Some("2001"));
    }

    #[test]
    fn test_session_end_reason_skips_none_detail() {
        let r = SessionEndReason {
            reason: SessionEndTag::Normal,
            detail: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("detail"));
    }
}
