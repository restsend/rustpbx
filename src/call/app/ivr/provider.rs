use crate::call::app::ivr::common::SessionData;
use crate::call::app::ivr::config::{ActionNode, EntryAction, IvrDefinition, MenuNode};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{info, warn};

// ── Provider Trait ───────────────────────────────────────────────────────────

#[async_trait]
pub trait ActionProvider: Send + Sync {
    /// Provider identifier for debug tracing
    fn name(&self) -> &str {
        "unknown"
    }

    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode>;

    async fn on_session_start(&self, ctx: &SessionContext) -> anyhow::Result<()> {
        let _ = ctx;
        Ok(())
    }

    async fn on_session_end(&self, reason: &EndReason, _session_id: &str) -> anyhow::Result<()> {
        let _ = reason;
        Ok(())
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
    pub caller: String,
    pub callee: String,
    pub direction: String,
    pub tenant_id: Option<String>,
    pub ivr_id: Option<String>,
    /// All SIP headers from the original INVITE request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sip_headers: Option<HashMap<String, String>>,
    /// Name of the matched route that sent this call into the IVR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_name: Option<String>,
    /// Extra headers configured on the route (from routing rule `option.headers`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_headers: Option<HashMap<String, String>>,
    /// Arbitrary passthrough data set by the caller / external system.
    /// The provider receives this and can use it for correlation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_data: Option<serde_json::Value>,
    /// Whether this session was re-entered from agent/queue (transfer-back).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transferred_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderContext {
    pub session_id: String,
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
    /// Extra headers configured on the route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_headers: Option<HashMap<String, String>>,
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
    /// Whether this session was re-entered from agent/queue.
    /// Values: `"agent"`, `"queue"`, or `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transferred_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    SessionStart,
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
    InputVoice {
        text: String,
        confidence: f32,
    },
    Error {
        reason: String,
    },
    DtmfMenuInvalid {
        digit: String,
    },
    DtmfMenuTimeout,
}

#[derive(Debug, Clone)]
pub enum EndReason {
    /// IVR completed normally (played all nodes, no transfer).
    Normal,
    /// IVR exited because the call was transferred to an agent or extension.
    Transfer(String),
    /// IVR exited because the call was sent to a queue.
    TransferToQueue(String),
    /// IVR exited because the call jumped to another IVR.
    TransferToIvr(String),
    /// System (PBX) initiated the hangup.
    Hangup,
    /// User / remote party hung up.
    UserHangup,
    /// Error during IVR execution.
    Error(String),
}

impl From<&str> for EndReason {
    fn from(s: &str) -> Self {
        match s {
            "hangup" => EndReason::Hangup,
            "normal" => EndReason::Normal,
            other => EndReason::Error(other.to_string()),
        }
    }
}

// ── RetryConfig ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub timeout_ms: u64,
    pub fallback_action: Option<ActionNode>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            timeout_ms: 1000,
            fallback_action: Some(ActionNode {
                action: EntryAction::Hangup {
                    prompt: Some("sounds/error.wav".into()),
                    prompt_text: None,
                    prompt_voice: None,
                },
                next: None,
                step_id: None,
                step_name: None,
                extra: None,
            }),
        }
    }
}

// ── TreeProvider ─────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct TreeProvider {
    definition: IvrDefinition,
    sess: SessionData,
    menu_stack: Vec<String>,
    state: TreeState,
    retry_count: u32,
}

#[allow(dead_code)]
enum TreeState {
    Idle,
    WaitingDtmf { menu_key: String },
}

impl TreeProvider {
    pub fn new(definition: IvrDefinition) -> Self {
        Self {
            definition,
            sess: SessionData::default(),
            menu_stack: vec!["root".to_string()],
            state: TreeState::Idle,
            retry_count: 0,
        }
    }

    fn current_menu(&self) -> Option<&MenuNode> {
        let key = self.menu_stack.last()?;
        self.definition.get_menu(key)
    }

