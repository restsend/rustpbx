use crate::call::app::ivr::common::SessionData;
use crate::call::app::ivr::config::{ActionNode, EntryAction, IvrDefinition, MenuNode};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::warn;

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

    async fn on_session_end(&self, reason: &EndReason) -> anyhow::Result<()> {
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
    pub event: Option<ProviderEvent>,
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
    Normal,
    Transfer(String),
    Hangup,
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
                },
            );
        }
        let timeout_action = menu.timeout_action.clone().map(|a| {
            Box::new(ActionNode {
                action: a,
                next: None,
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
            },
            next: None,
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
                // Try matching DTMF key
                if let Some(entry) = menu.entries.iter().find(|e| e.key == digit.as_str()) {
                    return Ok(ActionNode {
                        action: entry.action.clone(),
                        next: None,
                    });
                }
                // Unknown key
                if let Some(ref unknown) = menu.unknown_key_action {
                    return Ok(ActionNode {
                        action: unknown.clone(),
                        next: None,
                    });
                }
                // No match → return Repeat
                Ok(ActionNode::new(EntryAction::Repeat))
            }
            Some(ProviderEvent::DtmfMenuTimeout) | Some(ProviderEvent::DtmfTimeout) => {
                // Timeout → repeat or timeout_action
                let menu = self.current_menu().cloned().unwrap_or_default();
                if let Some(ta) = menu.timeout_action {
                    Ok(ActionNode {
                        action: ta,
                        next: None,
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
        for attempt in 0..self.retry.max_retries {
            let mut req = self.http_client.post(&self.url).json(&ctx);
            for (k, v) in &self.headers {
                req = req.header(k, v);
            }
            match tokio::time::timeout(Duration::from_millis(self.retry.timeout_ms), req.send())
                .await
            {
                Ok(Ok(resp)) => {
                    if resp.status().is_success() {
                        return resp
                            .json::<ActionNode>()
                            .await
                            .map_err(|e| anyhow::anyhow!("failed to parse ActionNode: {}", e));
                    }
                    last_err = anyhow::anyhow!("HTTP {}", resp.status());
                }
                Ok(Err(e)) => last_err = anyhow::anyhow!("request failed: {}", e),
                Err(_) => last_err = anyhow::anyhow!("timeout"),
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
        let mut req = self
            .http_client
            .post(format!("{}/start", self.url))
            .json(ctx);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Err(e) = req.send().await {
            warn!(url = %self.url, error = %e, "StepProvider on_session_start failed");
        }
        Ok(())
    }

    async fn on_session_end(&self, reason: &EndReason) -> anyhow::Result<()> {
        let mut req = self
            .http_client
            .post(format!("{}/end", self.url))
            .json(&serde_json::json!({
                "reason": format!("{:?}", reason),
            }));
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Err(e) = req.send().await {
            warn!(url = %self.url, reason = ?reason, error = %e, "StepProvider on_session_end failed");
        }
        Ok(())
    }

    async fn on_local_dtmf_match(&self, digit: &str, action: &ActionNode) {
        let mut req = self
            .http_client
            .post(format!("{}/dtmf-match", self.url))
            .json(&serde_json::json!({
                "digit": digit,
                "action": action,
            }));
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Err(e) = req.send().await {
            warn!(url = %self.url, digit = %digit, error = %e, "StepProvider on_local_dtmf_match failed");
        }
    }
}
