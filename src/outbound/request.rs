use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `POST /ami/v1/outbound/dial` request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DialRequest {
    /// Optional explicit call-id. If omitted, one is generated and returned in
    /// the first SSE event.
    #[serde(default)]
    pub call_id: Option<String>,

    /// SIP URI of the caller (A-leg identity). Example: `sip:1000@pbx.local`.
    pub caller_id: Option<String>,

    /// Destination SIP URI or phone number. Example: `sip:13800000000@trunk`
    /// or `13800000000` (normalized using the server realm).
    pub destination: String,

    /// Optional explicit trunk name. When set, the INVITE is routed directly
    /// out the named carrier trunk (stamping next-hop, credentials, PAI).
    #[serde(default)]
    pub trunk: Option<String>,

    /// Extra SIP headers to add to the outbound INVITE.
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,

    /// Ring timeout in seconds (default from `[outbound]` config or 30).
    #[serde(default)]
    pub ring_timeout: Option<u64>,

    /// What to do after the callee answers.
    #[serde(default)]
    pub on_answer: OnAnswer,

    /// Optional callback when the call fails (no answer, busy, rejected).
    #[serde(default)]
    pub on_failure: Option<OnFailure>,

    /// Arbitrary metadata forwarded to the webhook payload and stored in CDR.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// Post-answer action. All variants are supported; `ExecuteFlow` is the default.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OnAnswer {
    /// Continue executing the dialplan flow after answer (default).
    ExecuteFlow,
    /// Bridge the answered B-leg to an existing leg identified by
    /// `session_id` or `dialog_id`.
    BridgeToLeg {
        leg_id: String,
    },
    /// Place the answered leg into an ACD queue and wait for an agent.
    Enqueue {
        queue: String,
        #[serde(default)]
        priority: Option<u32>,
        #[serde(default)]
        skills: Option<Vec<String>>,
        #[serde(default)]
        max_wait_secs: Option<u32>,
    },
    /// Run an IVR / call application by name.
    App {
        app_name: String,
        #[serde(default)]
        app_params: HashMap<String, String>,
    },
    /// POST to a sync webhook and act on the returned instruction.
    Webhook(WebhookAction),
}

impl Default for OnAnswer {
    fn default() -> Self {
        Self::ExecuteFlow
    }
}

/// Sync-webhook post-answer action.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhookAction {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Sync response timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Fallback action when the webhook times out or returns an error.
    #[serde(default)]
    pub fallback: FallbackAction,
}

/// Fallback when the webhook is unreachable / slow / returns an error.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FallbackAction {
    /// Hangup the call.
    #[default]
    Hangup,
    /// Bridge to a specific leg.
    Bridge { leg_id: String },
    /// Enqueue into a queue.
    Enqueue { queue: String },
}

/// Failure callback.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OnFailure {
    pub webhook: WebhookAction,
}