    fn build_dtmf_menu_action(&self) -> ActionNode {
        let menu = self.current_menu().cloned().unwrap_or_default();
        let mut entries = HashMap::new();
        for entry in &menu.entries {
            // Convert TOML EntryAction to ActionNode
            entries.insert(
                entry.key.clone(),
                ActionNode {
                    action: entry.action.clone(),
                    next: None,
                    step_id: None,
                    step_name: None,
                    extra: None,
                },
            );
        }
        let timeout_action = menu.timeout_action.clone().map(|a| {
            Box::new(ActionNode {
                action: a,
                next: None,
                step_id: None,
                step_name: None,
                extra: None,
            })
        });
        let invalid_action = if menu.invalid_prompt.is_some() || menu.invalid_text.is_some() {
            let prompt = menu
                .invalid_prompt
                .clone()
                .or_else(|| menu.invalid_text.clone());
            Some(Box::new(ActionNode::with_next(
                EntryAction::Prompt {
                    file: prompt,
                    tts_text: None,
                    tts_voice: menu.invalid_voice.clone(),
                    record_name_list: None,
                    interruptible: false,
                    tts_api_url: None,
                },
                ActionNode::new(EntryAction::Repeat),
            )))
        } else {
            None
        };
        ActionNode {
            action: EntryAction::DtmfMenu {
                greeting: Some(menu.greeting).filter(|g| !g.is_empty()),
                greeting_text: menu.greeting_text,
                greeting_record_list: None,
                greeting_voice: menu.greeting_voice,
                timeout_ms: menu.timeout_ms,
                max_retries: menu.max_retries,
                entries,
                timeout_action,
                invalid_action,
                greeting_api_url: None,
            },
            next: None,
            step_id: None,
            step_name: None,
            extra: None,
        }
    }
}

#[async_trait]
impl ActionProvider for TreeProvider {
    fn name(&self) -> &str {
        "tree"
    }

    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        match ctx.event.as_ref() {
            Some(ProviderEvent::SessionStart) => Ok(self.build_dtmf_menu_action()),
            Some(ProviderEvent::Dtmf { digit }) => {
                let menu = self.current_menu().cloned().unwrap_or_default();
                if let Some(entry) = menu.entries.iter().find(|e| e.key == digit.as_str()) {
                    return Ok(ActionNode {
                        action: entry.action.clone(),
                        next: None,
                        step_id: None,
                        step_name: None,
                        extra: None,
                    });
                }
                if let Some(ref unknown) = menu.unknown_key_action {
                    return Ok(ActionNode {
                        action: unknown.clone(),
                        next: None,
                        step_id: None,
                        step_name: None,
                        extra: None,
                    });
                }
                Ok(ActionNode::new(EntryAction::Repeat))
            }
            Some(ProviderEvent::DtmfMenuTimeout) | Some(ProviderEvent::DtmfTimeout) => {
                let menu = self.current_menu().cloned().unwrap_or_default();
                if let Some(ta) = menu.timeout_action {
                    Ok(ActionNode {
                        action: ta,
                        next: None,
                        step_id: None,
                        step_name: None,
                        extra: None,
                    })
                } else {
                    Ok(ActionNode::new(EntryAction::Repeat))
                }
            }
            _ => Ok(ActionNode::new(EntryAction::Repeat)),
        }
    }
}

// ── StepProvider (HTTP) ──────────────────────────────────────────────────────

pub struct StepProvider {
    url: String,
    headers: HashMap<String, String>,
    http_client: reqwest::Client,
    retry: RetryConfig,
}

impl StepProvider {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: HashMap::new(),
            http_client: reqwest::Client::new(),
            retry: RetryConfig::default(),
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

    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = client;
        self
    }

    /// Add a single HTTP header.
    pub fn add_header(&mut self, key: &str, value: &str) {
        self.headers.insert(key.to_string(), value.to_string());
    }
}

#[async_trait]
impl ActionProvider for StepProvider {
    fn name(&self) -> &str {
        "step"
    }

    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        let mut last_err = anyhow::anyhow!("no retry attempted");
        let body_str = serde_json::to_string(&ctx).unwrap_or_default();
        for attempt in 0..self.retry.max_retries {
            let start = std::time::Instant::now();
            info!(
                url = %self.url,
                method = "POST",
                headers = ?self.headers,
                body = %body_str,
                attempt = attempt,
                "StepProvider next_action request"
            );
            let req = self.http_client.post(&self.url).json(&ctx);
            match crate::http_util::execute_request(
                req,
                &self.headers,
                Some(Duration::from_millis(self.retry.timeout_ms)),
            )
            .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let elapsed = start.elapsed();
                    let body = resp.text().await.unwrap_or_default();
                    info!(
                        url = %self.url,
                        status = %status,
                        duration_ms = %elapsed.as_millis(),
                        response_body = %body,
                        "StepProvider next_action response"
                    );
                    return serde_json::from_str(&body)
                        .map_err(|e| anyhow::anyhow!("failed to parse ActionNode: {}", e));
                }
                Err(e) => {
                    let elapsed = start.elapsed();
                    last_err = e;
                    info!(
                        url = %self.url,
                        error = %last_err,
                        duration_ms = %elapsed.as_millis(),
                        "StepProvider next_action error"
                    );
                }
            }
            if attempt < self.retry.max_retries - 1 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        // All retries exhausted → fallback
        match &self.retry.fallback_action {
            Some(node) => Ok(node.clone()),
            None => Err(last_err),
        }
    }

    async fn on_session_start(&self, ctx: &SessionContext) -> anyhow::Result<()> {
        let url = format!("{}/start", self.url);
        let body_str = serde_json::to_string(ctx).unwrap_or_default();
        info!(
            url = %url,
            method = "POST",
            headers = ?self.headers,
            body = %body_str,
            "StepProvider on_session_start request"
        );
        let start = std::time::Instant::now();
        let req = self.http_client.post(&url).json(ctx);
        if let Err(e) = crate::http_util::execute_request(req, &self.headers, None).await {
            warn!(
                url = %url,
                error = %e,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_session_start failed"
            );
        } else {
            info!(
                url = %url,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_session_start response"
            );
        }
        Ok(())
    }

    async fn on_session_end(&self, reason: &EndReason, session_id: &str) -> anyhow::Result<()> {
        let url = format!("{}/end", self.url);
        let body = serde_json::json!({
            "session_id": session_id,
            "reason": format!("{:?}", reason),
        });
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        info!(
            url = %url,
            method = "POST",
            headers = ?self.headers,
            body = %body_str,
            "StepProvider on_session_end request"
        );
        let start = std::time::Instant::now();
        let req = self.http_client.post(&url).json(&body);
        if let Err(e) = crate::http_util::execute_request(req, &self.headers, None).await {
            warn!(
                url = %url,
                error = %e,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_session_end failed"
            );
        } else {
            info!(
                url = %url,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_session_end response"
            );
        }
        Ok(())
    }

    async fn on_local_dtmf_match(&self, digit: &str, action: &ActionNode) {
        let url = format!("{}/dtmf-match", self.url);
        let body = serde_json::json!({ "digit": digit, "action": action });
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        info!(
            url = %url,
            method = "POST",
            headers = ?self.headers,
            body = %body_str,
            "StepProvider on_local_dtmf_match request"
        );
        let start = std::time::Instant::now();
        let req = self.http_client.post(&url).json(&body);
        if let Err(e) = crate::http_util::execute_request(req, &self.headers, None).await {
            warn!(
                url = %url,
                error = %e,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_local_dtmf_match failed"
            );
        } else {
            info!(
                url = %url,
                duration_ms = %start.elapsed().as_millis(),
                "StepProvider on_local_dtmf_match response"
            );
        }
    }
}
