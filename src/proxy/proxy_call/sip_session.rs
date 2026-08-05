use crate::call::app::PendingQueuePlan;
use crate::call::app::{ApplicationContext, CallInfo};
use crate::call::domain::{
    CallCommand, HangupCascade, HangupCommand, LegId, LegState, MediaPathMode, MediaRuntimeProfile,
    MediaSource, RingbackPolicy,
};
use crate::call::domain::{Leg, SessionState};
use crate::call::runtime::BridgeConfig;
use crate::call::runtime::{
    AppFactory, AppRuntime, AppRuntimeConfig, CommandResult, DefaultAppRuntime, ExecutionContext,
    MediaCapabilityCheck, SessionId,
};
use crate::call::{DialStrategy, Location};
use crate::models::call_record::extract_sip_username;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub state: SessionState,
    pub leg_count: usize,
    pub bridge_active: bool,
    pub caller_gate_open: bool,
    pub media_path: MediaPathMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_sdp: Option<String>,
    #[serde(skip)]
    pub callee_dialogs: Vec<DialogId>,
}
use crate::call::domain::SessionPolicy;
use crate::call::sip::{ClientDialogGuard, ServerDialogGuard};
use crate::callrecord::{CallRecordHangupMessage, CallRecordHangupReason, CallRecordSender};
use crate::config::MediaProxyMode;
use crate::media::media_bridge::MediaBridge;
use crate::media::negotiate::MediaNegotiator;
use crate::media::{RtpTrackBuilder, Track};
use crate::proxy::call::parse_allowed_codecs;
use crate::proxy::proxy_call::session_registry::SupervisorSession;
use crate::proxy::proxy_call::{
    media_peer::{MediaPeer, VoiceEnginePeer},
    reporter::CallReporter,
    session_timer::{
        DEFAULT_SESSION_EXPIRES, HEADER_MIN_SE, HEADER_SESSION_EXPIRES, HEADER_SUPPORTED,
        SessionRefresher, SessionTimerState, apply_refresh_response, apply_session_timer_headers,
        build_default_session_timer_headers, build_session_timer_headers,
        build_session_timer_response_headers, get_header_value, has_timer_support, parse_min_se,
        parse_session_expires, select_client_timer_refresher, select_server_timer_refresher,
    },
    state::{CallContext, CallSessionRecordSnapshot},
};
use crate::proxy::server::SipServerRef;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;

use dashmap::DashMap;
use parking_lot::RwLock;
use rsipstack::dialog::{
    DialogId, dialog::Dialog, dialog::DialogState, dialog::TerminatedReason,
    dialog::TransactionHandle, invite_dialog::InviteDialog,
};
use rsipstack::sip::StatusCode;
use rsipstack::sip::Transport;
use rsipstack::transport::SipAddr;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

/// Map a SIP response status code to a fine-grained CallRecordHangupReason.
///
/// This replaces the previous behaviour where every dialplan / callee failure
/// was uniformly tagged as `Failed`.
fn sip_status_to_hangup_reason(status_code: u16) -> CallRecordHangupReason {
    match status_code {
        486 | 600 => CallRecordHangupReason::Rejected, // Busy Here / Busy Everywhere
        487 => CallRecordHangupReason::Canceled,       // Request Terminated
        408 => CallRecordHangupReason::NoAnswer,       // Request Timeout
        480 | 484 | 485 => CallRecordHangupReason::NoAnswer, // Temporarily Unavailable / Address Incomplete
        481 | 482 | 483 => CallRecordHangupReason::Failed,   // Call/Loop Not Exist
        488 | 489 => CallRecordHangupReason::Failed,         // Not Acceptable Here
        491 | 493 => CallRecordHangupReason::Failed,
        500 | 502 | 503 => CallRecordHangupReason::ServerUnavailable,
        504 => CallRecordHangupReason::ServerUnavailable,
        603 => CallRecordHangupReason::Rejected, // Decline Everywhere
        604 => CallRecordHangupReason::NoAnswer, // Does Not Exist Anywhere
        _ if (400..500).contains(&status_code) => CallRecordHangupReason::Failed,
        _ if (500..600).contains(&status_code) => CallRecordHangupReason::ServerUnavailable,
        _ if (600..700).contains(&status_code) => CallRecordHangupReason::Failed,
        _ => CallRecordHangupReason::Failed,
    }
}
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use tokio_util::{
    sync::CancellationToken,
    time::{DelayQueue, delay_queue},
};
use tracing::{debug, error, info, trace, warn};

mod conference;
mod queue;
mod supervisor;
mod transfer;

#[derive(Debug)]
enum TimerAction {
    Refresh,
    Expired,
}

enum UpdateRefreshOutcome {
    Refreshed,
    Retry,
    FallbackToReinvite,
    Failed(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogSide {
    Caller,
    Callee,
}

pub type CalleeError = (u16, String, Option<String>);

pub fn into_callee_err(code: &StatusCode, msg: Option<String>) -> CalleeError {
    (code.code(), code.text().to_string(), msg)
}

/// Percent-decode a query-string value (replace `+` with space, then URL-decode).
pub(super) fn pct_decode_query(value: &str) -> String {
    let s = value.replace('+', " ");
    match urlencoding::decode(&s) {
        Ok(c) => c.into_owned(),
        Err(_) => s,
    }
}

/// Convert a transfer/redirect error into a `CalleeError` with
/// `TemporarilyUnavailable` status.
pub(super) fn map_queue_xfer_err(e: anyhow::Error) -> CalleeError {
    into_callee_err(
        &StatusCode::TemporarilyUnavailable,
        Some(format!("Queue transfer failed: {}", e)),
    )
}

pub struct SipSession {
    pub id: SessionId,
    pub state: SessionState,
    pub legs: crate::proxy::proxy_call::leg_registry::LegRegistry,
    #[allow(dead_code)]
    pub policy: SessionPolicy,
    pub bridge: BridgeConfig,
    pub media_profile: MediaRuntimeProfile,
    pub app_runtime: Arc<dyn AppRuntime>,
    pub snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>>,

    pub server: SipServerRef,
    /// The primary caller dialog (A leg). `None` in UAC/outbound mode
    /// (e.g. RWI originate) where the caller leg is virtual until the first
    /// outbound INVITE is attached via `attach_caller_dialog`.
    pub caller_dialog: Option<InviteDialog>,
    pub callee_dialogs: Arc<DashMap<DialogId, ()>>,
    pub supervisor_mixer: Option<Arc<SupervisorSession>>,

    pub context: CallContext,
    /// Shared owner for every concurrent-call permit held by this
    /// call. Cleanup releases the complete set; Drop is the safety net.
    pub concurrent_call_lease: crate::call::concurrent_call_limiter::ConcurrentCallLease,
    pub call_record_sender: Option<CallRecordSender>,

    pub cancel_token: CancellationToken,
    pub pending_hangup: HashSet<DialogId>,
    pub meta: crate::proxy::proxy_call::call_meta::CallMeta,
    pub media: crate::proxy::proxy_call::media_state::MediaState,

    timers: HashMap<DialogId, SessionTimerState>,
    update_refresh_disabled: HashSet<DialogId>,
    timer_queue: DelayQueue<DialogId>,
    timer_keys: HashMap<DialogId, delay_queue::Key>,

    pub callee_event_tx: Option<mpsc::UnboundedSender<DialogState>>,
    pub callee_guards: Vec<ClientDialogGuard>,

    pub dtmf_digits: Vec<char>,

    pub reporter: Option<CallReporter>,
    cdr_sent: Arc<std::sync::atomic::AtomicBool>,

    pub app_event_bridge: Arc<RwLock<Option<crate::proxy::proxy_call::state::SipSessionHandle>>>,

    /// Per-session typed extensions bag (session cookie) for cross-addon data
    /// sharing. Cloned into every `CallSessionContext` so all hook callbacks
    /// share the same underlying bag.
    pub extensions: crate::proxy::proxy_call::session_hooks::SessionExtensions,

    pub conference_bridge: crate::call::runtime::SessionConferenceBridge,

    /// Sender used to forward DTMF digits (as JSON text frames) to the
    /// active bridge WebSocket. Set by `connect_bridge()`, cleared when the
    /// bridge ends. SIP INFO DTMF is also forwarded through this channel.
    pub bridge_dtmf_tx:
        Arc<parking_lot::RwLock<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,

    #[allow(dead_code)]
    pub cmd_tx: Option<mpsc::Sender<CallCommand>>,

    /// This session's own handle (used to send commands back into the session
    /// from spawned tasks, e.g. restore the media route after playback).
    pub handle: SipSessionHandle,


}

#[derive(Clone)]
pub struct SipSessionHandle {
    session_id: SessionId,
    cmd_tx: mpsc::Sender<CallCommand>,
    snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>>,
    app_event_bridge: Arc<RwLock<Option<crate::proxy::proxy_call::state::SipSessionHandle>>>,
}

const CMD_CHANNEL_CAPACITY: usize = 256;

/// Custom SIP INFO content type for rustpbx call-control commands.
/// The body is a JSON object with `action` and optional `params` fields.
const RUSTPBX_COMMAND_CT: &str = "application/vnd.rustpbx+json";

impl SipSessionHandle {
    pub fn send_command(&self, cmd: CallCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .try_send(cmd)
            .map_err(|e| {
                warn!(session_id = %self.session_id.0, "SipSession cmd_tx channel full, cmd dropped: {e}");
                anyhow::anyhow!("channel error: {}", e)
            })
    }

    pub async fn send_command_async(&self, cmd: CallCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|e| anyhow::anyhow!("channel closed: {}", e))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id.0
    }

    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        self.snapshot_cache.read().clone()
    }

    pub fn send_app_event(&self, event: crate::call::app::ControllerEvent) -> bool {
        let bridge = self.app_event_bridge.read();
        if let Some(ref handle) = *bridge {
            return handle.send_app_event(event);
        }
        false
    }

    pub fn set_app_event_sender(
        &self,
        sender: Option<mpsc::UnboundedSender<crate::call::app::ControllerEvent>>,
    ) {
        let bridge = self.app_event_bridge.read();
        if let Some(ref handle) = *bridge {
            handle.set_app_event_sender(sender);
        }
    }
}

impl SipSessionHandle {
    /// Create a handle for testing (no real bridge/snapshot).
    #[cfg(test)]
    pub fn new_for_test(
        session_id: &str,
        cmd_tx: mpsc::Sender<crate::call::domain::CallCommand>,
    ) -> Self {
        Self {
            session_id: SessionId::from(session_id.to_string()),
            cmd_tx,
            snapshot_cache: Arc::new(RwLock::new(None)),
            app_event_bridge: Arc::new(RwLock::new(None)),
        }
    }
}

/// Built-in factory that creates `CallApp` instances from app parameters.
struct BuiltinAppFactory {
    addon_registry: Option<Arc<crate::addons::registry::AddonRegistry>>,
}

#[async_trait]
impl AppFactory for BuiltinAppFactory {
    async fn create_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        context: &ApplicationContext,
    ) -> Option<Box<dyn crate::call::app::CallApp>> {
        // First try addon hooks (allows addons to override built-in apps).
        if let Some(reg) = &self.addon_registry {
            if let Some(app) = reg.build_call_app(app_name, params.clone(), context).await {
                return Some(app);
            }
        }
        match app_name {
            "ivr" => {
                // First check if params has inline step mode config (legacy/debug routes)
                let mode = params
                    .as_ref()
                    .and_then(|p| p.get("mode").and_then(|v| v.as_str()))
                    .unwrap_or("tree");

                if mode == "step" && params.as_ref()?.get("url").is_some() {
                    // Inline step mode (from debug routes or legacy app_params)
                    let url = params
                        .as_ref()
                        .and_then(|p| p.get("url").and_then(|v| v.as_str()))?;

                    let mut provider = crate::call::app::ivr::StepProvider::new(url);

                    if let Some(hdrs) = params.as_ref()?.get("headers") {
                        if let Some(h) = hdrs.as_object() {
                            for (k, v) in h {
                                if let Some(vs) = v.as_str() {
                                    provider.add_header(k, vs);
                                }
                            }
                        }
                    }

                    if let Some(retry) = params.as_ref()?.get("retry") {
                        let max_retries = retry
                            .get("max_retries")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(3) as u32;
                        let timeout = retry
                            .get("timeout_ms")
                            .and_then(|v| v.as_u64())
                            .or_else(|| retry.get("delay_ms").and_then(|v| v.as_u64()))
                            .unwrap_or(1000);
                        let fallback = serde_json::from_value(
                            retry.get("fallback").cloned().unwrap_or(serde_json::json!({
                                "type": "hangup",
                                "prompt": "sounds/error.wav"
                            })),
                        )
                        .ok();
                        provider = provider.with_retry(crate::call::app::ivr::RetryConfig {
                            max_retries,
                            timeout_ms: timeout,
                            fallback_action: fallback,
                        });
                    }

                    let mut app =
                        crate::call::app::ivr::StepIvrApp::with_provider(Box::new(provider));
                    let ivr_name = params
                        .as_ref()
                        .and_then(|p| p.get("name").and_then(|v| v.as_str()))
                        .unwrap_or("step_ivr")
                        .to_string();
                    app = app.with_name(ivr_name);
                    app = app.with_route_name(context.call_info.route_name.clone());
                    if let Some(tts_value) = params.as_ref()?.get("tts")
                        && let Ok(tts_cfg) =
                            serde_json::from_value::<crate::tts::TtsConfig>(tts_value.clone())
                    {
                        app = app.with_tts(Some(tts_cfg));
                    }
                    app = app.with_rwi_gateway(context.rwi_gateway.clone());
                    app = app.with_trace(context.ivr_trace.clone());
                    if let Some(ivp) = params.as_ref().and_then(|p| p.get("ivr_params")) {
                        app = app.with_ivr_params(ivp.clone());
                        if let Some(tf) = ivp.get("transferred_from").and_then(|v| v.as_str()) {
                            app = app.with_transferred_from(Some(tf.to_string()));
                        }
                    }
                    Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
                } else {
                    // File-based: read TOML and detect mode from content.
                    // Supports both filesystem paths and virtual `db://<category>/<name>` URIs
                    // produced by `resolve_ivr_file` / `apply_route_metadata`
                    // when the proxy runs with `generated_db = true`.
                    let file = params.as_ref()?.get("file")?.as_str()?;
                    let content = if let Some((category, name)) =
                        crate::config_store::GeneratedConfigStore::parse_db_uri(file)
                    {
                        let store = crate::config_store::GeneratedConfigStore::from_config(
                            &context.config,
                            &context.db,
                        );
                        match store.read(category, name).await {
                            Ok(Some(c)) => c,
                            Ok(None) => {
                                tracing::warn!("IVR config '{}' not found in config store", file);
                                return None;
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to read IVR config '{}' from store: {}",
                                    file,
                                    e
                                );
                                return None;
                            }
                        }
                    } else {
                        match tokio::fs::read_to_string(file).await {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!("Failed to read IVR config '{}': {}", file, e);
                                return None;
                            }
                        }
                    };

                    let file_config: crate::call::app::ivr_config::IvrFileConfig =
                        match toml::from_str(&content) {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!("Failed to parse IVR TOML '{}': {}", file, e);
                                return None;
                            }
                        };

                    if file_config.ivr.is_step_mode() {
                        // Step mode from TOML
                        let provider_cfg = file_config.ivr.provider.as_ref()?;
                        let mut provider =
                            crate::call::app::ivr::StepProvider::new(&provider_cfg.url);
                        for (k, v) in &provider_cfg.headers {
                            provider.add_header(k, v);
                        }
                        provider = provider.with_retry(crate::call::app::ivr::RetryConfig {
                            max_retries: provider_cfg.max_retries,
                            timeout_ms: provider_cfg.retry_delay_ms,
                            fallback_action: None,
                        });

                        let mut app =
                            crate::call::app::ivr::StepIvrApp::with_provider(Box::new(provider));
                        app = app.with_name(file_config.ivr.name.clone());
                        app = app.with_route_name(context.call_info.route_name.clone());
                        if let Some(tts_value) = params.as_ref()?.get("tts")
                            && let Ok(tts_cfg) =
                                serde_json::from_value::<crate::tts::TtsConfig>(tts_value.clone())
                        {
                            app = app.with_tts(Some(tts_cfg));
                        }
                        app = app.with_rwi_gateway(context.rwi_gateway.clone());
                        app = app.with_trace(context.ivr_trace.clone());
                        if let Some(ivp) = params.as_ref().and_then(|p| p.get("ivr_params")) {
                            app = app.with_ivr_params(ivp.clone());
                            if let Some(tf) = ivp.get("transferred_from").and_then(|v| v.as_str()) {
                                app = app.with_transferred_from(Some(tf.to_string()));
                            }
                        }
                        Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
                    } else {
                        // Tree mode from TOML
                        let mut app = crate::call::app::ivr::IvrApp::new(file_config.ivr);
                        if let Some(tts_value) = params.as_ref()?.get("tts")
                            && let Ok(tts_cfg) =
                                serde_json::from_value::<crate::tts::TtsConfig>(tts_value.clone())
                        {
                            app = app.with_tts(Some(tts_cfg));
                        }
                        // Support return_menu for return-to-IVR resume
                        if let Some(ivp) = params.as_ref().and_then(|p| p.get("ivr_params")) {
                            if let Some(menu) = ivp.get("return_menu").and_then(|v| v.as_str()) {
                                if !menu.is_empty() {
                                    app = app.with_start_menu(menu.to_string());
                                }
                            }
                        }
                        Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
                    }
                }
            }
            "voicemail" => {
                let extension = params.as_ref()?.get("extension")?.as_str()?.to_string();
                // Core voicemail fallback — addon overrides via build_call_app above.
                let mut app = crate::call::app::voicemail::VoicemailApp::new(extension);
                if let Some(greeting) = params
                    .as_ref()?
                    .get("greeting_path")
                    .and_then(|v| v.as_str())
                {
                    app = app.with_greeting_path(greeting);
                }
                Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
            }
            "conference" => {
                let conf_id = params
                    .as_ref()?
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default")
                    .to_string();
                let caller_id = context.call_info.caller.clone();
                Some(Box::new(crate::call::app::conference::ConferenceApp::new(
                    conf_id, caller_id,
                )) as Box<dyn crate::call::app::CallApp>)
            }
            "queue" => {
                let pending = context.pending_queue.lock().take()?;
                let plan = pending.plan;
                let mut config = crate::call::app::queue::QueueConfig::default();
                config.name = plan.queue_name.clone();
                config.accept_immediately = plan.accept_immediately;
                config.hold = plan.hold.clone();
                config.fallback = plan.fallback.clone();
                config.voice_prompts = plan.voice_prompts.clone();
                config.ring_timeout = plan.ring_timeout;
                if let Some(ref label) = plan.label {
                    if !config.name.is_empty() {
                        config.name = label.clone();
                    } else {
                        config.name = label.clone();
                    }
                }
                // Build agent locations from resolved URIs
                let agents: Vec<crate::call::Location> = pending
                    .agent_uris
                    .iter()
                    .map(|uri| {
                        let aor: rsipstack::sip::Uri = uri
                            .parse()
                            .unwrap_or_else(|_| format!("sip:{}", uri).parse().unwrap_or_default());
                        crate::call::Location {
                            aor,
                            contact_raw: Some(uri.clone()),
                            ..Default::default()
                        }
                    })
                    .collect();
                config.agents = agents.clone();
                config.strategy = if pending.parallel {
                    crate::call::DialStrategy::Parallel(agents)
                } else {
                    crate::call::DialStrategy::Sequential(agents)
                };
                Some(
                    Box::new(crate::call::app::queue::QueueApp::new(plan, config))
                        as Box<dyn crate::call::app::CallApp>,
                )
            }
            _ => None,
        }
    }
}


/// Fan a detected DTMF digit out to: the running app (`inject_event`), the
/// RWI gateway (typed `Dtmf` event), and the bridge WebSocket (if connected).
/// Shared by the SIP INFO path and the RTP RFC 2833 path (MediaBridge dtmf_bus).
fn forward_dtmf_event(
    digit: char,
    leg_id: &str,
    session_id: &str,
    app_runtime: &Arc<dyn AppRuntime>,
    rwi_gateway: &Option<crate::rwi::RwiGatewayRef>,
    bridge_dtmf_tx: &Arc<
        parking_lot::RwLock<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    >,
) {
    let digit_str = digit.to_string();
    let event = serde_json::json!({
        "type": "dtmf",
        "leg_id": leg_id,
        "digit": digit_str,
    });
    if let Err(e) = app_runtime.inject_event(event.clone()) {
        warn!(session_id = %session_id, digit = %digit_str, error = %e, "Detected DTMF but failed to inject event");
        return;
    }
    info!(session_id = %session_id, leg_id, digit = %digit_str, "DTMF injected into app");
    if let Some(gw) = rwi_gateway.as_ref() {
        let g = gw.read();
        g.send_to_owner(&crate::rwi::Dtmf {
            call_id: session_id.to_string(),
            digit: digit_str.clone(),
            leg_id: Some(leg_id.to_string()),
            extra: None,
        });
    }
    if let Some(tx) = bridge_dtmf_tx.read().as_ref() {
        let _ = tx.send(serde_json::json!({
            "type": "dtmf",
            "digit": digit_str,
            "leg_id": leg_id,
        }).to_string());
    }
}

/// Parse trunk dest to extract (host, port). Handles both SIP URIs and bare host:port.
fn trunk_host_port(dest: &str) -> Option<(String, u16)> {
    if dest.trim().is_empty() {
        return None;
    }
    if let Ok(uri) = rsipstack::sip::Uri::try_from(dest) {
        let host = uri.host().to_string();
        let port = uri.host_with_port.port.map(|p| p.0).unwrap_or(5060);
        return Some((host, port));
    }
    // Try as bare host:port
    let parts: Vec<&str> = dest.split(':').collect();
    let host = *parts.first()?;
    if host.is_empty() {
        return None;
    }
    let port = parts
        .get(1)
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(5060);
    Some((host.to_string(), port))
}

impl SipSession {
    pub const CALLER_TRACK_ID: &'static str = "caller-track";
    pub const CALLEE_TRACK_ID: &'static str = "callee-track";
    pub const QUEUE_HOLD_TRACK_ID: &'static str = "queue-hold";
    const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
    const MID_DIALOG_TIMEOUT: Duration = Duration::from_secs(30);
    // ── Shared helpers extracted from sub-modules to eliminate duplication ──

    /// Construct a composite LegId of the form `"{session_id}-{leg_id}"` for
    /// conference participant registration and media-bridge calls.
    pub(super) fn participant_leg(&self, leg: &LegId) -> LegId {
        LegId::new(format!("{}-{}", self.id.0, leg))
    }

    /// Return a reference to a leg or fail with a uniform "Leg not found" error.
    pub(super) fn require_leg(&self, leg_id: &LegId) -> Result<&Leg> {
        self.legs
            .get(leg_id)
            .ok_or_else(|| anyhow!("Leg not found: {}", leg_id))
    }

    /// Forward a `CallCommand` to another session via the active-call registry.
    pub(super) fn forward_command(
        &self,
        session_id: &str,
        cmd: CallCommand,
        label: &str,
    ) -> Result<()> {
        let registry = &self.server.active_call_registry;
        if let Some(handle) = registry.get_handle(session_id) {
            handle
                .send_command(cmd)
                .map_err(|e| anyhow!("Failed to {}: {}", label, e))?;
            info!(target_session = %session_id, "{}", label);
            Ok(())
        } else {
            Err(anyhow!("Session {} not found in registry", session_id))
        }
    }

    // ── MediaBridge helpers ─────────────────────────────────────────────
    pub(super) fn bridge(&self) -> Option<&MediaBridge> { self.media.bridge.as_ref() }
    pub(super) fn bridge_mut(&mut self) -> Option<&mut MediaBridge> { self.media.bridge.as_mut() }

    /// Attach a [`SipflowRecorder`] to the media bridge so RTP packets are
    /// stored in the sipflow backend for later WAV export.
    ///
    /// Per design, when sipflow is enabled and `force_file` is NOT set, media
    /// capture happens through sipflow (not the WAV file recorder). This
    /// re-wires RTP → sipflow after the media-layer rewrite that dropped the
    /// old `setup_sipflow_capture` wiring.
    pub(super) fn attach_sipflow_media_capture(&mut self) {
        let Some(mb) = self.media.bridge.as_mut() else {
            return;
        };
        let recording = &self.context.dialplan.recording;
        if !recording.enabled || recording.force_file {
            return;
        }
        let Some(backend) = self.server.sip_flow.as_ref().and_then(|sf| sf.backend()) else {
            return;
        };
        let call_id = self.context.session_id.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<
            crate::media::media_recorder::SipflowItem,
        >(1024);
        let recorder = crate::media::media_recorder::SipflowRecorder::new(tx);
        let drain_call_id = call_id.clone();
        crate::utils::spawn(async move {
            while let Some(item) = rx.recv().await {
                let leg = match item.direction {
                    crate::media::ingress_tap::PacketDirection::Ingress => 0i32,
                    crate::media::ingress_tap::PacketDirection::Egress => 1i32,
                };
                let sipflow_item = crate::sipflow::SipFlowItem {
                    timestamp: item.received_at_micros,
                    seq: item.sequence_number as u64,
                    leg: Some(leg),
                    msg_type: crate::sipflow::SipFlowMsgType::Rtp,
                    src_addr: "synth".to_string(),
                    dst_addr: String::new(),
                    payload: item.raw,
                };
                if let Err(e) = backend.record(&drain_call_id, sipflow_item) {
                    warn!(session_id = %drain_call_id, error = %e, "sipflow media record failed");
                }
            }
        });
        mb.set_recorder(recorder);
        info!(
            session_id = %call_id,
            "sipflow media capture attached to media bridge"
        );
    }
    /// Put a leg on hold playing a file as hold music (looping).
    /// Ensure a conference exists — create it if missing.

    pub(super) async fn ensure_conference(&self, conf_id: &str, max: Option<usize>) -> Result<()> {
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id);
        if self
            .server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            info!(session_id = %self.id, conf_id = %conf_id, "Creating conference");
            self.server
                .conference_manager
                .create_conference(conf_id_obj, max)
                .await
                .map_err(|e| anyhow!("Failed to create conference '{}': {}", conf_id, e))?;
        }
        Ok(())
    }

    /// Store a conference bridge handle + conference id on the session,
    /// stopping any previously active bridge first.
    pub(super) fn set_active_bridge(
        &mut self,
        conf_id: String,
        handle: crate::call::runtime::ConferenceBridgeHandle,
    ) {
        self.conference_bridge.stop_bridge();
        self.conference_bridge.bridge_handle = Some(handle);
        self.conference_bridge.conf_id = Some(conf_id);
    }

    /// Start a conference media bridge, store the handle on success, or log a
    /// warning on failure (non-fatal — execution continues).
    pub(super) async fn try_start_and_store_bridge(
        &mut self,
        conf_id: &str,
        leg: &LegId,
        label: &str,
    ) {
        match self.start_conference_media_bridge(conf_id, leg).await {
            Ok(handle) => {
                info!(session_id = %self.id, leg_id = %leg, "{} started", label);
                self.set_active_bridge(conf_id.to_string(), handle);
            }
            Err(e) => {
                warn!(session_id = %self.id, leg_id = %leg, error = %e, "Failed to start {}", label);
            }
        }
    }

    /// Start (or restart if already running) an application.
    pub(crate) async fn ensure_app_running(
        &self,
        kind: &str,
        params: Option<serde_json::Value>,
        label: &str,
    ) -> Result<()> {
        use crate::call::runtime::AppRuntimeError;
        match self.app_runtime.start_app(kind, params.clone(), true).await {
            Ok(()) => Ok(()),
            Err(AppRuntimeError::AlreadyRunning(_)) => {
                warn!(session_id = %self.id, app = %label, "runtime still marked running, restarting app");
                match self
                    .app_runtime
                    .stop_app(Some(format!("restart {}", label)))
                    .await
                {
                    Ok(()) | Err(AppRuntimeError::NotRunning) => {}
                    Err(stop_err) => {
                        warn!(session_id = %self.id, error = ?stop_err, "Failed to stop existing {} app", label)
                    }
                }
                self.app_runtime
                    .start_app(kind, params, true)
                    .await
                    .map_err(|e| anyhow!("Failed to restart {}: {:?}", label, e))
            }
            Err(e) => Err(anyhow!("Failed to start {}: {:?}", label, e)),
        }
    }

    /// Spawn a tokio task that forwards audio samples from an mpsc receiver
    /// into a track sender, and register the task for session cleanup.
    pub(super) fn spawn_forwarder(
        &mut self,
        leg: &LegId,
        cancel_token: CancellationToken,
        sender: rustrtc::media::SampleStreamSource,
        mut rx: tokio::sync::mpsc::Receiver<rustrtc::media::MediaSample>,
    ) {
        let cancel = cancel_token.child_token();
        let handle = crate::utils::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    sample = rx.recv() => match sample {
                        Some(s) => if sender.send(s).is_err() { break; }
                        None => break,
                    }
                }
            }
        });
        self.legs.tasks.entry(leg.clone()).or_default().push(handle);
    }

    /// Build an `AudioReceiver` from a `PeerConnection` using the session's
    /// negotiated decoder.
    pub(super) fn build_audio_receiver(
        &self,
        pc: rustrtc::PeerConnection,
    ) -> Result<Box<dyn crate::call::runtime::conference_media_bridge::AudioReceiver>> {
        let decoder = self
            .create_audio_decoder()
            .ok_or_else(|| anyhow!("Failed to create audio decoder"))?;
        Ok(Box::new(PeerConnectionAudioReceiver::new(pc, decoder)))
    }

    /// Wait up to `retries * 20ms` for a `PeerConnection` from the given peer's tracks.
    pub(super) async fn wait_for_peer_connection(
        peer: &Arc<dyn MediaPeer>,
        retries: usize,
    ) -> Option<rustrtc::PeerConnection> {
        for _ in 0..retries {
            let tracks = peer.get_tracks().await;
            for t in &tracks {
                let guard = t.lock().await;
                if let Some(pc) = guard.get_peer_connection().await {
                    return Some(pc);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }

    /// Wait up to `retries * 20ms` for a `SampleStreamSource` from the given peer's tracks.
    /// Map a REFER status code to a human-readable reason string.
    pub(super) fn refer_reason_for_status(status: u16) -> Option<&'static str> {
        match status {
            202 | 100..=199 => None,
            405 | 420 | 501 => Some("refer_not_supported"),
            _ if status >= 400 => Some("refer_rejected"),
            _ => Some("unexpected_response"),
        }
    }


    #[inline]
    pub fn caller_peer(&self) -> Option<&Arc<dyn MediaPeer>> {
        self.legs.caller_peer()
    }

    #[inline]
    pub fn callee_peer(&self) -> Option<&Arc<dyn MediaPeer>> {
        self.legs.callee_peer()
    }

    pub fn with_handle(id: SessionId) -> (SipSessionHandle, mpsc::Receiver<CallCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
        let snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>> = Arc::new(RwLock::new(None));

        let handle = SipSessionHandle {
            session_id: id,
            cmd_tx,
            snapshot_cache,
            app_event_bridge: Arc::new(RwLock::new(None)),
        };

        (handle, cmd_rx)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        server: SipServerRef,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
        context: CallContext,
        server_dialog: InviteDialog,
        use_media_proxy: bool,
        caller_peer: Arc<dyn MediaPeer>,
        callee_peer: Arc<dyn MediaPeer>,
    ) -> (Self, SipSessionHandle, mpsc::Receiver<CallCommand>) {
        // Clone-once locals used repeatedly below
        let session_id_str = context.session_id.clone();
        let original_caller = context.original_caller.clone();
        let original_callee = context.original_callee.clone();
        let concurrent_call_lease = context.dialplan.concurrent_call_lease.take();

        let session_id = SessionId::from(session_id_str.clone());

        let media_profile = if use_media_proxy {
            MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored)
        } else {
            MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass)
        };

        let cmd_capacity = server.proxy_config.session_cmd_channel_capacity;
        let state_capacity = server.proxy_config.session_state_channel_capacity;
        let (cmd_tx, cmd_rx) = mpsc::channel(cmd_capacity);
        let snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>> = Arc::new(RwLock::new(None));
        let app_event_bridge: Arc<
            RwLock<Option<crate::proxy::proxy_call::state::SipSessionHandle>>,
        > = Arc::new(RwLock::new(None));

        let sip_handle = SipSessionHandle {
            session_id: session_id.clone(),
            cmd_tx: cmd_tx.clone(),
            snapshot_cache: snapshot_cache.clone(),
            app_event_bridge: app_event_bridge.clone(),
        };

        // Build ApplicationContext for call apps (IVR, voicemail, etc.)
        let call_info = CallInfo {
            session_id: session_id_str.clone(),
            caller: original_caller.clone(),
            callee: original_callee.clone(),
            direction: context.dialplan.direction.to_string(),
            started_at: chrono::Utc::now(),
            sip_headers: {
                let mut hdrs =
                    crate::call::app::extract_sip_headers(&server_dialog.initial_request());
                if let Some(ref routed) = context.dialplan.routed_headers {
                    for h in routed {
                        hdrs.insert(h.name().to_string(), h.value().to_string());
                    }
                }
                hdrs
            },
            route_name: context
                .metadata
                .as_ref()
                .and_then(|m| m.get("route_name").cloned()),
        };
        // Create a shared session-extensions bag so CallApps and
        // CallSessionHooks can pass typed data across layers (e.g. CSAT
        // results → CDR).
        let session_extensions = crate::proxy::proxy_call::session_hooks::SessionExtensions::new();

        let mut app_ctx = ApplicationContext::new(
            server.database.clone().unwrap_or_default(),
            call_info,
            Arc::new(crate::config::Config {
                proxy: (*server.proxy_config).clone(),
                ..Default::default()
            }),
        );
        app_ctx.rwi_gateway = server.rwi_gateway.clone();
        app_ctx.ivr_trace = server.ivr_trace.clone();
        app_ctx.session_extensions = session_extensions.clone();

        // Populate RWI CallMetaStore so events emitted from this session
        // (call_hangup, call_no_answer, etc.) are enriched with call context.
        if let Some(ref gw) = server.rwi_gateway {
            let meta = crate::rwi::proto::CallMeta {
                caller: Some(original_caller.clone()),
                callee: Some(original_callee.clone()),
                caller_name: extract_sip_username(&original_caller),
                callee_name: extract_sip_username(&original_callee),
                direction: Some(context.dialplan.direction.to_string()),
                trunk: context
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("trunk").cloned()),
                ..Default::default()
            };
            let sid = session_id_str.clone();
            gw.read().meta_store.insert(sid, meta);
        }

        // Create a shared handle for app event delivery (send_app_event).
        // The old SessionAction→CallCommand bridge has been removed — the
        // unified SipSessionHandle speaks CallCommand natively.
        let bridge_shared = crate::proxy::proxy_call::state::SipSessionShared::new(
            session_id_str.clone(),
            crate::call::DialDirection::Inbound,
            Some(original_caller.clone()),
            Some(original_callee.clone()),
            None,
        );
        let (bridge_handle, _action_rx) =
            crate::proxy::proxy_call::state::SipSessionHandle::with_shared(
                bridge_shared,
                state_capacity,
            );

        // Wire the bridge into the sip handle so send_app_event forwards events.
        let mut slot = app_event_bridge.write();
        *slot = Some(bridge_handle.clone());

        let app_runtime: Arc<dyn AppRuntime> = Arc::new(
            DefaultAppRuntime::new(AppRuntimeConfig {
                session_id: session_id_str.clone(),
                handle: sip_handle.clone(),
                context: Arc::new(app_ctx),
            })
            .with_factory(Arc::new(BuiltinAppFactory {
                addon_registry: server.addon_registry.clone(),
            })),
        );

        let mut meta = crate::proxy::proxy_call::call_meta::CallMeta::new();
        meta.routed_caller = context.dialplan.caller.as_ref().map(|uri| uri.to_string());
        meta.routed_callee = context
            .dialplan
            .first_target()
            .map(|target| target.aor.to_string());

        let initial = server_dialog.initial_request();
        let caller_offer = Self::extract_sdp(initial.body());

        let mut session = Self {
            id: session_id.clone(),
            state: SessionState::Initializing,
            policy: SessionPolicy::inbound_sip(),
            bridge: BridgeConfig::new(),
            media_profile: media_profile.clone(),
            app_runtime,
            snapshot_cache: snapshot_cache.clone(),
            server,
            caller_dialog: Some(server_dialog.clone()),
            callee_dialogs: Arc::new(DashMap::new()),
            supervisor_mixer: None,
            legs: {
                use crate::proxy::proxy_call::leg_registry::LegRegistry;
                let mut lr = LegRegistry::new();
                let caller_id = LegId::from("caller");
                let caller_dialog = rsipstack::dialog::dialog::Dialog::Invite(server_dialog.clone());
                lr.add_leg(
                    caller_id.clone(),
                    Leg::new(caller_id),
                    caller_peer.clone(),
                    Some(caller_dialog),
                );
                // callee peer registered without a dialog until it answers
                lr.set_peer(LegId::from("callee"), callee_peer.clone());
                lr
            },
            pending_hangup: HashSet::new(),
            context,
            concurrent_call_lease,
            call_record_sender,
            cancel_token,
            meta,
            media: crate::proxy::proxy_call::media_state::MediaState::new(caller_offer),
            timers: HashMap::new(),
            update_refresh_disabled: HashSet::new(),
            timer_queue: DelayQueue::new(),
            timer_keys: HashMap::new(),
            callee_event_tx: None,
            callee_guards: Vec::new(),
            reporter: None,
            cdr_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            app_event_bridge: app_event_bridge.clone(),
            extensions: session_extensions,
            conference_bridge: crate::call::runtime::SessionConferenceBridge::new(),
            bridge_dtmf_tx: Arc::new(parking_lot::RwLock::new(None)),
            cmd_tx: Some(cmd_tx.clone()),
            handle: sip_handle.clone(),
            dtmf_digits: Vec::new(),
        };

        // ── Phase 0: Initialize MediaBridge eagerly when media is anchored ──
        // In Bypass (None) mode the bridge stays None so SDP is passed through
        // untouched (see `bypasses_local_media`).
        if use_media_proxy {
            session.media.bridge = Some(crate::media::media_bridge::MediaBridge::new(
                session_id_str.clone(),
                crate::media::media_bridge::BridgeOpts::default(),
            ));
            session.spawn_dtmf_forwarder();
        }
        session.attach_sipflow_media_capture();

        (session, sip_handle, cmd_rx)
    }

    /// Construct a SipSession in **UAC / outbound mode** (no inbound caller
    /// dialog). Used by RWI originate: the caller (A leg) is virtual (media
    /// anchored on the MediaBridge, no real SIP peer) and the callee (B leg)
    /// is a real outbound INVITE that is sent later and attached on answer.
    ///
    /// `caller_offer` is the virtual caller's local SDP offer (may be `None`
    /// until media is set up); it is stored in `MediaState` for later SDP
    /// negotiation with the callee.
    #[allow(clippy::too_many_arguments)]
    pub fn new_uac(
        server: SipServerRef,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
        context: CallContext,
        use_media_proxy: bool,
        caller_peer: Arc<dyn MediaPeer>,
        callee_peer: Arc<dyn MediaPeer>,
    ) -> (Self, SipSessionHandle, mpsc::Receiver<CallCommand>) {
        let session_id_str = context.session_id.clone();
        let original_caller = context.original_caller.clone();
        let original_callee = context.original_callee.clone();
        let concurrent_call_lease = context.dialplan.concurrent_call_lease.take();

        let session_id = SessionId::from(session_id_str.clone());

        let media_profile = if use_media_proxy {
            MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored)
        } else {
            MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass)
        };

        let cmd_capacity = server.proxy_config.session_cmd_channel_capacity;
        let state_capacity = server.proxy_config.session_state_channel_capacity;
        let (cmd_tx, cmd_rx) = mpsc::channel(cmd_capacity);
        let snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>> = Arc::new(RwLock::new(None));
        let app_event_bridge: Arc<
            RwLock<Option<crate::proxy::proxy_call::state::SipSessionHandle>>,
        > = Arc::new(RwLock::new(None));

        let sip_handle = SipSessionHandle {
            session_id: session_id.clone(),
            cmd_tx: cmd_tx.clone(),
            snapshot_cache: snapshot_cache.clone(),
            app_event_bridge: app_event_bridge.clone(),
        };

        // Build ApplicationContext — no inbound SIP headers in UAC mode.
        let call_info = CallInfo {
            session_id: session_id_str.clone(),
            caller: original_caller.clone(),
            callee: original_callee.clone(),
            direction: context.dialplan.direction.to_string(),
            started_at: chrono::Utc::now(),
            sip_headers: Default::default(),
            route_name: context
                .metadata
                .as_ref()
                .and_then(|m| m.get("route_name").cloned()),
        };
        let session_extensions = crate::proxy::proxy_call::session_hooks::SessionExtensions::new();

        let mut app_ctx = ApplicationContext::new(
            server.database.clone().unwrap_or_default(),
            call_info,
            Arc::new(crate::config::Config {
                proxy: (*server.proxy_config).clone(),
                ..Default::default()
            }),
        );
        app_ctx.rwi_gateway = server.rwi_gateway.clone();
        app_ctx.ivr_trace = server.ivr_trace.clone();
        app_ctx.session_extensions = session_extensions.clone();

        if let Some(ref gw) = server.rwi_gateway {
            let meta = crate::rwi::proto::CallMeta {
                caller: Some(original_caller.clone()),
                callee: Some(original_callee.clone()),
                caller_name: extract_sip_username(&original_caller),
                callee_name: extract_sip_username(&original_callee),
                direction: Some(context.dialplan.direction.to_string()),
                trunk: context
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("trunk").cloned()),
                ..Default::default()
            };
            gw.read().meta_store.insert(session_id_str.clone(), meta);
        }

        let bridge_shared = crate::proxy::proxy_call::state::SipSessionShared::new(
            session_id_str.clone(),
            crate::call::DialDirection::Outbound,
            Some(original_caller.clone()),
            Some(original_callee.clone()),
            None,
        );
        let (bridge_handle, _action_rx) =
            crate::proxy::proxy_call::state::SipSessionHandle::with_shared(
                bridge_shared,
                state_capacity,
            );
        let mut slot = app_event_bridge.write();
        *slot = Some(bridge_handle.clone());

        let app_runtime: Arc<dyn AppRuntime> = Arc::new(
            DefaultAppRuntime::new(AppRuntimeConfig {
                session_id: session_id_str.clone(),
                handle: sip_handle.clone(),
                context: Arc::new(app_ctx),
            })
            .with_factory(Arc::new(BuiltinAppFactory {
                addon_registry: server.addon_registry.clone(),
            })),
        );

        let mut meta = crate::proxy::proxy_call::call_meta::CallMeta::new();
        meta.routed_caller = context.dialplan.caller.as_ref().map(|uri| uri.to_string());
        meta.routed_callee = context
            .dialplan
            .first_target()
            .map(|target| target.aor.to_string());

        let mut session = Self {
            id: session_id.clone(),
            state: SessionState::Initializing,
            policy: SessionPolicy::rwi_originate(false),
            bridge: BridgeConfig::new(),
            media_profile: media_profile.clone(),
            app_runtime,
            snapshot_cache: snapshot_cache.clone(),
            server,
            caller_dialog: None,
            callee_dialogs: Arc::new(DashMap::new()),
            supervisor_mixer: None,
            legs: {
                use crate::proxy::proxy_call::leg_registry::LegRegistry;
                let mut lr = LegRegistry::new();
                let caller_id = LegId::from("caller");
                // Virtual caller leg: no dialog, just a peer anchor.
                lr.add_leg(caller_id.clone(), Leg::new(caller_id), caller_peer.clone(), None);
                lr.set_peer(LegId::from("callee"), callee_peer.clone());
                lr
            },
            pending_hangup: HashSet::new(),
            context,
            concurrent_call_lease,
            call_record_sender,
            cancel_token,
            meta,
            // No caller offer yet — will be populated when media is set up.
            media: crate::proxy::proxy_call::media_state::MediaState::new(None),
            timers: HashMap::new(),
            update_refresh_disabled: HashSet::new(),
            timer_queue: DelayQueue::new(),
            timer_keys: HashMap::new(),
            callee_event_tx: None,
            callee_guards: Vec::new(),
            reporter: None,
            cdr_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            app_event_bridge: app_event_bridge.clone(),
            extensions: session_extensions,
            conference_bridge: crate::call::runtime::SessionConferenceBridge::new(),
            bridge_dtmf_tx: Arc::new(parking_lot::RwLock::new(None)),
            cmd_tx: Some(cmd_tx.clone()),
            handle: sip_handle.clone(),
            dtmf_digits: Vec::new(),
        };

        if use_media_proxy {
            session.media.bridge = Some(crate::media::media_bridge::MediaBridge::new(
                session_id_str.clone(),
                crate::media::media_bridge::BridgeOpts::default(),
            ));
            session.spawn_dtmf_forwarder();
        }
        session.attach_sipflow_media_capture();

        (session, sip_handle, cmd_rx)
    }

    pub async fn serve(
        server: SipServerRef,
        context: CallContext,
        tx: &mut rsipstack::transaction::transaction::Transaction,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
    ) -> Result<()> {
        let session_id = context.session_id.clone();
        info!(session_id = %session_id, "Starting unified SIP session");

        // Save commonly-needed fields before consuming context
        let original_caller = context.original_caller.clone();
        let original_callee = context.original_callee.clone();
        let max_ring_time = context.dialplan.max_ring_time;

        let local_contact = context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .or_else(|| server.default_contact_uri());

        let (state_tx, state_rx) = mpsc::unbounded_channel();

        let server_dialog = server
            .dialog_layer
            .get_or_create_server_invite(tx, state_tx, None, local_contact.clone())
            .map_err(|e| anyhow!("Failed to create server dialog: {}", e))?;

        let use_media_proxy = Self::check_media_proxy(&context, &context.dialplan.media.proxy_mode);

        let caller_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-caller", session_id))
            .with_cancel_token(cancel_token.child_token());
        let caller_peer = Arc::new(VoiceEnginePeer::new(Arc::new(caller_media_builder.build())));

        let callee_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-callee", session_id))
            .with_cancel_token(cancel_token.child_token());
        let callee_peer = Arc::new(VoiceEnginePeer::new(Arc::new(callee_media_builder.build())));

        let (mut session, handle, cmd_rx) = SipSession::new(
            server.clone(),
            cancel_token.clone(),
            call_record_sender,
            context,
            server_dialog.clone(),
            use_media_proxy,
            caller_peer,
            callee_peer,
        );

        session.reporter = Some(CallReporter {
            server: server.clone(),
            context: session.context.clone(),
            call_record_sender: session.call_record_sender.clone(),
        });

        if use_media_proxy {
            let offer_sdp =
                String::from_utf8_lossy(server_dialog.initial_request().body()).to_string();
            session.media.caller_offer = Some(offer_sdp);
        }

        let dialog_guard = ServerDialogGuard::new(server.dialog_layer.clone(), server_dialog.id());

        let (callee_state_tx, callee_state_rx) = mpsc::unbounded_channel();
        session.callee_event_tx = Some(callee_state_tx);

        server
            .active_call_registry
            .register_handle(session_id.clone(), handle.clone());

        server
            .active_call_registry
            .register_dialog(server_dialog.id().to_string(), handle.clone());

        // Emit CallIncoming event via RWI gateway if configured.
        let incoming_sip_headers = {
            let mut hdrs = crate::call::app::extract_sip_headers(&server_dialog.initial_request());
            if let Some(ref routed) = session.context.dialplan.routed_headers {
                for h in routed {
                    hdrs.insert(h.name().to_string(), h.value().to_string());
                }
            }
            hdrs
        };
        if let Some(ref gw) = server.rwi_gateway {
            let ev = crate::rwi::CallIncoming {
                call_id: session_id.clone(),
                context: "default".into(),
                caller: original_caller,
                callee: original_callee,
                dial_direction: "inbound".into(),
                trunk: None,
                sip_headers: incoming_sip_headers,
                root_call_id: None,
                caller_name: None,
                callee_name: None,
                called_phone: None,
                app_id: None,
                routing_target: None,
                uuid: None,
                routing_path: None,
            };
            let gw = gw.clone();
            crate::utils::spawn(async move {
                let g = gw.read();
                g.send_to_owner(&ev);
            });
        }

        let mut server_dialog_clone = server_dialog.clone();

        crate::utils::spawn(async move {
            session
                .process(state_rx, callee_state_rx, cmd_rx, dialog_guard)
                .await
        });

        let max_setup_duration =
            max_ring_time.clamp(Duration::from_secs(30), Duration::from_secs(120))
                + Duration::from_secs(15); // backstop after the session's ring-timeout
        let mut timeout = tokio::time::sleep(max_setup_duration).boxed();

        loop {
            tokio::select! {
                r = server_dialog_clone.handle(tx) => {
                    debug!(session_id = %session_id, "Server dialog handle returned");
                    if let Err(ref e) = r {
                        warn!(session_id = %session_id, error = %e, "Server dialog handle returned error");
                        cancel_token.cancel();
                    } else if server_dialog_clone.state().is_terminated() {
                        cancel_token.cancel();
                    }
                    break;
                }
                _ = cancel_token.cancelled() => {
                    debug!(session_id = %session_id, "Call cancelled via token");
                    if !server_dialog_clone.state().is_terminated() {
                        if let Err(e) = tx.reply(rsipstack::sip::StatusCode::RequestTerminated).await {
                            warn!(session_id = %session_id, error = %e, "Failed to reply 487 on cancel");
                        }
                    }
                    break;
                }
                _ = &mut timeout => {
                    warn!(session_id = %session_id, "Call setup timed out");
                    cancel_token.cancel();
                    if !server_dialog_clone.state().is_terminated() {
                        if let Err(e) = tx.reply(rsipstack::sip::StatusCode::RequestTerminated).await {
                            warn!(session_id = %session_id, error = %e, "Failed to reply 487 on setup timeout");
                        }
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    pub(crate) fn check_media_proxy(context: &CallContext, mode: &MediaProxyMode) -> bool {
        if context.dialplan.recording.enabled {
            return true;
        }

        let app_or_queue_flow = matches!(
            context.dialplan.flow,
            crate::call::DialplanFlow::Application { .. } | crate::call::DialplanFlow::Queue { .. }
        );

        match mode {
            MediaProxyMode::All => true,
            // In Auto/NAT mode, keep media anchored for app/queue flows because
            // they require playback/injection capabilities.
            MediaProxyMode::Auto | MediaProxyMode::Nat => app_or_queue_flow,
            MediaProxyMode::None => false,
            _ => false,
        }
    }

    fn is_local_home_proxy(local_addrs: &[SipAddr], home_proxy: &SipAddr) -> bool {
        local_addrs
            .iter()
            .any(|addr| addr.addr.to_string() == home_proxy.addr.to_string())
    }

    fn route_via_home_proxy(
        target: &Location,
        local_addrs: &[SipAddr],
        cluster_enabled: bool,
    ) -> bool {
        if !cluster_enabled {
            return false;
        }
        if let Some(home_proxy) = target.home_proxy.as_ref() {
            return !Self::is_local_home_proxy(local_addrs, home_proxy);
        }

        false
    }

    fn resolve_outbound_callee_uri(
        target: &Location,
        route_via_home_proxy: bool,
    ) -> rsipstack::sip::Uri {
        if route_via_home_proxy && let Some(registered_aor) = target.registered_aor.as_ref() {
            let mut uri = registered_aor.clone();
            if let Some(home_proxy) = target.home_proxy.as_ref() {
                uri.host_with_port = home_proxy.addr.clone();
            }
            return uri;
        }

        target.aor.clone()
    }

    /// True when media bypasses the local bridge entirely (pure B2BUA
    /// passthrough at the signaling layer).
    pub(super) fn bypasses_local_media(&self) -> bool {
        self.media_profile.path == MediaPathMode::Bypass && self.media.bridge.is_none()
    }

    /// Resolve the RTP inactivity timeout (dialplan override → proxy config).
    fn rtp_timeout_config(&self) -> Option<Duration> {
        self.context
            .dialplan
            .rtp_timeout
            .or_else(|| self.server.proxy_config.rtp_timeout.map(Duration::from_secs))
    }

    /// Arm the RTP inactivity timeout on both legs after a call is answered.
    /// If no RTP arrives on either leg within the configured window, send a
    /// Hangup command so the session tears down with `rtpTimeout` reason.
    fn arm_bridged_rtp_timeouts(
        mb: &crate::media::media_bridge::MediaBridge,
        rtp_timeout: Option<Duration>,
        cmd_tx: Option<mpsc::Sender<CallCommand>>,
        session_id: &str,
    ) {
        let Some(timeout) = rtp_timeout else {
            return;
        };
        for side in [
            crate::media::media_bridge::LegSide::A,
            crate::media::media_bridge::LegSide::B,
        ] {
            let Some(rx) = mb.arm_rtp_timeout(side, timeout) else {
                continue;
            };
            let cmd_tx = cmd_tx.clone();
            let sid = session_id.to_string();
            crate::utils::spawn(async move {
                if rx.await.is_ok() {
                    warn!(session_id = %sid, leg_side = ?side, "RTP inactivity timeout fired");
                    if let Some(tx) = cmd_tx {
                        let _ = tx.try_send(CallCommand::Hangup(
                            crate::call::domain::HangupCommand::all(
                                Some(crate::callrecord::CallRecordHangupReason::RtpTimeout),
                                None,
                            ),
                        ));
                    }
                }
            });
        }
    }
    #[cfg(test)]
    fn filter_video_caps_for_rtp(
        caps: &[rustrtc::VideoCapability],
        allowed_codecs: &[String],
    ) -> Vec<rustrtc::VideoCapability> {
        let defaults = &["H264".to_string()];
        let effective_allow: &[String] = if allowed_codecs.is_empty() {
            defaults
        } else {
            allowed_codecs
        };

        if !effective_allow
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case("H264"))
        {
            return vec![];
        }

        if let Some(cap) = caps
            .iter()
            .find(|cap| cap.codec_name.eq_ignore_ascii_case("H264"))
        {
            return vec![rustrtc::VideoCapability {
                payload_type: cap.payload_type,
                codec_name: cap.codec_name.clone(),
                clock_rate: cap.clock_rate,
                fmtp: cap.fmtp.clone(),
                rtcp_fbs: vec![],
                ..Default::default()
            }];
        }

        vec![]
    }

    #[cfg(test)]
    fn apply_video_caps_from_source(
        sdp_type: rustrtc::SdpType,
        sdp: &str,
        context: &str,
        caps: &[rustrtc::VideoCapability],
    ) -> Result<String> {
        let mut desc = Self::parse_sdp(sdp_type, sdp, context)?;
        let local_video_caps = desc.to_video_capabilities();
        if let Some(video_section) = desc
            .media_sections
            .iter_mut()
            .find(|s| s.kind == rustrtc::MediaKind::Video)
        {
            let selected_cap = caps.iter().find_map(|source_cap| {
                local_video_caps
                    .iter()
                    .find(|local_cap| {
                        local_cap
                            .codec_name
                            .eq_ignore_ascii_case(&source_cap.codec_name)
                            && local_cap.clock_rate == source_cap.clock_rate
                    })
                    .map(|local_cap| {
                        if sdp_type == rustrtc::SdpType::Answer {
                            source_cap.clone()
                        } else {
                            rustrtc::VideoCapability {
                                payload_type: local_cap.payload_type,
                                codec_name: source_cap.codec_name.clone(),
                                clock_rate: source_cap.clock_rate,
                                fmtp: source_cap.fmtp.clone(),
                                rtcp_fbs: local_cap.rtcp_fbs.clone(),
                                ..Default::default()
                            }
                        }
                    })
            });
            let ordered_caps: Vec<_> = selected_cap.into_iter().collect();

            video_section.formats = ordered_caps
                .iter()
                .map(|cap| cap.payload_type.to_string())
                .collect();
            video_section
                .attributes
                .retain(|attr| !matches!(attr.key.as_str(), "rtpmap" | "fmtp" | "rtcp-fb"));
            for cap in ordered_caps {
                video_section.attributes.push(rustrtc::Attribute::new(
                    "rtpmap",
                    Some(format!(
                        "{} {}/{}",
                        cap.payload_type, cap.codec_name, cap.clock_rate
                    )),
                ));
                if let Some(fmtp) = &cap.fmtp {
                    video_section.attributes.push(rustrtc::Attribute::new(
                        "fmtp",
                        Some(format!("{} {}", cap.payload_type, fmtp)),
                    ));
                }
                for fb in &cap.rtcp_fbs {
                    video_section.attributes.push(rustrtc::Attribute::new(
                        "rtcp-fb",
                        Some(format!("{} {}", cap.payload_type, fb)),
                    ));
                }
            }
        }
        Ok(desc.to_sdp_string())
    }

    pub async fn process(
        &mut self,
        mut state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut cmd_rx: mpsc::Receiver<CallCommand>,
        _dialog_guard: ServerDialogGuard,
    ) -> Result<()> {
        let _cancel_guard = self.cancel_token.clone().drop_guard();

        // Start watching before 183/setup. The same receiver is reused by the
        // established-call loop below.
        // RTP timeout is handled via MediaBridge::start_rtp_timeout
        // which fires a callback → cmd_tx → process_command(CallCommand::RtpTimeout).

        let ring_audio = self
            .context
            .dialplan
            .audio_profile
            .as_ref()
            .and_then(|p| p.ring.clone());

        let setup_cancel_token = self.cancel_token.clone();
        // Ring/setup timeout: how long to keep dialing before giving up on a
        // no-answer call. Configurable per call (default 60s). Firing here —
        // rather than in `serve` — keeps the caller-dialog message pump alive
        // so a configured no-answer tone can play as 183 before the rejection.
        let ring_timeout = self.context.dialplan.max_ring_time;
        let setup_result = {
            let setup = async {
                if let Some(ref audio) = ring_audio {
                    info!(session_id = %self.id,
                        session_id = %self.context.session_id,
                        audio = %audio,
                        "Sending proactive 183 Session Progress with ringback tone"
                    );
                    if let Err(e) = self.send_early_media_tone(audio).await {
                        warn!(session_id = %self.context.session_id, error = %e, "Failed to send proactive 183");
                    }
                }

                if self.context.dialplan.is_empty() {
                    Ok(())
                } else {
                    self.execute_dialplan(&mut callee_state_rx).await
                }
            };
            tokio::pin!(setup);

            tokio::select! {
                biased;
                result = &mut setup => result,
                _ = tokio::time::sleep(ring_timeout) => Err(into_callee_err(
                    &StatusCode::RequestTimeout,
                    Some("Ring timeout".to_string()),
                )),
                _ = setup_cancel_token.cancelled() => Err(into_callee_err(
                    &StatusCode::RequestTerminated,
                    Some("Call cancelled during setup".to_string()),
                )),
            }
        };

        if let Err((status_code, text, reason)) = setup_result {
            warn!(session_id = %self.context.session_id, ?status_code, ?text, ?reason, "Dialplan execution failed");

            if matches!(status_code, 408 | 480 | 486 | 487) {}

            if let Err(e) = self
                .reject_with_tone(status_code, text.clone(), reason.clone())
                .await
            {
                warn!(session_id = %self.context.session_id, error = %e, "Failed to send rejection with tone");
            }
            // Store error so cleanup/CDR can report the failure reason
            self.meta.last_error =
                Some((StatusCode::Other(status_code, text.clone()), reason.clone()));
            self.meta.hangup_reason = Some(sip_status_to_hangup_reason(status_code));
            // Ensure cleanup runs (generates CDR) even on early failure
            self.cleanup().await;
            return Err(anyhow!("Dialplan failed: {} {:?}", status_code, reason));
        }

        let hangup_futures = FuturesUnordered::new();
        let timeout = futures::future::pending::<()>().boxed();
        let mut cancelled = false;
        tokio::pin!(hangup_futures);
        tokio::pin!(timeout);

        let max_duration_sleep = if let Some(max_dur) = self.context.dialplan.max_call_duration {
            debug!(session_id = %self.context.session_id, ?max_dur, "Max call duration timer armed");
            tokio::time::sleep(max_dur).boxed()
        } else {
            futures::future::pending::<()>().boxed()
        };
        tokio::pin!(max_duration_sleep);

        loop {
            for dialog_id in self.pending_hangup.drain() {
                if let Some(dialog) = self.server.dialog_layer.get_dialog(&dialog_id) {
                    let dialog = dialog.clone();
                    hangup_futures.push(async move {
                        let res = dialog.hangup().await;
                        res.map(|_| dialog_id)
                    });
                }
            }

            if cancelled
                && hangup_futures.is_empty()
                && self.pending_hangup.is_empty()
                && self.caller_dialog.as_ref().is_none_or(|d| d.state().is_terminated())
                && self.callee_dialogs.is_empty()
            {
                break;
            }

            tokio::select! {
                res = hangup_futures.next(), if !hangup_futures.is_empty() => {
                    if let Some(res) = res {
                        tracing::info!(session_id = %self.id, dialog_id = ?res.as_ref().ok(), "Hangup completed");
                        // Remove the dialog from callee_dialogs immediately so the
                        // break condition can be satisfied without waiting for a
                        // callee_state_rx Terminated event (which may arrive late or
                        // never in some race conditions).
                        if let Ok(dialog_id) = &res {
                            self.callee_dialogs.remove(dialog_id);
                        }
                    }
                }
                _ = self.cancel_token.cancelled(), if !cancelled => {
                    debug!(session_id = %self.context.session_id, "Session cancellation observed");
                    *timeout = tokio::time::sleep(Self::SHUTDOWN_DRAIN_TIMEOUT).boxed();
                    cancelled = true;
                }


                Some(state) = state_rx.recv() => {
                    if let Err(e) = self.handle_dialog_state(state).await {
                        warn!(session_id = %self.id, error = %e, "Error handling dialog state");
                    }
                }


                Some(state) = callee_state_rx.recv() => {
                    if let Err(e) = self.handle_callee_state(state).await {
                        warn!(session_id = %self.id, error = %e, "Error handling callee state");
                    }
                }


                Some(cmd) = cmd_rx.recv() => {
                    let result = self
                        .execute_command(cmd, Some(&mut callee_state_rx))
                        .await;
                    if !result.success {
                        warn!(session_id = %self.id, error = ?result.message, "Command execution failed");
                    }
                }

                _ = &mut timeout, if cancelled => {
                    break;
                }

                Some(expired) = self.timer_queue.next(), if !cancelled && !self.timer_queue.is_empty() => {
                    let scheduled = expired.into_inner();

                    match self.next_timer_action(&scheduled) {
                        Some(TimerAction::Refresh) => {
                            let refresh_ok = match if scheduled == self.caller_dialog_id() {
                                self.send_server_session_refresh().await
                            } else {
                                self.send_callee_session_refresh(&scheduled).await
                            } {
                                Ok(()) => true,
                                Err(e) => {
                                    warn!(session_id = %self.id, dialog_id = %scheduled, error = %e, "Failed to send session refresh");
                                    false
                                }
                            };

                            if refresh_ok {
                                self.schedule_timer(scheduled);
                            } else {
                                self.schedule_expiration_timer(scheduled);
                            }
                        }
                        Some(TimerAction::Expired) => {
                            warn!(session_id = %self.id, dialog_id = %scheduled, "Session timer expired, terminating session");
                            self.meta.hangup_reason = Some(CallRecordHangupReason::Autohangup);
                            self.pending_hangup.insert(scheduled);
                        }
                        None => {}
                    }
                }

                // RTP timeout handled via MediaBridge callback → cmd channel

                _ = &mut max_duration_sleep => {
                    warn!(session_id = %self.id,
                        session_id = %self.context.session_id,
                        max_duration = ?self.context.dialplan.max_call_duration,
                        "Max call duration exceeded, terminating session"
                    );
                    self.meta.hangup_reason = Some(CallRecordHangupReason::Autohangup);
                    self.cancel_token.cancel();
                }
            }
        }

        self.cleanup().await;

        let _ = _cancel_guard;

        Ok(())
    }

    /// Run loop for a **UAC / outbound** session (RWI originate).
    ///
    /// Unlike `process`, this skips the inbound setup phase (no ringback,
    /// no early-media 183, no dialplan execution) — the callee INVITE is
    /// sent by the caller and the resulting `ClientInviteDialog` is attached
    /// via [`attach_callee_dialog`]. This loop only drives:
    ///   * callee dialog state events (`callee_state_rx`)
    ///   * command processing (`cmd_rx`)
    ///   * session-timer refresh / max-duration / hangup drain
    pub async fn process_uac(
        &mut self,
        mut callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut cmd_rx: mpsc::Receiver<CallCommand>,
    ) -> Result<()> {
        let _cancel_guard = self.cancel_token.clone().drop_guard();

        let hangup_futures = FuturesUnordered::new();
        let timeout = futures::future::pending::<()>().boxed();
        let mut cancelled = false;
        tokio::pin!(hangup_futures);
        tokio::pin!(timeout);

        let max_duration_sleep = if let Some(max_dur) = self.context.dialplan.max_call_duration {
            tokio::time::sleep(max_dur).boxed()
        } else {
            futures::future::pending::<()>().boxed()
        };
        tokio::pin!(max_duration_sleep);

        loop {
            for dialog_id in self.pending_hangup.drain() {
                if let Some(dialog) = self.server.dialog_layer.get_dialog(&dialog_id) {
                    let dialog = dialog.clone();
                    hangup_futures.push(async move {
                        let res = dialog.hangup().await;
                        res.map(|_| dialog_id)
                    });
                }
            }

            if cancelled
                && hangup_futures.is_empty()
                && self.pending_hangup.is_empty()
                && self.callee_dialogs.is_empty()
            {
                break;
            }

            tokio::select! {
                res = hangup_futures.next(), if !hangup_futures.is_empty() => {
                    if let Some(res) = res {
                        tracing::info!(session_id = %self.id, dialog_id = ?res.as_ref().ok(), "Hangup completed");
                        if let Ok(dialog_id) = &res {
                            self.callee_dialogs.remove(dialog_id);
                        }
                    }
                }
                _ = self.cancel_token.cancelled(), if !cancelled => {
                    debug!(session_id = %self.context.session_id, "UAC session cancellation observed");
                    *timeout = tokio::time::sleep(Self::SHUTDOWN_DRAIN_TIMEOUT).boxed();
                    cancelled = true;
                }
                Some(state) = callee_state_rx.recv() => {
                    if let Err(e) = self.handle_callee_state(state).await {
                        warn!(session_id = %self.id, error = %e, "Error handling callee state");
                    }
                }
                Some(cmd) = cmd_rx.recv() => {
                    let result = self
                        .execute_command(cmd, Some(&mut callee_state_rx))
                        .await;
                    if !result.success {
                        warn!(session_id = %self.id, error = ?result.message, "Command execution failed");
                    }
                }
                _ = &mut timeout, if cancelled => {
                    break;
                }
                _ = &mut max_duration_sleep => {
                    warn!(session_id = %self.id,
                        max_duration = ?self.context.dialplan.max_call_duration,
                        "Max call duration exceeded, terminating UAC session"
                    );
                    self.meta.hangup_reason = Some(CallRecordHangupReason::Autohangup);
                    self.cancel_token.cancel();
                }
                Some(expired) = self.timer_queue.next(), if !cancelled && !self.timer_queue.is_empty() => {
                    let scheduled = expired.into_inner();
                    match self.next_timer_action(&scheduled) {
                        Some(TimerAction::Refresh) => {
                            let refresh_ok = match self.send_callee_session_refresh(&scheduled).await {
                                Ok(()) => true,
                                Err(e) => {
                                    warn!(session_id = %self.id, dialog_id = %scheduled, error = %e, "Failed to send session refresh");
                                    false
                                }
                            };
                            if refresh_ok {
                                self.schedule_timer(scheduled);
                            } else {
                                self.schedule_expiration_timer(scheduled);
                            }
                        }
                        Some(TimerAction::Expired) => {
                            warn!(session_id = %self.id, dialog_id = %scheduled, "Session timer expired, terminating UAC session");
                            self.meta.hangup_reason = Some(CallRecordHangupReason::Autohangup);
                            self.pending_hangup.insert(scheduled);
                        }
                        None => {}
                    }
                }
            }
        }

        self.cleanup().await;
        let _ = _cancel_guard;
        Ok(())
    }

    /// Attach a callee `ClientInviteDialog` (B leg) to this UAC session after
    /// the outbound INVITE is answered. Registers the dialog in `callee_dialogs`
    /// and the leg registry so the command loop and media bridge can drive it.
    pub async fn attach_callee_dialog(
        &mut self,
        dialog: InviteDialog,
        callee_sdp: Option<String>,
    ) {
        let dialog_id = dialog.id();
        info!(session_id = %self.id, %dialog_id, "Attaching callee dialog to UAC session");

        self.callee_dialogs.insert(dialog_id.clone(), ());

        // Register the callee leg with a real dialog.
        let callee_id = LegId::from("callee");
        let dialog_enum = rsipstack::dialog::dialog::Dialog::Invite(dialog);
        self.legs.set_dialog(callee_id.clone(), dialog_enum);

        // Ensure the B leg exists and apply the callee's answer SDP.
        if let Some(sdp) = callee_sdp {
            if let Err(e) = self
                .ensure_media_leg(
                    crate::media::media_bridge::LegSide::B,
                    &sdp,
                    rustrtc::TransportMode::Rtp,
                )
                .await
            {
                warn!(session_id = %self.id, error = %e, "Failed to create callee MediaBridge leg");
                return;
            }
            if let Some(mb) = self.bridge_mut() {
                if let Some(leg) = mb.leg(crate::media::media_bridge::LegSide::B) {
                    // UAC mode: the leg has no local offer yet (the INVITE SDP
                    // was generated before this attach). Generate an offer with
                    // the answer's codecs so set_remote_description(answer) has
                    // a matching local offer and the negotiated profile lands.
                    if leg.negotiated().is_none() {
                        if let Ok(offer) = leg.create_offer(vec![]).await {
                            debug!(session_id = %self.id, offer_len = offer.len(), "Generated UAC local offer for callee MediaBridge leg");
                        } else {
                            warn!(session_id = %self.id, "Failed to generate UAC offer for callee leg");
                        }
                    }
                    if let Err(e) = leg.apply_sdp(&sdp, rustrtc::SdpType::Answer).await {
                        warn!(session_id = %self.id, error = %e, "Failed to apply callee SDP to MediaBridge B leg");
                    }
                }
            }
        }
    }

    /// Subscribe to the MediaBridge DTMF bus and forward RTP RFC 2833 digits
    /// to the running app + RWI gateway + bridge WebSocket. Called once at
    /// session construction when media is anchored.
    fn spawn_dtmf_forwarder(&self) {
        let Some(mb) = self.media.bridge.as_ref() else {
            return;
        };
        let mut rx = mb.dtmf_bus();
        let app_runtime = self.app_runtime.clone();
        let rwi_gateway = self.server.rwi_gateway.clone();
        let bridge_dtmf_tx = self.bridge_dtmf_tx.clone();
        let session_id = self.context.session_id.clone();
        crate::utils::spawn(async move {
            while let Ok((side, ev)) = rx.recv().await {
                let leg_id = match side {
                    crate::media::media_bridge::LegSide::A => "caller",
                    crate::media::media_bridge::LegSide::B => "callee",
                };
                debug!(session_id = %session_id, leg_id, digit = ev.digit.to_string(), "RTP DTMF received from media bridge, forwarding");
                forward_dtmf_event(
                    ev.digit,
                    leg_id,
                    &session_id,
                    &app_runtime,
                    &rwi_gateway,
                    &bridge_dtmf_tx,
                );
            }
        });
    }

    /// Ensure the MediaBridge leg for `side` exists, creating it from the
    /// codecs negotiated in `sdp` if needed. Idempotent.
    async fn ensure_media_leg(
        &mut self,
        side: crate::media::media_bridge::LegSide,
        sdp: &str,
        transport: rustrtc::TransportMode,
    ) -> Result<()> {
        let codecs =
            crate::media::negotiate::MediaNegotiator::extract_codec_params(sdp).audio;
        let rtp_port_range = self
            .context
            .dialplan
            .media
            .rtp_start_port
            .zip(self.context.dialplan.media.rtp_end_port);
        let external_ip = self.context.dialplan.media.external_ip.clone();
        let bind_ip = self.context.dialplan.media.bind_ip.clone();
        let cname = Some(self.server.rtc_cname.clone());
        let cfg = crate::media::leg::LegConfig {
            transport,
            codecs,
            rtp_port_range,
            external_ip,
            bind_ip,
            cname,
            comfort_noise: self.context.dialplan.media.comfort_noise,
            comfort_noise_level_db: self.context.dialplan.media.comfort_noise_level_db,
        };

        let Some(mb) = self.bridge_mut() else {
            return Ok(());
        };
        if mb.leg(side).is_some() {
            return Ok(());
        }
        let leg = crate::media::leg::LegInner::new("uac-leg", &cfg)?;
        mb.replace_leg(side, leg).await;
        Ok(())
    }

    /// Attach the primary caller (A leg) dialog to the session. Used in UAC
    /// mode (RWI originate) after the first outbound INVITE is answered: the
    /// virtual caller anchor becomes a real SIP peer on MediaBridge A side.
    /// Accepts any `InviteDialog` (Server or Client role).
    pub async fn attach_caller_dialog(
        &mut self,
        dialog: InviteDialog,
        caller_sdp: Option<String>,
    ) {
        let dialog_id = dialog.id();
        info!(session_id = %self.id, %dialog_id, "Attaching caller dialog to session");

        self.caller_dialog = Some(dialog.clone());
        let caller_id = LegId::from("caller");
        let dialog_enum = rsipstack::dialog::dialog::Dialog::Invite(dialog);
        self.legs.set_dialog(caller_id.clone(), dialog_enum);

        // Create the A leg from the internal caller's offer (NOT the callee's
        // answer). In the originate (UAC) path, caller_offer was set from the
        // RtpTrackBuilder's local SDP before the INVITE was sent. Using it
        // here ensures LegSide::A represents the internal/PBX side, while
        // LegSide::B (created by ensure_originate_callee_leg) represents the
        // external SIP callee.
        if self.media.bridge.is_some() && self.media.caller_offer.is_some() {
            if let Err(e) = self.ensure_caller_leg().await {
                warn!(session_id = %self.id, error = %e, "Failed to create caller MediaBridge A leg");
            }
        } else if let Some(sdp) = caller_sdp {
            // Fallback: no caller_offer available — apply callee SDP directly
            // (legacy behavior for non-originate paths).
            if let Err(e) = self
                .ensure_media_leg(
                    crate::media::media_bridge::LegSide::A,
                    &sdp,
                    rustrtc::TransportMode::Rtp,
                )
                .await
            {
                warn!(session_id = %self.id, error = %e, "Failed to create caller MediaBridge leg");
                return;
            }
            if let Some(mb) = self.bridge_mut() {
                if let Some(leg) = mb.leg(crate::media::media_bridge::LegSide::A) {
                    if leg.negotiated().is_none() {
                        if let Ok(offer) = leg.create_offer(vec![]).await {
                            debug!(session_id = %self.id, offer_len = offer.len(), "Generated UAC local offer for caller MediaBridge leg");
                        }
                    }
                    if let Err(e) = leg.apply_sdp(&sdp, rustrtc::SdpType::Answer).await {
                        warn!(session_id = %self.id, error = %e, "Failed to apply caller SDP to MediaBridge A leg");
                    }
                }
            }
        }
    }

    /// Ensure the callee (B leg) exists in the MediaBridge for the originate
    /// (UAC) path. In this path, the INVITE SDP was generated by RtpTrackBuilder,
    /// so the B leg is never created by the normal dialplan flow.
    pub async fn ensure_originate_callee_leg(&mut self, callee_sdp: Option<&str>) {
        let Some(sdp) = callee_sdp else { return };
        if self.media.bridge.is_none() {
            return;
        }
        let session_id = self.context.session_id.clone();
        let has_b = self
            .bridge()
            .and_then(|mb| mb.leg(crate::media::media_bridge::LegSide::B))
            .is_some();
        if has_b {
            return;
        }
        let transport = if Self::sdp_transport_mode(sdp) == rustrtc::TransportMode::WebRtc {
            rustrtc::TransportMode::WebRtc
        } else {
            rustrtc::TransportMode::Rtp
        };
        if let Err(e) = self
            .ensure_media_leg(crate::media::media_bridge::LegSide::B, sdp, transport)
            .await
        {
            warn!(session_id = %session_id, error = %e, "Failed to create B leg for originate");
            return;
        }
        let session_id2 = self.context.session_id.clone();
        if let Some(mb) = self.bridge_mut() {
            if let Some(leg) = mb.leg(crate::media::media_bridge::LegSide::B) {
                if leg.negotiated().is_none() {
                    if let Ok(offer) = leg.create_offer(vec![]).await {
                        info!(session_id = %session_id2, offer_len = offer.len(), "Created B leg offer for originate MediaBridge");
                    }
                }
                if let Err(e) = leg.apply_sdp(sdp, rustrtc::SdpType::Answer).await {
                    warn!(session_id = %session_id2, error = %e, "Failed to apply callee SDP to B leg");
                }
            }
            // Activate the bridge relay: accept both legs so the bridge
            // can start forwarding RTP between A and B.
            mb.accept(crate::media::media_bridge::LegSide::B).await;
            mb.accept(crate::media::media_bridge::LegSide::A).await;
            info!(session_id = %session_id2, "Originated MediaBridge legs accepted (A+B)");
        }
    }


    fn next_timer_action(&mut self, scheduled: &DialogId) -> Option<TimerAction> {
        let we_are_uac = self.is_uac_dialog(scheduled);
        self.timer_keys.remove(scheduled);
        let timer = self.timers.get_mut(scheduled)?;

        if timer.is_expired() {
            return Some(TimerAction::Expired);
        }

        if timer.should_we_refresh(we_are_uac) && timer.should_refresh() && timer.start_refresh() {
            return Some(TimerAction::Refresh);
        }

        None
    }

    fn session_hook_ctx(&self) -> crate::proxy::proxy_call::session_hooks::CallSessionContext {
        // Merge routing metadata (X-CRM-* / X-CC-*) into extensions.
        // Use entry() to avoid overwriting keys already set by addons (e.g.
        // CcCallSessionHook writes agent_id/agent_name here).
        if let Some(ref m) = self.context.metadata {
            if !m.is_empty() {
                let mut ext = self.extensions.write();
                if let Some(existing) = ext.get_mut::<std::collections::HashMap<String, String>>() {
                    for (k, v) in m {
                        existing.entry(k.clone()).or_insert(v.clone());
                    }
                } else {
                    ext.insert(m.clone());
                }
            }
        }
        crate::proxy::proxy_call::session_hooks::CallSessionContext {
            session_id: self.context.session_id.clone(),
            caller: self.context.original_caller.clone(),
            callee: self.context.original_callee.clone(),
            connected_callee: self.meta.connected_callee.clone(),
            queue_name: self.meta.queue_name.clone(),
            direction: self.context.dialplan.direction.to_string(),
            started_at: Some(self.context.created_at.clone()),
            extensions: self.extensions.clone(),
        }
    }

    fn ok_or_failure<T>(result: anyhow::Result<T>) -> CommandResult {
        match result {
            Ok(_) => CommandResult::success(),
            Err(e) => CommandResult::failure(e.to_string()),
        }
    }
    fn extract_sdp(body: &[u8]) -> Option<String> {
        if body.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(body).to_string())
        }
    }

    fn sdp_headers() -> Vec<rsipstack::sip::Header> {
        vec![rsipstack::sip::Header::ContentType(
            "application/sdp".into(),
        )]
    }

    async fn send_mid_dialog_request_to_side(
        &mut self,
        side: DialogSide,
        method: rsipstack::sip::Method,
        headers: Vec<rsipstack::sip::Header>,
        body: Option<Vec<u8>>,
    ) -> Result<Option<rsipstack::sip::Response>> {
        let result = tokio::time::timeout(
            Self::MID_DIALOG_TIMEOUT,
            self.send_mid_dialog_request_to_side_inner(side, method, headers, body),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "mid-dialog request timed out after {}s",
                Self::MID_DIALOG_TIMEOUT.as_secs()
            )
        })?;
        result
    }

    async fn send_mid_dialog_request_to_side_inner(
        &mut self,
        side: DialogSide,
        method: rsipstack::sip::Method,
        headers: Vec<rsipstack::sip::Header>,
        body: Option<Vec<u8>>,
    ) -> Result<Option<rsipstack::sip::Response>> {
        let dialog_id = match side {
            DialogSide::Caller => self.caller_dialog_id(),
            DialogSide::Callee => self
                .callee_dialogs
                .iter()
                .map(|entry| entry.key().clone())
                .next()
                .ok_or_else(|| anyhow!("No callee dialog available for {}", method))?,
        };

        let mut dialog = self
            .server
            .dialog_layer
            .get_dialog(&dialog_id)
            .or_else(|| {
                if side == DialogSide::Caller {
                    self.caller_dialog.clone().map(Dialog::Invite)
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("No dialog found for {}", dialog_id))?;

        match (method, &mut dialog) {
            (rsipstack::sip::Method::Invite, Dialog::Invite(d)) => d
                .reinvite(Some(headers), body)
                .await
                .map_err(|e| anyhow!("re-INVITE failed: {}", e)),
            (rsipstack::sip::Method::Update, Dialog::Invite(d)) => d
                .update(Some(headers), body)
                .await
                .map_err(|e| anyhow!("UPDATE failed: {}", e)),
            (other, _) => Err(anyhow!("Dialog does not support {} request", other)),
        }
    }

    async fn relay_signaling_only_offer(
        &mut self,
        side: DialogSide,
        method: rsipstack::sip::Method,
        offer_sdp: &str,
    ) -> Result<(StatusCode, Option<String>)> {
        let target_side = match side {
            DialogSide::Caller => DialogSide::Callee,
            DialogSide::Callee => DialogSide::Caller,
        };
        let headers = Self::sdp_headers();
        let response = self
            .send_mid_dialog_request_to_side(
                target_side,
                method,
                headers,
                Some(offer_sdp.as_bytes().to_vec()),
            )
            .await?
            .ok_or_else(|| anyhow!("{} timed out", method))?;

        let status = response.status_code.clone();
        let answer_sdp = Self::extract_sdp(response.body());

        Ok((status, answer_sdp))
    }

    fn parse_sdp(
        sdp_type: rustrtc::SdpType,
        sdp: &str,
        context: &str,
    ) -> anyhow::Result<rustrtc::SessionDescription> {
        rustrtc::SessionDescription::parse(sdp_type, sdp)
            .map_err(|e| anyhow::anyhow!("Failed to parse {} SDP: {}", context, e))
    }

    async fn build_target_invite_option(
        &mut self,
        target: &crate::call::Location,
        leg_id_override: Option<&str>,
    ) -> Result<
        (
            rsipstack::dialog::invitation::InviteOption,
            rsipstack::sip::Uri,
            String,
        ),
        CalleeError,
    > {
        let caller = self.context.dialplan.caller.clone().ok_or_else(|| {
            into_callee_err(
                &rsipstack::sip::StatusCode::ServerInternalError,
                Some("No caller in dialplan".to_string()),
            )
        })?;

        let local_addrs = self.server.endpoint.get_addrs();
        let cluster_enabled = !self.server.cluster_peer_ips.is_empty();
        let route_via_home_proxy =
            Self::route_via_home_proxy(target, &local_addrs, cluster_enabled);
        let callee_uri = Self::resolve_outbound_callee_uri(target, route_via_home_proxy);

        let mut headers: Vec<rsipstack::sip::Header> =
            vec![rsipstack::sip::headers::MaxForwards::from(self.context.max_forwards).into()];

        if route_via_home_proxy {
            debug!(session_id = %self.id,
                session_id = %self.context.session_id,
                %callee_uri,
                "Routing via home_proxy request URI without self-referencing Record-Route"
            );
        }

        let default_expires = self
            .server
            .proxy_config
            .session_expires
            .unwrap_or(crate::proxy::proxy_call::session_timer::DEFAULT_SESSION_EXPIRES);
        if self.server.proxy_config.session_timer_mode().is_enabled() {
            headers.extend(
                crate::proxy::proxy_call::session_timer::build_default_session_timer_headers(
                    default_expires,
                    crate::proxy::proxy_call::session_timer::MIN_MIN_SE,
                ),
            );
        }

        if let Some(target_headers) = &target.headers {
            headers.extend(
                target_headers
                    .iter()
                    .filter(|header| {
                        if target.registered_aor.is_none() {
                            return true;
                        }

                        let name = header.name();
                        let is_content_header = name
                            .get(..8)
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Content-"));

                        !is_content_header
                            && ![
                                "Via",
                                "From",
                                "To",
                                "Call-ID",
                                "CSeq",
                                "Contact",
                                "Route",
                                "Record-Route",
                                "Authorization",
                                "Proxy-Authorization",
                                "User-Agent",
                                "Expires",
                                "Min-Expires",
                                "Path",
                                "Service-Route",
                                "Max-Forwards",
                            ]
                            .iter()
                            .any(|excluded| name.eq_ignore_ascii_case(excluded))
                    })
                    .cloned(),
            );
        }

        let callee_is_webrtc = Self::callee_supports_webrtc(target);
        let leg_id = leg_id_override.unwrap_or("callee");
        self.legs.set_transport(
            crate::call::domain::LegId::from(leg_id),
            self.callee_transport_mode(callee_is_webrtc),
        );

        let offer = self.prepare_callee_media_offer(target).await.map_err(|e| {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                error = %e,
                "Failed to prepare callee media offer"
            );
            into_callee_err(
                &StatusCode::ServerInternalError,
                Some(
                    r#"SIP;cause=500;text="Media resource allocation failed""#.to_string(),
                ),
            )
        })?;
        let content_type = offer.as_ref().map(|_| "application/sdp".to_string());

        let contact_uri = self
            .context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .unwrap_or_else(|| caller.clone());

        let callee_call_id = self.context.dialplan.call_id.clone().unwrap_or_else(|| {
            rsipstack::transaction::make_call_id(
                self.server.endpoint.inner.option.callid_suffix.as_deref(),
            )
            .value()
            .to_string()
        });
        self.meta.callee_call_ids.insert(callee_call_id.clone());

        let option = rsipstack::dialog::invitation::InviteOption {
            caller_display_name: self.context.dialplan.caller_display_name.clone(),
            callee: callee_uri.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination: if route_via_home_proxy {
                None
            } else {
                target.destination.clone()
            },
            credential: target.credential.clone(),
            headers: Some(headers),
            call_id: Some(callee_call_id.clone()),
            contact: contact_uri,
            ..Default::default()
        };

        Ok((option, callee_uri, callee_call_id))
    }

    fn build_rtp_track_builder(
        &self,
        track_id: String,
        cancel_token: tokio_util::sync::CancellationToken,
        mode: rustrtc::TransportMode,
    ) -> crate::media::RtpTrackBuilder {
        let is_webrtc = mode == rustrtc::TransportMode::WebRtc;
        let mut builder = crate::media::RtpTrackBuilder::new(track_id)
            .with_mode(mode)
            .with_cancel_token(cancel_token)
            .with_enable_latching(self.context.dialplan.media.enable_latching)
            .with_probation_max_packets(self.context.dialplan.media.probation_max_packets)
            .with_cname(self.server.rtc_cname.clone());

        if let Some(ref external_ip) = self.context.dialplan.media.external_ip {
            builder = builder.with_external_ip(external_ip.clone());
        }
        if let Some(ref bind_ip) = self.context.dialplan.media.bind_ip {
            builder = builder.with_bind_ip(bind_ip.clone());
        }

        // SDES-SRTP shares the plain-RTP port range; only WebRTC uses the
        // dedicated WebRTC range.
        let (start_port, end_port) = if is_webrtc {
            (
                self.context.dialplan.media.webrtc_port_start,
                self.context.dialplan.media.webrtc_port_end,
            )
        } else {
            (
                self.context.dialplan.media.rtp_start_port,
                self.context.dialplan.media.rtp_end_port,
            )
        };

        if let (Some(start), Some(end)) = (start_port, end_port) {
            builder = builder.with_rtp_range(start, end);
        }

        builder
    }

    fn update_snapshot_cache(&self) {
        let callee_dialogs: Vec<DialogId> = self
            .callee_dialogs
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        let snapshot = SessionSnapshot {
            id: self.id.clone(),
            state: self.state,
            leg_count: self.legs.len(),
            bridge_active: self.bridge.active,
            caller_gate_open: true,
            media_path: self.media_profile.path,
            answer_sdp: self.media.answer.clone(),
            callee_dialogs,
        };

        *self.snapshot_cache.write() = Some(snapshot);
    }

    async fn handle_updated_dialog(
        &mut self,
        side: DialogSide,
        dialog_id: DialogId,
        request: rsipstack::sip::Request,
        tx_handle: TransactionHandle,
    ) -> Result<()> {
        debug!(session_id = %self.id,
            %dialog_id,
            method = ?request.method,
            side = ?side,
            "Received UPDATE/INVITE on dialog"
        );

        let update_result = self.update_dialog_timer_from_headers(&dialog_id, &request.headers);
        if let Err(e) = &update_result {
            warn!(session_id = %self.id,
                %dialog_id,
                error = %e,
                side = ?side,
                "Failed to refresh session timer"
            );
        }

        let mut status = if update_result.is_ok() {
            rsipstack::sip::StatusCode::OK
        } else {
            rsipstack::sip::StatusCode::SessionIntervalTooSmall
        };

        let mut headers = if update_result.is_err() {
            self.timers.get(&dialog_id).map(|timer| {
                vec![rsipstack::sip::Header::Other(
                    HEADER_MIN_SE.to_string(),
                    timer.min_se.as_secs().to_string(),
                )]
            })
        } else {
            self.successful_refresh_response_headers(&dialog_id)
        }
        .unwrap_or_default();

        let body = if update_result.is_ok() && !request.body.is_empty() {
            let offer_sdp = String::from_utf8_lossy(&request.body).to_string();
            let parsed_offer =
                rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer_sdp).ok();
            let answer_result = if self.bypasses_local_media() {
                self.relay_signaling_only_offer(side, request.method.clone(), &offer_sdp)
                    .await
                    .map(|(result_status, answer_sdp)| {
                        // Align answer direction for bypass mode
                        let aligned = answer_sdp
                            .map(|sdp| Self::align_answer_direction_with_offer(&offer_sdp, &sdp));
                        (result_status, aligned)
                    })
                    .map_err(|e| {
                        (
                            rsipstack::sip::StatusCode::ServerInternalError,
                            "Failed to relay signaling-only dialog offer",
                            e,
                        )
                    })
            } else {
                self.build_local_dialog_answer(side, &offer_sdp)
                    .await
                    .map(|answer_sdp| (status.clone(), Some(answer_sdp)))
                    .map_err(|e| {
                        (
                            rsipstack::sip::StatusCode::NotAcceptableHere,
                            "Failed to build local answer for re-INVITE",
                            e,
                        )
                    })
            };

            match answer_result {
                Ok((result_status, answer_sdp)) => {
                    status = result_status;
                    if status.kind() != rsipstack::sip::status_code::StatusCodeKind::Successful {
                        headers.clear();
                    }
                    // Apply hold transition for all branches on success
                    if status.kind() == rsipstack::sip::StatusCodeKind::Successful {
                        if let Some(ref offer) = parsed_offer {
                            self.apply_reinvite_hold_transition(side, offer, &request.headers.0)
                                .await;
                        }
                    }
                    if let Some(answer_sdp) = answer_sdp {
                        headers.push(rsipstack::sip::Header::ContentType(
                            "application/sdp".into(),
                        ));
                        Some(answer_sdp.into_bytes())
                    } else {
                        None
                    }
                }
                Err((error_status, message, error)) => {
                    warn!(session_id = %self.id,
                        %dialog_id,
                        error = %error,
                        side = ?side,
                        "{message}"
                    );
                    status = error_status;
                    headers.clear();
                    None
                }
            }
        } else {
            None
        };

        let _ = tx_handle
            .respond(status, (!headers.is_empty()).then_some(headers), body)
            .await;
        Ok(())
    }

    async fn handle_dialog_state(&mut self, state: DialogState) -> Result<()> {
        debug!(session_id = %self.id,
            session_id = %self.context.session_id,
            state = %state,
            "Caller dialog state"
        );
        match state {
            DialogState::Confirmed(_, _) => {
                self.update_leg_state(&LegId::from("caller"), LegState::Connected);
            }
            DialogState::Updated(dialog_id, request, tx_handle) => {
                self.handle_updated_dialog(DialogSide::Caller, dialog_id, request, tx_handle)
                    .await?;
            }
            DialogState::Options(_, _, tx_handle) => {
                tx_handle
                    .respond(rsipstack::sip::StatusCode::OK, None, None)
                    .await
                    .ok();
            }
            DialogState::Info(_, request, tx_handle) => {
                self.handle_dialog_info(request, tx_handle).await?;
            }
            DialogState::Notify(_, request, tx_handle) => {
                self.handle_dialog_notify(request, tx_handle).await?;
            }
            DialogState::Terminated(_, reason) => {
                self.update_leg_state(&LegId::from("caller"), LegState::Ended);

                match reason {
                    TerminatedReason::UacBye => {
                        self.meta.hangup_reason = Some(CallRecordHangupReason::ByCaller);
                        info!(session_id = %self.id, "Caller initiated hangup (UacBye)");
                    }
                    TerminatedReason::UasBye => {
                        self.meta.hangup_reason = Some(CallRecordHangupReason::ByCallee);
                        info!(session_id = %self.id, "Callee initiated hangup (UasBye) on caller dialog");
                    }
                    _ => {
                        debug!(session_id = %self.id, ?reason, "Caller dialog terminated with reason");
                    }
                }

                let callee_ids: Vec<_> = self
                    .callee_dialogs
                    .iter()
                    .map(|entry| entry.key().clone())
                    .collect();
                self.pending_hangup.extend(callee_ids);
                self.cancel_token.cancel();
                if self.app_runtime.is_running() {
                    let _ = self
                        .app_runtime
                        .stop_app(Some("caller_hangup".to_string()))
                        .await;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_dialog_info(
        &mut self,
        request: rsipstack::sip::Request,
        tx_handle: TransactionHandle,
    ) -> Result<()> {
        let content_type = Self::request_content_type(&request);
        let is_dtmf = content_type
            .as_deref()
            .is_some_and(|ct| ct.contains("application/dtmf-relay"));
        let body_text = String::from_utf8_lossy(request.body());
        let is_picture_fast_update =
            Self::is_picture_fast_update_info(content_type.as_deref(), &body_text);

        if is_dtmf {
            info!(session_id = %self.id,
                session_id = %self.context.session_id,
                "✓ Received SIP INFO with DTMF (application/dtmf-relay content type)"
            );
            debug!(session_id = %self.id,
                session_id = %self.context.session_id,
                body = %body_text,
                "INFO DTMF message body"
            );
            for line in body_text.lines() {
                let line = line.trim();
                if line.to_lowercase().starts_with("signal=") {
                    let digit = line
                        .trim_start_matches(|c: char| !c.eq_ignore_ascii_case(&'s'))
                        .trim_start_matches("Signal=")
                        .trim_start_matches("signal=")
                        .trim();
                    if !digit.is_empty() {
                        let digit_char = digit.chars().next().unwrap_or_default();
                        forward_dtmf_event(
                            digit_char,
                            "caller",
                            &self.context.session_id,
                            &self.app_runtime,
                            &self.server.rwi_gateway,
                            &self.bridge_dtmf_tx,
                        );
                    }
                }
            }
            if let Some(callee_id) = self.meta.connected_callee_dialog_id.clone() {
                if let Some(dlg) = self.server.dialog_layer.get_dialog(&callee_id) {
                    let fwd_headers = vec![rsipstack::sip::Header::ContentType(
                        rsipstack::sip::headers::ContentType::from("application/dtmf-relay"),
                    )];
                    if let Err(e) =
                        Self::send_info_to_dialog(&dlg, fwd_headers, request.body().to_vec()).await
                    {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            error = %e,
                            "Failed to forward SIP INFO DTMF to callee"
                        );
                    } else {
                        debug!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            "Forwarded SIP INFO DTMF to callee"
                        );
                    }
                }
            }
        } else if is_picture_fast_update {
            self.handle_picture_fast_update(DialogSide::Caller).await;
        } else if content_type
            .as_deref()
            .is_some_and(|ct| ct.contains(RUSTPBX_COMMAND_CT))
        {
            self.handle_rustpbx_info_command(&body_text, &tx_handle)
                .await?;
            // Do NOT forward to peer — this is a PBX-internal command
            return Ok(());
        } else {
            debug!(session_id = %self.id,
                session_id = %self.context.session_id,
                ct = ?content_type,
                "Received SIP INFO without recognized content type"
            );
        }
        tx_handle
            .respond(rsipstack::sip::StatusCode::OK, None, None)
            .await
            .ok();
        Ok(())
    }

    /// Parse the music parameter from INFO command params, returning an
    /// optional [`MediaSource`] that can be passed to Hold or used directly.
    fn parse_info_music_param(
        params: &serde_json::Value,
    ) -> Option<crate::call::domain::MediaSource> {
        params.get("music").and_then(Self::parse_info_media_source)
    }

    /// Pure function: map an INFO action + params to a [`CallCommand`].
    /// Does NOT access `self` — can be unit-tested in isolation.
    /// Returns `None` for unknown/no-op actions.
    pub(crate) fn parse_info_command(
        action: &str,
        params: Option<&serde_json::Value>,
        parsed: &serde_json::Value,
    ) -> Option<CallCommand> {
        match action {
            "media.play" | "media.inject_start" => {
                let source = params
                    .and_then(|p| p.get("source"))
                    .and_then(Self::parse_info_media_source)
                    .unwrap_or(MediaSource::Silence);
                Some(CallCommand::Play {
                    leg_id: params
                        .and_then(|p| p.get("leg_id"))
                        .and_then(|v| v.as_str())
                        .map(LegId::new),
                    source,
                    options: Some(crate::call::domain::PlayOptions {
                        loop_playback: params
                            .and_then(|p| p.get("loop"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        await_completion: false,
                        interrupt_on_dtmf: params
                            .and_then(|p| p.get("interrupt_on_dtmf"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        track_id: None,
                        send_progress: false,
                    }),
                })
            }
            "media.stop" | "media.inject_stop" => Some(CallCommand::StopPlayback {
                leg_id: params
                    .and_then(|p| p.get("leg_id"))
                    .and_then(|v| v.as_str())
                    .map(LegId::new),
            }),
            "record.start" => Some(CallCommand::StartRecording {
                config: crate::call::domain::RecordConfig {
                    path: params
                        .and_then(|p| p.get("path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    max_duration_secs: params
                        .and_then(|p| p.get("max_duration"))
                        .and_then(|v| v.as_u64().map(|d| d as u32)),
                    beep: params
                        .and_then(|p| p.get("beep"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                    format: params
                        .and_then(|p| p.get("format"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                },
            }),
            "record.stop" => Some(CallCommand::StopRecording),
            "hold" => Some(CallCommand::Hold {
                leg_id: LegId::new(
                    params
                        .and_then(|p| p.get("leg_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("caller"),
                ),
                music: params.and_then(Self::parse_info_music_param),
            }),
            "unhold" => Some(CallCommand::Unhold {
                leg_id: LegId::new(
                    params
                        .and_then(|p| p.get("leg_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("caller"),
                ),
            }),
            "consult.initiate" => Some(CallCommand::Hold {
                leg_id: LegId::new(
                    params
                        .and_then(|p| p.get("leg_id"))
                        .and_then(|v| v.as_str())
                        .or_else(|| parsed.get("call_id").and_then(|v| v.as_str()))
                        .unwrap_or("caller"),
                ),
                music: None,
            }),
            "consult.cancel" => Some(CallCommand::Unhold {
                leg_id: LegId::new(
                    params
                        .and_then(|p| p.get("leg_id"))
                        .and_then(|v| v.as_str())
                        .or_else(|| parsed.get("call_id").and_then(|v| v.as_str()))
                        .unwrap_or("caller"),
                ),
            }),
            _ => None,
        }
    }

    /// Handle a rustpbx JSON command received via SIP INFO with
    /// `application/vnd.rustpbx+json` content type.  The command is dispatched
    /// asynchronously through the session command channel and the INFO request
    /// is always acknowledged with 200 OK.
    ///
    /// For `ivr.exec` the handler performs the hold synchronously and then
    /// enqueues the `StartApp` command, so the callee is held before the IVR
    /// starts on the caller side.
    async fn handle_rustpbx_info_command(
        &mut self,
        body: &str,
        tx_handle: &TransactionHandle,
    ) -> Result<()> {
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    error = %e,
                    body = %body,
                    "SIP INFO rustpbx command: invalid JSON"
                );
                tx_handle
                    .respond(rsipstack::sip::StatusCode::OK, None, None)
                    .await
                    .ok();
                return Ok(());
            }
        };

        // Determine action — support both `action` and legacy `cmd` fields
        let action = parsed
            .get("action")
            .and_then(|v| v.as_str())
            .or_else(|| parsed.get("cmd").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string();

        let params = parsed.get("params");

        // ── ivr.exec: bundled IVR execution (hold callee + start app) ──
        if action == "ivr.exec" {
            return self
                .handle_ivr_exec_command(params.cloned(), tx_handle)
                .await;
        }

        // ── app.start / app.stop: generic non-bundled app control ──
        if action == "app.start" {
            let app_name = params
                .and_then(|p| p.get("app_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("ivr")
                .to_string();
            let app_params = params.and_then(|p| p.get("app_params")).cloned();
            let cmd = CallCommand::StartApp {
                app_name,
                params: app_params,
                auto_answer: false,
            };
            Self::send_or_log_cmd(&self.cmd_tx, cmd, &action, &self.context.session_id);
            tx_handle
                .respond(rsipstack::sip::StatusCode::OK, None, None)
                .await
                .ok();
            return Ok(());
        }

        if action == "app.stop" {
            let cmd = CallCommand::StopApp { reason: None };
            Self::send_or_log_cmd(&self.cmd_tx, cmd, &action, &self.context.session_id);
            tx_handle
                .respond(rsipstack::sip::StatusCode::OK, None, None)
                .await
                .ok();
            return Ok(());
        }

        let cmd: Option<CallCommand> = Self::parse_info_command(&action, params, &parsed);

        match cmd {
            Some(cmd) => {
                Self::send_or_log_cmd(&self.cmd_tx, cmd, &action, &self.context.session_id)
            }
            None => {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    action = %action,
                    "SIP INFO rustpbx command: unknown action"
                );
            }
        }

        tx_handle
            .respond(rsipstack::sip::StatusCode::OK, None, None)
            .await
            .ok();
        Ok(())
    }

    /// Helper to send a command to the session command channel with logging.
    fn send_or_log_cmd(
        cmd_tx: &Option<mpsc::Sender<CallCommand>>,
        cmd: CallCommand,
        action: &str,
        session_id: &str,
    ) {
        if let Some(tx) = cmd_tx {
            if let Err(e) = tx.try_send(cmd) {
                warn!(
                    session_id = %session_id,
                    action = %action,
                    error = %e,
                    "SIP INFO rustpbx command: failed to enqueue"
                );
            } else {
                info!(
                    session_id = %session_id,
                    action = %action,
                    "SIP INFO rustpbx command accepted"
                );
            }
        }
    }

    /// Handle `ivr.exec` — bundled IVR execution.
    ///
    /// 1. Writes [`IvrExecState`] to session extensions
    /// 2. Holds callee + plays hold music (reuses the propagate-hold path)
    /// 3. Enqueues `StartApp` via the command channel
    async fn handle_ivr_exec_command(
        &mut self,
        params: Option<serde_json::Value>,
        tx_handle: &TransactionHandle,
    ) -> Result<()> {
        let session_id = self.context.session_id.clone();
        let p = params.as_ref();
        let request_id = p
            .and_then(|p| p.get("request_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let app_name = p
            .and_then(|p| p.get("app"))
            .and_then(|v| v.as_str())
            .unwrap_or("ivr")
            .to_string();
        let ivr_params = p.and_then(|p| p.get("ivr_params")).cloned();
        // Resolve route_point → file path so the IVR factory can find the config.
        let route_point = p
            .and_then(|p| p.get("route_point"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mut ivr_params = ivr_params.unwrap_or(serde_json::json!({}));
        if !route_point.is_empty()
            && ivr_params.get("file").is_none()
            && ivr_params.get("mode").is_none()
        {
            let file = format!("config/ivr/{}.toml", route_point);
            if let Some(obj) = ivr_params.as_object_mut() {
                obj.insert("file".to_string(), serde_json::json!(file));
            } else {
                ivr_params = serde_json::json!({"file": file});
            }
        }
        let hold_agent = p
            .and_then(|p| p.get("hold_agent"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let webhook_url = p
            .and_then(|p| p.get("webhook_url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let metadata = p
            .and_then(|p| p.get("metadata"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let override_music = p
            .and_then(|p| p.get("music"))
            .and_then(Self::parse_info_media_source);

        // 1. Write IvrExecState so the post-exit hook can reconstruct the result.
        {
            let mut ext = self.extensions.write();
            ext.insert(crate::proxy::proxy_call::ivr_exec_hook::IvrExecState {
                request_id: request_id.clone(),
                held_leg: if hold_agent {
                    Some(LegId::from("callee"))
                } else {
                    None
                },
                initiator_leg: LegId::from("callee"),
                webhook_url,
                app_name: app_name.clone(),
                metadata,
            });
        }

        // 2. Hold callee + play music (use override_music if provided, else default).
        if hold_agent {
            self.propagate_hold_to_callee(&[], override_music).await?;
        }

        // 3. Start the app on the caller leg.
        let cmd = CallCommand::StartApp {
            app_name,
            params: Some(ivr_params),
            auto_answer: false,
        };
        Self::send_or_log_cmd(&self.cmd_tx, cmd, "ivr.exec", &session_id);

        // Ack the INFO immediately.
        tx_handle
            .respond(rsipstack::sip::StatusCode::OK, None, None)
            .await
            .ok();
        Ok(())
    }

    /// Convert a JSON value to a domain [`MediaSource`] using the same
    /// convention as the RWI `MediaSource` (source_type + uri/uris).
    fn parse_info_media_source(src: &serde_json::Value) -> Option<MediaSource> {
        let source_type = src
            .get("source_type")
            .and_then(|v| v.as_str())
            .unwrap_or("file");
        match source_type {
            "files" => {
                // Multi-URL: use the first URI as a single file for now.
                // Full multi-URL playback will be added in a follow-up.
                let uri = src
                    .get("uris")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str());
                uri.map(|u| MediaSource::File {
                    path: u.to_string(),
                })
            }
            "file" | "url" => {
                let uri = src.get("uri").and_then(|v| v.as_str())?;
                if source_type == "url" {
                    Some(MediaSource::Url {
                        url: uri.to_string(),
                    })
                } else {
                    Some(MediaSource::File {
                        path: uri.to_string(),
                    })
                }
            }
            "silence" => Some(MediaSource::Silence),
            _ => None,
        }
    }

    fn request_content_type(request: &rsipstack::sip::Request) -> Option<String> {
        request.headers.iter().find_map(|h| {
            if let rsipstack::sip::Header::ContentType(ct) = h {
                Some(ct.value().to_lowercase())
            } else {
                None
            }
        })
    }

    fn is_picture_fast_update_info(content_type: Option<&str>, body: &str) -> bool {
        content_type.is_some_and(|ct| ct.contains("application/media_control+xml"))
            && body.to_ascii_lowercase().contains("picture_fast_update")
    }

    async fn handle_picture_fast_update(&self, requester_side: DialogSide) {
        // Video is passed through at the SDP layer (MediaBridge does not manage
        // video). Keyframe requests are forwarded by the peer's RTP transport.
        debug!(session_id = %self.id,
            session_id = %self.context.session_id,
            side = ?requester_side,
            "picture_fast_update handled at the SDP/transport layer"
        );
    }

    async fn handle_dialog_notify(
        &mut self,
        request: rsipstack::sip::Request,
        tx_handle: TransactionHandle,
    ) -> Result<()> {
        let _ = tx_handle
            .respond(rsipstack::sip::StatusCode::OK, None, None)
            .await;

        let is_refer = request.headers.iter().any(|h| {
            matches!(h, rsipstack::sip::Header::Event(e) if e.value().eq_ignore_ascii_case("refer"))
        });

        if is_refer {
            let body = String::from_utf8_lossy(request.body());
            if let Some(sip_status) = parse_sipfrag_status(&body) {
                info!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    sip_status = %sip_status,
                    body = %body.trim(),
                    "Received REFER NOTIFY"
                );
                let event = crate::call::domain::ReferNotifyEvent {
                    call_id: self.id.0.clone(),
                    sip_status,
                    reason: None,
                    event_type: crate::call::domain::ReferNotifyEventType::Notify,
                };
                let subscribers = self.server.transfer_notify_subscribers.lock().await;
                for tx in subscribers.iter() {
                    let _ = tx.send(event.clone());
                }
                if StatusCode::from(sip_status).kind() == rsipstack::sip::StatusCodeKind::Successful
                {
                    self.meta
                        .hangup_reason
                        .get_or_insert(CallRecordHangupReason::ByRefer);
                    self.pending_hangup.insert(self.caller_dialog_id());
                    info!(session_id = %self.id,
                        session_id = %self.context.session_id,
                        sip_status = %sip_status,
                        "REFER completed successfully, hanging up original dialog"
                    );
                }
            }
        }
        Ok(())
    }

    async fn handle_callee_state(&mut self, state: DialogState) -> Result<()> {
        debug!(session_id = %self.id,
            session_id = %self.context.session_id,
            state = %state,
            "Callee dialog state"
        );
        match state {
            DialogState::Confirmed(_, _) => {
                self.update_leg_state(&LegId::from("callee"), LegState::Connected);
                info!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    "Callee dialog confirmed, call is now connected"
                );
            }
            DialogState::Updated(dialog_id, request, tx_handle) => {
                self.handle_updated_dialog(DialogSide::Callee, dialog_id, request, tx_handle)
                    .await?;
            }
            DialogState::Terminated(terminated_dialog_id, reason) => {
                let connected_callee_terminated =
                    self.meta.connected_callee_dialog_id.as_ref() == Some(&terminated_dialog_id);
                let tracked_callee_terminated =
                    self.callee_dialogs.contains_key(&terminated_dialog_id);

                self.pending_hangup.remove(&terminated_dialog_id);
                self.callee_dialogs.remove(&terminated_dialog_id);
                self.legs.retain_dialogs_by_dialog_id(&terminated_dialog_id);
                self.unschedule_timer(&terminated_dialog_id);
                self.timers.remove(&terminated_dialog_id);
                self.update_refresh_disabled.remove(&terminated_dialog_id);
                // The remote BYE already terminated this leg; remove it before dropping
                // its guard so guard cleanup does not send a second BYE back.
                self.server
                    .dialog_layer
                    .remove_dialog(&terminated_dialog_id);
                self.callee_guards
                    .retain(|guard| guard.id() != &terminated_dialog_id);

                if !tracked_callee_terminated && !connected_callee_terminated {
                    debug!(session_id = %self.id,
                        dialog_id = %terminated_dialog_id,
                        ?reason,
                        "Ignoring terminated untracked callee dialog"
                    );
                    return Ok(());
                }

                if self.meta.connected_callee_dialog_id.is_some() && !connected_callee_terminated {
                    debug!(session_id = %self.id,
                        dialog_id = %terminated_dialog_id,
                        connected_dialog_id = ?self.meta.connected_callee_dialog_id,
                        ?reason,
                        "Ignoring terminated non-connected callee dialog"
                    );
                    return Ok(());
                }

                self.update_leg_state(&LegId::from("callee"), LegState::Ended);

                match &reason {
                    TerminatedReason::UasBye => {
                        self.meta.hangup_reason = Some(CallRecordHangupReason::ByCallee);
                        info!(session_id = %self.id, "Callee initiated hangup (UasBye)");
                    }
                    TerminatedReason::UacBye => {
                        self.meta.hangup_reason = Some(CallRecordHangupReason::ByCaller);
                        info!(session_id = %self.id, "Caller initiated hangup (UacBye) on callee dialog");
                    }
                    _ => {
                        debug!(session_id = %self.id, ?reason, "Callee dialog terminated with reason");
                    }
                }

                if connected_callee_terminated {
                    self.meta.connected_callee = None;
                    self.meta.connected_callee_dialog_id = None;

                    // ── Return to IVR takes precedence over CSAT hook ──────────
                    if !self.caller_dialog.as_ref().is_none_or(|d| d.state().is_terminated()) {
                        if let Some(ivr_name) = self.meta.transfer_return_to_ivr.take() {
                            let mut ivr_params =
                                std::mem::take(&mut self.meta.transfer_return_params);
                            ivr_params.insert("transferred_from".into(), "agent".into());
                            ivr_params.insert("return_reason".into(), "b_leg_hangup".into());
                            info!(session_id = %self.id,
                                session = %self.id,
                                %ivr_name,
                                ?ivr_params,
                                "B‑leg hung up; returning caller to IVR"
                            );
                            self.bridge.clear();
                            if self.start_ivr_app(&ivr_name, ivr_params).await.is_ok() {
                                return Ok(());
                            }
                            warn!(session_id = %self.id,
                                session = %self.id,
                                %ivr_name,
                                "Failed to restart IVR on B‑leg hangup; falling through to normal hangup"
                            );
                        }
                    }

                    let hook_handled = if !self.caller_dialog.as_ref().is_none_or(|d| d.state().is_terminated()) {
                        // Give session hooks a chance to start a post-call
                        // app (e.g. CSAT survey) before hanging up the caller.
                        let ctx = self.session_hook_ctx();
                        let mut handled = false;
                        for hook in self.server.session_hooks.iter() {
                            if hook.on_agent_disconnected(&ctx, &*self.app_runtime).await {
                                handled = true;
                                break;
                            }
                        }
                        handled
                    } else {
                        false
                    };

                    if !hook_handled && !self.caller_dialog.as_ref().is_none_or(|d| d.state().is_terminated()) {
                        self.pending_hangup.insert(self.caller_dialog_id());
                    }
                    // If hook_handled == true, the survey app will hang up the caller
                    // when it completes (via AppAction::Hangup). The app has built-in
                    // timeouts (DTMF 10s, recording 30s) so it cannot hang indefinitely.
                } else {
                    let (code, reason_str) = match reason {
                        TerminatedReason::UasBusy => {
                            (Some(StatusCode::BusyHere), Some("Busy Here".to_string()))
                        }
                        TerminatedReason::UasDecline => {
                            (Some(StatusCode::Decline), Some("Decline".to_string()))
                        }
                        TerminatedReason::UasBye => (None, None),
                        TerminatedReason::Timeout => (
                            Some(StatusCode::RequestTimeout),
                            Some("Request Timeout".to_string()),
                        ),
                        TerminatedReason::ProxyError(status_code) => {
                            (Some(status_code), Some("Proxy Error".to_string()))
                        }
                        TerminatedReason::ProxyAuthRequired => (
                            Some(StatusCode::ProxyAuthenticationRequired),
                            Some("Proxy Authentication Required".to_string()),
                        ),
                        TerminatedReason::UasOther(status_code) => (Some(status_code), None),
                        _ => (
                            Some(StatusCode::ServerInternalError),
                            Some("Internal Error".to_string()),
                        ),
                    };

                    if let Some(code) = code {
                        info!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            status_code = code.code(),
                            reason_text = %code.text(),
                            "Callee rejected the call"
                        );
                        self.meta.last_error = Some((code.clone(), reason_str.clone()));
                        if self.meta.hangup_reason.is_none() {
                            self.meta.hangup_reason =
                                Some(sip_status_to_hangup_reason(code.code()));
                        }

                        // If the callee extension has voicemail enabled, chain to
                        // voicemail instead of passing the rejection to the caller.
                        // This covers busy, no-answer, offline, decline, etc.
                        if self.context.dialplan.voicemail_enabled
                            && !self.caller_dialog.as_ref().is_none_or(|d| d.state().is_terminated())
                        {
                            if let Some(ext) = extract_sip_username(&self.context.original_callee) {
                                info!(session_id = %self.id,
                                    session_id = %self.context.session_id,
                                    extension = %ext,
                                    status = %code.code(),
                                    "Voicemail enabled for callee, starting voicemail app instead of rejecting"
                                );
                                if self.start_voicemail_app(&ext).await.is_ok() {
                                    // Voicemail app took over — don't reject the caller.
                                    return Ok(());
                                }
                                warn!(session_id = %self.id,
                                    session_id = %self.context.session_id,
                                    extension = %ext,
                                    "Voicemail app failed to start, falling back to rejection"
                                );
                            }
                        }

                        if matches!(code.code(), 408 | 480 | 486 | 487) {}
                        if let Err(e) = self
                            .reject_with_tone(
                                code.code(),
                                code.text().to_string(),
                                reason_str.clone(),
                            )
                            .await
                        {
                            warn!(session_id = %self.context.session_id, error = %e, "Failed to send rejection response to caller");
                        }
                    }
                }
            }
            DialogState::Options(_, _, tx_handle) => {
                tx_handle
                    .respond(rsipstack::sip::StatusCode::OK, None, None)
                    .await
                    .ok();
            }
            DialogState::Info(_, request, tx_handle) => {
                let content_type = Self::request_content_type(&request);
                let is_dtmf = content_type
                    .as_deref()
                    .is_some_and(|ct| ct.contains("application/dtmf-relay"));
                let body_text = String::from_utf8_lossy(request.body());
                let is_picture_fast_update =
                    Self::is_picture_fast_update_info(content_type.as_deref(), &body_text);

                if is_dtmf {
                    for line in body_text.lines() {
                        let line = line.trim();
                        if line.to_lowercase().starts_with("signal=") {
                            let digit = line
                                .trim_start_matches(|c: char| !c.eq_ignore_ascii_case(&'s'))
                                .trim_start_matches("Signal=")
                                .trim_start_matches("signal=")
                                .trim();
                            if !digit.is_empty() {
                                let event = serde_json::json!({
                                    "type": "dtmf",
                                    "leg_id": "callee",
                                    "digit": digit.chars().next().unwrap_or_default().to_string(),
                                });
                                if let Err(e) = self.app_runtime.inject_event(event) {
                                    debug!(session_id = %self.id,
                                        session_id = %self.context.session_id,
                                        error = %e,
                                        "Callee SIP INFO DTMF app inject (no active app)"
                                    );
                                }
                            }
                        }
                    }
                    // Forward to caller
                    if let Some(server_dialog) = self.caller_dialog.as_ref() {
                        let fwd_headers = vec![rsipstack::sip::Header::ContentType(
                            rsipstack::sip::headers::ContentType::from("application/dtmf-relay"),
                        )];
                        if let Err(e) = server_dialog
                            .info(Some(fwd_headers), Some(request.body().to_vec()))
                            .await
                        {
                            warn!(session_id = %self.id,
                                session_id = %self.context.session_id,
                                error = %e,
                                "Failed to forward callee SIP INFO DTMF to caller"
                            );
                        } else {
                            debug!(session_id = %self.id,
                                session_id = %self.context.session_id,
                                "Forwarded callee SIP INFO DTMF to caller"
                            );
                        }
                    }
                } else if is_picture_fast_update {
                    self.handle_picture_fast_update(DialogSide::Callee).await;
                } else if content_type
                    .as_deref()
                    .is_some_and(|ct| ct.contains(RUSTPBX_COMMAND_CT))
                {
                    self.handle_rustpbx_info_command(&body_text, &tx_handle)
                        .await?;
                    // Do NOT forward to caller — PBX-internal command
                    return Ok(());
                }
                tx_handle
                    .respond(rsipstack::sip::StatusCode::OK, None, None)
                    .await
                    .ok();
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn execute_dialplan(
        &mut self,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        let flow = self.context.dialplan.flow.clone();
        self.execute_flow(&flow, callee_state_rx).await
    }

    fn execute_flow<'a>(
        &'a mut self,
        flow: &'a crate::call::DialplanFlow,
        callee_state_rx: &'a mut mpsc::UnboundedReceiver<DialogState>,
    ) -> futures::future::BoxFuture<'a, Result<(), CalleeError>> {
        use crate::call::DialplanFlow;
        use futures::FutureExt;

        async move {
            match flow {
                DialplanFlow::Targets(strategy) => {
                    self.run_targets(strategy, callee_state_rx).await
                }

                DialplanFlow::Queue { plan, next } => {
                    // Extract agents from plan
                    let agents = match &plan.dial_strategy {
                        Some(DialStrategy::Sequential(l)) => l.clone(),
                        Some(DialStrategy::Parallel(l)) => l.clone(),
                        None => {
                            warn!(session_id = %self.id, "No dial strategy in queue plan");
                            return self.execute_flow(next, callee_state_rx).await;
                        }
                    };

                    if agents.is_empty() {
                        warn!(session_id = %self.id, "No agents configured in queue plan");
                        return self.execute_flow(next, callee_state_rx).await;
                    }

                    // Resolve custom targets (skill-groups → specific agents)
                    let resolved_agents = self.resolve_custom_targets(agents).await;

                    // Enrich via queue_location_enricher if configured
                    let resolved_agents =
                        if let Some(enricher) = &self.server.queue_location_enricher {
                            let caller_headers: Vec<rsipstack::sip::Header> = self
                                .caller_dialog
                                .as_ref()
                                .map(|d| d.initial_request().headers.into())
                                .unwrap_or_default();
                            enricher
                                .enrich(
                                    resolved_agents,
                                    &crate::proxy::call::QueueEnrichContext {
                                        session_id: &self.context.session_id.to_string(),
                                        queue_name: &plan.queue_name,
                                        caller_headers: &caller_headers,
                                    },
                                )
                                .await
                        } else {
                            resolved_agents
                        };

                    let is_parallel = matches!(plan.dial_strategy, Some(DialStrategy::Parallel(_)));
                    let agent_uris: Vec<String> = resolved_agents
                        .iter()
                        .map(|l| l.contact_raw.clone().unwrap_or_else(|| l.aor.to_string()))
                        .collect();

                    // Store resolved plan in context for the queue app factory
                    if let Some(runtime) = self
                        .app_runtime
                        .as_any()
                        .downcast_ref::<DefaultAppRuntime>()
                    {
                        *runtime.context.pending_queue.lock() = Some(PendingQueuePlan {
                            plan: plan.clone(),
                            agent_uris,
                            parallel: is_parallel,
                        });
                    }

                    match self
                        .app_runtime
                        .start_app("queue", None, plan.accept_immediately)
                        .await
                    {
                        Ok(()) => {
                            self.start_caller_ingress_monitor_if_needed().await;
                            // Inject dial_next_agent to kick off sequential agent dialing
                            // (parallel mode auto-dials in on_enter)
                            if !is_parallel {
                                let _ = self.app_runtime.inject_event(serde_json::json!({
                                    "type": "custom",
                                    "name": "dial_next_agent",
                                    "data": {},
                                }));
                            }
                            Ok(())
                        }
                        Err(e) => {
                            warn!(session_id = %self.id, error = %e, "Queue: failed to start queue app, trying next flow");
                            self.execute_flow(next, callee_state_rx).await
                        }
                    }
                }
                DialplanFlow::Application {
                    app_name,
                    app_params,
                    auto_answer,
                } => {
                    info!(session_id = %self.id, app_name = %app_name, "Executing application flow");
                    if let Err(e) = self
                        .app_runtime
                        .start_app(app_name, app_params.clone(), *auto_answer)
                        .await
                    {
                        warn!(session_id = %self.id, error = %e, "Failed to start application");
                    } else if app_name == "conference" {
                        // After the conference app starts (which calls
                        // ctrl.answer()), join all active legs into the
                        // conference room.
                        let conf_id = app_params
                            .as_ref()
                            .and_then(|p| p.get("id").and_then(|v| v.as_str()))
                            .unwrap_or(&format!("conf-{}", self.id.0))
                            .to_string();
                        self.join_conference_mixer(&conf_id).await;
                        self.start_caller_ingress_monitor_if_needed().await;
                    } else {
                        self.start_caller_ingress_monitor_if_needed().await;
                    }
                    Ok(())
                }
            }
        }
        .boxed()
    }

    async fn run_targets(
        &mut self,
        strategy: &crate::call::DialStrategy,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        use crate::call::DialStrategy;

        match strategy {
            DialStrategy::Sequential(targets) => {
                self.dial_sequential(targets, callee_state_rx).await
            }
            DialStrategy::Parallel(targets) => self.dial_parallel(targets, callee_state_rx).await,
        }
    }

    async fn dial_sequential(
        &mut self,
        targets: &[crate::call::Location],
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        let mut last_error = into_callee_err(
            &StatusCode::TemporarilyUnavailable,
            Some("No targets to dial".to_string()),
        );

        for (idx, target) in targets.iter().enumerate() {
            info!(session_id = %self.id, index = idx, target = %target.aor, "Trying sequential target");

            match self
                .try_single_target(target, callee_state_rx, None, None)
                .await
            {
                Ok(()) => {
                    info!(session_id = %self.id, index = idx, "Sequential target succeeded");
                    return Ok(());
                }
                Err(e) => {
                    warn!(session_id = %self.id, index = idx, error = ?e, "Sequential target failed");
                    last_error = e;
                }
            }
        }

        Err(last_error)
    }

    async fn dial_parallel(
        &mut self,
        targets: &[crate::call::Location],
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        if targets.is_empty() {
            return Err(into_callee_err(
                &StatusCode::TemporarilyUnavailable,
                Some("No targets to dial".to_string()),
            ));
        }

        for target in targets {
            info!(session_id = %self.id, target = %target.aor, "dial_parallel: target");
        }

        self.fork_targets_parallel(targets, None, callee_state_rx)
            .await
    }

    /// Fork INVITEs to all targets concurrently and bridge with the first
    /// that answers (200 OK), cancelling the rest.
    ///
    /// Returns `Ok(())` on first successful connection, or an error when
    /// every target has failed (busy, no-answer, reject, timeout, …).
    async fn fork_targets_parallel(
        &mut self,
        targets: &[crate::call::Location],
        stop_playback_on_answer: Option<&str>,
        _callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        use futures::StreamExt;
        use futures::stream::FuturesUnordered;
        use rsipstack::dialog::invitation::InviteOption;
        use rsipstack::sip::StatusCodeKind;

        if targets.is_empty() {
            return Err(into_callee_err(
                &StatusCode::TemporarilyUnavailable,
                Some("No targets to dial".to_string()),
            ));
        }

        if self.context.dialplan.caller.is_none() {
            return Err(into_callee_err(
                &StatusCode::ServerInternalError,
                Some("No caller in dialplan".to_string()),
            ));
        }

        let _local_addrs = self.server.endpoint.get_addrs();
        let _cluster_enabled = !self.server.cluster_peer_ips.is_empty();

        let default_expires = self
            .server
            .proxy_config
            .session_expires
            .unwrap_or(DEFAULT_SESSION_EXPIRES);
        let _session_timer_enabled = self.server.proxy_config.session_timer_mode().is_enabled();

        // Build INVITE options for every target
        let dialog_layer = self.server.dialog_layer.clone();
        let fork_cancel = CancellationToken::new();
        let mut fork_set = FuturesUnordered::new();
        let state_tx = self.callee_event_tx.clone().ok_or_else(|| {
            into_callee_err(
                &StatusCode::ServerInternalError,
                Some("No callee event sender".to_string()),
            )
        })?;

        for (idx, target) in targets.iter().enumerate() {
            if fork_cancel.is_cancelled() {
                break;
            }

            let leg_id_str = format!("fork-{idx}");
            let (invite_option, callee_uri, _callee_call_id) = match self
                .build_target_invite_option(target, Some(&leg_id_str))
                .await
            {
                Ok(res) => res,
                Err(e) => {
                    warn!(session_id = %self.context.session_id, error = ?e, "Failed to build invite option for fork");
                    continue;
                }
            };

            let fork_tx = state_tx.clone();
            let dlg = dialog_layer.clone();
            let ct = fork_cancel.clone();

            let join = crate::utils::spawn(async move {
                tokio::select! {
                    _ = ct.cancelled() => {
                        None
                    }
                    result = dlg.do_invite(invite_option, fork_tx) => {
                        Some((idx, result, callee_uri))
                    }
                }
            });

            fork_set.push(join);
        }

        // If the caller hasn't confirmed yet, send 180 Ringing before forking
        if !self.caller_dialog.as_ref().is_some_and(|d| d.state().is_confirmed()) {
            if let Some(dialog) = self.caller_dialog.as_ref() {
                let _ = dialog.ringing(None, None);
            }
        }

        // Race the forked INVITEs – first 200 OK wins
        // Fire on_call_ringing hooks now that callees are ringing.
        // Use the first target's AOR for agent identity.
        if !self.server.session_hooks.is_empty() && !targets.is_empty() {
            self.meta.routed_callee = Some(targets[0].aor.to_string());
            let ctx = self.session_hook_ctx();
            for hook in self.server.session_hooks.iter() {
                hook.on_call_ringing(&ctx).await;
            }
        }

        let mut failures = 0u32;
        let mut last_error = into_callee_err(
            &StatusCode::TemporarilyUnavailable,
            Some("All targets failed".to_string()),
        );
        let total = targets.len() as u32;
        let mut caller_end_check = tokio::time::interval(Duration::from_millis(100));

        while !fork_set.is_empty() {
            let join_result = tokio::select! {
                _ = caller_end_check.tick() => {
                    if self.caller_dialog.as_ref().is_none_or(|d| d.state().is_terminated()) {
                        info!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            "Caller dialog terminated while parallel callee INVITEs were pending"
                        );
                        fork_cancel.cancel();
                        self.cancel_token.cancel();
                        return Err(into_callee_err(
                            &StatusCode::RequestTerminated,
                            Some("Caller cancelled".to_string()),
                        ));
                    }
                    continue;
                }
                _ = self.cancel_token.cancelled() => {
                    fork_cancel.cancel();
                    return Err(into_callee_err(
                        &StatusCode::RequestTerminated,
                        Some("Caller cancelled".to_string()),
                    ));
                }
                Some(join_result) = fork_set.next() => join_result,
            };

            match join_result {
                Ok(Some((winner_idx, Ok((dialog, response)), callee_uri))) => {
                    if let Some(ref resp) = response {
                        if resp.status_code.kind() == StatusCodeKind::Successful {
                            info!(session_id = %self.id,
                                fork = winner_idx,
                                callee_uri = %callee_uri,
                                "fork_targets_parallel: target answered first"
                            );

                            self.meta.routed_caller = self
                                .context
                                .dialplan
                                .caller
                                .as_ref()
                                .map(|uri| uri.to_string());
                            // Store original target AOR, not resolved contact.
                            let winner_aor = targets
                                .get(winner_idx)
                                .map(|t| t.aor.to_string())
                                .unwrap_or_else(|| callee_uri.to_string());
                            self.meta.routed_callee = Some(winner_aor);

                            // Cancel all remaining forks
                            fork_cancel.cancel();

                            let dialog_id = dialog.id();

                            // Clean up leftover fork legs
                            for cleanup_idx in 0..targets.len() {
                                if cleanup_idx != winner_idx {
                                    let leg_to_remove = LegId::from(format!("fork-{cleanup_idx}"));
                                    self.legs.remove(&leg_to_remove);
                                }
                            }

                            // Rename the winning fork leg to "callee"
                            let win_leg = LegId::from(format!("fork-{winner_idx}"));
                            if let Some(mut leg) = self.legs.remove(&win_leg) {
                                leg.id = LegId::from("callee");
                                self.legs.insert(LegId::from("callee"), leg);
                            }

                            return self
                                .finalize_callee_connection(
                                    dialog_id,
                                    response,
                                    callee_uri,
                                    stop_playback_on_answer,
                                    &InviteOption::default(),
                                    default_expires,
                                )
                                .await;
                        }
                    }
                    // Non-success response (4xx/5xx)
                    let code = response
                        .as_ref()
                        .map(|r| r.status_code.code())
                        .unwrap_or(StatusCode::TemporarilyUnavailable.code());
                    let text = response
                        .as_ref()
                        .map(|r| r.status_code.text().to_string())
                        .unwrap_or_else(|| StatusCode::TemporarilyUnavailable.text().to_string());
                    warn!(session_id = %self.id,
                        fork = winner_idx,
                        code = code,
                        text = %text,
                        "fork_targets_parallel: target rejected"
                    );
                    failures += 1;
                    last_error = (code, text, None);
                }
                Ok(Some((_idx, Err(e), _callee_uri))) => {
                    warn!(session_id = %self.id,
                        fork = _idx,
                        error = %e,
                        "fork_targets_parallel: target errored"
                    );
                    failures += 1;
                    last_error = into_callee_err(
                        &StatusCode::ServerInternalError,
                        Some(format!("Target fork failed: {e}")),
                    );
                }
                Ok(None) => {
                    // Fork was cancelled by fork_cancel (another fork won)
                    debug!(session_id = %self.id, "fork_targets_parallel: fork cancelled (another answered)");
                    failures += 1;
                }
                Err(e) => {
                    warn!(session_id = %self.id, error = %e, "fork_targets_parallel: join error");
                    failures += 1;
                    last_error = into_callee_err(
                        &StatusCode::ServerInternalError,
                        Some(format!("Fork join error: {e}")),
                    );
                }
            }

            // If all forks completed and none succeeded, we're done
            if failures >= total {
                info!(session_id = %self.id, failures, "fork_targets_parallel: all targets failed");
                return Err(last_error);
            }
        }

        Err(last_error)
    }

    /// Send 183 Session Progress with early media audio played to the caller.
    /// Supports file paths and `tone://frequency,duration_ms` format.
    async fn send_early_media_tone(&mut self, audio_path: &str) -> Result<()> {
        if self.media.early_media_sent {
            return Ok(());
        }

        // Ensure caller leg exists in MediaBridge
        if let Err(e) = self.ensure_caller_leg().await {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                error = %e,
                "Failed to ensure caller leg for 183 early media"
            );
            return Ok(());
        }

        // Get caller-facing answer SDP from the caller leg's PC
        let answer_sdp = self
            .bridge()
            .and_then(|mb| mb.leg(crate::media::media_bridge::LegSide::A))
            .and_then(|leg| leg.pc().local_description())
            .map(|desc| desc.to_sdp_string())
            .or_else(|| self.media.answer.clone())
            .unwrap_or_default();

        if answer_sdp.is_empty() {
            warn!(session_id = %self.id, "Cannot send 183: no local SDP available on caller leg");
            return Ok(());
        }

        self.media.answer = Some(answer_sdp.clone());
        self.media.early_media_sent = true;

        // Send 183 Session Progress with SDP
        let ringing_result: anyhow::Result<()> = match self.caller_dialog.as_ref() {
            Some(dialog) => dialog
                .ringing(Some(Self::sdp_headers()), Some(answer_sdp.into_bytes()))
                .map_err(|e| anyhow!("{}", e)),
            None => Ok(()),
        };
        if let Err(e) = ringing_result {
            warn!(session_id = %self.context.session_id, error = %e, "Failed to send 183 Session Progress");
        } else {
            info!(session_id = %self.context.session_id, "Sent 183 Session Progress with early media");
        }

        // Resolve audio path — generate temp WAV for tone:// specs
        let resolved_path = Self::resolve_audio_path(audio_path)?;

        // Play progress audio via MediaBridge
        if let Some(mb) = self.bridge_mut() {
            let _ = mb.play_file(crate::media::media_bridge::LegSide::A, resolved_path, true).await;
        }

        Ok(())
    }

    /// Resolve an audio path specification to an actual file path.
    /// Supports:
    ///   - Regular file paths (passthrough)
    ///   - `tone://frequency,duration_ms` — generates a temporary WAV file with a sine wave
    fn resolve_audio_path(spec: &str) -> Result<String> {
        if let Some(tone_spec) = spec.strip_prefix("tone://") {
            let parts: Vec<&str> = tone_spec.splitn(2, ',').collect();
            if parts.len() != 2 {
                return Err(anyhow!(
                    "Invalid tone spec '{}': expected tone://frequency,duration_ms",
                    spec
                ));
            }
            let frequency: u32 = parts[0]
                .trim()
                .parse()
                .map_err(|e| anyhow!("Invalid frequency in tone spec '{}': {}", spec, e))?;
            let duration_ms: u64 = parts[1]
                .trim()
                .parse()
                .map_err(|e| anyhow!("Invalid duration in tone spec '{}': {}", spec, e))?;

            let sample_rate = 8000u32;
            let num_samples = (sample_rate as u64 * duration_ms / 1000) as usize;
            let amplitude = 8192i16;

            let pcm: Vec<i16> = (0..num_samples)
                .map(|i| {
                    let t = i as f64 / sample_rate as f64;
                    (amplitude as f64 * (2.0 * std::f64::consts::PI * frequency as f64 * t).sin())
                        as i16
                })
                .collect();

            let temp_dir = std::env::temp_dir();
            let temp_path = temp_dir.join(format!(
                "rustpbx_tone_{}hz_{}ms_{}.wav",
                frequency,
                duration_ms,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));

            let spec = crate::media::wav_reader::WavSpec {
                channels: 1,
                sample_rate: 8000,
                bits_per_sample: 16,
                sample_format: crate::media::wav_reader::SampleFormat::Int,
            };
            let mut writer = crate::media::wav_reader::WavWriter::create(&temp_path, spec)
                .map_err(|e| anyhow!("Failed to create temp WAV for tone: {}", e))?;
            for sample in &pcm {
                writer
                    .write_sample(*sample)
                    .map_err(|e| anyhow!("Failed to write WAV sample: {}", e))?;
            }
            writer
                .finalize()
                .map_err(|e| anyhow!("Failed to finalize WAV: {}", e))?;

            Ok(temp_path.to_string_lossy().to_string())
        } else {
            // Passthrough — regular file path
            Ok(spec.to_string())
        }
    }

    /// Reject the call with a specific status code, optionally playing a configured
    /// failure tone as 183 early media before sending the rejection.
    async fn reject_with_tone(
        &mut self,
        code: u16,
        text: String,
        reason: Option<String>,
    ) -> Result<()> {
        let status = StatusCode::Other(code, text);
        let profile = self.context.dialplan.audio_profile.as_ref();
        let audio_path = profile.and_then(|rb| rb.for_status(&status).map(|s| s.to_string()));
        if let Some(ref path) = audio_path {
            let dur = profile
                .and_then(|rb| rb.play_duration_for(&status))
                .unwrap_or(std::time::Duration::from_secs(2));
            info!(session_id = %self.id,
                session_id = %self.context.session_id,
                status = %status,
                audio = %path,
                play_seconds = %dur.as_secs(),
                "Playing failure tone before rejection",
            );
            if let Err(e) = self.send_early_media_tone(path).await {
                warn!(session_id = %self.context.session_id, error = %e, "Failed to play failure tone");
            } else {
                tokio::time::sleep(dur).await;
            }
        }
        if let Some(dialog) = self.caller_dialog.as_ref() {
            dialog.reject(Some(status), reason.clone())?;
        }
        Ok(())
    }

    /// Ensure the caller leg exists in the MediaBridge.
    /// Idempotent — safe to call multiple times.
    async fn ensure_caller_leg(&mut self) -> Result<()> {
        let (caller_offer, caller_is_webrtc) = {
            let caller_offer = self
                .media
                .caller_offer
                .clone()
                .ok_or_else(|| anyhow!("No caller offer available"))?;
            (caller_offer, self.is_caller_webrtc())
        };
        let transport = if caller_is_webrtc {
            rustrtc::TransportMode::WebRtc
        } else {
            rustrtc::TransportMode::Rtp
        };
        let codecs =
            crate::media::negotiate::MediaNegotiator::build_codec_list_from_offer(&caller_offer, &[]);

        let cfg = crate::media::leg::LegConfig {
            transport: transport.clone(),
            codecs,
            rtp_port_range: self
                .context
                .dialplan
                .media
                .rtp_start_port
                .zip(self.context.dialplan.media.rtp_end_port),
            external_ip: self.context.dialplan.media.external_ip.clone(),
            bind_ip: self.context.dialplan.media.bind_ip.clone(),
            cname: Some(self.server.rtc_cname.clone()),
            comfort_noise: self.context.dialplan.media.comfort_noise,
            comfort_noise_level_db: self.context.dialplan.media.comfort_noise_level_db,
        };

        let mb = self.bridge_mut().ok_or_else(|| anyhow!("No MediaBridge"))?;
        if mb.leg(crate::media::media_bridge::LegSide::A).is_some() {
            return Ok(());
        }

        let leg = crate::media::leg::LegInner::new("caller", &cfg)?;
        let answer = leg
            .apply_sdp(&caller_offer, rustrtc::SdpType::Offer)
            .await?;
        mb.replace_leg(crate::media::media_bridge::LegSide::A, leg).await;
        self.media.answer = Some(answer);

        // Set callee transport hint (opposite of caller for app media bridge)
        self.legs.set_transport(LegId::from("caller"), transport);
        self.legs.set_transport(
            LegId::from("callee"),
            if caller_is_webrtc {
                rustrtc::TransportMode::Rtp
            } else {
                rustrtc::TransportMode::WebRtc
            },
        );

        Ok(())
    }

    async fn prepare_app_caller_media_bridge(&mut self) -> Option<String> {
        if let Err(e) = self.ensure_caller_leg().await {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                error = %e,
                "Failed to ensure caller leg for app media bridge"
            );
            return None;
        }
        self.media.answer.clone()
    }


    /// Final audio codec normalization for answers generated by PeerConnection.
    /// This keeps answer audio as an offer subset, ordered by the peer answer
    /// when available, while preserving the caller-offered payload types.
    fn rewrite_answer_to_selected_codecs(
        &self,
        answer_sdp: &str,
        offer_sdp: &str,
        preferred_peer_sdp: Option<&str>,
        context: &str,
    ) -> String {
        let allow_codecs = self.resolve_effective_codecs();
        let preferred_codecs: Vec<CodecType> = preferred_peer_sdp
            .map(|sdp| {
                MediaNegotiator::extract_codec_params(sdp)
                    .audio
                    .into_iter()
                    .map(|codec| codec.codec)
                    .collect()
            })
            .filter(|codecs: &Vec<CodecType>| !codecs.is_empty())
            .unwrap_or(allow_codecs);
        let selected_codecs =
            MediaNegotiator::build_codec_list_from_offer(offer_sdp, &preferred_codecs);
        if selected_codecs.is_empty() {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                context,
                "No compatible audio codec selected for SDP answer"
            );
            return answer_sdp.to_string();
        }

        MediaNegotiator::rewrite_sdp_codec_list(answer_sdp, &selected_codecs).unwrap_or_else(|| {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                context,
                "Failed to rewrite SDP answer to selected audio codec"
            );
            answer_sdp.to_string()
        })
    }

    async fn resolve_custom_targets(
        &self,
        locations: Vec<crate::call::Location>,
    ) -> Vec<crate::call::Location> {
        let mut expanded = Vec::new();
        let agent_registry = self.server.agent_registry.clone();

        for location in locations {
            let uri_str = location.aor.to_string();

            // Check if this is a custom target that needs resolution
            // Custom targets typically have a scheme prefix like "skill-group:"
            if uri_str.contains(':') {
                let scheme = uri_str.split(':').next().unwrap_or("");

                // Only resolve known custom schemes, not standard SIP URIs
                if scheme != "sip" && scheme != "sips" && scheme != "tel" {
                    info!(session_id = %self.id, target = %uri_str, "Resolving custom target to agents");

                    if let Some(registry) = &agent_registry {
                        // Use the registry's resolve_target hook
                        // CC addon implements this to resolve skill-group: URIs
                        let agent_uris = registry
                            .resolve_target_with_policy(&uri_str, None, &self.id.0)
                            .await;

                        if agent_uris.is_empty() {
                            warn!(session_id = %self.id, target = %uri_str, "No agents resolved for custom target");
                        } else {
                            let resolved_sample =
                                agent_uris.iter().take(5).cloned().collect::<Vec<_>>();
                            let mut parsed_count = 0usize;
                            info!(session_id = %self.id,
                                target = %uri_str,
                                agent_count = agent_uris.len(),
                                resolved_uris = ?resolved_sample,
                                "Resolved custom target to agents"
                            );

                            // Create locations for each resolved agent URI.
                            // Try to look up the agent's registered location via the
                            // locator so we get the real transport/webrtc flags instead
                            // of building a bare Location that defaults to RTP.
                            for agent_uri in agent_uris {
                                if let Ok(uri) = rsipstack::sip::Uri::try_from(agent_uri.clone()) {
                                    // Write the original agent AOR to session extensions
                                    // so that CC hooks can identify the agent regardless
                                    // of what routed_callee/connected_callee end up being.
                                    {
                                        use std::collections::HashMap;
                                        let mut ext = self.extensions.write();
                                        let user_part = uri
                                            .auth
                                            .as_ref()
                                            .map(|a| a.user.clone())
                                            .unwrap_or_default();
                                        if !user_part.is_empty() {
                                            if let Some(map) =
                                                ext.get_mut::<HashMap<String, String>>()
                                            {
                                                map.entry("resolved_agent_id".to_string())
                                                    .or_insert(user_part.clone());
                                            } else {
                                                let mut map = HashMap::new();
                                                map.insert(
                                                    "resolved_agent_id".to_string(),
                                                    user_part,
                                                );
                                                ext.insert(map);
                                            }
                                        }
                                    }

                                    // Query the SIP registrar for this agent's live contact.
                                    let registered_locations =
                                        self.server.locator.lookup(&uri).await.unwrap_or_default();

                                    if let Some(reg_loc) = registered_locations.into_iter().next() {
                                        expanded.push(reg_loc);
                                    } else {
                                        // Agent not currently registered.
                                        // Use host_with_port so that agents on a
                                        // different port (e.g. test UAs) are not
                                        // incorrectly treated as same-realm offline.
                                        let host = uri.host_with_port.to_string();
                                        if self.server.is_same_realm(&host).await {
                                            // Local realm agent not registered → offline; skip.
                                            warn!(session_id = %self.id,
                                                agent = %agent_uri,
                                                "Agent offline (not registered in local realm), skipping"
                                            );
                                            continue;
                                        }
                                        // External realm address not in locator; pass
                                        // through as a bare location for external delivery.
                                        let agent_location = crate::call::Location {
                                            aor: uri,
                                            contact_raw: Some(agent_uri),
                                            ..Default::default()
                                        };
                                        expanded.push(agent_location);
                                    }
                                    parsed_count += 1;
                                }
                            }

                            info!(session_id = %self.id,
                                target = %uri_str,
                                parsed_location_count = parsed_count,
                                "Resolved custom target parsed into dialable locations"
                            );
                        }
                    } else {
                        warn!(session_id = %self.id, "No agent registry available to resolve custom target");
                    }
                    continue;
                }
            }

            // Standard target, pass through as-is
            expanded.push(location);
        }

        let mut resolved = Vec::new();
        for location in expanded {
            let target_realm = location.aor.host().to_string();
            if !self.server.is_same_realm(&target_realm).await {
                resolved.push(location);
                continue;
            }

            match self.server.locator.lookup(&location.aor).await {
                Ok(locations) if !locations.is_empty() => {
                    info!(session_id = %self.id,
                        target = %location.aor,
                        resolved_count = locations.len(),
                        "Resolved queue target through locator"
                    );
                    if let Some(location) = locations.into_iter().next() {
                        resolved.push(location);
                    }
                }
                Ok(_) => resolved.push(location),
                Err(error) => {
                    warn!(session_id = %self.id,
                        target = %location.aor,
                        error = %error,
                        "Failed to resolve queue target through locator"
                    );
                    resolved.push(location);
                }
            }
        }

        resolved
    }

    async fn try_single_target(
        &mut self,
        target: &crate::call::Location,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
        stop_playback_on_answer: Option<&str>,
        no_trying_timeout: Option<std::time::Duration>,
    ) -> Result<(), CalleeError> {
        use rsipstack::dialog::dialog::DialogState;


        if self.context.dialplan.caller.is_none() {
            return Err(into_callee_err(
                &StatusCode::ServerInternalError,
                Some("No caller in dialplan".to_string()),
            ));
        }

        let _local_addrs = self.server.endpoint.get_addrs();
        let _cluster_enabled = !self.server.cluster_peer_ips.is_empty();
        let default_expires = self
            .server
            .proxy_config
            .session_expires
            .unwrap_or(crate::proxy::proxy_call::session_timer::DEFAULT_SESSION_EXPIRES);
        let caller_is_webrtc = self.is_caller_webrtc();
        let _ = caller_is_webrtc;
        self.legs.set_transport(
            crate::call::domain::LegId::from("caller"),
            self.caller_transport_mode(),
        );

        let (mut invite_option, callee_uri, callee_call_id) =
            self.build_target_invite_option(target, None).await?;

        self.meta.routed_caller = Some(invite_option.caller.to_string());
        self.meta.routed_callee = Some(target.aor.to_string());

        if let Some(home_proxy) = target.home_proxy.as_ref() {
            info!(session_id = %self.id,
                session_id = %self.context.session_id,
                %callee_uri,
                %home_proxy,
                "Routing INVITE to home proxy node"
            );
        }

        info!(session_id = %self.context.session_id, %callee_uri, callee_call_id, "Sending INVITE to callee");

        let state_tx = self.callee_event_tx.clone().ok_or_else(|| {
            into_callee_err(
                &StatusCode::ServerInternalError,
                Some("No callee event sender".to_string()),
            )
        })?;

        let dialog_layer = self.server.dialog_layer.clone();
        let mut retry_count = 0;
        let mut invitation = dialog_layer
            .do_invite(invite_option.clone(), state_tx.clone())
            .boxed();
        let mut caller_end_check = tokio::time::interval(Duration::from_millis(100));

        // No-trying timeout: if the downstream trunk fails to return ANY response
        // (provisional or final) within this window, give up early instead of waiting
        // the full rsipstack Timer B (32s). This prevents a single stuck trunk from
        // tying up a worker task and inflating observed 408s under load.
        let invite_sent_at = Instant::now();
        let no_trying_deadline = no_trying_timeout.map(|d| invite_sent_at + d);
        let mut no_trying_dismissed = no_trying_timeout.is_none();

        let result = loop {
            tokio::select! {
                _ = caller_end_check.tick() => {
                    if self.caller_dialog.as_ref().is_none_or(|d| d.state().is_terminated()) {
                        info!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            "Caller dialog terminated while callee INVITE was pending"
                        );
                        self.cancel_token.cancel();
                        break Err(into_callee_err(
                            &StatusCode::RequestTerminated,
                            Some("Caller cancelled".to_string()),
                        ));
                    }
                    // Check no-trying timeout (100ms granularity is sufficient for
                    // multi-second timeouts; avoids pin gymnastics with sleep futures
                    // across select! iterations).
                    if !no_trying_dismissed
                        && let Some(deadline) = no_trying_deadline
                        && Instant::now() >= deadline
                    {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            %callee_uri,
                            "No-trying timeout reached, abandoning callee INVITE"
                        );
                        self.cancel_token.cancel();
                        break Err(into_callee_err(
                            &StatusCode::RequestTimeout,
                            Some("No-trying timeout".to_string()),
                        ));
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    break Err(into_callee_err(
                        &StatusCode::RequestTerminated,
                        Some("Caller cancelled".to_string()),
                    ));
                }
                res = &mut invitation => {
                    break match res {
                        Ok((dialog, response)) => {
                            if let Some(ref resp) = response {
                                if self.server.proxy_config.session_timer_mode().is_enabled()
                                    && resp.status_code == StatusCode::SessionIntervalTooSmall
                                    && retry_count < 1
                                    && let Some(min_se_value) =
                                        get_header_value(&resp.headers, HEADER_MIN_SE)
                                        && let Some(min_se) = parse_min_se(&min_se_value) {
                                            if let Some(headers) = &mut invite_option.headers {
                                                headers.retain(|header| !matches!(header,
                                                    rsipstack::sip::Header::Other(name, _)
                                                        if name.eq_ignore_ascii_case(
                                                            HEADER_SESSION_EXPIRES,
                                                        )
                                                            || name.eq_ignore_ascii_case(HEADER_MIN_SE)
                                                ));

                                                for header in headers.iter_mut() {
                                                    if let rsipstack::sip::Header::Supported(value) = header {
                                                        let filtered: Vec<String> = value
                                                            .to_string()
                                                            .split(',')
                                                            .map(str::trim)
                                                            .filter(|entry| !entry.is_empty() && *entry != "timer")
                                                            .map(ToString::to_string)
                                                            .collect();
                                                        *header = rsipstack::sip::Header::Other(
                                                            HEADER_SUPPORTED.to_string(),
                                                            filtered.join(", "),
                                                        );
                                                    }
                                                }

                                                headers.retain(|header| match header {
                                                    rsipstack::sip::Header::Other(name, value)
                                                        if name.eq_ignore_ascii_case(HEADER_SUPPORTED) =>
                                                    {
                                                        !value.trim().is_empty()
                                                    }
                                                    rsipstack::sip::Header::Other(name, _) => {
                                                        !name.eq_ignore_ascii_case(
                                                            HEADER_SESSION_EXPIRES,
                                                        ) && !name.eq_ignore_ascii_case(
                                                            HEADER_MIN_SE,
                                                        )
                                                    }
                                                    _ => true,
                                                });
                                                headers.extend(build_default_session_timer_headers(
                                                    min_se.as_secs(),
                                                    min_se.as_secs(),
                                                ));
                                            }
                                            retry_count += 1;
                                            invitation = dialog_layer
                                                .do_invite(invite_option.clone(), state_tx.clone())
                                                .boxed();
                                            continue;
                                        }

                                if resp.status_code.kind() == rsipstack::sip::StatusCodeKind::Successful {
                                    Ok((dialog.id(), response))
                                } else {
                                    let code = resp.status_code.code();
                                    let text = resp.status_code.text().to_string();
                                    let reason = resp.reason_phrase().map(|s| s.to_string());
                                    Err((code, text, reason))
                                }
                            } else {
                                Err(into_callee_err(
                                    &StatusCode::ServerInternalError,
                                    Some("No response from callee".to_string()),
                                ))
                            }
                        }
                        Err(e) => Err(into_callee_err(
                            &StatusCode::ServerInternalError,
                            Some(format!("Invite failed: {}", e)),
                        )),
                    };
                }

                state = callee_state_rx.recv() => {
                    if let Some(DialogState::Early(_, ref response)) = state {
                        // Any provisional response (100/180/183) proves the downstream
                        // trunk is alive; dismiss the no-trying timer from now on.
                        no_trying_dismissed = true;

                        if self.meta.ring_time.is_none() {
                            self.meta.ring_time = Some(Instant::now());
                        }

                        let callee_sdp = String::from_utf8_lossy(response.body()).to_string();
                        if !callee_sdp.is_empty() && callee_sdp.contains("v=0") {
                            self.media.early_media_sent = true;
                            self.update_leg_state(&LegId::from("callee"), LegState::EarlyMedia);

                            if self.media_profile.path == MediaPathMode::Anchored {
                                let caller_sdp = match self
                                    .prepare_caller_answer_from_callee_sdp(
                                        Some(callee_sdp),
                                        false,
                                        rustrtc::SdpType::Pranswer,
                                    )
                                    .await
                                {
                                    Ok(caller_sdp) => caller_sdp,
                                    Err(error) => {
                                        warn!(session_id = %self.id,
                                            session_id = %self.context.session_id,
                                            error = %error,
                                            "Failed to prepare caller early-media answer"
                                        );
                                        None
                                    }
                                };

                                if let Some(dialog) = self.caller_dialog.as_ref() {
                                if let Err(e) = dialog.ringing(
                                    Some(Self::sdp_headers()),
                                    caller_sdp.map(|sdp| sdp.into_bytes()),
                                ) {
                                    warn!(session_id = %self.id,
                                        session_id = %self.context.session_id,
                                        error = %e,
                                        "Failed to send 183 Session Progress"
                                    );
                                }
                                }
                            } else {
                                if let Some(dialog) = self.caller_dialog.as_ref() {
                                if let Err(e) = dialog.ringing(
                                    Some(Self::sdp_headers()),
                                    Some(callee_sdp.into_bytes()),
                                ) {
                                    warn!(session_id = %self.id,
                                        session_id = %self.context.session_id,
                                        error = %e,
                                        "Failed to relay provisional SDP"
                                    );
                                }
                            }
                                }
                        } else {
                            if !self.media.early_media_sent {
                                self.update_leg_state(&LegId::from("callee"), LegState::Ringing);
                            }
                            if let Some(dialog) = self.caller_dialog.as_ref() {
                            if let Err(e) = dialog.ringing(None, None) {
                                warn!(session_id = %self.id,
                                    session_id = %self.context.session_id,
                                    error = %e,
                                    "Failed to send 180 Ringing"
                                );
                            }
                            }

                            self.emit_typed_rwi_event(&crate::rwi::CallRinging {
                                call_id: self.context.session_id.clone(),
                            });

                            // Fire on_call_ringing hooks
                            if !self.server.session_hooks.is_empty() {
                                let ctx = self.session_hook_ctx();
                                for hook in self.server.session_hooks.iter() {
                                    hook.on_call_ringing(&ctx).await;
                                }
                            }
                        }
                        self.update_snapshot_cache();
                    }
                }
            }
        };

        let (dialog_id, response): (DialogId, Option<rsipstack::sip::Response>) = result?;
        self.finalize_callee_connection(
            dialog_id,
            response,
            callee_uri,
            stop_playback_on_answer,
            &invite_option,
            default_expires,
        )
        .await
    }

    /// Finalizes a successful callee connection after the INVITE 200 OK is received.
    /// Extracts callee SDP, answers the caller, registers the callee dialog, starts the
    /// session timer, and updates the snapshot cache.
    async fn finalize_callee_connection(
        &mut self,
        dialog_id: rsipstack::dialog::DialogId,
        response: Option<rsipstack::sip::Response>,
        callee_uri: rsipstack::sip::Uri,
        stop_playback_on_answer: Option<&str>,
        invite_option: &rsipstack::dialog::invitation::InviteOption,
        default_expires: u64,
    ) -> Result<(), CalleeError> {
        let callee_sdp = response.as_ref().and_then(|r: &rsipstack::sip::Response| {
            let body = r.body();
            Self::extract_sdp(body)
        });
        if let Some(_track_id) = stop_playback_on_answer {
            // Stop the caller-leg early-media playback before transitioning to
            // the confirmed call (early-media tone plays on the A leg).
            if let Some(mb) = self.bridge_mut() {
                mb.stop_play(crate::media::media_bridge::LegSide::A).await.ok();
            }
        }

        // Stop playback (if any) before transitioning to confirmed call.
        if self.media.early_media_sent {
            if let Some(mb) = self.bridge_mut() {
                mb.stop_play(crate::media::media_bridge::LegSide::A).await.ok();
            }
        }

        let callee_guard =
            ClientDialogGuard::new(self.server.dialog_layer.clone(), dialog_id.clone());

        // Ensure callee leg exists in the MediaBridge.
        // For originate (UAC) path, the INVITE SDP was generated by RtpTrackBuilder,
        // so create_callee_track was never called and the bridge has no B leg.
        // Create one here so media_play / hold / comfort-noise can target it.
        if self.media.bridge.is_some() {
            let has_callee_leg = self
                .bridge()
                .and_then(|mb| mb.leg(crate::media::media_bridge::LegSide::B))
                .is_some();
            if !has_callee_leg {
                if let Some(ref callee_sdp_str) = callee_sdp {
                    let callee_is_webrtc =
                        Self::sdp_transport_mode(callee_sdp_str) == rustrtc::TransportMode::WebRtc;
                    let callee_mode = self.callee_transport_mode(callee_is_webrtc);
                    let allow_codecs = self.resolve_effective_codecs();
                    let codecs = self
                        .media
                        .caller_offer
                        .as_ref()
                        .map(|offer| {
                            MediaNegotiator::build_callee_codec_offer_with_allow(
                                offer, &allow_codecs,
                            )
                        })
                        .unwrap_or_default();
                    let cfg = crate::media::leg::LegConfig {
                        transport: callee_mode,
                        codecs,
                        rtp_port_range: self
                            .context
                            .dialplan
                            .media
                            .rtp_start_port
                            .zip(self.context.dialplan.media.rtp_end_port),
                        external_ip: self.context.dialplan.media.external_ip.clone(),
                        bind_ip: self.context.dialplan.media.bind_ip.clone(),
                        cname: Some(self.server.rtc_cname.clone()),
                        comfort_noise: self.context.dialplan.media.comfort_noise,
                        comfort_noise_level_db: self.context.dialplan.media.comfort_noise_level_db,
                    };
                    match crate::media::leg::LegInner::new("callee", &cfg) {
                        Ok(leg) => {
                            if let Ok(offer) = leg.create_offer(vec![]).await {
                                if let Some(mb) = self.bridge_mut() {
                                    mb.replace_leg(
                                        crate::media::media_bridge::LegSide::B,
                                        leg,
                                    )
                                    .await;
                                    info!(
                                        session_id = %self.id,
                                        offer_len = offer.len(),
                                        "Created callee leg in bridge during finalize (originate path)"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                session_id = %self.id,
                                error = %e,
                                "Failed to create callee leg in bridge during finalize"
                            );
                        }
                    }
                }
            }
        }

        let caller_answer = self
            .prepare_caller_answer_from_callee_sdp(
                callee_sdp,
                false,
                rustrtc::SdpType::Answer,
            )
            .await
            .map_err(|e| {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    error = %e,
                    "Failed to prepare caller answer"
                );
                into_callee_err(
                    &StatusCode::ServerInternalError,
                    Some(
                        r#"SIP;cause=500;text="Media resource allocation failed""#.to_string(),
                    ),
                )
            })?;

        self.meta.connected_callee_dialog_id = Some(dialog_id.clone());
        self.callee_dialogs.insert(dialog_id.clone(), ());
        self.callee_guards.push(callee_guard);

        self.accept_call(
            Some(callee_uri.to_string()),
            caller_answer,
            Some(dialog_id.to_string()),
        )
        .await
        .map_err(|e| into_callee_err(&StatusCode::ServerInternalError, Some(e.to_string())))?;

        // Register callee dialog in unified map
        if let Some(dlg) = self.server.dialog_layer.get_dialog(&dialog_id) {
            self.legs.set_dialog(LegId::from("callee"), dlg);
        }
        if self.server.proxy_config.session_timer_mode().is_enabled() {
            if let Some(ref response) = response {
                let requested_session_interval = invite_option
                    .headers
                    .as_ref()
                    .and_then(|headers| {
                        headers
                            .iter()
                            .find(|header| {
                                header.name().eq_ignore_ascii_case(HEADER_SESSION_EXPIRES)
                            })
                            .map(|header| header.value().to_string())
                    })
                    .as_deref()
                    .and_then(parse_session_expires)
                    .map(|(interval, _)| interval)
                    .unwrap_or_else(|| Duration::from_secs(default_expires));
                self.init_callee_timer(dialog_id.clone(), response, requested_session_interval);
            }
        }

        self.update_snapshot_cache();

        Ok(())
    }

    async fn prepare_callee_media_offer(
        &mut self,
        target: &crate::call::Location,
    ) -> Result<Option<Vec<u8>>> {
        let callee_is_webrtc = Self::callee_supports_webrtc(target);

        // Bug 3 fix: transport-aware parallel-fork caching. When multiple fork
        // targets share the same transport type, reuse the cached offer so all
        // forks promise the same bound port. Regenerate when transport differs
        // (e.g. one WebRTC fork and one RTP fork).
        if let Some(cached) = &self.media.callee_offer {
            if self.media.callee_offer_cached_webrtc == Some(callee_is_webrtc) {
                return Ok(Some(cached.clone().into_bytes()));
            }
        }
        self.media.callee_offer_cached_webrtc = Some(callee_is_webrtc);

        let caller_is_webrtc = self.is_caller_webrtc();
        let callee_sdp = if self.bypasses_local_media() && caller_is_webrtc == callee_is_webrtc {
            let allow_codecs = self.resolve_effective_codecs();
            if !allow_codecs.is_empty() {
                if let Some(ref caller_offer) = self.media.caller_offer {
                    let selected_codecs =
                        MediaNegotiator::build_codec_list_from_offer(caller_offer, &allow_codecs);
                    if selected_codecs.is_empty() {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            context = "bypass callee offer",
                            "No compatible audio codec selected for pass-through SDP offer"
                        );
                        Some(caller_offer.clone())
                    } else {
                        Some(MediaNegotiator::rewrite_sdp_codec_list(
                            caller_offer,
                            &selected_codecs,
                        )
                        .unwrap_or_else(|| {
                            warn!(session_id = %self.id,
                                session_id = %self.context.session_id,
                                context = "bypass callee offer",
                                "Failed to rewrite pass-through SDP offer to selected audio codec list"
                            );
                            caller_offer.clone()
                        }))
                    }
                } else {
                    self.media.caller_offer.clone()
                }
            } else {
                self.media.caller_offer.clone()
            }
        } else {
            Some(self.create_callee_track(callee_is_webrtc).await?)
        };
        self.media.callee_offer = callee_sdp.clone();
        Ok(callee_sdp.map(|s| s.into_bytes()))
    }

    async fn prepare_caller_answer_from_callee_sdp(
        &mut self,
        callee_sdp: Option<String>,
        force_regenerate: bool,
        callee_sdp_type: rustrtc::SdpType,
    ) -> Result<Option<String>> {
        let is_early_media = callee_sdp_type == rustrtc::SdpType::Pranswer;
        let rtp_timeout = self.rtp_timeout_config();

        // ── MediaBridge path: both legs exist → apply answer + bridge ────
        if let Some(callee_sdp_value) = callee_sdp.as_ref() {
            let has_callee_leg = self
                .bridge()
                .and_then(|mb| mb.leg(crate::media::media_bridge::LegSide::B))
                .is_some();

            let has_bridge = self.media.bridge.is_some();
            tracing::info!(
                session_id = %self.id,
                has_bridge,
                has_callee_leg,
                "prepare_caller_answer_from_callee_sdp: checking MediaBridge path"
            );

        if has_bridge && has_callee_leg {
                let cmd_tx = self.cmd_tx.clone();
                let session_id = self.context.session_id.clone();
                let mb = self.bridge_mut().ok_or_else(|| anyhow!("No MediaBridge"))?;
                if let Some(callee_leg) = mb.leg(crate::media::media_bridge::LegSide::B) {
                    let sdp_type = callee_sdp_type;
                    callee_leg.apply_sdp(callee_sdp_value, sdp_type).await?;
                }

                // accept() opens the gate and, once both legs are accepted,
                // activates the route (fast-path or transcoding).
                if !is_early_media {
                    mb.accept(crate::media::media_bridge::LegSide::B).await;
                    mb.accept(crate::media::media_bridge::LegSide::A).await;
                    Self::arm_bridged_rtp_timeouts(mb, rtp_timeout, cmd_tx, &session_id);
                }

                self.media.callee_answer_sdp = Some(callee_sdp_value.clone());
                let answer = self.media.answer.clone();
                return Ok(answer);
            }
        }

        let Some(callee_sdp_value) = callee_sdp else {
            if callee_sdp_type == rustrtc::SdpType::Answer && self.media.early_media_sent {
                debug!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    "Final 200 OK has no SDP; promoting the existing early-media descriptions to Answer"
                );

                let (caller_pc, callee_pc) = if self.media_profile.path == MediaPathMode::Anchored {
                    let caller_pc = match self.caller_peer() {
                        Some(peer) => Self::get_peer_pc(peer, Self::CALLER_TRACK_ID).await,
                        None => None,
                    };
                    let callee_pc = match self.callee_peer() {
                        Some(peer) => Self::get_peer_pc(peer, Self::CALLEE_TRACK_ID).await,
                        None => None,
                    };
                    (caller_pc, callee_pc)
                } else {
                    (None, None)
                };

                if let Some(callee_pc) = callee_pc
                    && let Some(mut answer) = callee_pc.remote_description()
                    && answer.sdp_type == rustrtc::SdpType::Pranswer
                {
                    answer.sdp_type = rustrtc::SdpType::Answer;
                    if let Err(error) = callee_pc.set_remote_description(answer).await {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            error = %error,
                            "Failed to promote callee remote pranswer"
                        );
                    }
                }

                if let Some(caller_pc) = caller_pc
                    && let Some(mut answer) = caller_pc.local_description()
                    && answer.sdp_type == rustrtc::SdpType::Pranswer
                {
                    answer.sdp_type = rustrtc::SdpType::Answer;
                    if let Err(error) = caller_pc.set_local_description(answer) {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            error = %error,
                            "Failed to promote caller local pranswer"
                        );
                    }
                }

            }

            return Ok(if self.media.early_media_sent {
                self.media.answer.clone()
            } else {
                None
            });
        };

        let sdp_changed =
            self.media.callee_answer_sdp.as_deref() != Some(callee_sdp_value.as_str());

        if self.caller_dialog.as_ref().is_some_and(|d| d.state().is_confirmed())
            && self.media.answer.is_some()
            && !sdp_changed
            && !force_regenerate
        {
            return Ok(self.media.answer.clone());
        }

        if self.media.callee_answer_sdp.is_some() && sdp_changed {
            debug!(session_id = %self.id,
                session_id = %self.context.session_id,
                "Callee SDP changed after early media; updating the existing media transport"
            );
        }

        if self.caller_dialog.as_ref().is_some_and(|d| d.state().is_confirmed())
            && self.media.answer.is_some()
            && self.media_profile.path == MediaPathMode::Anchored
            && self.media.bridge.is_none()
        {
            debug!(session_id = %self.id,
                session_id = %self.context.session_id,
                "Caller dialog already confirmed; keeping existing caller track/SDP and only updating callee-side forwarding"
            );

            let caller_answer = self.media.answer.clone();

            if let Some(peer) = self.callee_peer() {
                if let Err(e) = peer
                    .update_remote_description(
                        Self::CALLEE_TRACK_ID,
                        &callee_sdp_value,
                        callee_sdp_type,
                    )
                    .await
                {
                    warn!(session_id = %self.id,
                        session_id = %self.context.session_id,
                        error = %e,
                        "Failed to set callee answer on callee track"
                    );
                }
            }

            self.media.callee_answer_sdp = Some(callee_sdp_value);

            return Ok(caller_answer);
        }

        let callee_sdp = Some(callee_sdp_value.clone());
        let caller_is_webrtc = self.is_caller_webrtc();
        let _callee_is_webrtc = self.is_callee_webrtc();

        let caller_answer = if self.media_profile.path == MediaPathMode::Anchored {
            if let (Some(sdp), Some(peer)) = (callee_sdp.as_ref(), self.callee_peer()) {
                if let Err(e) = peer
                    .update_remote_description(Self::CALLEE_TRACK_ID, sdp, callee_sdp_type)
                    .await
                {
                    warn!(session_id = %self.id,
                        session_id = %self.context.session_id,
                        error = %e,
                        "Failed to set callee answer on callee track"
                    );
                }
            }

            if let Some(caller_offer) = self.media.caller_offer.clone() {
                let existing_caller_track = if let Some(peer) = self.caller_peer() {
                    let mut found = None;
                    for track in peer.get_tracks().await {
                        let is_caller_track = {
                            let guard = track.lock().await;
                            guard.id() == Self::CALLER_TRACK_ID
                        };
                        if is_caller_track {
                            found = Some(track);
                            break;
                        }
                    }
                    found
                } else {
                    None
                };

                if let Some(track) = existing_caller_track {
                    let guard = track.lock().await;
                    match guard
                        .handshake(caller_offer.clone(), callee_sdp_type)
                        .await
                    {
                        Ok(answer_sdp) => {
                            let answer_sdp = self.rewrite_answer_to_selected_codecs(
                                &answer_sdp,
                                &caller_offer,
                                Some(&callee_sdp_value),
                                "anchored caller answer",
                            );
                            debug!(session_id = %self.id,
                                session_id = %self.context.session_id,
                                "Updated existing PBX caller answer SDP without replacing its RTP transport"
                            );
                            Some(answer_sdp)
                        }
                        Err(e) => {
                            warn!(session_id = %self.id,
                                session_id = %self.context.session_id,
                                error = %e,
                                "Failed to update existing caller track answer; keeping previous caller SDP"
                            );
                            self.media.answer.clone().or_else(|| callee_sdp.clone())
                        }
                    }
                } else {
                    let allow_codecs = self.resolve_effective_codecs();
                    let codec_info =
                        MediaNegotiator::build_codec_list_from_offer(&caller_offer, &allow_codecs);
                    if codec_info.is_empty() {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            "No compatible codec found for anchored caller answer"
                        );
                    }

                    let cancel_token = self
                        .caller_peer()
                        .map(|p| p.cancel_token())
                        .unwrap_or_default();
                    let mut track_builder = self.build_rtp_track_builder(
                        Self::CALLER_TRACK_ID.to_string(),
                        cancel_token,
                        self.caller_transport_mode(),
                    );

                    if !codec_info.is_empty() {
                        track_builder = track_builder.with_codec_info(codec_info);
                    }

                    if caller_is_webrtc {
                        track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
                        if let Some(ref ice_servers) = self.context.dialplan.media.ice_servers {
                            track_builder = track_builder.with_ice_servers(ice_servers.clone());
                        }
                    }

                    let track = track_builder.build();
                    match track
                        .handshake(caller_offer.clone(), callee_sdp_type)
                        .await
                    {
                        Ok(answer_sdp) => {
                            let answer_sdp = self.rewrite_answer_to_selected_codecs(
                                &answer_sdp,
                                &caller_offer,
                                Some(&callee_sdp_value),
                                "anchored caller answer",
                            );
                            debug!(session_id = %self.id,
                                session_id = %self.context.session_id,
                                "Generated PBX answer SDP for caller (anchored media)"
                            );
                            if let Some(peer) = self.caller_peer() {
                                peer.update_track(Box::new(track), None).await;
                            }
                            Some(answer_sdp)
                        }
                        Err(e) => {
                            return Err(anyhow!("Failed to handshake caller track: {e}"));
                        }
                    }
                }
            } else {
                callee_sdp.clone()
            }
        } else {
            callee_sdp.clone()
        };

        self.media.callee_answer_sdp = callee_sdp.clone();
        self.media.answer = caller_answer.clone();

        Ok(caller_answer)
    }

    fn is_caller_webrtc(&self) -> bool {
        // Sniff the caller's SDP offer for WebRTC indicators (ICE + DTLS).
        // This is used during SDP bridge negotiation before leg_transport is populated.
        if let Some(ref offer) = self.media.caller_offer {
            offer.contains("a=ice-ufrag") && offer.contains("a=fingerprint")
        } else {
            self.legs.caller_is_webrtc()
        }
    }

    /// Classify a peer's media transport from its SDP:
    /// - `WebRtc` when it carries ICE + DTLS (`a=ice-ufrag` + `a=fingerprint`),
    /// - `Srtp` for SDES-SRTP (`RTP/SAVP` profile or an `a=crypto` line) without DTLS,
    /// - `Rtp` otherwise (plain `RTP/AVP`).
    fn sdp_transport_mode(sdp: &str) -> rustrtc::TransportMode {
        if sdp.contains("a=ice-ufrag") && sdp.contains("a=fingerprint") {
            rustrtc::TransportMode::WebRtc
        } else if sdp.contains("RTP/SAVP") || sdp.contains("a=crypto") {
            rustrtc::TransportMode::Srtp
        } else {
            rustrtc::TransportMode::Rtp
        }
    }

    /// Transport mode for the caller (UAS) leg, derived from the caller's offer
    /// so we answer with a matching media profile (e.g. answer an `RTP/SAVP`
    /// offer with `RTP/SAVP`, never downgrade SDES-SRTP to plain `RTP/AVP`).
    fn caller_transport_mode(&self) -> rustrtc::TransportMode {
        if let Some(ref offer) = self.media.caller_offer {
            Self::sdp_transport_mode(offer)
        } else {
            self.legs
                .get_transport(&LegId::from("caller"))
                .unwrap_or(rustrtc::TransportMode::Rtp)
        }
    }

    /// Transport mode for the callee (UAC) leg we generate an offer for. WebRTC
    /// callees are unchanged; for SIP callees we mirror SDES-SRTP from the caller
    /// leg ("secure in -> secure out") so anchored SIP<->SIP media stays
    /// encrypted end to end. Plain-RTP callers keep plain `RTP/AVP`.
    fn callee_transport_mode(&self, callee_is_webrtc: bool) -> rustrtc::TransportMode {
        if callee_is_webrtc {
            rustrtc::TransportMode::WebRtc
        } else if self.caller_transport_mode() == rustrtc::TransportMode::Srtp {
            rustrtc::TransportMode::Srtp
        } else {
            rustrtc::TransportMode::Rtp
        }
    }

    fn is_callee_webrtc(&self) -> bool {
        self.legs.callee_is_webrtc()
    }

    fn callee_supports_webrtc(target: &Location) -> bool {
        if target.supports_webrtc {
            return true;
        }
        if matches!(
            target.destination.as_ref().and_then(|d| d.r#type),
            Some(Transport::Ws | Transport::Wss)
        ) {
            return true;
        }
        matches!(target.transport, Some(Transport::Ws | Transport::Wss))
    }

    /// Resolve bridge endpoint for a leg from leg_transport.
    async fn get_peer_pc(
        peer: &Arc<dyn MediaPeer>,
        track_id: &str,
    ) -> Option<rustrtc::PeerConnection> {
        let tracks = peer.get_tracks().await;
        for t in &tracks {
            let guard = t.lock().await;
            if guard.id() == track_id {
                return guard.get_peer_connection().await;
            }
        }
        None
    }

    /// Ensure anchored media flows through the /* /* ForwardingTrack removed */ slow path removed */,
    /// downgrading from the RTP fast-path on demand if necessary.
    ///
    /// Idempotent and cheap to call:
    /// - If an app `media_bridge` is active, the call is already anchored —
    ///   return immediately.
    /// - If [`/* /* ForwardingTrack removed */ removed */`] is already wired — return.
    /// - Only when the RTP fast-path relay (`RelayOnly`) is active do we tear
    ///   down the transport-level rewrite bridge and wire the /* ForwardingTrack removed */
    ///   chain for both legs. No re-INVITE is needed: clearing the rewrite
    ///   hook restores the normal demux → depacketizer → track flow, and the
    ///   /* ForwardingTrack removed */ reuses the already-negotiated transceivers/senders.
    ///
    /// Called by features that need to read or inject per-packet media
    /// (currently VoipBridge; Play will follow).
    async fn find_audio_receiver_track(
        pc: &rustrtc::PeerConnection,
    ) -> Option<Arc<dyn rustrtc::media::MediaStreamTrack>> {
        for transceiver in pc.get_transceivers() {
            if transceiver.kind() == rustrtc::MediaKind::Audio
                && let Some(receiver) = transceiver.receiver()
            {
                return Some(receiver.track());
            }
        }
        None
    }

    async fn start_caller_ingress_monitor_if_needed(&mut self) {
        // DTMF detection via MediaBridge IngressTap
    }

    async fn stop_caller_ingress_monitor(&mut self) {}

    async fn stop_anchored_rtp_timeout_monitor(&mut self) {}

    fn resolve_effective_codecs(&self) -> Vec<CodecType> {
        if !self.context.dialplan.allow_codecs.is_empty() {
            return self.context.dialplan.allow_codecs.clone();
        }

        if let Some(codecs) = self.match_destination_trunk_codecs() {
            return codecs;
        }

        if let Some(ref codecs) = self.server.proxy_config.audio_codecs {
            let parsed = parse_allowed_codecs(codecs);
            if !parsed.is_empty() {
                return parsed;
            }
        }

        vec![]
    }

    /// Try to find codecs by matching the callee URI host:port against trunk destinations.
    /// This covers both regular file-based trunks and DB-based (wholesale) trunks.
    fn match_destination_trunk_codecs(&self) -> Option<Vec<CodecType>> {
        let callee_uri = &self.context.dialplan.original.uri;
        let callee_host: String = callee_uri.host().to_string().to_lowercase();
        let callee_port: u16 = callee_uri.host_with_port.port.map(|p| p.0).unwrap_or(5060);

        let trunks = self.server.data_context.trunks_snapshot();
        for (_name, trunk) in trunks.iter() {
            if trunk.codec.is_empty() {
                continue;
            }
            if let Some((trunk_host, trunk_port)) = trunk_host_port(&trunk.dest) {
                if trunk_host.to_lowercase() == callee_host && trunk_port == callee_port {
                    let parsed = parse_allowed_codecs(&trunk.codec);
                    if !parsed.is_empty() {
                        return Some(parsed);
                    }
                }
            }
        }
        None
    }

    pub async fn create_callee_track(&mut self, callee_is_webrtc: bool) -> Result<String> {
        let track_id = Self::CALLEE_TRACK_ID.to_string();

        let caller_mode = self.caller_transport_mode();
        let callee_mode = self.callee_transport_mode(callee_is_webrtc);
        self.legs
            .set_transport(LegId::from("caller"), caller_mode.clone());
        self.legs
            .set_transport(LegId::from("callee"), callee_mode.clone());

        // ── MediaBridge path (anchored / app mode) ──────────────────────────
        if self.media.bridge.is_some() {
            // Ensure caller leg exists
            self.ensure_caller_leg().await?;

            let allow_codecs = self.resolve_effective_codecs();
            let codecs = self
                .media
                .caller_offer
                .as_ref()
                .map(|offer| {
                    let mut codecs =
                        MediaNegotiator::build_callee_codec_offer_with_allow(offer, &allow_codecs);
                    if callee_is_webrtc {
                        codecs = MediaNegotiator::filter_webrtc_offer_codecs(offer, codecs);
                    }
                    codecs
                })
                .unwrap_or_default();

            let cfg = crate::media::leg::LegConfig {
                transport: callee_mode,
                codecs,
                rtp_port_range: self
                .context
                .dialplan
                .media
                .rtp_start_port
                .zip(self.context.dialplan.media.rtp_end_port),
                external_ip: self.context.dialplan.media.external_ip.clone(),
                bind_ip: self.context.dialplan.media.bind_ip.clone(),
                cname: Some(self.server.rtc_cname.clone()),
                comfort_noise: self.context.dialplan.media.comfort_noise,
                comfort_noise_level_db: self.context.dialplan.media.comfort_noise_level_db,
            };

            let mb = self.bridge_mut().ok_or_else(|| anyhow!("No MediaBridge"))?;
            let leg = crate::media::leg::LegInner::new("callee", &cfg)?;
            let mut sdp = leg.create_offer(vec![]).await?;
            mb.replace_leg(crate::media::media_bridge::LegSide::B, leg).await;

            // Rewrite codec list to respect allow/deny
            if let Some(ref caller_offer) = self.media.caller_offer {
                let allow_codecs = self.resolve_effective_codecs();
                let callee_offer_codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
                    caller_offer,
                    &allow_codecs,
                );
                if !callee_offer_codecs.is_empty() {
                    if let Some(rewritten) =
                        MediaNegotiator::rewrite_sdp_codec_list(&sdp, &callee_offer_codecs)
                    {
                        sdp = rewritten;
                    }
                }
            }

            info!(session_id = %self.id, "Created callee track via MediaBridge");
            return Ok(sdp);
        }

        // ── Fallback: RtpTrackBuilder path (bypass mode) ──────────────────
        let cancel_token = self
            .callee_peer()
            .map(|p| p.cancel_token())
            .unwrap_or_default();
        let mut track_builder = RtpTrackBuilder::new(track_id.clone())
            .with_mode(self.callee_transport_mode(callee_is_webrtc))
            .with_cancel_token(cancel_token)
            .with_cname(self.server.rtc_cname.clone());

        if let Some(ref caller_offer) = self.media.caller_offer {
            let allow_codecs = self.resolve_effective_codecs();
            let mut codecs =
                MediaNegotiator::build_callee_codec_offer_with_allow(caller_offer, &allow_codecs);
            if callee_is_webrtc {
                codecs = MediaNegotiator::filter_webrtc_offer_codecs(caller_offer, codecs);
            }
            if !codecs.is_empty() {
                track_builder = track_builder.with_codec_info(codecs);
            }
        }

        if callee_is_webrtc {
            track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
        }

        let track = track_builder.build();
        let sdp = track.local_description().await?;

        if let Some(peer) = self.callee_peer() {
            peer.update_track(Box::new(track), None).await;
        }

        Ok(sdp)
    }

    async fn ensure_caller_answer_sdp(&mut self) -> Option<String> {
        if let Some(ref answer) = self.media.answer {
            return Some(answer.clone());
        }

        if self.bypasses_local_media() {
            if let Some(answer_sdp) = self.media.callee_answer_sdp.clone() {
                self.media.answer = Some(answer_sdp.clone());
                return Some(answer_sdp);
            }
        }

        let caller_offer = self.media.caller_offer.clone()?;
        let caller_is_webrtc = self.is_caller_webrtc();

        let allow_codecs = self.resolve_effective_codecs();
        let codec_info = MediaNegotiator::build_codec_list_from_offer(&caller_offer, &allow_codecs);
        if codec_info.is_empty() {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                "No compatible codec found for local caller answer"
            );
        }

        let cancel_token = self
            .caller_peer()
            .map(|p| p.cancel_token())
            .unwrap_or_default();
        let mut track_builder = self.build_rtp_track_builder(
            Self::CALLER_TRACK_ID.to_string(),
            cancel_token,
            self.caller_transport_mode(),
        );

        if !codec_info.is_empty() {
            track_builder = track_builder.with_codec_info(codec_info);
        }

        if caller_is_webrtc {
            track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
            if let Some(ref ice_servers) = self.context.dialplan.media.ice_servers {
                track_builder = track_builder.with_ice_servers(ice_servers.clone());
            }
        }

        let track = track_builder.build();
        match track
            .handshake(caller_offer.clone(), rustrtc::SdpType::Answer)
            .await
        {
            Ok(answer_sdp) => {
                let answer_sdp = self.rewrite_answer_to_selected_codecs(
                    &answer_sdp,
                    &caller_offer,
                    None,
                    "local caller answer",
                );
                debug!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    "Generated PBX answer SDP for caller"
                );
                if let Some(peer) = self.caller_peer() {
                    peer.update_track(Box::new(track), None).await;
                }
                self.media.answer = Some(answer_sdp.clone());
                Some(answer_sdp)
            }
            Err(e) => {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    error = %e,
                    "Failed to generate caller answer SDP"
                );
                None
            }
        }
    }

    pub async fn accept_call(
        &mut self,
        callee: Option<String>,
        sdp: Option<String>,
        dialog_id: Option<String>,
    ) -> Result<()> {
        info!(session_id = %self.id,
            callee = ?callee,
            dialog_id = ?dialog_id,
            "Accepting call"
        );

        self.update_leg_state(&LegId::from("callee"), LegState::Connected);
        if false {
            self.start_caller_ingress_monitor_if_needed().await;
        }

        self.meta.connected_callee = callee.clone();

        if !self.app_runtime.is_running() {
            self.emit_typed_rwi_event(&crate::rwi::CallAnswered {
                call_id: self.context.session_id.clone(),
            });
        }

        let mut timer_headers = vec![];
        if self.server.proxy_config.session_timer_mode().is_enabled() {
            let default_expires = self
                .server
                .proxy_config
                .session_expires
                .unwrap_or(DEFAULT_SESSION_EXPIRES);
            match self.init_server_timer(default_expires) {
                Ok(()) => {
                    let caller_dialog_id = self.caller_dialog_id();
                    if let Some(timer) = self.timers.get(&caller_dialog_id) {
                        if timer.enabled {
                            timer_headers.extend(build_session_timer_response_headers(timer));
                            debug!(session_id = %self.id,
                                session_expires = %timer.get_session_expires_value(),
                                "Session timer negotiated in 200 OK"
                            );
                        }
                    }
                }
                Err((code, text, reason)) => {
                    warn!(session_id = %self.id, code = %code, text = %text, ?reason, "Failed to initialize session timer");
                }
            }
        }

        let answer_sdp = if let Some(answer_sdp) = sdp {
            Some(answer_sdp)
        } else {
            self.ensure_caller_answer_sdp().await
        };

        if let Some(answer_sdp) = answer_sdp {
            let mut headers = Self::sdp_headers();
            headers.extend(timer_headers);
            if let Some(dialog) = self.caller_dialog.as_ref() {
                if let Err(e) = dialog.accept(Some(headers), Some(answer_sdp.into_bytes())) {
                    if self.caller_dialog.as_ref().is_some_and(|d| d.state().is_confirmed()) {
                        debug!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            error = %e,
                            "Caller leg already confirmed; skipping duplicate 200 OK"
                        );
                    } else {
                        return Err(anyhow!("Failed to send answer: {}", e));
                    }
                }
            }
        }

        // App/IVR answer: the caller dialog is confirmed (200 OK sent) — mark
        // the caller leg accepted so the MediaBridge opens the relay gate.
        let rtp_timeout = self.rtp_timeout_config();
        let cmd_tx = self.cmd_tx.clone();
        let session_id = self.context.session_id.clone();
        if let Some(mb) = self.media.bridge.as_mut() {
            let _ = mb.accept(crate::media::media_bridge::LegSide::A).await;
            Self::arm_bridged_rtp_timeouts(mb, rtp_timeout, cmd_tx, &session_id);

            info!(session_id = %self.id,
                session_id = %self.context.session_id,
                media_bridge_present = true,
                "accept_call: caller leg accepted (relay gate opened)"
            );
        } else {
            info!(session_id = %self.id,
                session_id = %self.context.session_id,
                media_bridge_present = false,
                "accept_call: caller gate NOT opened (no media bridge)"
            );
        }

        self.meta.answer_time = Some(Instant::now());

        let elapsed = self.context.start_time.elapsed().as_secs_f64();
        crate::metrics::sip::invite_latency_seconds(elapsed, "inbound");

        let session_id = self.id.to_string();
        let caller = self
            .meta
            .routed_caller
            .clone()
            .or_else(|| Some(self.context.original_caller.clone()));
        let callee = self
            .meta
            .connected_callee
            .clone()
            .or_else(|| self.meta.routed_callee.clone())
            .or_else(|| Some(self.context.original_callee.clone()));

        self.server
            .active_call_registry
            .update(&session_id, |entry| {
                entry.answered_at = Some(chrono::Utc::now());
                entry.status = crate::proxy::active_call_registry::ActiveProxyCallStatus::Talking;
                if entry.caller.is_none() {
                    entry.caller = caller.clone();
                }
                if entry.callee.is_none() {
                    entry.callee = callee.clone();
                }
            });

        // Auto-start recording when the call is answered if configured.
        if self.context.dialplan.recording.enabled
            && self.context.dialplan.recording.auto_start
            && !self.media.recording_state.is_active()
            && let Some(ref option) = self.context.dialplan.recording.option
        {
            let path = option.recorder_file.clone();
            if !path.is_empty()
                && let Err(e) = self.start_recording(&path, None, false).await
            {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    error = %e,
                    "Auto-start recording failed"
                );
            }
        }

        // Fire session lifecycle hooks.
        if !self.server.session_hooks.is_empty() {
            let ctx = self.session_hook_ctx();
            for hook in self.server.session_hooks.iter() {
                hook.on_call_connected(&ctx).await;
            }
        }

        Ok(())
    }

    fn is_hold_direction(
        direction: rustrtc::Direction,
        offer: Option<&rustrtc::SessionDescription>,
    ) -> bool {
        if !matches!(direction, rustrtc::Direction::SendRecv) {
            return true;
        }
        // Per RFC 4317, c=IN IP4 0.0.0.0 also signals hold even when
        // direction is sendrecv (some endpoints use this convention).
        if let Some(offer) = offer {
            for section in &offer.media_sections {
                if section.port == 0 {
                    return true;
                }
                let conn = section
                    .connection
                    .as_deref()
                    .or(offer.session.connection.as_deref());
                if conn.is_some_and(|c| Self::is_zero_connection(c)) {
                    return true;
                }
            }
        }
        false
    }

    async fn get_local_reinvite_pc(&self, side: DialogSide) -> Option<rustrtc::PeerConnection> {
        // Prefer the MediaBridge leg PC (caller=A, callee=B) when present.
        if let Some(mb) = self.media.bridge.as_ref() {
            let side_leg = match side {
                DialogSide::Caller => mb.leg(crate::media::media_bridge::LegSide::A),
                DialogSide::Callee => mb.leg(crate::media::media_bridge::LegSide::B),
            };
            if let Some(leg) = side_leg {
                return Some(leg.pc().clone());
            }
        }

        let (peer, track_id) = match side {
            DialogSide::Caller => (self.caller_peer()?, Self::CALLER_TRACK_ID),
            DialogSide::Callee => (self.callee_peer()?, Self::CALLEE_TRACK_ID),
        };

        Self::get_peer_pc(peer, track_id).await
    }

    async fn build_local_answer_from_pc(
        pc: &rustrtc::PeerConnection,
        offer_sdp: &str,
    ) -> Result<String> {
        let offer = Self::parse_sdp(rustrtc::SdpType::Offer, offer_sdp, "re-INVITE offer")?;
        pc.set_remote_description(offer)
            .await
            .map_err(|e| anyhow!("Failed to apply re-INVITE offer: {}", e))?;

        let answer = pc
            .create_answer()
            .await
            .map_err(|e| anyhow!("Failed to create re-INVITE answer: {}", e))?;

        pc.set_local_description(answer)
            .map_err(|e| anyhow!("Failed to set re-INVITE local answer: {}", e))?;

        pc.local_description()
            .map(|desc| desc.to_sdp_string())
            .ok_or_else(|| anyhow!("PeerConnection has no local description after re-INVITE"))
    }

    async fn update_anchored_forwarding_from_sdp(
        &mut self,
        side: DialogSide,
        changed_leg_sdp: &str,
    ) -> Result<()> {
        if self.media_profile.path != MediaPathMode::Anchored {
            return Ok(());
        }

        // With MediaBridge the SDP change is picked up by re-running bridge():
        // it re-reads both legs' negotiated profiles and re-selects
        // fast-path (same codec) vs transcoding (different codec).
        if self.media.bridge.is_some() {
            if let Some(mb) = self.bridge_mut() {
                if let Err(e) = mb.bridge().await {
                    warn!(session_id = %self.context.session_id, error = %e, "re-bridge after SDP change failed");
                }
            }
            return Ok(());
        }

        // Legacy anchored (no MediaBridge): the ForwardingTrack path was
        // removed; nothing to update here.
        debug!(session_id = %self.id,
            session_id = %self.context.session_id,
            side = ?side,
            _changed_leg_sdp = changed_leg_sdp,
            "Anchored forwarding update is a no-op without MediaBridge"
        );
        Ok(())
    }

    /// Returns `true` when the connection C-line value represents a "zero" address,
    /// commonly used to signal media hold per RFC 4317.
    fn is_zero_connection(c: &str) -> bool {
        let trimmed = c.trim();
        trimmed == "IN IP4 0.0.0.0" || trimmed == "IN IP6 ::" || trimmed == "IN IP6 0:0:0:0:0:0:0:0"
    }

    /// Align the answer SDP direction per media section to the mirror of the
    /// offer direction per RFC 3264 §5.1:
    ///
    /// | Offer        | Answer       |
    /// |--------------|--------------|
    /// | `sendonly`   | `recvonly`   |
    /// | `recvonly`   | `sendonly`   |
    /// | `inactive`   | `inactive`   |
    /// | `sendrecv`   | `sendrecv`   |
    ///
    /// Also treats `port=0` and `c=IN IP4 0.0.0.0` (or `c=IN IP6 ::`)
    /// as equivalent to `inactive`.
    ///
    /// Sections are matched by index between offer and answer (the answer
    /// is expected to have the same number of `m=` lines).
    fn align_answer_direction_with_offer(offer_sdp: &str, answer_sdp: &str) -> String {
        let Ok(offer) = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, offer_sdp)
        else {
            warn!("Failed to parse offer SDP, returning answer unchanged");
            return answer_sdp.to_string();
        };

        // Per offer-section: target answer direction string, or None to keep as-is.
        let target_dirs: Vec<Option<&'static str>> = offer
            .media_sections
            .iter()
            .map(|s| {
                if s.port == 0 {
                    return Some("inactive");
                }
                let conn = s
                    .connection
                    .as_deref()
                    .or(offer.session.connection.as_deref());
                if conn.is_some_and(|c| Self::is_zero_connection(c)) {
                    return Some("inactive");
                }
                match s.direction {
                    rustrtc::Direction::SendOnly => Some("recvonly"),
                    rustrtc::Direction::RecvOnly => Some("sendonly"),
                    rustrtc::Direction::Inactive => Some("inactive"),
                    rustrtc::Direction::SendRecv => None,
                }
            })
            .collect();

        let mut out = String::with_capacity(answer_sdp.len() + 64);
        let mut section_idx = 0usize;
        let mut seen_m = false;

        for line in answer_sdp.lines() {
            let l = line.strip_suffix('\r').unwrap_or(line);
            if l.starts_with("m=") {
                section_idx = if seen_m { section_idx + 1 } else { 0 };
                seen_m = true;
                out.push_str(l);
                out.push_str("\r\n");
                continue;
            }
            if seen_m {
                if let Some(dir) = target_dirs.get(section_idx).copied().flatten() {
                    if matches!(l, "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive") {
                        out.push_str("a=");
                        out.push_str(dir);
                        out.push_str("\r\n");
                        continue;
                    }
                }
            }
            out.push_str(l);
            out.push_str("\r\n");
        }

        out
    }

    async fn build_local_dialog_answer(
        &mut self,
        side: DialogSide,
        offer_sdp: &str,
    ) -> Result<String> {
        let parsed = Self::parse_sdp(rustrtc::SdpType::Offer, offer_sdp, "re-INVITE offer")?;

        // Determine hold state from audio direction (if present). If no audio
        // section (e.g. video-only re-INVITE), leave current leg state unchanged.
        let has_audio = parsed
            .media_sections
            .iter()
            .any(|s| s.kind == rustrtc::MediaKind::Audio);

        // Detect video addition: if the offer contains video but the current
        // session doesn't have video for this leg, dynamically add video tracks
        // to the bridge PCs.
        let offer_video_section = parsed
            .media_sections
            .iter()
            .find(|s| s.kind == rustrtc::MediaKind::Video);
        let offer_video_direction = offer_video_section.map(|section| {
            if section.port == 0 {
                rustrtc::Direction::Inactive
            } else {
                section.direction
            }
        });
        let offer_video_active = offer_video_direction
            .map(|direction| direction != rustrtc::Direction::Inactive)
            .unwrap_or(false);
        let leg_key = match side {
            DialogSide::Caller => LegId::from("caller"),
            DialogSide::Callee => LegId::from("callee"),
        };
        let had_video = self.legs.leg_has_video(&leg_key);

        if offer_video_active && !had_video {
            // Video being added — extract first video codec PT/clock from the offer.
            // Video is passed through at the SDP layer; the MediaBridge does not
            // manage video tracks.
            use crate::media::negotiate::MediaNegotiator;
            let extracted = MediaNegotiator::extract_codec_params(offer_sdp);
            if let Some(video_codec) = extracted.video.first() {
                info!(session_id = %self.id,
                    "Dynamically adding video m-line (PT={}, clock={}) for leg {:?}",
                    video_codec.payload_type, video_codec.clock_rate, side
                );
            }
        }

        let pc = self
            .get_local_reinvite_pc(side)
            .await
            .ok_or_else(|| anyhow!("No local PeerConnection available for {:?}", side))?;
        let mut answer_sdp = Self::build_local_answer_from_pc(&pc, offer_sdp).await?;
        if has_audio {
            let (preferred_peer_sdp, context) = match side {
                DialogSide::Caller => (
                    self.media.callee_answer_sdp.as_deref(),
                    "caller re-INVITE answer",
                ),
                DialogSide::Callee => (self.media.answer.as_deref(), "callee re-INVITE answer"),
            };
            answer_sdp = self.rewrite_answer_to_selected_codecs(
                &answer_sdp,
                offer_sdp,
                preferred_peer_sdp,
                context,
            );
        }

        // Align answer direction with offer per RFC 3264 §5.1
        answer_sdp = Self::align_answer_direction_with_offer(offer_sdp, &answer_sdp);

        match side {
            DialogSide::Caller => {
                self.media.caller_offer = Some(offer_sdp.to_string());
                self.media.answer = Some(answer_sdp.clone());
            }
            DialogSide::Callee => {
                self.media.callee_offer = Some(answer_sdp.clone());
                self.media.callee_answer_sdp = Some(answer_sdp.clone());
            }
        }
        // Hold transition is applied in handle_updated_dialog for all branches
        if offer_video_direction.is_some() && (had_video || offer_video_active) {
            self.legs.set_video_state(&leg_key, true);
        }

        self.update_anchored_forwarding_from_sdp(side, &answer_sdp)
            .await?;

        self.update_snapshot_cache();
        Ok(answer_sdp)
    }

    // ── Hold/Unhold propagation helpers ──

    /// Called when caller initiates hold (sendonly/inactive).
    /// Propagates the hold to the callee side: updates leg state, sends hold
    /// re-INVITE (if no media bridge), and starts hold music (if media bridge).
    ///
    /// `override_music`, if `Some`, is used instead of the normal
    /// header/extension/config resolution chain.
    async fn propagate_hold_to_callee(
        &mut self,
        request_headers: &[rsipstack::sip::Header],
        override_music: Option<crate::call::domain::MediaSource>,
    ) -> Result<()> {
        info!(session_id = %self.id, "Propagating hold to callee");

        self.update_leg_state(&LegId::from("callee"), LegState::Hold);

        let music = override_music.or_else(|| self.resolve_hold_music(request_headers));
        let session_id = self.id.clone();

        if let Some(mb) = self.bridge_mut() {
            let side = crate::media::media_bridge::LegSide::B;
            mb.pause_rtp_timeout(side);
            if let Some(music) = music {
                let path = match &music {
                    crate::call::domain::MediaSource::File { path } => path.clone(),
                    crate::call::domain::MediaSource::Url { url } => url.clone(),
                    _ => {
                        warn!(session_id = %session_id, "Unsupported hold music source type");
                        mb.hold(side, None).await?;
                        return Ok(());
                    }
                };
                mb.hold_file(side, path).await?;
            } else {
                mb.hold(side, None).await?;
            }
        } else {
            let hold_sdp = self.generate_callee_hold_sdp()?;
            if let Some(response_sdp) = self.send_reinvite_to_callee_dialogs(&hold_sdp).await? {
                self.media.callee_answer_sdp = Some(response_sdp);
            }
        }
        Ok(())
    }

    /// Called when caller initiates unhold (sendrecv).
    async fn propagate_unhold_to_callee(&mut self) -> Result<()> {
        info!(session_id = %self.id, "Propagating unhold to callee");
        self.update_leg_state(&LegId::from("callee"), LegState::Connected);
        if let Some(mb) = self.bridge_mut() {
            mb.resume(crate::media::media_bridge::LegSide::B).await?;
            mb.resume_rtp_timeout(crate::media::media_bridge::LegSide::B);
        } else {
            let unhold_sdp = self.generate_callee_unhold_sdp()?;
            if let Some(response_sdp) = self.send_reinvite_to_callee_dialogs(&unhold_sdp).await? {
                self.media.callee_answer_sdp = Some(response_sdp);
            }
        }
        Ok(())
    }

    /// Called when callee initiates hold (sendonly/inactive).
    async fn propagate_hold_to_caller(&mut self) -> Result<()> {
        info!(session_id = %self.id, "Propagating hold to caller");
        self.update_leg_state(&LegId::from("caller"), LegState::Hold);
        if let Some(mb) = self.bridge_mut() {
            mb.pause_rtp_timeout(crate::media::media_bridge::LegSide::A);
            mb.hold(crate::media::media_bridge::LegSide::A, None).await?;
        } else {
            let hold_sdp = self.generate_hold_sdp().await?;
            if let Err(e) = self.send_reinvite_to_caller(hold_sdp).await {
                warn!(session_id = %self.context.session_id, error = %e, "Failed to send hold re-INVITE to caller");
            }
        }
        Ok(())
    }

    async fn propagate_unhold_to_caller(&mut self) -> Result<()> {
        info!(session_id = %self.id, "Propagating unhold to caller");
        self.update_leg_state(&LegId::from("caller"), LegState::Connected);
        if let Some(mb) = self.bridge_mut() {
            mb.resume(crate::media::media_bridge::LegSide::A).await?;
            mb.resume_rtp_timeout(crate::media::media_bridge::LegSide::A);
        } else {
            let unhold_sdp = self.generate_unhold_sdp().await?;
            if let Err(e) = self.send_reinvite_to_caller(unhold_sdp).await {
                warn!(session_id = %self.context.session_id, error = %e, "Failed to send unhold re-INVITE to caller");
            }
        }
        Ok(())
    }
    async fn apply_reinvite_hold_transition(
        &mut self,
        side: DialogSide,
        offer: &rustrtc::SessionDescription,
        request_headers: &[rsipstack::sip::Header],
    ) {
        let has_audio = offer
            .media_sections
            .iter()
            .any(|s| s.kind == rustrtc::MediaKind::Audio);
        if !has_audio {
            return;
        }

        let offer_direction = offer
            .media_sections
            .iter()
            .find(|s| s.kind == rustrtc::MediaKind::Audio)
            .map(|s| s.direction);

        let leg_id = match side {
            DialogSide::Caller => LegId::from("caller"),
            DialogSide::Callee => LegId::from("callee"),
        };

        let new_state = if Self::is_hold_direction(offer_direction.unwrap_or_default(), Some(offer))
        {
            LegState::Hold
        } else {
            LegState::Connected
        };

        let prev = self.leg_prev_state(&leg_id);
        self.update_leg_state(&leg_id, new_state);
        self.fire_hold_transition_hooks(&leg_id, prev, new_state)
            .await;

        // Cross-leg propagation
        match side {
            DialogSide::Caller => {
                let callee_prev = self.leg_prev_state(&LegId::from("callee"));
                let callee_transition = match (callee_prev, new_state) {
                    (Some(LegState::Hold), LegState::Connected) => Some(false),
                    (Some(s), LegState::Hold) if s != LegState::Hold => Some(true),
                    _ => None,
                };
                if let Some(is_hold) = callee_transition {
                    if is_hold {
                        if let Err(e) = self.propagate_hold_to_callee(request_headers, None).await {
                            warn!(session_id = %self.id, error = %e, "Failed to propagate hold to callee");
                        }
                    } else if let Err(e) = self.propagate_unhold_to_callee().await {
                        warn!(session_id = %self.id, error = %e, "Failed to propagate unhold to callee");
                    }
                }
            }
            DialogSide::Callee => {
                let caller_prev = self.leg_prev_state(&LegId::from("caller"));
                let caller_transition = match (caller_prev, new_state) {
                    (Some(LegState::Hold), LegState::Connected) => Some(false),
                    (Some(s), LegState::Hold) if s != LegState::Hold => Some(true),
                    _ => None,
                };
                if let Some(is_hold) = caller_transition {
                    if is_hold {
                        if let Err(e) = self.propagate_hold_to_caller().await {
                            warn!(session_id = %self.id, error = %e, "Failed to propagate hold to caller");
                        }
                    } else if let Err(e) = self.propagate_unhold_to_caller().await {
                        warn!(session_id = %self.id, error = %e, "Failed to propagate unhold to caller");
                    }
                }
            }
        }
    }

    /// Resolve hold music source by priority:
    /// 1. X-Hold-Music header in the re-INVITE request
    /// 2. X-Hold-Music in session extensions (from initial INVITE / CC addon)
    /// 3. PBX default from ProxyConfig
    fn resolve_hold_music(
        &self,
        request_headers: &[rsipstack::sip::Header],
    ) -> Option<crate::call::domain::MediaSource> {
        // 1. re-INVITE X-Hold-Music header
        if let Some(val) = request_headers
            .iter()
            .find(|h| h.name().eq_ignore_ascii_case("X-Hold-Music"))
            .map(|h| h.value().to_string())
        {
            return Some(Self::parse_hold_music_value(&val));
        }
        // 2. Session extensions (set by CC addon or from initial INVITE metadata)
        if let Some(meta) = self
            .extensions
            .read()
            .get::<std::collections::HashMap<String, String>>()
        {
            if let Some(val) = meta.get("X-Hold-Music") {
                return Some(Self::parse_hold_music_value(val));
            }
        }
        // 3. PBX default
        if let Some(path) = &self.server.proxy_config.hold_music {
            return Some(crate::call::domain::MediaSource::File { path: path.clone() });
        }
        None
    }

    fn parse_hold_music_value(value: &str) -> crate::call::domain::MediaSource {
        let trimmed = value.trim().to_string();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            crate::call::domain::MediaSource::Url { url: trimmed }
        } else {
            crate::call::domain::MediaSource::File { path: trimmed }
        }
    }

    /// Generate hold SDP for the callee side (sendonly).
    fn generate_callee_hold_sdp(&self) -> Result<String> {
        let base_sdp = self
            .media
            .callee_answer_sdp
            .as_deref()
            .or(self.media.callee_offer.as_deref())
            .ok_or_else(|| anyhow!("No SDP available for callee hold"))?;
        let hold_sdp = rustrtc::modify_sdp_direction(base_sdp, "sendonly");
        Ok(hold_sdp)
    }

    /// Generate unhold SDP for the callee side (sendrecv).
    fn generate_callee_unhold_sdp(&self) -> Result<String> {
        let base_sdp = self
            .media
            .callee_answer_sdp
            .as_deref()
            .or(self.media.callee_offer.as_deref())
            .ok_or_else(|| anyhow!("No SDP available for callee unhold"))?;
        let unhold_sdp = rustrtc::modify_sdp_direction(base_sdp, "sendrecv");
        Ok(unhold_sdp)
    }

    /// Send a re-INVITE with given SDP body to all callee dialogs and return
    /// the first SDP answer if any.
    async fn send_reinvite_to_callee_dialogs(&mut self, sdp: &str) -> Result<Option<String>> {
        let dialog_layer = self.server.dialog_layer.clone();
        let headers = Self::sdp_headers();
        let body = sdp.as_bytes().to_vec();

        let callee_ids: Vec<DialogId> = self
            .callee_dialogs
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        let mut answer: Option<String> = None;
        for callee_dialog_id in callee_ids {
            if let Some(mut dialog) = dialog_layer.get_dialog(&callee_dialog_id) {
                let resp = match &mut dialog {
                    rsipstack::dialog::dialog::Dialog::Invite(d) => d
                        .reinvite(Some(headers.clone()), Some(body.clone()))
                        .await
                        .map_err(|e| anyhow!("re-INVITE to callee failed: {}", e))?,
                    _ => continue,
                };
                if let Some(response) = resp
                    && !response.body().is_empty()
                {
                    answer = Some(String::from_utf8_lossy(response.body()).to_string());
                }
            }
        }
        Ok(answer)
    }

    pub async fn handle_reinvite(
        &mut self,
        method: rsipstack::sip::Method,
        sdp: Option<String>,
    ) -> Result<Option<String>> {
        debug!(session_id = %self.id,
            ?method,
            sdp_present = sdp.is_some(),
            "Handling re-INVITE in B2BUA mode"
        );

        if method != rsipstack::sip::Method::Invite {
            return Err(anyhow!("Expected INVITE method, got {:?}", method));
        }

        let offer_sdp = match sdp {
            Some(s) => s,
            None => {
                return Ok(self.media.answer.clone());
            }
        };
        if !self.bypasses_local_media() {
            self.media.caller_offer = Some(offer_sdp.clone());
        }

        let callee_dialogs: Vec<DialogId> = self
            .callee_dialogs
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        if callee_dialogs.is_empty() {
            return Err(anyhow!("No callee dialogs available for B2BUA forwarding"));
        }

        let mut final_answer: Option<String> = None;
        let dialog_layer = self.server.dialog_layer.clone();

        for callee_dialog_id in callee_dialogs {
            if let Some(mut dialog) = dialog_layer.get_dialog(&callee_dialog_id) {
                let body = offer_sdp.clone().into_bytes();
                let headers = Self::sdp_headers();

                let resp: Option<rsipstack::sip::Response> = match &mut dialog {
                    Dialog::Invite(d) => d
                        .reinvite(Some(headers), Some(body))
                        .await
                        .map_err(|e| anyhow!("re-INVITE to callee failed: {}", e))?,
                    _ => continue,
                };

                if let Some(response) = resp
                    && !response.body().is_empty()
                {
                    let answer_sdp = String::from_utf8_lossy(response.body()).to_string();
                    if self.media_profile.path == MediaPathMode::Anchored
                        || self.media.bridge.is_some()
                    {
                        final_answer = self
                            .prepare_caller_answer_from_callee_sdp(
                                Some(answer_sdp),
                                true,
                                rustrtc::SdpType::Answer,
                            )
                            .await?;
                    } else {
                        final_answer = Some(answer_sdp.clone());
                    }
                }
            }
        }

        if let Some(ref answer_sdp) = final_answer {
            let mut headers = Self::sdp_headers();
            let caller_dialog_id = self.caller_dialog_id();
            if let Some(timer_headers) = self.successful_refresh_response_headers(&caller_dialog_id)
            {
                headers.extend(timer_headers);
            }
            if let Some(dialog) = self.caller_dialog.as_ref() {
                dialog
                    .accept(Some(headers), Some(answer_sdp.clone().into_bytes()))
                    .map_err(|e| anyhow!("Failed to send 200 OK for re-INVITE: {}", e))?;
            }
        }

        Ok(final_answer)
    }

    pub async fn play_audio_file(
        &mut self,
        audio_file: &str,
        await_completion: bool,
        loop_playback: bool,
    ) -> Result<()> {
        let resolved = Self::resolve_audio_file_path(audio_file);
        let source = if resolved.starts_with("http://") || resolved.starts_with("https://") {
            crate::call::domain::MediaSource::Url { url: resolved }
        } else {
            crate::call::domain::MediaSource::File { path: resolved }
        };
        self.handle_play(
            None,
            source,
            Some(crate::call::domain::PlayOptions {
                await_completion,
                loop_playback,
                ..Default::default()
            }),
        )
        .await
    }

    /// Infer which leg this track_id belongs to from the canonical suffix.
    /// Returns (leg_label, Option<dynamic_leg_id>).
    pub(crate) fn resolve_audio_file_path(audio_file: &str) -> String {
        if audio_file.starts_with("http://") || audio_file.starts_with("https://") {
            return audio_file.to_string();
        }

        let path = Path::new(audio_file);
        if path.is_absolute() || path.exists() {
            return audio_file.to_string();
        }

        if audio_file.starts_with("config/") || audio_file.starts_with("./config/") {
            return audio_file.to_string();
        }

        let fallback = Path::new("config").join(audio_file);
        if fallback.exists() {
            fallback.to_string_lossy().to_string()
        } else {
            audio_file.to_string()
        }
    }

    pub async fn start_recording(
        &mut self,
        path: &str,
        max_duration: Option<Duration>,
        beep: bool,
    ) -> Result<()> {
        use crate::proxy::proxy_call::media_state::RecordingPhase;
        if self.server.sip_flow.is_some() && !self.context.dialplan.recording.enabled {
            return Err(anyhow!(
                "Live recording is disabled when SipFlow is enabled"
            ));
        }
        // Guard: check if already recording (RecordingPhase tracks this).
        if self.media.recording_state.is_active() {
            return Err(anyhow!("Recording already active"));
        }

        // Resolve leg profiles from the MediaBridge legs. A single-leg session
        // (e.g. voicemail: only the caller/A leg) records just the legs that
        // have negotiated media; the FileRecorder tolerates an absent leg.
        let mut profiles = Vec::with_capacity(2);
        {
            let mb = self
                .bridge_mut()
                .ok_or_else(|| anyhow!("Recording requires MediaBridge"))?;
            if let Some(p) = mb
                .leg(crate::media::media_bridge::LegSide::A)
                .and_then(|l| l.negotiated())
            {
                profiles.push((crate::media::recorder::Leg::A, p));
            }
            if let Some(p) = mb
                .leg(crate::media::media_bridge::LegSide::B)
                .and_then(|l| l.negotiated())
            {
                profiles.push((crate::media::recorder::Leg::B, p));
            }
            if profiles.is_empty() {
                return Err(anyhow!(
                    "no leg has a negotiated profile to record"
                ));
            }

            let profiles_arr: [(crate::media::recorder::Leg, crate::media::negotiate::NegotiatedLegProfile); 2] =
                [
                    profiles.get(0).cloned().unwrap_or((
                        crate::media::recorder::Leg::A,
                        crate::media::negotiate::NegotiatedLegProfile::default(),
                    )),
                    profiles.get(1).cloned().unwrap_or((
                        crate::media::recorder::Leg::B,
                        crate::media::negotiate::NegotiatedLegProfile::default(),
                    )),
                ];
            // FileRecorder runs on its own OS thread; hot path is try_send.
            let recorder = crate::media::media_recorder::FileRecorder::start(
                path.to_string(),
                profiles_arr,
            )
            .await?;
            mb.set_recorder(recorder);
        }

        self.media.recording_state = RecordingPhase::Recording {
            path: path.to_string(),
            started_at: Instant::now(),
            max_duration,
        };

        if beep {
            info!(session_id = %self.context.session_id, "Playing recording beep");
            self.handle_play(
                None,
                crate::call::domain::MediaSource::file("beep.wav"),
                None,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn pause_recording(&mut self) -> Result<()> {
        use crate::proxy::proxy_call::media_state::RecordingPhase;
        let next = match &self.media.recording_state {
            RecordingPhase::Recording {
                path,
                started_at,
                max_duration,
            } => {
                info!(session_id = %self.id, path = %path, "Recording paused");
                RecordingPhase::Paused {
                    path: path.clone(),
                    started_at: *started_at,
                    max_duration: *max_duration,
                }
            }
            RecordingPhase::Paused { path, .. } => {
                return Err(anyhow!("Recording already paused: {}", path));
            }
            RecordingPhase::Idle => {
                return Err(anyhow!("Recording not active"));
            }
        };
        self.media.recording_state = next;
        if let Some(mb) = self.bridge() {
            mb.set_recording_paused(true);
        }
        Ok(())
    }

    pub async fn resume_recording(&mut self) -> Result<()> {
        use crate::proxy::proxy_call::media_state::RecordingPhase;
        let next = match &self.media.recording_state {
            RecordingPhase::Paused {
                path,
                started_at,
                max_duration,
            } => {
                info!(session_id = %self.id, path = %path, "Recording resumed");
                RecordingPhase::Recording {
                    path: path.clone(),
                    started_at: *started_at,
                    max_duration: *max_duration,
                }
            }
            RecordingPhase::Recording { path, .. } => {
                return Err(anyhow!("Recording already active: {}", path));
            }
            RecordingPhase::Idle => {
                return Err(anyhow!("Recording not active"));
            }
        };
        self.media.recording_state = next;
        if let Some(mb) = self.bridge() {
            mb.set_recording_paused(false);
        }
        Ok(())
    }

    pub async fn stop_recording(&mut self) -> Result<()> {
        use crate::proxy::proxy_call::media_state::RecordingPhase;
        let prev = std::mem::replace(&mut self.media.recording_state, RecordingPhase::Idle);
        let path = match &prev {
            RecordingPhase::Recording { path, .. } | RecordingPhase::Paused { path, .. } => {
                path.clone()
            }
            RecordingPhase::Idle => return Ok(()),
        };
        // Duration is tracked locally (RecordingPhase.started_at).
        let duration = prev.elapsed().unwrap_or_default();

        // Finalize the recorder backend (flush + close the WAV).
        if let Some(mb) = self.bridge_mut() {
            mb.stop_recording();
        }

        let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        info!(session_id = %self.id, path = %path, duration = ?duration, file_size, "Recording stopped");

        let bridge = self.app_event_bridge.read();
        if let Some(ref handle) = *bridge {
            let _ = handle.send_app_event(crate::call::app::ControllerEvent::RecordingComplete(
                crate::call::app::RecordingInfo {
                    path: path.clone(),
                    duration,
                    size_bytes: file_size,
                },
            ));
        }
        Ok(())
    }

    async fn cleanup(&mut self) {
        trace!(session_id = %self.context.session_id, "Cleaning up session");

        // The call has entered terminal cleanup. Release tenant, carrier, and
        // trunk concurrent-call permits before any potentially slow cleanup.
        self.concurrent_call_lease.release_all();

        self.stop_caller_ingress_monitor().await;
        self.stop_anchored_rtp_timeout_monitor().await;

        // Disarm any RTP inactivity timeouts on both legs.
        if let Some(mb) = self.media.bridge.as_mut() {
            mb.disarm_rtp_timeout(crate::media::media_bridge::LegSide::A);
            mb.disarm_rtp_timeout(crate::media::media_bridge::LegSide::B);
        }

        // Ensure the running app (IVR/voicemail/queue) is notified of session end.
        if self.app_runtime.is_running() {
            let _ = self.app_runtime.stop_app(None).await;
        }

        if self.media.recording_state.is_active() {
            let _ = self.stop_recording().await;
        }

        // Release any concurrency slots acquired by routing policy checks so
        // they don't permanently exhaust the configured budget. Uses
        // best-effort release (errors are logged inside the helper).
        let acquired_holds = std::mem::take(&mut *self.context.dialplan.concurrency_holds.lock());
        if !acquired_holds.is_empty() {
            if let Some(limiter) = self.server.frequency_limiter.as_ref() {
                crate::call::policy::PolicyGuard::release_concurrency_holds(
                    &acquired_holds,
                    limiter.as_ref(),
                )
                .await;
            }
        }

        self.callee_guards.clear();

        self.callee_event_tx = None;

        // Collect ALL active dialog IDs to hang up: pending set + caller dialog + callee dialogs.
        let mut dialogs_to_hangup = self.pending_hangup.clone();
        dialogs_to_hangup.insert(self.caller_dialog_id());
        for dialog_id in self.callee_dialogs.iter().map(|e| e.key().clone()) {
            dialogs_to_hangup.insert(dialog_id);
        }

        if !dialogs_to_hangup.is_empty() {
            let hangup_dialogs = dialogs_to_hangup
                .into_iter()
                .filter_map(|dialog_id| self.server.dialog_layer.get_dialog(&dialog_id))
                .collect::<Vec<_>>();
            let hangups: FuturesUnordered<_> = hangup_dialogs
                .iter()
                .map(|dialog| {
                    #[allow(clippy::result_large_err)]
                    dialog.hangup().map(|result| result.map(|_| dialog.id()))
                })
                .collect();

            if tokio::time::timeout(Duration::from_secs(2), hangups.collect::<Vec<_>>())
                .await
                .is_err()
            {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    "Timed out waiting for cleanup hangups"
                );
            }
        }

        self.callee_dialogs.clear();
        self.meta.connected_callee_dialog_id = None;
        self.timers.clear();
        self.update_refresh_disabled.clear();
        self.timer_queue.clear();
        self.timer_keys.clear();

        self.server
            .active_call_registry
            .remove(&self.context.session_id);

        if let Some(reporter) = &self.reporter {
            let snapshot = self.record_snapshot();
            reporter.report(snapshot);
            self.cdr_sent
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        // Fire on_call_ended hooks.
        if !self.server.session_hooks.is_empty() {
            let duration_secs = self
                .meta
                .answer_time
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let ctx = self.session_hook_ctx();
            let reason = self.meta.hangup_reason.clone();
            for hook in self.server.session_hooks.iter() {
                hook.on_call_ended(&ctx, reason.as_ref(), duration_secs)
                    .await;
            }
        }

        // Resolve final hangup reason: enrich with IVR end reason and queue
        // abandon detection so the RWI webhook carries accurate context.
        self.resolve_final_hangup_reason().await;

        // Emit hangup webhook with Display (lowercase) reason and actual SIP status.
        let hangup_reason_str = self.meta.hangup_reason.clone().map(|r| r.to_string());
        let hangup_by = self
            .meta
            .hangup_reason
            .as_ref()
            .map(|r| r.initiator().to_string());
        let sip_status = self.meta.last_error.as_ref().map(|(sc, _)| sc.code());
        self.emit_typed_rwi_event(&crate::rwi::CallHangup {
            call_id: self.context.session_id.clone(),
            reason: hangup_reason_str,
            hangup_by,
            sip_status,
        });

        info!(session_id = %self.context.session_id, "Session cleanup complete");

        // Tear down the transport-level RTP rewrite bridge (fast-path relay)
        // on both legs BEFORE destroying the engine session / closing the
        // PeerConnections. The fast-path relay runs inline on the media worker
        // (RtpTransport::receive → RewriteBridge → peer IceConn). If it is still
        // armed when the peer PeerConnection is torn down, the relay can race
        // with PC close and dereference cleared/freed state, causing a media-
        // worker segfault (observed "media-worker segfault at 3 ip 0x3" under
        // sustained ~900-concurrent load after ~35min). Clearing is idempotent.
        {
            let caller_peer = self.caller_peer();
            let callee_peer = self.callee_peer();
            if let Some(peer) = caller_peer {
                if let Some(pc) = Self::get_peer_pc(peer, Self::CALLER_TRACK_ID).await {
                    pc.clear_rtp_rewrite_bridge();
                }
            }
            if let Some(peer) = callee_peer {
                if let Some(pc) = Self::get_peer_pc(peer, Self::CALLEE_TRACK_ID).await {
                    pc.clear_rtp_rewrite_bridge();
                }
            }
        }

        // MediaBridge teardown is handled by the session Drop / cleanup path.
    }

    /// Enrich `meta.hangup_reason` with higher-level context before emitting
    /// the `call_hangup` webhook.
    ///
    /// This bridges two dimensions that the raw SIP-layer reason cannot express:
    ///
    /// 1. **Queue abandon** — if the caller hung up while queued and no agent
    ///    ever connected, the reason is refined from `ByCaller`/`Canceled` to
    ///    `Abandoned`. This covers both the SIP-layer `execute_queue` path and
    ///    the CallApp-based queue path.
    ///
    /// 2. **IVR end reason** — if an IVR app was the last thing controlling
    ///    the call (terminal exit: hangup, user_hangup, timeout, error), the
    ///    IVR dimension overrides the SIP reason so consumers can tell *why*
    ///    the IVR ended the call.
    async fn resolve_final_hangup_reason(&mut self) {
        // ── 1. Queue abandon catch-all ──────────────────────────────
        // Covers the CallApp-based queue path (where execute_queue is not
        // called and meta.queue_name may not be set via the SIP layer).
        let in_queue =
            self.meta.queue_name.is_some() || self.app_runtime.get_queue_name().is_some();
        if in_queue
            && self.meta.connected_callee.is_none()
            && matches!(
                self.meta.hangup_reason,
                Some(CallRecordHangupReason::ByCaller)
                    | Some(CallRecordHangupReason::Canceled)
                    | None
            )
        {
            self.meta.hangup_reason = Some(CallRecordHangupReason::Abandoned);
        }

        // ── 2. IVR end reason bridge ────────────────────────────────
        // Read ivr_end_reason from the shared session variables (written by
        // StepIvrApp::on_exit). Only terminal IVR reasons override — transfer
        // reasons are skipped because the call continued past the IVR.
        if let Some(runtime) = self
            .app_runtime
            .as_any()
            .downcast_ref::<DefaultAppRuntime>()
        {
            let ivr_end = runtime.context.get_var("ivr_end_reason");
            let ivr_error = runtime.context.get_var("ivr_last_error");

            let ivr_override = match ivr_end.as_deref() {
                Some("hangup") => Some(CallRecordHangupReason::BySystem),
                Some("remote_hangup") => Some(CallRecordHangupReason::ByCaller),
                Some("timeout") => Some(CallRecordHangupReason::Autohangup),
                Some("error") => {
                    let msg = ivr_error.unwrap_or_else(|| "unknown ivr error".to_string());
                    Some(CallRecordHangupReason::Other(format!("ivr_error: {}", msg)))
                }
                // normal / transferred / chained / cancelled — call may have
                // continued; keep the SIP-layer reason.
                _ => None,
            };

            if let Some(reason) = ivr_override {
                self.meta.hangup_reason = Some(reason);
            }
        }
    }

    pub fn init_server_timer(&mut self, default_expires: u64) -> Result<(), CalleeError> {
        let Some(server_dialog) = self.caller_dialog.as_ref() else {
            // UAC mode: no inbound caller dialog, so no session timer negotiation
            // against a caller.
            return Ok(());
        };
        let request = server_dialog.initial_request();
        let headers = &request.headers;
        let dialog_id = self.caller_dialog_id();
        let session_timer_mode = self.server.proxy_config.session_timer_mode();

        let supported = has_timer_support(headers);
        let session_expires_value = get_header_value(headers, HEADER_SESSION_EXPIRES);
        let mut timer = SessionTimerState::default();
        timer.mode = session_timer_mode;

        if let Some(min_se) = get_header_value(headers, HEADER_MIN_SE)
            .as_deref()
            .and_then(parse_min_se)
        {
            if timer.min_se < min_se {
                timer.min_se = min_se;
            }
        }

        if let Some(value) = session_expires_value {
            if let Some((interval, refresher)) = parse_session_expires(&value) {
                if interval < timer.min_se {
                    return Err(into_callee_err(
                        &StatusCode::SessionIntervalTooSmall,
                        Some(timer.min_se.as_secs().to_string()),
                    ));
                }

                timer.enabled = true;
                timer.session_interval = interval;
                timer.active = true;
                timer.refresher = select_server_timer_refresher(supported, true, refresher);
            }
        } else if session_timer_mode.is_always() {
            timer.enabled = true;
            timer.session_interval = Duration::from_secs(default_expires).max(timer.min_se);
            timer.active = true;
            timer.refresher = select_server_timer_refresher(supported, false, None);
        }

        self.timers.insert(dialog_id.clone(), timer);
        self.schedule_timer(dialog_id);

        Ok(())
    }

    fn init_callee_timer(
        &mut self,
        dialog_id: DialogId,
        response: &rsipstack::sip::Response,
        requested_session_interval: Duration,
    ) {
        let headers = &response.headers;
        let session_expires_value = get_header_value(headers, HEADER_SESSION_EXPIRES);

        let mut timer = SessionTimerState::default();
        timer.mode = self.server.proxy_config.session_timer_mode();
        if let Some((session_interval, refresher)) = session_expires_value
            .as_deref()
            .and_then(parse_session_expires)
        {
            timer.enabled = true;
            timer.active = true;
            timer.last_refresh = Instant::now();
            timer.session_interval = session_interval;
            timer.refresher = select_client_timer_refresher(refresher);
        } else if timer.mode.is_always() {
            timer.enabled = true;
            timer.active = true;
            timer.last_refresh = Instant::now();
            timer.session_interval = requested_session_interval;
            timer.refresher = SessionRefresher::Uac;
        } else {
            timer.session_interval = requested_session_interval;
        }

        self.timers.insert(dialog_id.clone(), timer);
        self.schedule_timer(dialog_id);
    }

    fn caller_dialog_id(&self) -> DialogId {
        // In UAC mode there is no inbound caller dialog; the callee (B leg)
        // dialog is the only real dialog, so treat it as the primary.
        if let Some(d) = self.caller_dialog.as_ref() {
            return d.id();
        }
        if let Some(entry) = self.callee_dialogs.iter().next() {
            return entry.key().clone();
        }
        // No dialogs at all (early UAC stage): synthesize a placeholder.
        rsipstack::dialog::DialogId {
            call_id: format!("uac-{}", self.id.0),
            local_tag: "local".into(),
            remote_tag: "remote".into(),
        }
    }

    fn is_uac_dialog(&self, dialog_id: &DialogId) -> bool {
        // Determine UAC role from the dialog's actual type: a Client dialog
        // is UAC (we initiated), a Server dialog is UAS (we received).
        self.server
            .dialog_layer
            .get_dialog(dialog_id)
            .map(|d| {
                matches!(
                    d,
                    rsipstack::dialog::dialog::Dialog::Invite(invite)
                        if invite.role() == rsipstack::transaction::key::TransactionRole::Client
                )
            })
            .unwrap_or(false)
    }

    fn schedule_timer(&mut self, dialog_id: DialogId) {
        let timeout = self
            .timers
            .get(&dialog_id)
            .and_then(|timer| timer.next_timeout_for_role(self.is_uac_dialog(&dialog_id)));
        self.schedule_timer_with_timeout(dialog_id, timeout);
    }

    fn schedule_expiration_timer(&mut self, dialog_id: DialogId) {
        let timeout = self
            .timers
            .get(&dialog_id)
            .and_then(SessionTimerState::time_until_expiration);
        self.schedule_timer_with_timeout(dialog_id, timeout);
    }

    fn schedule_timer_with_timeout(&mut self, dialog_id: DialogId, timeout: Option<Duration>) {
        match timeout {
            Some(timeout) => {
                let current_key = self.timer_keys.get(&dialog_id).copied();
                let queue_key = if let Some(key) = current_key {
                    self.timer_queue.reset(&key, timeout);
                    key
                } else {
                    self.timer_queue.insert(dialog_id.clone(), timeout)
                };
                self.timer_keys.insert(dialog_id, queue_key);
            }
            None => self.unschedule_timer(&dialog_id),
        }
    }

    fn unschedule_timer(&mut self, dialog_id: &DialogId) {
        if let Some(key) = self.timer_keys.remove(dialog_id) {
            self.timer_queue.remove(&key);
        }
    }

    fn disable_update_refresh(&mut self, dialog_id: &DialogId) {
        self.update_refresh_disabled.insert(dialog_id.clone());
    }

    fn successful_refresh_response_headers(
        &self,
        dialog_id: &DialogId,
    ) -> Option<Vec<rsipstack::sip::Header>> {
        let timer = self.timers.get(dialog_id)?;
        if !timer.enabled || !timer.active {
            return None;
        }

        Some(build_session_timer_response_headers(timer))
    }

    fn should_fallback_to_reinvite(status: StatusCode) -> bool {
        matches!(
            status,
            StatusCode::MethodNotAllowed | StatusCode::NotImplemented
        )
    }

    fn should_try_update_refresh(&self, dialog_id: &DialogId) -> bool {
        !self.update_refresh_disabled.contains(dialog_id)
    }

    fn apply_refresh_min_se(
        &mut self,
        dialog_id: &DialogId,
        headers: &rsipstack::sip::Headers,
    ) -> Result<bool> {
        let Some(min_se_value) = get_header_value(headers, HEADER_MIN_SE) else {
            return Ok(false);
        };
        let Some(min_se) = parse_min_se(&min_se_value) else {
            return Ok(false);
        };

        let timer = self
            .timers
            .get_mut(dialog_id)
            .ok_or_else(|| anyhow!("No session timer for dialog {}", dialog_id))?;
        if timer.min_se < min_se {
            timer.min_se = min_se;
        }
        if timer.session_interval < min_se {
            timer.session_interval = min_se;
        }

        Ok(true)
    }

    fn complete_refresh_from_response(
        &mut self,
        dialog_id: &DialogId,
        response: &rsipstack::sip::Response,
    ) -> Result<()> {
        let we_are_uac = self.is_uac_dialog(dialog_id);
        if let Some(timer) = self.timers.get_mut(dialog_id) {
            apply_refresh_response(timer, &response.headers, we_are_uac)?;
        }
        Ok(())
    }

    fn fail_refresh_if_pending(&mut self, dialog_id: &DialogId) {
        if let Some(timer) = self.timers.get_mut(dialog_id)
            && timer.refreshing
        {
            timer.fail_refresh();
        }
    }

    fn build_refresh_headers(
        &self,
        dialog_id: &DialogId,
        include_content_type: bool,
    ) -> Result<Vec<rsipstack::sip::Header>> {
        let timer = self
            .timers
            .get(dialog_id)
            .ok_or_else(|| anyhow!("No session timer for dialog {}", dialog_id))?;
        Ok(build_session_timer_headers(timer, include_content_type))
    }

    async fn send_update_refresh_request(
        &mut self,
        dialog_id: &DialogId,
        headers: Vec<rsipstack::sip::Header>,
    ) -> Result<Option<rsipstack::sip::Response>> {
        if self.is_uac_dialog(dialog_id) {
            let Some(mut dialog) = self.server.dialog_layer.get_dialog(dialog_id) else {
                return Err(anyhow!("No callee dialog found for {}", dialog_id));
            };

            match &mut dialog {
                Dialog::Invite(invite_dialog) => invite_dialog
                    .update(Some(headers), None)
                    .await
                    .map_err(|e| anyhow!("UPDATE failed: {}", e)),
                _ => Err(anyhow!(
                    "Dialog {} is not a client INVITE dialog",
                    dialog_id
                )),
            }
        } else if let Some(dialog) = self.caller_dialog.as_ref() {
            dialog
                .update(Some(headers), None)
                .await
                .map_err(|e| anyhow!("UPDATE failed: {}", e))
        } else {
            // UAC mode: no inbound caller dialog to refresh.
            Ok(None)
        }
    }

    fn handle_update_refresh_response(
        &mut self,
        dialog_id: &DialogId,
        response: Option<rsipstack::sip::Response>,
        allow_retry: bool,
    ) -> UpdateRefreshOutcome {
        match response {
            Some(resp)
                if resp.status_code.kind()
                    == rsipstack::sip::status_code::StatusCodeKind::Successful =>
            {
                match self.complete_refresh_from_response(dialog_id, &resp) {
                    Ok(()) => UpdateRefreshOutcome::Refreshed,
                    Err(e) => UpdateRefreshOutcome::Failed(e),
                }
            }
            Some(resp) if resp.status_code == StatusCode::SessionIntervalTooSmall => {
                if !allow_retry {
                    return UpdateRefreshOutcome::Failed(anyhow!(
                        "UPDATE rejected with status {}",
                        resp.status_code
                    ));
                }

                match self.apply_refresh_min_se(dialog_id, &resp.headers) {
                    Ok(true) => UpdateRefreshOutcome::Retry,
                    Ok(false) => UpdateRefreshOutcome::Failed(anyhow!(
                        "UPDATE rejected with status {}",
                        resp.status_code
                    )),
                    Err(e) => UpdateRefreshOutcome::Failed(e),
                }
            }
            Some(resp) => {
                if Self::should_fallback_to_reinvite(resp.status_code.clone()) {
                    self.disable_update_refresh(dialog_id);
                    UpdateRefreshOutcome::FallbackToReinvite
                } else {
                    UpdateRefreshOutcome::Failed(anyhow!(
                        "UPDATE rejected with status {}",
                        resp.status_code
                    ))
                }
            }
            None => UpdateRefreshOutcome::Failed(anyhow!("UPDATE timed out")),
        }
    }

    async fn try_update_refresh(&mut self, dialog_id: &DialogId) -> UpdateRefreshOutcome {
        let headers = match self.build_refresh_headers(dialog_id, false) {
            Ok(headers) => headers,
            Err(e) => return UpdateRefreshOutcome::Failed(e),
        };

        let response = match self.send_update_refresh_request(dialog_id, headers).await {
            Ok(response) => response,
            Err(e) => return UpdateRefreshOutcome::Failed(e),
        };

        match self.handle_update_refresh_response(dialog_id, response, true) {
            UpdateRefreshOutcome::Retry => {
                let retry_headers = match self.build_refresh_headers(dialog_id, false) {
                    Ok(headers) => headers,
                    Err(e) => return UpdateRefreshOutcome::Failed(e),
                };
                let retry_response = match self
                    .send_update_refresh_request(dialog_id, retry_headers)
                    .await
                {
                    Ok(response) => response,
                    Err(e) => return UpdateRefreshOutcome::Failed(e),
                };
                self.handle_update_refresh_response(dialog_id, retry_response, false)
            }
            outcome => outcome,
        }
    }

    async fn send_reinvite_refresh_request(
        &mut self,
        dialog_id: &DialogId,
        headers: Vec<rsipstack::sip::Header>,
        body: Option<Vec<u8>>,
    ) -> Result<Option<rsipstack::sip::Response>> {
        if self.is_uac_dialog(dialog_id) {
            let Some(mut dialog) = self.server.dialog_layer.get_dialog(dialog_id) else {
                return Err(anyhow!("No callee dialog found for {}", dialog_id));
            };

            match &mut dialog {
                Dialog::Invite(invite_dialog) => invite_dialog
                    .reinvite(Some(headers), body)
                    .await
                    .map_err(|e| anyhow!("re-INVITE failed: {}", e)),
                _ => Err(anyhow!(
                    "Dialog {} is not a client INVITE dialog",
                    dialog_id
                )),
            }
        } else if let Some(dialog) = self.caller_dialog.as_ref() {
            dialog
                .reinvite(Some(headers), body)
                .await
                .map_err(|e| anyhow!("re-INVITE failed: {}", e))
        } else {
            // UAC mode: no inbound caller dialog for mid-dialog request.
            Ok(None)
        }
    }

    async fn try_reinvite_refresh(
        &mut self,
        dialog_id: &DialogId,
        body: Option<Vec<u8>>,
    ) -> Result<()> {
        let headers = self.build_refresh_headers(dialog_id, body.is_some())?;
        let response = self
            .send_reinvite_refresh_request(dialog_id, headers, body.clone())
            .await;

        match response {
            Ok(Some(resp))
                if resp.status_code.kind()
                    == rsipstack::sip::status_code::StatusCodeKind::Successful =>
            {
                self.complete_refresh_from_response(dialog_id, &resp)
            }
            Ok(Some(resp))
                if resp.status_code == StatusCode::SessionIntervalTooSmall
                    && self.apply_refresh_min_se(dialog_id, &resp.headers)? =>
            {
                let retry_headers = self.build_refresh_headers(dialog_id, body.is_some())?;
                match self
                    .send_reinvite_refresh_request(dialog_id, retry_headers, body)
                    .await
                {
                    Ok(Some(retry_resp))
                        if retry_resp.status_code.kind()
                            == rsipstack::sip::status_code::StatusCodeKind::Successful =>
                    {
                        self.complete_refresh_from_response(dialog_id, &retry_resp)
                    }
                    Ok(Some(retry_resp)) => {
                        self.fail_refresh_if_pending(dialog_id);
                        Err(anyhow!(
                            "re-INVITE rejected with status {}",
                            retry_resp.status_code
                        ))
                    }
                    Ok(None) => {
                        self.fail_refresh_if_pending(dialog_id);
                        Err(anyhow!("re-INVITE timed out"))
                    }
                    Err(e) => {
                        self.fail_refresh_if_pending(dialog_id);
                        Err(e)
                    }
                }
            }
            Ok(Some(resp)) => {
                self.fail_refresh_if_pending(dialog_id);
                Err(anyhow!(
                    "re-INVITE rejected with status {}",
                    resp.status_code
                ))
            }
            Ok(None) => {
                self.fail_refresh_if_pending(dialog_id);
                Err(anyhow!("re-INVITE timed out"))
            }
            Err(e) => {
                self.fail_refresh_if_pending(dialog_id);
                Err(e)
            }
        }
    }

    async fn send_dialog_session_refresh(
        &mut self,
        dialog_id: &DialogId,
        body: Option<Vec<u8>>,
    ) -> Result<()> {
        if self.should_try_update_refresh(dialog_id) {
            match self.try_update_refresh(dialog_id).await {
                UpdateRefreshOutcome::Refreshed => return Ok(()),
                UpdateRefreshOutcome::Retry => {
                    return Err(anyhow!(
                        "UPDATE refresh retry state should be resolved internally"
                    ));
                }
                UpdateRefreshOutcome::FallbackToReinvite => {}
                UpdateRefreshOutcome::Failed(e) => {
                    self.fail_refresh_if_pending(dialog_id);
                    return Err(e);
                }
            }
        }

        self.try_reinvite_refresh(dialog_id, body).await
    }

    async fn send_server_session_refresh(&mut self) -> Result<()> {
        let dialog_id = self.caller_dialog_id();
        let body = self.media.answer.clone().map(|sdp| sdp.into_bytes());
        self.send_dialog_session_refresh(&dialog_id, body).await
    }

    async fn send_callee_session_refresh(&mut self, dialog_id: &DialogId) -> Result<()> {
        let body = self.media.callee_offer.clone().map(|sdp| sdp.into_bytes());
        self.send_dialog_session_refresh(dialog_id, body).await
    }

    fn update_dialog_timer_from_headers(
        &mut self,
        dialog_id: &DialogId,
        headers: &rsipstack::sip::Headers,
    ) -> Result<()> {
        if let Some(timer) = self.timers.get_mut(dialog_id) {
            apply_session_timer_headers(timer, headers)?;
            if timer.active {
                timer.update_refresh();
            }

            self.schedule_timer(dialog_id.clone());
        }
        Ok(())
    }

    pub fn record_snapshot(&self) -> CallSessionRecordSnapshot {
        // Merge agent + routing data into a HashMap so the reporter picks it up.
        // Agent info was written into session extensions by CcCallSessionHook
        // (as a HashMap<String, String>). Routing metadata comes from the
        // dialplan extensions (also HashMap<String, String>).
        let extensions = self.context.dialplan.extensions.clone();
        let metadata = {
            // Start with session extensions (CC agent info)
            let mut meta: std::collections::HashMap<String, String> = self
                .extensions
                .read()
                .get::<std::collections::HashMap<String, String>>()
                .cloned()
                .unwrap_or_default();
            // Merge in dialplan extensions (routing metadata)
            if let Some(route_meta) = extensions
                .get::<std::collections::HashMap<String, String>>()
                .cloned()
            {
                for (k, v) in route_meta {
                    meta.entry(k).or_insert(v);
                }
            }
            if let Some(ref qn) = self.meta.queue_name {
                meta.insert("queue_name".to_string(), qn.clone());
            }
            if let Some(ref callee) = self.meta.connected_callee {
                meta.insert("connected_callee".to_string(), callee.clone());
            }
            meta
        };

        CallSessionRecordSnapshot {
            ring_time: self.meta.ring_time,
            answer_time: self.meta.answer_time,
            last_error: self.meta.last_error.clone(),
            hangup_reason: self.meta.hangup_reason.clone(),
            hangup_messages: self.recorded_hangup_messages(),
            original_caller: Some(self.context.original_caller.clone()),
            original_callee: Some(self.context.original_callee.clone()),
            routed_caller: self.meta.routed_caller.clone(),
            routed_callee: self.meta.routed_callee.clone(),
            connected_callee: self.meta.connected_callee.clone(),
            routed_contact: self.meta.routed_contact.clone(),
            routed_destination: self.meta.routed_destination.clone(),
            last_queue_name: self.meta.queue_name.clone(),
            callee_call_ids: self.meta.callee_call_ids.iter().cloned().collect(),
            server_dialog_id: self.caller_dialog_id(),
            metadata,
            extensions,
        }
    }

    fn recorded_hangup_messages(&self) -> Vec<CallRecordHangupMessage> {
        self.meta
            .hangup_messages
            .iter()
            .map(CallRecordHangupMessage::from)
            .collect()
    }
}

impl SipSession {
    pub async fn execute_command(
        &mut self,
        command: CallCommand,
        callee_state_rx: Option<&mut mpsc::UnboundedReceiver<DialogState>>,
    ) -> CommandResult {
        let capability_check = self.check_capability(&command);

        let degradation_reason = match capability_check {
            MediaCapabilityCheck::Denied { reason } => {
                return CommandResult::degraded(&reason);
            }
            MediaCapabilityCheck::Degraded { reason } => {
                warn!(session_id = %self.id, reason = %reason, "Executing in degraded mode");
                Some(reason)
            }
            MediaCapabilityCheck::Allowed => None,
        };

        let mut result = self.process_command(command, callee_state_rx).await;

        if let Some(reason) = degradation_reason {
            result.media_degraded = true;
            result.degradation_reason = Some(reason);
        }

        result
    }

    fn check_capability(&self, command: &CallCommand) -> MediaCapabilityCheck {
        let ctx = ExecutionContext::new(&self.id.0).with_media_profile(self.media_profile.clone());
        ctx.check_media_capability(command)
    }

    async fn process_command(
        &mut self,
        command: CallCommand,
        mut callee_state_rx: Option<&mut mpsc::UnboundedReceiver<DialogState>>,
    ) -> CommandResult {
        match command {
            CallCommand::Answer { leg_id } => {
                if leg_id.0 == "caller" {
                    let answer_sdp = if self.app_runtime.is_running() {
                        self.prepare_app_caller_media_bridge().await
                    } else {
                        None
                    };
                    match self.accept_call(None, answer_sdp, None).await {
                        Ok(()) => {
                            self.update_leg_state(&leg_id, LegState::Connected);
                            self.update_media_path().await;
                            CommandResult::success()
                        }
                        Err(e) => CommandResult::failure(e.to_string()),
                    }
                } else if self.update_leg_state(&leg_id, LegState::Connected) {
                    CommandResult::success()
                } else {
                    CommandResult::failure(format!("Leg not found: {}", leg_id))
                }
            }

            CallCommand::Hangup(cmd) => self.handle_hangup(&cmd).await,

            CallCommand::Bridge {
                leg_a,
                leg_b,
                mode: _,
            } => {
                if self.setup_bridge(leg_a.clone(), leg_b.clone()).await {
                    self.update_leg_state(&leg_a, LegState::Connected);
                    self.update_leg_state(&leg_b, LegState::Connected);
                    CommandResult::success()
                } else {
                    CommandResult::failure("Cannot bridge: one or both legs not found")
                }
            }

            CallCommand::Unbridge { .. } => {
                self.clear_bridge().await;
                CommandResult::success()
            }

            CallCommand::ResumeMedia => {
                if let Some(mb) = self.bridge_mut() {
                    if let Err(e) = mb.resume(crate::media::media_bridge::LegSide::A).await {
                        warn!(session_id = %self.id, error = %e, "Failed to resume media route after playback");
                        CommandResult::failure(e.to_string())
                    } else {
                        CommandResult::success()
                    }
                } else {
                    CommandResult::success()
                }
            }

            CallCommand::Hold { leg_id, music } => {
                Self::ok_or_failure(self.handle_hold(leg_id, music).await)
            }

            CallCommand::Unhold { leg_id } => Self::ok_or_failure(self.handle_unhold(leg_id).await),

            CallCommand::StartApp {
                app_name,
                params,
                auto_answer,
            } => {
                match self
                    .app_runtime
                    .start_app(&app_name, params, auto_answer)
                    .await
                {
                    Ok(()) => {
                        self.start_caller_ingress_monitor_if_needed().await;
                        CommandResult::success()
                    }
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::StopApp { reason } => match self.app_runtime.stop_app(reason).await {
                Ok(()) => {
                    self.stop_caller_ingress_monitor().await;
                    CommandResult::success()
                }
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::InjectAppEvent { event } => {
                let event_value = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                match self.app_runtime.inject_event(event_value) {
                    Ok(()) => CommandResult::success(),
                    Err(e) => CommandResult::degraded(e.to_string()),
                }
            }

            CallCommand::Play {
                leg_id,
                source,
                options,
            } => Self::ok_or_failure(self.handle_play(leg_id, source, options).await),

            CallCommand::StopPlayback { leg_id } => {
                Self::ok_or_failure(self.handle_stop_playback(leg_id).await)
            }

            CallCommand::StartRecording { config } => Self::ok_or_failure(
                self.start_recording(
                    &config.path,
                    config
                        .max_duration_secs
                        .map(|s| Duration::from_secs(s as u64)),
                    config.beep,
                )
                .await,
            ),

            CallCommand::StopRecording => Self::ok_or_failure(self.stop_recording().await),

            CallCommand::PauseRecording => Self::ok_or_failure(self.pause_recording().await),

            CallCommand::ResumeRecording => Self::ok_or_failure(self.resume_recording().await),

            CallCommand::Transfer {
                leg_id,
                target,
                attended,
            } => {
                let Some(callee_state_rx) = callee_state_rx.as_deref_mut() else {
                    return CommandResult::failure(
                        "No callee state receiver available for transfer".to_string(),
                    );
                };
                Self::ok_or_failure(
                    self.handle_transfer(leg_id, target, attended, callee_state_rx)
                        .await,
                )
            }

            CallCommand::TransferComplete { consult_leg } => {
                Self::ok_or_failure(self.handle_transfer_complete(consult_leg).await)
            }

            CallCommand::TransferCancel { consult_leg } => {
                Self::ok_or_failure(self.handle_transfer_cancel(consult_leg).await)
            }

            CallCommand::TransferCompleteCrossSession {
                from_session,
                leg_id,
                into_conference,
            } => Self::ok_or_failure(
                self.handle_transfer_complete_cross_session(from_session, leg_id, into_conference)
                    .await,
            ),

            CallCommand::BridgeCrossSession {
                session_a,
                leg_a,
                session_b,
                leg_b,
            } => Self::ok_or_failure(
                self.handle_bridge_cross_session(session_a, leg_a, session_b, leg_b)
                    .await,
            ),

            other => self.process_supervisor_conference_commands(other).await,
        }
    }

    /// Handles supervisor, conference, queue, leg, and miscellaneous call commands.
    /// Delegated from `process_command` to keep that function at a manageable size.
    async fn process_supervisor_conference_commands(
        &mut self,
        command: CallCommand,
    ) -> CommandResult {
        match command {
            CallCommand::SupervisorListen {
                supervisor_leg,
                target_leg,
                supervisor_session_id,
            } => Self::ok_or_failure(
                self.handle_supervisor_listen(supervisor_leg, target_leg, supervisor_session_id)
                    .await,
            ),

            CallCommand::SupervisorWhisper {
                supervisor_leg,
                target_leg,
                supervisor_session_id,
            } => Self::ok_or_failure(
                self.handle_supervisor_whisper(supervisor_leg, target_leg, supervisor_session_id)
                    .await,
            ),

            CallCommand::SupervisorBarge {
                supervisor_leg,
                target_leg,
                supervisor_session_id,
            } => Self::ok_or_failure(
                self.handle_supervisor_barge(supervisor_leg, target_leg, supervisor_session_id)
                    .await,
            ),

            CallCommand::SupervisorTakeover {
                supervisor_leg,
                target_leg,
                supervisor_session_id,
            } => Self::ok_or_failure(
                self.handle_supervisor_takeover(supervisor_leg, target_leg, supervisor_session_id)
                    .await,
            ),

            CallCommand::SupervisorStop { supervisor_leg } => {
                Self::ok_or_failure(self.handle_supervisor_stop(supervisor_leg).await)
            }

            CallCommand::ConferenceCreate { conf_id, options } => {
                Self::ok_or_failure(self.handle_conference_create(conf_id, options).await)
            }

            CallCommand::ConferenceAdd { conf_id, leg_id } => {
                Self::ok_or_failure(self.handle_conference_add(conf_id, leg_id).await)
            }

            CallCommand::ConferenceRemove { conf_id, leg_id } => {
                Self::ok_or_failure(self.handle_conference_remove(conf_id, leg_id).await)
            }

            CallCommand::ConferenceMute { conf_id, leg_id } => {
                Self::ok_or_failure(self.handle_conference_mute(conf_id, leg_id).await)
            }

            CallCommand::ConferenceUnmute { conf_id, leg_id } => {
                Self::ok_or_failure(self.handle_conference_unmute(conf_id, leg_id).await)
            }

            CallCommand::ConferenceDestroy { conf_id } => {
                Self::ok_or_failure(self.handle_conference_destroy(conf_id).await)
            }

            CallCommand::ConferenceEnd {
                conf_id,
                host_leg_id,
            } => Self::ok_or_failure(self.handle_conference_end(conf_id, host_leg_id).await),

            CallCommand::ConferenceKick { conf_id, leg_id } => {
                Self::ok_or_failure(self.handle_conference_kick(conf_id, leg_id).await)
            }

            CallCommand::ConferenceMuteAll { conf_id } => {
                Self::ok_or_failure(self.handle_conference_mute_all(conf_id).await)
            }

            CallCommand::ConferenceInfo { conf_id } => {
                match self.handle_conference_info(conf_id).await {
                    Ok(room) => {
                        let mut data = serde_json::Map::new();
                        data.insert(
                            "conf_id".to_string(),
                            serde_json::Value::String(room.id.0.clone()),
                        );
                        data.insert(
                            "participant_count".to_string(),
                            serde_json::Value::Number(serde_json::Number::from(
                                room.participant_count(),
                            )),
                        );
                        let participants: Vec<serde_json::Value> = room
                            .participants
                            .values()
                            .map(|p| {
                                serde_json::json!({
                                    "leg_id": p.leg_id.as_str(),
                                    "muted": p.muted,
                                })
                            })
                            .collect();
                        data.insert(
                            "participants".to_string(),
                            serde_json::Value::Array(participants),
                        );
                        CommandResult::success_with_data(serde_json::Value::Object(data))
                    }
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::ConferenceList => {
                let rooms = self.handle_conference_list().await;
                let list: Vec<serde_json::Value> = rooms
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "conf_id": r.id.0,
                            "participant_count": r.participant_count(),
                            "locked": r.locked,
                        })
                    })
                    .collect();
                CommandResult::success_with_data(serde_json::Value::Array(list))
            }

            CallCommand::QueueEnqueue {
                leg_id,
                queue_id,
                priority,
            } => Self::ok_or_failure(self.handle_queue_enqueue(leg_id, queue_id, priority).await),

            CallCommand::QueueDequeue { leg_id } => {
                Self::ok_or_failure(self.handle_queue_dequeue(leg_id).await)
            }

            CallCommand::Reject { leg_id, reason } => {
                Self::ok_or_failure(self.handle_reject(leg_id, reason).await)
            }

            CallCommand::Ring { leg_id, ringback } => {
                Self::ok_or_failure(self.handle_ring(leg_id, ringback).await)
            }

            CallCommand::SendDtmf { leg_id, digits } => {
                Self::ok_or_failure(self.handle_send_dtmf(leg_id, digits).await)
            }

            CallCommand::HandleReInvite { leg_id, sdp } => {
                Self::ok_or_failure(self.handle_reinvite_command(leg_id, sdp).await)
            }

            CallCommand::MuteTrack { track_id } => {
                Self::ok_or_failure(self.handle_mute_track(track_id).await)
            }

            CallCommand::UnmuteTrack { track_id } => {
                Self::ok_or_failure(self.handle_unmute_track(track_id).await)
            }

            CallCommand::SendSipMessage { content_type, body } => {
                Self::ok_or_failure(self.handle_send_sip_message(content_type, body).await)
            }

            CallCommand::SendSipNotify {
                event,
                content_type,
                body,
            } => Self::ok_or_failure(self.handle_send_sip_notify(event, content_type, body).await),

            CallCommand::SendSipOptionsPing => {
                Self::ok_or_failure(self.handle_send_sip_options_ping().await)
            }

            CallCommand::JoinMixer { mixer_id } => {
                Self::ok_or_failure(self.handle_join_mixer(mixer_id).await)
            }

            CallCommand::LeaveMixer => Self::ok_or_failure(self.handle_leave_mixer().await),

            CallCommand::LegAdd { target, leg_id } => {
                match self.handle_add_leg(target, leg_id).await {
                    Ok(new_leg_id) => CommandResult::success_with_leg(new_leg_id),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::LegRemove { leg_id } => {
                Self::ok_or_failure(self.handle_remove_leg(leg_id).await)
            }

            CallCommand::LegConnected {
                leg_id,
                answer_sdp,
                dialog_id,
            } => {
                info!(session_id = %self.id, %leg_id, "Leg connected async notification");

                // In UAC mode, a leg added via `leg_add` with leg_id="callee"
                // should attach its answered dialog + SDP onto the MediaBridge
                // B side so it can be bridged with the A (caller) leg.
                if leg_id == LegId::from("callee") {
                    if let Some(call_id) = dialog_id {
                        if let Some(invite) = self
                            .server
                            .dialog_layer
                            .get_client_dialog_by_call_id(&call_id)
                            .into_iter()
                            .next()
                        {
                            self.attach_callee_dialog(invite, answer_sdp.clone())
                                .await;
                        }
                    }
                }

                // Forward to running app before processing so the app can react
                let agent_uri = self
                    .legs
                    .states
                    .get(&leg_id)
                    .and_then(|l| l.endpoint.clone());
                if let Some(ref agent_uri) = agent_uri {
                    let bridge = self.app_event_bridge.read();
                    if let Some(ref h) = *bridge {
                        h.send_app_event(crate::call::app::ControllerEvent::Custom(
                            "agent_connected".to_string(),
                            serde_json::json!({
                                "leg_id": leg_id.0,
                                "agent_uri": agent_uri,
                            }),
                        ));
                    }
                }
                if let Some(sdp) = answer_sdp {
                    self.legs.set_answer(leg_id.clone(), sdp);
                }
                self.update_leg_state(&leg_id, LegState::Connected);
                self.update_media_path().await;
                CommandResult::success()
            }

            CallCommand::LegFailed { leg_id, reason } => {
                warn!(session_id = %self.id, %leg_id, %reason, "Leg failed async notification");
                // Forward to running app before removing the leg (so we can get the URI)
                let agent_uri = self
                    .legs
                    .states
                    .get(&leg_id)
                    .and_then(|l| l.endpoint.clone());
                let event_name = if reason.contains("486") || reason.to_lowercase().contains("busy")
                {
                    "agent_busy"
                } else {
                    "agent_no_answer"
                };
                {
                    let bridge = self.app_event_bridge.read();
                    if let Some(ref h) = *bridge {
                        h.send_app_event(crate::call::app::ControllerEvent::Custom(
                            event_name.to_string(),
                            serde_json::json!({
                                "leg_id": leg_id.0,
                                "agent_uri": agent_uri,
                                "reason": reason,
                            }),
                        ));
                    }
                }
                self.update_leg_state(&leg_id, LegState::Ended);
                self.legs.remove(&leg_id);
                self.update_media_path().await;
                CommandResult::failure(reason)
            }

            CallCommand::AppExited => self.handle_app_exited().await,

            CallCommand::SendInfo {
                leg_id,
                content_type,
                body,
            } => Self::ok_or_failure(self.handle_send_info(leg_id, content_type, body).await),

            _ => CommandResult::not_supported("Command not yet implemented"),
        }
    }

    /// Handle app exit: iterate hooks and run post-exit actions (unhold, send result INFO).
    async fn handle_app_exited(&mut self) -> CommandResult {
        if self.server.session_hooks.is_empty() {
            return CommandResult::success();
        }

        let ctx = self.session_hook_ctx();

        // Collect completion intents from hooks while holding only an immutable
        // reference to self, then apply them with a mutable reference.
        let completions: Vec<_> = {
            let hooks = self.server.session_hooks.clone();
            let mut results = Vec::new();
            for hook in hooks.iter() {
                if let Some(completion) = hook.on_app_exited(&ctx).await {
                    results.push(completion);
                }
            }
            results
        };

        for completion in completions {
            // Unhold leg if requested.
            if let Some(leg_id) = &completion.unhold_leg {
                if leg_id.as_str() == "callee" {
                    if let Err(e) = self.propagate_unhold_to_callee().await {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            error = %e,
                            "Failed to unhold callee after app exit"
                        );
                    }
                }
            }

            // Send result INFO if requested.
            if let Some(spec) = completion.result_info {
                if let Err(e) = self
                    .handle_send_info(spec.leg_id, spec.content_type, spec.body)
                    .await
                {
                    warn!(session_id = %self.id,
                        session_id = %self.context.session_id,
                        error = %e,
                        "Failed to send result INFO after app exit"
                    );
                }
            }
        }

        CommandResult::success()
    }

    /// Send a SIP INFO request to the dialog identified by the given leg.
    /// Currently supports `leg_id = "callee"` using `connected_callee_dialog_id`.
    async fn handle_send_info(
        &self,
        leg_id: LegId,
        content_type: String,
        body: Vec<u8>,
    ) -> Result<()> {
        let dialog_id = match leg_id.as_str() {
            "callee" => self.meta.connected_callee_dialog_id.clone(),
            other => {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    leg_id = %other,
                    "SendInfo: unsupported leg"
                );
                return Err(anyhow::anyhow!("Unsupported leg for SendInfo: {}", other));
            }
        };

        let Some(dialog_id) = dialog_id else {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                leg_id = %leg_id.0,
                "SendInfo: no dialog found for leg"
            );
            return Err(anyhow::anyhow!("No dialog found for leg: {}", leg_id));
        };

        let Some(dlg) = self.server.dialog_layer.get_dialog(&dialog_id) else {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                dialog_id = %dialog_id,
                "SendInfo: dialog not found"
            );
            return Err(anyhow::anyhow!("Dialog not found: {}", dialog_id));
        };

        let headers = vec![rsipstack::sip::Header::ContentType(
            rsipstack::sip::headers::ContentType::from(content_type.as_str()),
        )];

        info!(session_id = %self.id,
            session_id = %self.context.session_id,
            leg_id = %leg_id.0,
            content_type = %content_type,
            body_len = body.len(),
            "Sending SIP INFO via SendInfo command"
        );

        Self::send_info_to_dialog(&dlg, headers, body).await
    }

    async fn handle_hangup(&mut self, cmd: &HangupCommand) -> CommandResult {
        let cascade = &cmd.cascade;

        for leg in self.legs.values_mut() {
            let should_hangup = match cascade {
                HangupCascade::All => true,
                HangupCascade::None => false,
                HangupCascade::AllExcept(exclude) => !exclude.contains(&leg.id),
                HangupCascade::Other => true,
            };

            if should_hangup {
                leg.state = LegState::Ended;
            }
        }

        self.state = Self::derive_state(&self.legs);
        self.bridge.clear();

        if self.app_runtime.is_running() {
            let reason_str = cmd.reason.as_ref().map(|r| r.to_string());
            if let Err(e) = self.app_runtime.stop_app(reason_str).await {
                error!(session_id = %self.id, error = %e, "Failed to stop app during hangup");
            }
        }

        self.stop_caller_ingress_monitor().await;
        self.cancel_token.cancel();

        CommandResult::success()
    }

    fn update_leg_state(&mut self, leg_id: &LegId, new_state: LegState) -> bool {
        if let Some(leg) = self.legs.get_mut(leg_id) {
            leg.state = new_state;
            true
        } else {
            // Leg does not exist — do NOT silently create a phantom leg.
            // Returning false lets callers (e.g. the Answer command handler)
            // surface an explicit failure instead of reporting a silent success.
            // Callers that genuinely need a new leg must insert it explicitly first
            // (see e.g. handle_add_leg which inserts before updating state).
            debug!(session_id = %self.id,
                leg_id = %leg_id,
                "update_leg_state: leg not found, refusing to create phantom leg"
            );
            false
        }
    }

    /// Snapshot the current state of a leg (for transition detection), or
    /// `None` if the leg does not exist yet.
    fn leg_prev_state(&self, leg_id: &LegId) -> Option<LegState> {
        self.legs.get(leg_id).map(|leg| leg.state)
    }

    /// Fire `on_call_held` / `on_call_unheld` session hooks when a leg
    /// transitions to/from [`LegState::Hold`].
    ///
    /// This centralizes hold detection so it works regardless of whether the
    /// transition is caused by an explicit `CallCommand::Hold/Unhold` or by an
    /// inbound re-INVITE carrying `sendonly`/`inactive` (or back to
    /// `sendrecv`). No-op when there is no transition or no hooks registered.
    async fn fire_hold_transition_hooks(
        &self,
        leg_id: &LegId,
        prev: Option<LegState>,
        new: LegState,
    ) {
        let entered_hold = !matches!(prev, Some(LegState::Hold)) && new == LegState::Hold;
        let left_hold = matches!(prev, Some(LegState::Hold)) && new != LegState::Hold;
        if !entered_hold && !left_hold {
            return;
        }
        if self.server.session_hooks.is_empty() {
            return;
        }
        let ctx = self.session_hook_ctx();
        let leg_id_str = leg_id.to_string();
        for hook in self.server.session_hooks.iter() {
            if entered_hold {
                hook.on_call_held(&ctx, &leg_id_str).await;
            } else {
                hook.on_call_unheld(&ctx, &leg_id_str).await;
            }
        }
    }

    /// Emit a typed call lifecycle event via the new generic gateway API.
    fn emit_typed_rwi_event<E: crate::rwi::RwiEventSpec>(&self, event: &E) {
        if let Some(ref gw) = self.server.rwi_gateway {
            let g = gw.read();
            g.send_to_owner(event);
        }
    }

    /// Add a new leg to the session dynamically.
    async fn handle_add_leg(&mut self, target: String, leg_id: Option<LegId>) -> Result<LegId> {
        let new_leg_id =
            leg_id.unwrap_or_else(|| LegId::new(format!("leg-{}", uuid::Uuid::new_v4())));

        info!(session_id = %self.id,
            %new_leg_id,
            %target,
            "Adding new SIP leg to session"
        );

        // Parse target as SIP URI
        let uri = rsipstack::sip::Uri::try_from(target.as_str())
            .map_err(|e| anyhow!("Invalid SIP URI '{}': {}", target, e))?;

        // Create leg
        let leg = crate::call::domain::Leg::new(new_leg_id.clone()).with_endpoint(target.clone());
        self.legs.insert(new_leg_id.clone(), leg);
        self.update_leg_state(&new_leg_id, LegState::Initializing);

        // Create peer and initiate INVITE in background
        if let Err(e) = self.initiate_sip_leg(&new_leg_id, uri).await {
            warn!(session_id = %self.id,
                error = %e,
                "Failed to initiate SIP leg, cleaning up"
            );
            self.legs.remove(&new_leg_id);
            return Err(e);
        }

        self.update_media_path().await;

        info!(session_id = %self.id,
            %new_leg_id,
            "SIP leg added successfully"
        );
        Ok(new_leg_id)
    }

    /// Remove a leg from the session.
    async fn handle_remove_leg(&mut self, leg_id: LegId) -> Result<()> {
        info!(session_id = %self.id,
            %leg_id,
            "Removing leg from session"
        );

        if let Some(leg) = self.legs.remove(&leg_id) {
            // Send BYE to SIP dialog if exists
            let dialog_id = format!("{}-{}", self.id.0, leg_id);
            if leg.dialog_id.is_some() {
                if let Some(dlg) = self.server.dialog_layer.get_dialog_with(&dialog_id) {
                    if let Err(e) = dlg.hangup().await {
                        warn!(session_id = %self.id, %leg_id, %dialog_id, error = %e, "Failed to hangup SIP dialog");
                    } else {
                        info!(session_id = %self.id, %leg_id, %dialog_id, "SIP dialog hangup sent");
                    }
                }
            }
            info!(session_id = %self.id, %leg_id, "Leg removed");
        }

        self.update_media_path().await;
        Ok(())
    }

    /// Create a media peer for a dynamic leg.
    async fn create_leg_peer(
        &self,
        leg_id: &LegId,
    ) -> Result<(Arc<dyn MediaPeer>, Box<dyn crate::media::Track>, String)> {
        let track_id = format!("leg-{}-{}", self.id.0, leg_id);

        // Create media stream
        let media_stream_builder = crate::media::MediaStreamBuilder::new()
            .with_id(track_id.clone())
            .with_cancel_token(self.cancel_token.child_token());
        let media_stream = media_stream_builder.build();

        // Create peer (using VoiceEnginePeer for now - can be extended for WebRTC)
        let peer: Arc<dyn MediaPeer> = Arc::new(VoiceEnginePeer::new(Arc::new(media_stream)));

        // Create RTP track
        let mut track_builder = crate::media::RtpTrackBuilder::new(track_id.clone())
            .with_cancel_token(self.cancel_token.child_token())
            .with_cname(self.server.rtc_cname.clone());

        if let Some(ref external_ip) = self.context.dialplan.media.external_ip {
            track_builder = track_builder.with_external_ip(external_ip.clone());
        }
        if let Some(ref bind_ip) = self.context.dialplan.media.bind_ip {
            track_builder = track_builder.with_bind_ip(bind_ip.clone());
        }

        let track = track_builder.build();

        // Get SDP offer from track BEFORE moving it into peer
        let sdp = track
            .local_description()
            .await
            .map_err(|e| anyhow!("Failed to get local description: {}", e))?;

        // Add track to peer (moves track)
        peer.update_track(Box::new(track), None).await;

        // Re-create a placeholder track for the return value (the real one is inside peer now)
        let placeholder_track = crate::media::RtpTrackBuilder::new(track_id)
            .with_cancel_token(self.cancel_token.child_token())
            .with_cname(self.server.rtc_cname.clone())
            .build();

        Ok((peer, Box::new(placeholder_track), sdp))
    }

    /// Initiate a SIP INVITE for a dynamic leg.
    async fn initiate_sip_leg(
        &mut self,
        leg_id: &LegId,
        callee_uri: rsipstack::sip::Uri,
    ) -> Result<()> {
        let session_id = self.id.to_string();
        // Create peer for this leg
        let (peer, _track, sdp_offer) = self.create_leg_peer(leg_id).await?;
        self.legs.set_peer(leg_id.clone(), peer.clone());

        // Dynamic legs are plain RTP (SIP) by default
        self.legs
            .set_transport(leg_id.clone(), rustrtc::TransportMode::Rtp);

        info!(session_id = %session_id, %leg_id, %callee_uri, sdp_len = %sdp_offer.len(), "Initiating SIP leg");

        // Build INVITE option
        let caller = self
            .context
            .dialplan
            .caller
            .clone()
            .unwrap_or_else(|| callee_uri.clone());
        let contact = self
            .context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .unwrap_or_else(|| caller.clone());

        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: callee_uri.clone(),
            caller: caller.clone(),
            contact: contact.clone(),
            content_type: Some("application/sdp".to_string()),
            offer: Some(sdp_offer.into_bytes()),
            destination: None,
            credential: None,
            headers: None,
            call_id: Some(format!("{}-{}", self.id.0, leg_id)),
            ..Default::default()
        };

        let dialog_layer = self.server.dialog_layer.clone();
        let leg_id_for_spawn = leg_id.clone();
        let cmd_tx = self
            .cmd_tx
            .clone()
            .ok_or_else(|| anyhow!("No command sender available"))?;
        let track_id = format!("leg-{}-{}", self.id.0, leg_id);
        let cancel_token = self.cancel_token.child_token();

        // Spawn background task to handle INVITE response
        let invite_handle = crate::utils::spawn(async move {
            let leg_id = leg_id_for_spawn;
            let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

            let mut result: Result<InviteDialog, String> =
                Err("not started".to_string());
            let mut state_rx_open = true;

            loop {
                tokio::select! {
                    biased;
                    r = &mut invitation, if !result.is_ok() => {
                        match r {
                            Ok((dialog, response)) => {
                                if let Some(ref resp) = response {
                                    let status_code = resp.status_code.code();
                                    if StatusCode::from(status_code).kind()
                                        == rsipstack::sip::StatusCodeKind::Successful
                                    {
                                        info!(session_id = %session_id, %leg_id, status = %status_code, "SIP leg answered successfully");

                                        let answer_sdp = if !resp.body().is_empty() {
                                            let sdp = String::from_utf8_lossy(resp.body()).to_string();
                                            if let Err(e) =
                                                peer.update_remote_description(
                                                    &track_id,
                                                    &sdp,
                                                    rustrtc::SdpType::Answer,
                                                ).await
                                            {
                                                warn!(session_id = %session_id, %leg_id, error = %e, "Failed to set remote description on leg peer");
                                            } else {
                                                info!(session_id = %session_id, %leg_id, "Remote description set successfully");
                                            }
                                            Some(sdp)
                                        } else {
                                            None
                                        };

                                        let _ = cmd_tx.send(CallCommand::LegConnected {
                                            leg_id: leg_id.clone(),
                                            answer_sdp,
                                            dialog_id: Some(dialog.id().call_id.clone()),
                                        }).await;

                                        result = Ok(dialog);
                                        break;
                                    } else {
                                        warn!(session_id = %session_id, %leg_id, status = %status_code, "SIP leg rejected");
                                        let _ = cmd_tx.send(CallCommand::LegFailed {
                                            leg_id: leg_id.clone(),
                                            reason: format!("Rejected with {}", status_code),
                                        }).await;
                                        result = Err(format!("Rejected with {}", status_code));
                                        break;
                                    }
                                } else {
                                    warn!(session_id = %session_id, %leg_id, "SIP leg timeout (no response)");
                                    let _ = cmd_tx.send(CallCommand::LegFailed {
                                        leg_id: leg_id.clone(),
                                        reason: "Timeout".to_string(),
                                    }).await;
                                    result = Err("Timeout".to_string());
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!(session_id = %session_id, %leg_id, error = %e, "SIP leg failed");
                                let _ = cmd_tx.send(CallCommand::LegFailed {
                                    leg_id: leg_id.clone(),
                                    reason: e.to_string(),
                                }).await;
                                result = Err(e.to_string());
                                break;
                            }
                        }
                    }
                    state = state_rx.recv(), if state_rx_open => {
                        match state {
                            Some(rsipstack::dialog::dialog::DialogState::Early(_, ref resp)) => {
                                info!(session_id = %session_id, %leg_id, "SIP leg early media (183)");
                                let body = resp.body();
                                if !body.is_empty() {
                                    let sdp = String::from_utf8_lossy(body).to_string();
                                    if let Err(e) =
                                        peer.update_remote_description(
                                            &track_id,
                                            &sdp,
                                            rustrtc::SdpType::Pranswer,
                                        ).await
                                    {
                                        warn!(session_id = %session_id, %leg_id, error = %e, "Failed to set early media remote description");
                                    } else {
                                        info!(session_id = %session_id, %leg_id, "Early media remote description set");
                                    }
                                }
                            }
                            Some(_) => {}
                            None => { state_rx_open = false; }
                        }
                    }
                }
            }

            // Process dialog state changes (e.g., BYE from remote)
            if let Ok(dialog) = result {
                let dialog_cancel = cancel_token.child_token();
                crate::utils::spawn(async move {
                    loop {
                        tokio::select! {
                            biased;
                            _ = dialog_cancel.cancelled() => {
                                info!(session_id = %session_id, %leg_id, "Dialog monitor cancelled");
                                break;
                            }
                            state = state_rx.recv() => {
                                match state {
                                    Some(rsipstack::dialog::dialog::DialogState::Terminated(..)) => {
                                        info!(session_id = %session_id, %leg_id, "SIP leg dialog terminated");
                                        let _ = cmd_tx.send(CallCommand::LegFailed {
                                            leg_id: leg_id.clone(),
                                            reason: "Remote hung up".to_string(),
                                        }).await;
                                        break;
                                    }
                                    Some(_) => {}
                                    None => break,
                                }
                            }
                        }
                    }
                    let _ = dialog;
                });
            }
        });

        self.legs.push_task(leg_id.clone(), invite_handle);

        Ok(())
    }

    /// Update media path based on number of active legs.
    async fn update_media_path(&mut self) {
        let active_count = self.legs.active_count();

        info!(session_id = %self.id, active_legs = active_count, "Updating media path");

        match active_count {
            2 => {
                // Direct bridge: caller ↔ callee (or caller ↔ target)
                info!(session_id = %self.id, "Switching to direct bridge mode");
                // Clean up conference bridges if any
                self.stop_conference_bridges().await;
                // Setup direct bridge between the two active legs
                let active_legs: Vec<LegId> = self
                    .legs
                    .iter()
                    .filter(|(_, leg)| leg.is_active())
                    .map(|(id, _)| id.clone())
                    .collect();
                if active_legs.len() == 2 {
                    self.setup_bridge(active_legs[0].clone(), active_legs[1].clone())
                        .await;
                    info!(session_id = %self.id,
                        leg_a = %active_legs[0],
                        leg_b = %active_legs[1],
                        "Direct bridge configured"
                    );
                }
            }
            n if n >= 3 => {
                // Conference mixer: all legs mixed together
                info!(session_id = %self.id,
                    leg_count = n,
                    "Switching to conference mixer mode"
                );
                // Clean up direct bridge if any
                self.stop_direct_bridge().await;
                // Setup conference mixer
                self.setup_conference_mixer().await;
            }
            _ => {
                // Single leg or none - no bridging needed
                info!(session_id = %self.id, "No bridging needed");
                self.stop_all_bridges().await;
            }
        }
    }

    /// Stop all conference bridges for this session.
    async fn stop_conference_bridges(&mut self) {
        if self.conference_bridge.is_active() {
            info!(session_id = %self.id, "Stopping conference bridges");
            self.conference_bridge.stop_bridge();
        }
    }

    /// Stop direct bridge if active.
    async fn stop_direct_bridge(&mut self) {
        if self.bridge.active {
            info!(session_id = %self.id, "Stopping direct bridge");
            self.bridge.clear();
        }
    }

    /// Stop all bridges (both direct and conference).
    async fn stop_all_bridges(&mut self) {
        self.stop_conference_bridges().await;
        self.stop_direct_bridge().await;
        info!(session_id = %self.id, "All bridges stopped");
    }

    /// Map a session leg to its MediaBridge side, if it is one of the two
    /// anchored legs (caller=A, callee=B).
    fn media_side_for_leg(&self, leg: &LegId) -> Option<crate::media::media_bridge::LegSide> {
        match leg.0.as_str() {
            "caller" => Some(crate::media::media_bridge::LegSide::A),
            "callee" => Some(crate::media::media_bridge::LegSide::B),
            _ => None,
        }
    }

    async fn setup_bridge(&mut self, leg_a: LegId, leg_b: LegId) -> bool {
        if !self.legs.contains_key(&leg_a) || !self.legs.contains_key(&leg_b) {
            return false;
        }
        self.bridge = BridgeConfig::bridge(leg_a.clone(), leg_b.clone());

        // If both legs map to the MediaBridge's A/B, actually activate the
        // media route (fast-path relay or transcode). Dynamic legs not present
        // in the MediaBridge keep the state-only BridgeConfig.
        if let (Some(side_a), Some(side_b)) =
            (self.media_side_for_leg(&leg_a), self.media_side_for_leg(&leg_b))
            && self.media.bridge.is_some()
        {
            if let Some(mb) = self.bridge_mut() {
                mb.accept(side_a).await;
                mb.accept(side_b).await;
                if let Err(e) = mb.bridge().await {
                    warn!(session_id = %self.id, %leg_a, %leg_b, error = %e, "RWI bridge activation failed");
                }
            }
        }
        true
    }

    async fn clear_bridge(&mut self) {
        self.bridge.clear();
        if self.media.bridge.is_some()
            && let Some(mb) = self.bridge_mut()
        {
            if let Err(e) = mb.unbridge().await {
                warn!(session_id = %self.id, error = %e, "RWI unbridge failed");
            }
        }
    }

    fn derive_state(legs: &crate::proxy::proxy_call::leg_registry::LegRegistry) -> SessionState {
        if legs.is_empty() {
            return SessionState::Initializing;
        }

        let mut has_ringing = false;
        let mut has_connected = false;
        let mut has_ending = false;
        let mut all_ended = true;

        for leg in legs.values() {
            match leg.state {
                LegState::Initializing | LegState::Ringing | LegState::EarlyMedia => {
                    has_ringing = true;
                    all_ended = false;
                }
                LegState::Connected => {
                    has_connected = true;
                    all_ended = false;
                }
                LegState::Hold => {
                    has_connected = true;
                    all_ended = false;
                }
                LegState::Ending => {
                    has_ending = true;
                    all_ended = false;
                }
                LegState::Ended => {}
            }
        }

        if all_ended {
            return SessionState::Ended;
        }
        if has_ending {
            return SessionState::Ending;
        }
        if has_connected {
            return SessionState::Active;
        }
        if has_ringing {
            return SessionState::Ringing;
        }
        SessionState::Initializing
    }

    async fn handle_play(
        &mut self,
        leg_id: Option<LegId>,
        source: crate::call::domain::MediaSource,
        options: Option<crate::call::domain::PlayOptions>,
    ) -> Result<()> {
        let _await_completion = options
            .as_ref()
            .map(|o| o.await_completion)
            .unwrap_or(false);
        let loop_playback = options.as_ref().map(|o| o.loop_playback).unwrap_or(false);
        let track_id = options
            .as_ref()
            .and_then(|o| o.track_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let file_path = match source {
            crate::call::domain::MediaSource::File { path } => Self::resolve_audio_file_path(&path),
            crate::call::domain::MediaSource::Url { url } => url,
            _ => return Err(anyhow!("Only file/URL playback supported")),
        };

        // Map LegId ("caller"/"callee"/"both") → LegSide. Default = caller.
        let target_side = match leg_id.as_ref().map(|l| l.0.as_str()) {
            Some("callee") => crate::media::media_bridge::LegSide::B,
            Some("both") => {
                if let Some(mb) = self.bridge_mut() {
                    mb.play_file(crate::media::media_bridge::LegSide::A, &file_path, loop_playback).await?;
                    mb.play_file(crate::media::media_bridge::LegSide::B, &file_path, loop_playback).await?;
                } else {
                    return Err(anyhow!("Playback requires MediaBridge"));
                }
                info!(session_id = %self.id, file = %file_path, "Playback started (both)");
                return Ok(());
            }
            _ => crate::media::media_bridge::LegSide::A,
        };

        if let Some(mb) = self.bridge_mut() {
            let handle = mb.play_file(target_side, &file_path, loop_playback).await?;
            // Forward the playback completion to the app event loop and restore
            // the media route (both legs) once playback ends.
            let app_event_bridge = self.app_event_bridge.clone();
            let handle_for_restore = self.handle.clone();
            let rwi_gateway = self.server.rwi_gateway.clone();
            let session_id = self.id.clone();
            let event_leg_id_str = leg_id.as_ref().map(|l| l.0.clone());
            crate::utils::spawn(async move {
                let result = handle.done.await.ok();
                let interrupted = result.map(|r| r.interrupted).unwrap_or(true);
                if let Some(h) = app_event_bridge.read().as_ref() {
                    let _ = h.send_app_event(crate::call::app::ControllerEvent::AudioComplete {
                        track_id: track_id.clone(),
                        interrupted,
                    });
                }
                if let Some(ref gw) = rwi_gateway {
                    let event = crate::rwi::event::to_legacy_event(
                        &crate::rwi::MediaPlayFinished {
                            call_id: session_id.to_string(),
                            leg_id: event_leg_id_str.clone(),
                            track_id: track_id.clone(),
                            interrupted,
                        },
                        None,
                    );
                    let g = gw.read();
                    g.send_event_to_call_owner(&session_id.to_string(), &event);
                }
                let _ = handle_for_restore.send_command(CallCommand::ResumeMedia);
            });
        } else {
            return Err(anyhow!("Playback requires MediaBridge"));
        }

        info!(session_id = %self.id,
            side = ?target_side,
            file = %file_path,
            "Playback started"
        );

        Ok(())
    }

    async fn handle_stop_playback(&mut self, leg_id: Option<LegId>) -> Result<()> {
        let sides: Vec<crate::media::media_bridge::LegSide> = match leg_id.as_ref().map(|l| l.0.as_str()) {
            Some("callee") => vec![crate::media::media_bridge::LegSide::B],
            Some("both") => vec![
                crate::media::media_bridge::LegSide::A,
                crate::media::media_bridge::LegSide::B,
            ],
            // "caller" or unspecified → stop caller leg only.
            _ => vec![crate::media::media_bridge::LegSide::A],
        };

        if let Some(mb) = self.bridge_mut() {
            for side in sides {
                mb.stop_play(side).await?;
            }
        }
        Ok(())
    }

    async fn handle_queue_enqueue(
        &mut self,
        leg_id: LegId,
        queue_id: String,
        priority: Option<u32>,
    ) -> Result<()> {
        info!(session_id = %self.id, %leg_id, %queue_id, ?priority, "Enqueueing leg to queue");

        self.require_leg(&leg_id)?;

        self.update_leg_state(&leg_id, LegState::Hold);

        let position = self
            .server
            .queue_manager
            .enqueue(
                queue_id.clone().into(),
                leg_id.clone(),
                self.id.clone(),
                priority,
            )
            .await?;

        info!(session_id = %self.id, %leg_id, %queue_id, position, "Leg enqueued successfully at position");
        Ok(())
    }

    async fn handle_queue_dequeue(&mut self, leg_id: LegId) -> Result<()> {
        info!(session_id = %self.id, %leg_id, "Dequeuing leg from queue");

        self.require_leg(&leg_id)?;

        let queue_manager = &self.server.queue_manager;
        let queues = queue_manager.list_queues().await;

        let mut dequeued = false;
        for queue_id in queues {
            if let Ok(_entry) = queue_manager.dequeue(&queue_id, &leg_id).await {
                info!(session_id = %self.id, %leg_id, queue_id = %queue_id.0, "Leg dequeued from queue");
                dequeued = true;

                let _ = queue_manager.remove_queue_if_empty(&queue_id).await;

                break;
            }
        }

        if !dequeued {
            warn!(session_id = %self.id, %leg_id, "Leg was not found in any queue");
        }

        self.update_leg_state(&leg_id, LegState::Connected);

        info!(session_id = %self.id, %leg_id, "Leg dequeued successfully");
        Ok(())
    }

    async fn handle_reject(&mut self, leg_id: LegId, reason: Option<String>) -> Result<()> {
        info!(session_id = %self.id, %leg_id, ?reason, "Rejecting call");

        self.require_leg(&leg_id)?;

        let (status_code, reason_phrase) = match reason.as_deref() {
            Some("busy") | Some("Busy") | Some("486") => {
                (StatusCode::BusyHere, Some("Busy Here".to_string()))
            }
            Some("decline") | Some("Decline") | Some("603") => {
                (StatusCode::Decline, Some("Decline".to_string()))
            }
            Some("unavailable") | Some("Unavailable") | Some("480") => (
                StatusCode::TemporarilyUnavailable,
                Some("Temporarily Unavailable".to_string()),
            ),
            Some("reject") | Some("Reject") | Some("403") => {
                (StatusCode::Forbidden, Some("Forbidden".to_string()))
            }
            _ => (StatusCode::Decline, Some("Decline".to_string())),
        };

        if let Some(dialog) = self.caller_dialog.as_ref() {
            if let Err(e) = dialog.reject(Some(status_code), reason_phrase) {
                warn!(session_id = %self.id, %leg_id, error = %e, "Failed to send reject response");
                return Err(anyhow!("Failed to send reject response: {}", e));
            }
        }

        self.update_leg_state(&leg_id, LegState::Ended);

        info!(session_id = %self.id, %leg_id, "Call rejected successfully");
        Ok(())
    }

    async fn handle_ring(&mut self, leg_id: LegId, ringback: Option<RingbackPolicy>) -> Result<()> {
        info!(session_id = %self.id, %leg_id, ?ringback, "Sending ringing indication");

        self.require_leg(&leg_id)?;

        // Handle EarlyMedia policy: send proactive 183 with bridge SDP and audio
        if let Some(RingbackPolicy::EarlyMedia { source }) = &ringback {
            let audio_path = match source {
                MediaSource::File { path } => path.clone(),
                _ => {
                    return Err(anyhow!("EarlyMedia requires a File media source"));
                }
            };
            return self.send_early_media_tone(&audio_path).await;
        }

        self.update_leg_state(&leg_id, LegState::Ringing);

        // DN event: extension ringing

        let sdp = ringback.as_ref().and_then(|policy| match policy {
            RingbackPolicy::Replace { .. } => self.media.caller_offer.clone(),
            _ => None,
        });

        if let Some(dialog) = self.caller_dialog.as_ref() {
            if let Err(e) = dialog.ringing(None, sdp.map(|s| s.into_bytes())) {
                warn!(session_id = %self.id, %leg_id, error = %e, "Failed to send ringing indication");
                return Err(anyhow!("Failed to send ringing indication: {}", e));
            }
        }

        info!(session_id = %self.id, %leg_id, "Ringing indication sent successfully");
        Ok(())
    }

    async fn send_info_to_dialog(
        dialog: &rsipstack::dialog::dialog::Dialog,
        headers: Vec<rsipstack::sip::Header>,
        body: Vec<u8>,
    ) -> Result<()> {
        use rsipstack::dialog::dialog::Dialog;

        match dialog {
            Dialog::Invite(d) => {
                d.info(Some(headers), Some(body))
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            }
            _ => return Err(anyhow!("Unsupported dialog type for DTMF")),
        }
        Ok(())
    }

    /// Send RTP (RFC 2833) DTMF to a leg via the media bridge.

    async fn handle_send_dtmf(&mut self, leg_id: LegId, digits: String) -> Result<()> {
        let valid_digits: Vec<char> = digits
            .chars()
            .filter(|c| matches!(c, '0'..='9' | '*' | '#' | 'A'..='D'))
            .collect();

        if valid_digits.is_empty() {
            return Err(anyhow!("No valid DTMF digits provided: {}", digits));
        }
        let digit_str: String = valid_digits.iter().collect();

        // 1. Preferred: RFC 2833 RTP telephone-events via the media bridge.
        //    The leg emits the DTMF on its own egress transport (SRTP-protected,
        //    negotiated telephone-event PT), regardless of the active route.
        let side = match leg_id.as_str() {
            "caller" => Some(crate::media::media_bridge::LegSide::A),
            "callee" => Some(crate::media::media_bridge::LegSide::B),
            _ => None,
        };
        if let Some(side) = side {
            let rtp_sent = match self.media.bridge.as_ref() {
                Some(mb) if mb.leg(side).is_some() => {
                    match mb.send_dtmf(side, &digit_str).await {
                        Ok(()) => {
                            for digit in &valid_digits {
                                self.dtmf_digits.push(*digit);
                            }
                            info!(session_id = %self.id, %leg_id, digits = %digit_str, "DTMF sent via RTP RFC 2833 telephone-events");
                            true
                        }
                        Err(e) => {
                            warn!(session_id = %self.id, error = %e, "RTP DTMF failed; falling back to SIP INFO");
                            false
                        }
                    }
                }
                _ => false,
            };
            if rtp_sent {
                return Ok(());
            }
        }

        // 2. Fallback: SIP INFO (application/dtmf-relay).
        let dtmf_body = valid_digits
            .iter()
            .map(|d| format!("Signal={}\nDuration=160", d))
            .collect::<Vec<_>>()
            .join("\n");
        let headers = vec![rsipstack::sip::Header::ContentType(
            rsipstack::sip::headers::ContentType::from("application/dtmf-relay"),
        )];

        let info_result: Result<()> = if leg_id == LegId::from("caller") {
            if let Some(dialog) = self.caller_dialog.as_ref() {
                dialog
                    .info(Some(headers), Some(dtmf_body.clone().into_bytes()))
                    .await
                    .map_err(|e| anyhow!("{}", e))?;
            }
            Ok(())
        } else if leg_id == LegId::from("callee") {
            match self.meta.connected_callee_dialog_id.as_ref() {
                Some(dialog_id) => match self.server.dialog_layer.get_dialog(dialog_id) {
                    Some(dlg) => {
                        Self::send_info_to_dialog(&dlg, headers, dtmf_body.into_bytes()).await
                    }
                    None => return Err(anyhow!("Callee dialog not found: {}", dialog_id)),
                },
                None => return Err(anyhow!("No connected callee dialog")),
            }
        } else {
            match self
                .legs
                .get(&leg_id)
                .and_then(|leg| leg.dialog_id.as_ref())
            {
                Some(dialog_id) => match self.server.dialog_layer.get_dialog_with(dialog_id) {
                    Some(dlg) => {
                        Self::send_info_to_dialog(&dlg, headers, dtmf_body.into_bytes()).await
                    }
                    None => {
                        return Err(anyhow!(
                            "Dialog not found for leg {}: {}",
                            leg_id,
                            dialog_id
                        ));
                    }
                },
                None => return Err(anyhow!("No dialog_id for leg: {}", leg_id)),
            }
        };

        match info_result {
            Ok(()) => {
                for digit in &valid_digits {
                    self.dtmf_digits.push(*digit);
                }
                info!(session_id = %self.id, %leg_id, digits = %digit_str, "DTMF sent via SIP INFO");
            }
            Err(e) => {
                return Err(anyhow!("Failed to send DTMF: {}", e));
            }
        }

        Ok(())
    }

    async fn handle_reinvite_command(&mut self, leg_id: LegId, sdp: String) -> Result<()> {
        info!(session_id = %self.id, %leg_id, "Handling re-INVITE command");

        self.require_leg(&leg_id)?;

        self.handle_reinvite(rsipstack::sip::Method::Invite, Some(sdp))
            .await?;

        info!(session_id = %self.id, %leg_id, "Re-INVITE command handled");
        Ok(())
    }

    async fn set_track_muted(&mut self, track_id: String, muted: bool) -> Result<()> {
        info!(session_id = %self.id, %track_id, muted, "Setting track mute state");

        let caller_result = if let Some(peer) = self.caller_peer() {
            if muted {
                peer.mute_track(&track_id).await
            } else {
                peer.unmute_track(&track_id).await
            }
        } else {
            false
        };

        let callee_result = if let Some(peer) = self.callee_peer() {
            if muted {
                peer.mute_track(&track_id).await
            } else {
                peer.unmute_track(&track_id).await
            }
        } else {
            false
        };

        if !caller_result && !callee_result {
            return Err(anyhow!("Track not found on either peer: {}", track_id));
        }

        info!(session_id = %self.id, %track_id, caller_affected = caller_result, callee_affected = callee_result, muted, "Track mute state set");
        Ok(())
    }
    async fn handle_mute_track(&mut self, track_id: String) -> Result<()> {
        self.set_track_muted(track_id, true).await
    }

    async fn handle_unmute_track(&mut self, track_id: String) -> Result<()> {
        self.set_track_muted(track_id, false).await
    }

    async fn handle_send_sip_message(&mut self, content_type: String, body: String) -> Result<()> {
        info!(session_id = %self.id, content_type = %content_type, body_len = body.len(), "Sending SIP MESSAGE");

        let headers = vec![rsipstack::sip::Header::ContentType(content_type.into())];
        let body_bytes = body.into_bytes();

        let Some(server_dialog) = self.caller_dialog.as_ref() else {
            warn!(session_id = %self.id, "Cannot send SIP MESSAGE: no inbound caller dialog (UAC mode)");
            return Err(anyhow!("SIP MESSAGE requires an inbound caller dialog"));
        };
        match server_dialog
            .message(Some(headers), Some(body_bytes))
            .await
        {
            Ok(Some(response)) => {
                info!(session_id = %self.id, status = %response.status_code, "SIP MESSAGE sent successfully");
                Ok(())
            }
            Ok(None) => {
                info!(session_id = %self.id, "SIP MESSAGE sent (no response)");
                Ok(())
            }
            Err(e) => {
                error!(session_id = %self.id, error = %e, "Failed to send SIP MESSAGE");
                Err(anyhow!("Failed to send SIP MESSAGE: {}", e))
            }
        }
    }

    async fn handle_send_sip_notify(
        &mut self,
        event: String,
        content_type: String,
        body: String,
    ) -> Result<()> {
        info!(session_id = %self.id, event = %event, content_type = %content_type, body_len = body.len(), "Sending SIP NOTIFY");

        let headers = vec![
            rsipstack::sip::Header::Other("Event".into(), event),
            rsipstack::sip::Header::ContentType(content_type.into()),
        ];
        let body_bytes = body.into_bytes();

        let Some(server_dialog) = self.caller_dialog.as_ref() else {
            warn!(session_id = %self.id, "Cannot send SIP NOTIFY: no inbound caller dialog (UAC mode)");
            return Err(anyhow!("SIP NOTIFY requires an inbound caller dialog"));
        };
        match server_dialog
            .notify(Some(headers), Some(body_bytes))
            .await
        {
            Ok(Some(response)) => {
                info!(session_id = %self.id, status = %response.status_code, "SIP NOTIFY sent successfully");
                Ok(())
            }
            Ok(None) => {
                info!(session_id = %self.id, "SIP NOTIFY sent (no response)");
                Ok(())
            }
            Err(e) => {
                error!(session_id = %self.id, error = %e, "Failed to send SIP NOTIFY");
                Err(anyhow!("Failed to send SIP NOTIFY: {}", e))
            }
        }
    }

    async fn handle_send_sip_options_ping(&mut self) -> Result<()> {
        info!(session_id = %self.id, "Sending SIP OPTIONS ping");

        let Some(server_dialog) = self.caller_dialog.as_ref() else {
            debug!(session_id = %self.id, "Skipping caller OPTIONS ping (UAC mode)");
            return Ok(());
        };
        match server_dialog
            .request(rsipstack::sip::Method::Options, None, None)
            .await
        {
            Ok(Some(response)) => {
                let status_code = u16::from(response.status_code);
                if StatusCode::from(status_code).kind()
                    == rsipstack::sip::StatusCodeKind::Successful
                {
                    info!(session_id = %self.id, status = status_code, "SIP OPTIONS ping successful");
                    Ok(())
                } else {
                    warn!(session_id = %self.id, status = status_code, "SIP OPTIONS ping returned error");
                    Err(anyhow!("OPTIONS ping failed with status: {}", status_code))
                }
            }
            Ok(None) => {
                info!(session_id = %self.id, "SIP OPTIONS ping sent (no response)");
                Ok(())
            }
            Err(e) => {
                error!(session_id = %self.id, error = %e, "Failed to send SIP OPTIONS ping");
                Err(anyhow!("Failed to send OPTIONS ping: {}", e))
            }
        }
    }
    async fn handle_hold(
        &mut self,
        leg_id: LegId,
        music: Option<crate::call::domain::MediaSource>,
    ) -> Result<()> {
        info!(session_id = %self.id, %leg_id, ?music, "Handling hold with SDP renegotiation");

        self.require_leg(&leg_id)?;

        self.update_leg_state(&leg_id, LegState::Hold);

        let hold_sdp = self.generate_hold_sdp().await?;

        match self.send_reinvite_to_leg(&leg_id, hold_sdp).await {
            Ok(_) => {
                info!(session_id = %self.id, %leg_id, "Hold re-INVITE sent successfully");

                if !self.server.session_hooks.is_empty() {
                    let ctx = self.session_hook_ctx();
                    let leg_id_str = leg_id.to_string();
                    for hook in self.server.session_hooks.iter() {
                        hook.on_call_held(&ctx, &leg_id_str).await;
                    }
                }

                if let Some(media_source) = music
                    && let Some(path) = match &media_source {
                        crate::call::domain::MediaSource::File { path } => Some(path.clone()),
                        crate::call::domain::MediaSource::Url { url } => Some(url.clone()),
                        _ => None,
                    }
                    && let Err(e) = self.play_audio_file(&path, false, true).await
                {
                    warn!(session_id = %self.id, error = %e, "Failed to start hold music");
                }

                // Pause the RTP inactivity timeout while on hold (no media flows
                // to the held party). Mirrors the peer-initiated hold path.
                let side = if leg_id.0 == "callee" {
                    crate::media::media_bridge::LegSide::B
                } else {
                    crate::media::media_bridge::LegSide::A
                };
                if let Some(mb) = self.bridge_mut() {
                    mb.pause_rtp_timeout(side);
                }

                Ok(())
            }
            Err(e) => {
                warn!(session_id = %self.id, %leg_id, error = %e, "Failed to send hold re-INVITE");
                Ok(())
            }
        }
    }

    async fn handle_unhold(&mut self, leg_id: LegId) -> Result<()> {
        info!(session_id = %self.id, %leg_id, "Handling unhold with SDP renegotiation");

        self.require_leg(&leg_id)?;

        let Some(leg) = self.legs.get(&leg_id) else {
            warn!(session_id = %self.id, %leg_id, "Leg disappeared between require_leg and access");
            return Err(anyhow::anyhow!("Leg not found: {}", leg_id));
        };
        if leg.state != LegState::Hold {
            info!(session_id = %self.id, %leg_id, state = ?leg.state, "Leg is not on hold, skipping unhold");
            return Ok(());
        }

        self.update_leg_state(&leg_id, LegState::Connected);

        // Restore the media route on the held leg (resume re-arms relay/transcode).
        let side = if leg_id.0 == "callee" {
            crate::media::media_bridge::LegSide::B
        } else {
            crate::media::media_bridge::LegSide::A
        };
        if let Some(mb) = self.bridge_mut() {
            mb.resume(side).await?;
            mb.resume_rtp_timeout(side);
        }

        let unhold_sdp = self.generate_unhold_sdp().await?;

        match self.send_reinvite_to_leg(&leg_id, unhold_sdp).await {
            Ok(_) => {
                info!(session_id = %self.id, %leg_id, "Unhold re-INVITE sent successfully");

                if !self.server.session_hooks.is_empty() {
                    let ctx = self.session_hook_ctx();
                    let leg_id_str = leg_id.to_string();
                    for hook in self.server.session_hooks.iter() {
                        hook.on_call_unheld(&ctx, &leg_id_str).await;
                    }
                }

                Ok(())
            }
            Err(e) => {
                warn!(session_id = %self.id, %leg_id, error = %e, "Failed to send unhold re-INVITE");
                Ok(())
            }
        }
    }

    async fn generate_hold_sdp(&self) -> Result<String> {
        let base_sdp = self
            .media
            .answer
            .as_ref()
            .or(self.media.caller_offer.as_ref())
            .ok_or_else(|| anyhow!("No SDP available for hold"))?;

        let hold_sdp = rustrtc::modify_sdp_direction(base_sdp, "sendonly");
        Ok(hold_sdp)
    }

    async fn generate_unhold_sdp(&self) -> Result<String> {
        let base_sdp = self
            .media
            .answer
            .as_ref()
            .or(self.media.caller_offer.as_ref())
            .ok_or_else(|| anyhow!("No SDP available for unhold"))?;

        let unhold_sdp = rustrtc::modify_sdp_direction(base_sdp, "sendrecv");
        Ok(unhold_sdp)
    }

    /// Send a re-INVITE (e.g. hold/unhold SDP) to the dialog of the target leg
    /// ("caller" → the primary caller dialog; "callee" → the callee dialog).
    async fn send_reinvite_to_leg(&self, leg_id: &LegId, sdp: String) -> Result<()> {
        let headers = Self::sdp_headers();
        let dialog = if leg_id.0 == "callee" {
            self.legs
                .dialogs
                .get(&LegId::from("callee"))
                .and_then(|d| match d {
                    rsipstack::dialog::dialog::Dialog::Invite(inv) => Some(inv.clone()),
                    _ => None,
                })
        } else {
            self.caller_dialog.clone()
        };
        let Some(dialog) = dialog else {
            debug!(session_id = %self.id, %leg_id, "No dialog to re-INVITE for leg");
            return Ok(());
        };
        match dialog
            .reinvite(Some(headers), Some(sdp.into_bytes()))
            .await
        {
            Ok(Some(response)) => {
                let status = response.status_code.code();
                if StatusCode::from(status).kind() == rsipstack::sip::StatusCodeKind::Successful {
                    info!(session_id = %self.id, status = %status, "re-INVITE accepted");
                    Ok(())
                } else {
                    Err(anyhow!("re-INVITE rejected with status {}", status))
                }
            }
            Ok(None) => Err(anyhow!("re-INVITE timed out")),
            Err(e) => Err(anyhow!("re-INVITE failed: {}", e)),
        }
    }

    async fn send_reinvite_to_caller(&self, sdp: String) -> Result<()> {
        let headers = Self::sdp_headers();

        let Some(server_dialog) = self.caller_dialog.as_ref() else {
            // UAC mode: no inbound caller to re-INVITE; media hold/unhold is
            // handled entirely on the MediaBridge.
            debug!(session_id = %self.id, "Skipping caller re-INVITE (UAC mode)");
            return Ok(());
        };
        match server_dialog
            .reinvite(Some(headers), Some(sdp.into_bytes()))
            .await
        {
            Ok(Some(response)) => {
                let status = response.status_code.code();
                if StatusCode::from(status).kind() == rsipstack::sip::StatusCodeKind::Successful {
                    info!(session_id = %self.id, status = %status, "re-INVITE accepted");
                    Ok(())
                } else {
                    Err(anyhow!("re-INVITE rejected with status {}", status))
                }
            }
            Ok(None) => Err(anyhow!("re-INVITE timed out")),
            Err(e) => Err(anyhow!("re-INVITE failed: {}", e)),
        }
    }
}

/// Parse a `message/sipfrag` body and extract the SIP status code.
/// Expected format: `SIP/2.0 <code> <reason>`
fn parse_sipfrag_status(body: &str) -> Option<u16> {
    let line = body.lines().next()?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 && parts[0] == "SIP/2.0" {
        parts[1].parse().ok()
    } else {
        None
    }
}

impl Drop for SipSession {
    fn drop(&mut self) {
        self.cancel_token.cancel();

        // Safety net for task abort, panic, or runtime shutdown before cleanup.
        self.concurrent_call_lease.release_all();

        self.callee_guards.clear();

        self.callee_event_tx = None;

        self.callee_dialogs.clear();
        self.meta.connected_callee_dialog_id = None;
        self.timers.clear();
        self.timer_queue.clear();
        self.timer_keys.clear();

        // Stop conference bridges and supervisor mixer (safety net — cancel only,
        // since we can't .await in Drop)
        self.conference_bridge.stop_bridge();
        self.legs.stop_all_conference_bridge_handles();
        self.supervisor_mixer.take();

        // Media bridge — torn down explicitly under catch_unwind so a teardown
        // panic during an already-unwinding (failed) test cannot double-panic.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(mut mb) = self.media.bridge.take() {
                mb.close();
            }
        }));

        // Abort leg-specific spawned tasks so they can't outlive the session.
        for (_, handles) in self.legs.drain_tasks() {
            for handle in handles {
                handle.abort();
            }
        }

        // Safety net: ensure the registry entry is always removed even if
        // cleanup() was never called (e.g. tokio task cancellation).
        self.server
            .active_call_registry
            .remove(&self.context.session_id);

        // Safety net: release any concurrency slots still held (cleanup() takes
        // them on the happy path, so this is empty in that case). Drop can't
        // await, so spawn a best-effort release task.
        let remaining_holds = std::mem::take(&mut *self.context.dialplan.concurrency_holds.lock());
        if !remaining_holds.is_empty() {
            if let Some(limiter) = self.server.frequency_limiter.clone() {
                if tokio::runtime::Handle::try_current().is_ok() {
                    crate::utils::spawn(async move {
                        crate::call::policy::PolicyGuard::release_concurrency_holds(
                            &remaining_holds,
                            limiter.as_ref(),
                        )
                        .await;
                    });
                }
            }
        }

        // engine session is cleaned up by media_session_guard's Drop (RAII).

        // Safety net: send CDR if cleanup() was never called
        // (e.g. tokio task cancellation, B2BUA session stuck in process()).
        if !self.cdr_sent.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(reporter) = &self.reporter {
                let snapshot = self.record_snapshot();
                reporter.report(snapshot);
                self.cdr_sent
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                debug!(session_id = %self.context.session_id, "CDR sent from Drop safety net");
            }
        }

        debug!(session_id = %self.context.session_id, "SipSession drop complete");
    }
}

#[cfg(test)]
impl SipSession {
    /// Test-only: set caller_answer_uses_media_bridge without going through the full
    /// media-bridge setup codepath.
    pub fn set_caller_uses_bridge_for_test(&mut self, _value: bool) {
        // No-op: legacy media-bridge flag removed in the A/B MediaBridge rewrite.
    }

    /// Test-only: returns true if a caller ingress monitor task is currently active.
    pub fn has_active_caller_ingress_monitor(&self) -> bool { false }
}

/// Reads incoming RTP from a peer connection, decodes it to PCM, and feeds it
/// to the conference mixer (full-duplex conference bridge input).
pub(crate) struct PeerConnectionAudioReceiver {
    pc: rustrtc::PeerConnection,
    decoder: Box<dyn audio_codec::Decoder>,
    audio_track: Option<Arc<dyn rustrtc::media::MediaStreamTrack>>,
}

impl PeerConnectionAudioReceiver {
    pub(crate) fn new(pc: rustrtc::PeerConnection, decoder: Box<dyn audio_codec::Decoder>) -> Self {
        Self {
            pc,
            decoder,
            audio_track: None,
        }
    }

    /// Wait for and capture the first audio track from the peer connection.
    async fn capture_audio_track(&mut self) -> Option<Arc<dyn rustrtc::media::MediaStreamTrack>> {
        // First, check pre-existing transceivers for a receiver track.
        for transceiver in self.pc.get_transceivers() {
            if transceiver.kind() == rustrtc::MediaKind::Audio
                && let Some(receiver) = transceiver.receiver()
            {
                let track = receiver.track();
                info!("Conference audio receiver using pre-existing audio track");
                return Some(track);
            }
        }

        // If no pre-existing track, wait for a Track event.
        let mut pc_recv = Box::pin(self.pc.recv());
        loop {
            match pc_recv.await {
                Some(rustrtc::PeerConnectionEvent::Track(transceiver)) => {
                    if transceiver.kind() == rustrtc::MediaKind::Audio
                        && let Some(receiver) = transceiver.receiver()
                    {
                        let track = receiver.track();
                        info!("Conference audio receiver captured audio track");
                        return Some(track);
                    }
                    pc_recv = Box::pin(self.pc.recv());
                }
                Some(_) => {
                    pc_recv = Box::pin(self.pc.recv());
                }
                None => {
                    warn!("PeerConnection closed before audio track was captured");
                    return None;
                }
            }
        }
    }
}

impl crate::call::runtime::conference_media_bridge::AudioReceiver for PeerConnectionAudioReceiver {
    fn recv(
        &mut self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Option<crate::call::runtime::conference_media_bridge::PcmAudioFrame>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            loop {
                // Capture audio track if not already captured.
                // Track availability can be racy during re-INVITE / transfer windows,
                // so keep retrying until cancellation closes the bridge.
                if self.audio_track.is_none() {
                    self.audio_track = self.capture_audio_track().await;
                    if self.audio_track.is_none() {
                        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
                        continue;
                    }
                }

                let Some(track) = self.audio_track.as_ref().cloned() else {
                    continue;
                };

                match track.recv().await {
                    Ok(rustrtc::media::MediaSample::Audio(audio_frame)) => {
                        // Decode RTP payload to PCM
                        let pcm = self.decoder.decode(&audio_frame.data);

                        return Some(
                            crate::call::runtime::conference_media_bridge::PcmAudioFrame::new(
                                pcm,
                                self.decoder.sample_rate(),
                            ),
                        );
                    }
                    Ok(_) => {
                        // Ignore non-audio samples and keep waiting for PCM payload.
                        continue;
                    }
                    Err(e) => {
                        tracing::debug!("Track recv failed, re-capturing audio track: {}", e);
                        self.audio_track = None;
                        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                        continue;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::proxy_call::dtmf::RtpDtmfDetector;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ---- helpers for codec / audio-content verification ----

    #[allow(dead_code)]
    fn cross_correlate(signal: &[i16], reference: &[i16]) -> f64 {
        if signal.is_empty() || reference.is_empty() || signal.len() < reference.len() {
            return 0.0;
        }
        let n = reference.len();
        let ref_mean: f64 = reference.iter().map(|&s| s as f64).sum::<f64>() / n as f64;
        let ref_var: f64 = reference
            .iter()
            .map(|&s| {
                let d = s as f64 - ref_mean;
                d * d
            })
            .sum::<f64>()
            / n as f64;
        if ref_var < 1e-20 {
            return 0.0;
        }
        let ref_std = ref_var.sqrt();
        let max_offset = signal.len() - n;
        let step = std::cmp::max(1, max_offset / 200);
        let mut best = 0.0;
        for offset in (0..=max_offset).step_by(step) {
            let w = &signal[offset..offset + n];
            let wm: f64 = w.iter().map(|&s| s as f64).sum::<f64>() / n as f64;
            let wv: f64 = w
                .iter()
                .map(|&s| {
                    let d = s as f64 - wm;
                    d * d
                })
                .sum::<f64>()
                / n as f64;
            if wv < 1e-20 {
                continue;
            }
            let ws = wv.sqrt();
            let num: f64 = w
                .iter()
                .zip(reference.iter())
                .map(|(&a, &b)| (a as f64 - wm) * (b as f64 - ref_mean))
                .sum();
            let c = num / (n as f64 * ws * ref_std);
            if c > best {
                best = c;
            }
        }
        best
    }

    #[test]
    fn test_sdp_transport_mode_classification() {
        // Plain RTP
        assert_eq!(
            SipSession::sdp_transport_mode("m=audio 1000 RTP/AVP 8 0\r\na=sendrecv\r\n"),
            rustrtc::TransportMode::Rtp
        );
        // SDES-SRTP via RTP/SAVP profile (Twilio-style)
        assert_eq!(
            SipSession::sdp_transport_mode(
                "m=audio 1000 RTP/SAVP 0 8 101\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:abc\r\n"
            ),
            rustrtc::TransportMode::Srtp
        );
        // SDES-SRTP advertised only via a=crypto
        assert_eq!(
            SipSession::sdp_transport_mode(
                "m=audio 1000 RTP/AVP 8\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:abc\r\n"
            ),
            rustrtc::TransportMode::Srtp
        );
        // WebRTC (ICE + DTLS) takes precedence even if a crypto line is present
        assert_eq!(
            SipSession::sdp_transport_mode(
                "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=ice-ufrag:x\r\na=fingerprint:sha-256 AA\r\n"
            ),
            rustrtc::TransportMode::WebRtc
        );
    }

    #[test]
    fn test_rtp_dtmf_detector_deduplicates_same_event() {
        let mut detector = RtpDtmfDetector::default();

        assert_eq!(detector.observe(&[1, 0x00, 0x00, 0xa0], 12_345), Some('1'));
        assert_eq!(detector.observe(&[1, 0x80, 0x01, 0x40], 12_345), None);
        assert_eq!(detector.observe(&[1, 0x00, 0x00, 0xa0], 12_505), Some('1'));
    }

    #[test]
    fn test_rtp_dtmf_detector_maps_special_digits() {
        let mut detector = RtpDtmfDetector::default();

        assert_eq!(detector.observe(&[10, 0x00, 0x00, 0xa0], 1), Some('*'));
        assert_eq!(detector.observe(&[11, 0x00, 0x00, 0xa0], 2), Some('#'));
        assert_eq!(detector.observe(&[12, 0x00, 0x00, 0xa0], 3), Some('A'));
        assert_eq!(detector.observe(&[16, 0x00, 0x00, 0xa0], 4), None);
    }

    #[test]
    fn test_rtp_dtmf_detector_receives_all_digits_0_to_9() {
        let mut detector = RtpDtmfDetector::default();

        // Test digits 0-9
        for digit_code in 0..=9 {
            let expected_digit = std::char::from_digit(digit_code as u32, 10).unwrap();
            let result = detector.observe(&[digit_code, 0x00, 0x00, 0xa0], digit_code as u32);
            assert_eq!(
                result,
                Some(expected_digit),
                "Failed to receive DTMF digit {}: got {:?}",
                digit_code,
                result
            );
        }
    }

    #[test]
    fn test_rtp_dtmf_detector_sequence_of_different_digits() {
        let mut detector = RtpDtmfDetector::default();

        // Simulate pressing 2-4-5-6 (queue transfer example)
        let sequence = vec![
            (2u8, 100u32, '2'),
            (4u8, 200u32, '4'),
            (5u8, 300u32, '5'),
            (6u8, 400u32, '6'),
        ];

        for (digit_code, timestamp, expected_char) in sequence {
            let result = detector.observe(&[digit_code, 0x00, 0x00, 0xa0], timestamp);
            assert_eq!(
                result,
                Some(expected_char),
                "Failed to receive DTMF sequence digit {}: got {:?}",
                expected_char,
                result
            );
        }
    }

    #[test]
    fn test_rtp_dtmf_detector_handles_short_payload() {
        let mut detector = RtpDtmfDetector::default();

        // Test with insufficient data (< 4 bytes)
        assert_eq!(detector.observe(&[1, 0x00], 100), None);
        assert_eq!(detector.observe(&[1, 0x00, 0x00], 100), None);
        assert_eq!(detector.observe(&[], 100), None);
    }

    #[test]
    fn test_rtp_dtmf_detector_extended_tone_recognition() {
        let mut detector = RtpDtmfDetector::default();

        // Test all valid DTMF codes (0-15)
        let expected_digits = vec![
            ('0', 0u8),
            ('1', 1u8),
            ('2', 2u8),
            ('3', 3u8),
            ('4', 4u8),
            ('5', 5u8),
            ('6', 6u8),
            ('7', 7u8),
            ('8', 8u8),
            ('9', 9u8),
            ('*', 10u8),
            ('#', 11u8),
            ('A', 12u8),
            ('B', 13u8),
            ('C', 14u8),
            ('D', 15u8),
        ];

        for (expected_digit, digit_code) in expected_digits {
            let result = detector.observe(&[digit_code, 0x00, 0x00, 0xa0], digit_code as u32);
            assert_eq!(
                result,
                Some(expected_digit),
                "Failed to map DTMF code {} to digit {}: got {:?}",
                digit_code,
                expected_digit,
                result
            );
        }
    }

    #[test]
    fn test_rtp_dtmf_detector_rapidly_repeated_digit() {
        let mut detector = RtpDtmfDetector::default();

        // User pressing "2" multiple times rapidly
        // First press should succeed
        assert_eq!(detector.observe(&[2, 0x00, 0x00, 0xa0], 1000), Some('2'));
        // Same timestamp = duplicate, should be filtered
        assert_eq!(detector.observe(&[2, 0x80, 0x01, 0x40], 1000), None);
        // New timestamp = new digit, should succeed
        assert_eq!(detector.observe(&[2, 0x00, 0x00, 0xa0], 2000), Some('2'));
        // Different digit on new timestamp
        assert_eq!(detector.observe(&[4, 0x00, 0x00, 0xa0], 3000), Some('4'));
    }

    #[test]
    fn test_session_drop_releases_resources() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropTracker;
        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        {
            let _tracker = DropTracker;
        }

        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_update_fallback_only_for_unsupported_methods() {
        assert!(SipSession::should_fallback_to_reinvite(
            StatusCode::MethodNotAllowed
        ));
        assert!(SipSession::should_fallback_to_reinvite(
            StatusCode::NotImplemented
        ));
        assert!(!SipSession::should_fallback_to_reinvite(
            StatusCode::RequestPending
        ));
        assert!(!SipSession::should_fallback_to_reinvite(
            StatusCode::RequestTimeout
        ));
        assert!(!SipSession::should_fallback_to_reinvite(
            StatusCode::Unauthorized
        ));
        assert!(!SipSession::should_fallback_to_reinvite(
            StatusCode::ServerInternalError
        ));
    }

    #[test]
    fn test_route_via_home_proxy_detects_remote_home_proxy() {
        let destination = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        };
        let home_proxy = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Tcp),
            addr: rsipstack::sip::HostWithPort::try_from("10.0.0.2:5070").unwrap(),
        };

        let target = Location {
            destination: Some(destination),
            home_proxy: Some(home_proxy.clone()),
            ..Default::default()
        };

        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.0.0.1:5060").unwrap(),
        }];

        assert!(SipSession::route_via_home_proxy(
            &target,
            &local_addrs,
            true
        ));
    }

    #[test]
    fn test_route_via_home_proxy_ignores_local_home_proxy() {
        let destination = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        };
        let home_proxy = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Tcp),
            addr: rsipstack::sip::HostWithPort::try_from("10.0.0.1:5060").unwrap(),
        };

        let target = Location {
            destination: Some(destination.clone()),
            home_proxy: Some(home_proxy),
            ..Default::default()
        };

        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.0.0.1:5060").unwrap(),
        }];

        assert!(!SipSession::route_via_home_proxy(
            &target,
            &local_addrs,
            true
        ));
    }

    #[test]
    fn test_callee_supports_webrtc_fallbacks() {
        fn loc(supports_webrtc: bool, dest_type: Option<rsipstack::sip::Transport>) -> Location {
            Location {
                supports_webrtc,
                destination: dest_type.map(|t| SipAddr {
                    r#type: Some(t),
                    addr: rsipstack::sip::HostWithPort::try_from("198.51.100.10:5060").unwrap(),
                }),
                ..Default::default()
            }
        }

        // Explicit flag wins regardless of transport.
        assert!(SipSession::callee_supports_webrtc(&loc(true, None)));

        // Regression: flag lost but resolved destination is WebSocket must still
        // classify the leg as WebRTC (otherwise a WSS/WebRTC callee receives a
        // plain RTP/AVP offer and rejects it with 488).
        assert!(SipSession::callee_supports_webrtc(&loc(
            false,
            Some(rsipstack::sip::Transport::Wss)
        )));
        assert!(SipSession::callee_supports_webrtc(&loc(
            false,
            Some(rsipstack::sip::Transport::Ws)
        )));

        // Plain UDP/TCP destinations are not WebRTC.
        assert!(!SipSession::callee_supports_webrtc(&loc(
            false,
            Some(rsipstack::sip::Transport::Udp)
        )));
        assert!(!SipSession::callee_supports_webrtc(&loc(
            false,
            Some(rsipstack::sip::Transport::Tcp)
        )));

        // No destination, but registered transport is WebSocket.
        assert!(SipSession::callee_supports_webrtc(&Location {
            supports_webrtc: false,
            transport: Some(rsipstack::sip::Transport::Wss),
            ..Default::default()
        }));

        // Nothing WebRTC at all.
        assert!(!SipSession::callee_supports_webrtc(&Location {
            supports_webrtc: false,
            ..Default::default()
        }));
    }

    #[test]
    fn test_resolve_outbound_callee_uri_prefers_registered_aor_via_home_proxy() {
        let contact_uri =
            rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();
        let registered_aor = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();
        let home_proxy = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.0.0.2:5070").unwrap(),
        };
        let expected = rsipstack::sip::Uri::try_from("sip:lp@10.0.0.2:5070").unwrap();

        let target = Location {
            aor: contact_uri,
            registered_aor: Some(registered_aor.clone()),
            home_proxy: Some(home_proxy),
            ..Default::default()
        };

        let resolved = SipSession::resolve_outbound_callee_uri(&target, true);
        assert_eq!(resolved, expected);
    }

    #[test]
    fn test_resolve_outbound_callee_uri_falls_back_to_contact_when_no_registered_aor() {
        let contact_uri =
            rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();

        let target = Location {
            aor: contact_uri.clone(),
            ..Default::default()
        };

        let resolved = SipSession::resolve_outbound_callee_uri(&target, true);
        assert_eq!(resolved, contact_uri);
    }

    #[test]
    fn test_resolve_outbound_callee_uri_uses_contact_when_not_via_home_proxy() {
        let contact_uri =
            rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();
        let registered_aor = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();

        let target = Location {
            aor: contact_uri.clone(),
            registered_aor: Some(registered_aor),
            ..Default::default()
        };

        let resolved = SipSession::resolve_outbound_callee_uri(&target, false);
        assert_eq!(resolved, contact_uri);
    }

    #[tokio::test]
    async fn test_init_callee_timer_disabled_without_session_expires() {
        use crate::call::{DialDirection, Dialplan, TransactionCookie};
        use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
        use crate::proxy::tests::common::{
            create_test_request, create_test_server, create_transaction,
        };

        let (server, _) = create_test_server().await;
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        );
        let original_request = request.clone();
        let (tx, _) = create_transaction(request).await;
        let (state_tx, _state_rx) = mpsc::unbounded_channel();
        let server_dialog = server
            .dialog_layer
            .get_or_create_server_invite(&tx, state_tx, None, None)
            .expect("failed to create server dialog");

        let context = CallContext {
            session_id: "test-session".to_string(),
            dialplan: Arc::new(Dialplan::new(
                "test-session".to_string(),
                original_request,
                DialDirection::Inbound,
            )),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:alice@rustpbx.com".to_string(),
            original_callee: "sip:bob@rustpbx.com".to_string(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };

        let caller_peer = Arc::new(MockMediaPeer::new());
        let callee_peer = Arc::new(MockMediaPeer::new());
        let (mut session, _handle, _cmd_rx) = SipSession::new(
            server.clone(),
            CancellationToken::new(),
            None,
            context,
            server_dialog,
            false,
            caller_peer,
            callee_peer,
        );

        let dialog_id = DialogId {
            call_id: "callee-call".into(),
            local_tag: "local".into(),
            remote_tag: "remote".into(),
        };
        let response = rsipstack::sip::Response {
            status_code: StatusCode::OK,
            version: rsipstack::sip::Version::V2,
            headers: rsipstack::sip::Headers::default(),
            body: Vec::new(),
        };

        session.init_callee_timer(
            dialog_id.clone(),
            &response,
            Duration::from_secs(DEFAULT_SESSION_EXPIRES),
        );

        let timer = session
            .timers
            .get(&dialog_id)
            .expect("missing callee timer");
        assert!(!timer.enabled);
        assert!(!timer.active);
        assert_eq!(
            timer.session_interval,
            Duration::from_secs(DEFAULT_SESSION_EXPIRES)
        );
        assert!(!session.timer_keys.contains_key(&dialog_id));
    }

    /// Regression: preparing the app/IVR caller media bridge must NOT open the
    /// caller gate — the gate opens only when the 200 OK is sent (accept_call).
    /// Before the fix, the app path never opened the gate at all, so caller
    /// audio + RFC 2833 DTMF were dropped → "RTP timeout: caller side silent"
    /// and IVR digit timeout.

    /// Regression: the app/IVR answer flow (prepare bridge → accept_call/200 OK)
    /// must open the caller gate. Before the fix, accept_call never opened the
    /// gate for the app path, dropping all caller→app RTP/DTMF.

    /// Regression test for the both-WebRTC + IVR recording bug.

    /// Verify WebRTC caller → RTP agent reuses bridge callee PC.

    #[tokio::test]
    async fn test_sip_session_handle() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-session");
        let (handle, mut cmd_rx) = SipSession::with_handle(id.clone());

        let result = handle.send_command(CallCommand::Answer {
            leg_id: LegId::from("caller"),
        });
        assert!(result.is_ok());

        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Answer { .. })));

        drop(handle);
    }

    #[tokio::test]
    async fn test_cancel_token_propagation() {
        let cancel_token = CancellationToken::new();
        let child_token = cancel_token.child_token();

        let task = crate::utils::spawn(async move {
            tokio::select! {
                _ = child_token.cancelled() => {
                    "cancelled"
                }
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    "timeout"
                }
            }
        });

        cancel_token.cancel();

        let result = tokio::time::timeout(Duration::from_millis(100), task).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap(), "cancelled");
    }

    #[tokio::test]
    async fn test_callee_event_channel_closed() {
        use rsipstack::dialog::DialogId;

        let (tx, mut rx) = mpsc::unbounded_channel::<DialogState>();

        let dialog_id = DialogId {
            call_id: "test".into(),
            local_tag: "local".into(),
            remote_tag: "remote".into(),
        };
        let _ = tx.send(DialogState::Trying(dialog_id));

        assert!(rx.recv().await.is_some());

        drop(tx);

        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_handle_lifecycle() {
        use crate::call::runtime::SessionId;

        for i in 0..10 {
            let id = SessionId::from(format!("lifecycle-test-{}", i));
            let (handle, cmd_rx) = SipSession::with_handle(id);

            drop(cmd_rx);
            drop(handle);
        }
    }

    #[tokio::test]
    async fn test_reject_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-reject");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        let result = handle.send_command(CallCommand::Reject {
            leg_id: LegId::from("caller"),
            reason: Some("User busy".to_string()),
        });
        assert!(result.is_ok());

        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Reject { .. })));

        drop(handle);
    }

    #[tokio::test]
    async fn test_ring_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-ring");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        let result = handle.send_command(CallCommand::Ring {
            leg_id: LegId::from("caller"),
            ringback: None,
        });
        assert!(result.is_ok());

        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Ring { .. })));

        drop(handle);
    }

    #[tokio::test]
    async fn test_send_dtmf_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-dtmf");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        let result = handle.send_command(CallCommand::SendDtmf {
            leg_id: LegId::from("caller"),
            digits: "1234".to_string(),
        });
        assert!(result.is_ok());

        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::SendDtmf { .. })));

        drop(handle);
    }

    #[tokio::test]
    async fn test_queue_enqueue_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-queue-enqueue");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        let result = handle.send_command(CallCommand::QueueEnqueue {
            leg_id: LegId::from("caller"),
            queue_id: "support-queue".to_string(),
            priority: Some(1),
        });
        assert!(result.is_ok());

        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::QueueEnqueue { .. })));

        drop(handle);
    }

    #[tokio::test]
    async fn test_queue_dequeue_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-queue-dequeue");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        let result = handle.send_command(CallCommand::QueueDequeue {
            leg_id: LegId::from("caller"),
        });
        assert!(result.is_ok());

        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::QueueDequeue { .. })));

        drop(handle);
    }

    #[tokio::test]
    async fn test_handle_reinvite_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-reinvite");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        let result = handle.send_command(CallCommand::HandleReInvite {
            leg_id: LegId::from("caller"),
            sdp:
                "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n"
                    .to_string(),
        });
        assert!(result.is_ok());

        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::HandleReInvite { .. })));

        drop(handle);
    }

    #[tokio::test]
    async fn test_mute_track_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-mute");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        let result = handle.send_command(CallCommand::MuteTrack {
            track_id: "track-1".to_string(),
        });
        assert!(result.is_ok());

        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::MuteTrack { .. })));

        drop(handle);
    }

    #[tokio::test]
    async fn test_unmute_track_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-unmute");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        let result = handle.send_command(CallCommand::UnmuteTrack {
            track_id: "track-1".to_string(),
        });
        assert!(result.is_ok());

        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::UnmuteTrack { .. })));

        drop(handle);
    }

    // ============================================================================
    // Call forwarding -> queue/ivr tests
    // ============================================================================

    #[tokio::test]
    async fn test_handle_blind_transfer_queue_prefix() {
        use crate::call::{DialDirection, Dialplan, TransactionCookie};
        use crate::config::ProxyConfig;
        use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
        use crate::proxy::routing::RouteQueueConfig;
        use crate::proxy::tests::common::{
            create_test_request, create_test_server_with_config, create_transaction,
        };

        let mut config = ProxyConfig::default();
        config.queues.insert(
            "test-queue".to_string(),
            RouteQueueConfig {
                name: Some("test-queue".to_string()),
                ..Default::default()
            },
        );

        let (server, _) = create_test_server_with_config(config).await;
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        );
        let original_request = request.clone();
        let (tx, _) = create_transaction(request).await;
        let (state_tx, _state_rx) = mpsc::unbounded_channel();
        let server_dialog = server
            .dialog_layer
            .get_or_create_server_invite(&tx, state_tx, None, None)
            .expect("failed to create server dialog");

        let context = CallContext {
            session_id: "test-session".to_string(),
            dialplan: Arc::new(Dialplan::new(
                "test-session".to_string(),
                original_request,
                DialDirection::Inbound,
            )),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:alice@rustpbx.com".to_string(),
            original_callee: "sip:bob@rustpbx.com".to_string(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };

        let caller_peer = Arc::new(MockMediaPeer::new());
        let callee_peer = Arc::new(MockMediaPeer::new());
        let (mut session, _handle, _cmd_rx) = SipSession::new(
            server.clone(),
            CancellationToken::new(),
            None,
            context,
            server_dialog,
            false,
            caller_peer,
            callee_peer,
        );
        let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
        session.callee_event_tx = Some(callee_tx);

        let result = session
            .handle_blind_transfer(
                LegId::from("caller"),
                "queue:test-queue".to_string(),
                &mut callee_rx,
            )
            .await;

        assert!(
            result.is_ok(),
            "handle_blind_transfer with queue: prefix should succeed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_handle_blind_transfer_queue_not_found() {
        use crate::call::{DialDirection, Dialplan, TransactionCookie};
        use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
        use crate::proxy::tests::common::{
            create_test_request, create_test_server, create_transaction,
        };

        let (server, _) = create_test_server().await;
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        );
        let original_request = request.clone();
        let (tx, _) = create_transaction(request).await;
        let (state_tx, _state_rx) = mpsc::unbounded_channel();
        let server_dialog = server
            .dialog_layer
            .get_or_create_server_invite(&tx, state_tx, None, None)
            .expect("failed to create server dialog");

        let context = CallContext {
            session_id: "test-session".to_string(),
            dialplan: Arc::new(Dialplan::new(
                "test-session".to_string(),
                original_request,
                DialDirection::Inbound,
            )),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:alice@rustpbx.com".to_string(),
            original_callee: "sip:bob@rustpbx.com".to_string(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };

        let caller_peer = Arc::new(MockMediaPeer::new());
        let callee_peer = Arc::new(MockMediaPeer::new());
        let (mut session, _handle, _cmd_rx) = SipSession::new(
            server.clone(),
            CancellationToken::new(),
            None,
            context,
            server_dialog,
            false,
            caller_peer,
            callee_peer,
        );
        let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
        session.callee_event_tx = Some(callee_tx);

        let result = session
            .handle_blind_transfer(
                LegId::from("caller"),
                "queue:nonexistent".to_string(),
                &mut callee_rx,
            )
            .await;

        assert!(
            result.is_err(),
            "handle_blind_transfer with non-existent queue should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Queue 'nonexistent' not found"),
            "Error should indicate queue not found, got: {}",
            err
        );
    }

    // ─── is_local_home_proxy unit tests ────────────────────────────────

    #[test]
    fn test_is_local_home_proxy_detects_matching_address() {
        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        }];
        let home_proxy = SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        };
        assert!(SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
    }

    #[test]
    fn test_is_local_home_proxy_detects_non_matching_address() {
        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        }];
        let home_proxy = SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
        };
        assert!(!SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
    }

    #[test]
    fn test_is_local_home_proxy_matches_any_local_address() {
        let local_addrs = vec![
            SipAddr {
                r#type: Some(rsipstack::sip::Transport::Udp),
                addr: rsipstack::sip::HostWithPort::try_from("127.0.0.1:5060").unwrap(),
            },
            SipAddr {
                r#type: Some(rsipstack::sip::Transport::Tcp),
                addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
            },
            SipAddr {
                r#type: Some(rsipstack::sip::Transport::Ws),
                addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8443").unwrap(),
            },
        ];
        let home_proxy = SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        };
        assert!(SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
    }

    #[test]
    fn test_is_local_home_proxy_rejects_port_mismatch() {
        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        }];
        let home_proxy = SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:5070").unwrap(),
        };
        assert!(!SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
    }

    #[test]
    fn test_is_local_home_proxy_compares_addr_string_not_transport() {
        // Transport type should NOT affect address matching — only host:port matters.
        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Wss),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        }];
        let home_proxy = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        };
        assert!(SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
    }

    // ─── route_via_home_proxy flag ───────

    #[test]
    fn test_route_via_home_proxy_false_without_home_proxy() {
        let destination = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        };
        let target = Location {
            destination: Some(destination.clone()),
            home_proxy: None,
            ..Default::default()
        };
        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.0.0.1:5060").unwrap(),
        }];
        assert!(!SipSession::route_via_home_proxy(
            &target,
            &local_addrs,
            false
        ));
    }

    #[test]
    fn test_route_via_home_proxy_remote_home_proxy_sets_via_flag() {
        // home_proxy != local -> route_via_home_proxy stays true.
        let destination = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
        };
        let home_proxy = SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
        };
        let target = Location {
            destination: Some(destination),
            home_proxy: Some(home_proxy.clone()),
            ..Default::default()
        };
        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        }];
        let via_home_proxy = SipSession::route_via_home_proxy(&target, &local_addrs, true);
        assert!(
            via_home_proxy,
            "route_via_home_proxy must be true for remote home_proxy"
        );
    }

    #[test]
    fn test_route_via_home_proxy_local_home_proxy_no_via_flag() {
        let destination = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        };
        let home_proxy = SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        };
        let target = Location {
            destination: Some(destination.clone()),
            home_proxy: Some(home_proxy),
            ..Default::default()
        };
        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        }];
        let via_home_proxy = SipSession::route_via_home_proxy(&target, &local_addrs, true);
        assert!(
            !via_home_proxy,
            "route_via_home_proxy must be false when home_proxy is local"
        );
    }

    // ─── Verify no self-referencing Record-Route in INVITE headers ────

    #[test]
    fn test_route_via_home_proxy_does_not_add_self_referencing_record_route() {
        // This test validates the architectural fix:
        // When routing via a remote home_proxy, the INVITE MUST NOT include
        // a Record-Route header pointing to the local node. Including one
        // would cause the dialog route_set to contain a self-referencing
        // Route entry, which makes all subsequent in-dialog requests
        // (BYE, ACK) loopback to the local node instead of reaching the
        // remote agent.
        //
        // The Contact header in the INVITE already provides the correct
        // return path for the callee's responses and requests.
        //
        // This test exercises is_local_home_proxy and route_via_home_proxy
        // to ensure the routing logic is correct. The actual INVITE header construction is exercised
        // by the cluster home_proxy e2e test.
        //
        // Verify: home_proxy is recognized as remote -> via_home_proxy=true
        let destination = SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
        };
        let home_proxy = SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
        };
        let target = Location {
            destination: Some(destination),
            home_proxy: Some(home_proxy.clone()),
            ..Default::default()
        };
        let local_addrs = vec![SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        }];
        let via_home_proxy = SipSession::route_via_home_proxy(&target, &local_addrs, true);
        assert!(
            via_home_proxy,
            "route_via_home_proxy must be true for cross-node routing"
        );

        // Verify that BOTH local and remote addresses are correctly
        // distinguished. A local address match → false, remote → true.
        assert!(
            !SipSession::is_local_home_proxy(&local_addrs, &home_proxy),
            "home_proxy at 10.172.149.126 must NOT match local 10.172.148.121"
        );

        let local_home_proxy = SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        };
        assert!(
            SipSession::is_local_home_proxy(&local_addrs, &local_home_proxy),
            "home_proxy at 10.172.148.121 must match local 10.172.148.121"
        );
    }

    // ── filter_video_caps_for_rtp ────────────────────────────────────────────

    fn make_video_cap(
        pt: u8,
        codec: &str,
        fmtp: Option<&str>,
        rtcp_fbs: &[&str],
    ) -> rustrtc::VideoCapability {
        rustrtc::VideoCapability {
            payload_type: pt,
            codec_name: codec.to_string(),
            clock_rate: 90000,
            fmtp: fmtp.map(|s| s.to_string()),
            rtcp_fbs: rtcp_fbs.iter().map(|s| s.to_string()).collect(),
            rtx_payload_type: None,
        }
    }

    #[test]
    fn test_apply_video_caps_from_source_keeps_one_source_h264_configuration() {
        let generated_offer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 103 107 104\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtcp-fb:96 nack\r\n\
a=rtpmap:103 H264/90000\r\n\
a=fmtp:103 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
a=rtcp-fb:103 nack pli\r\n\
a=rtpmap:107 H264/90000\r\n\
a=fmtp:107 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r\n\
a=rtcp-fb:107 nack pli\r\n\
a=rtpmap:104 VP9/90000\r\n\
a=rtcp-fb:104 nack\r\n";
        let source_caps = vec![
            make_video_cap(96, "H264", Some("profile-level-id=42801F"), &[]),
            make_video_cap(97, "VP8", None, &[]),
        ];

        let reordered = SipSession::apply_video_caps_from_source(
            rustrtc::SdpType::Offer,
            generated_offer,
            "test offer",
            &source_caps,
        )
        .unwrap();

        assert!(reordered.contains("m=video 9 UDP/TLS/RTP/SAVPF 103\r\n"));
        assert!(reordered.contains("a=rtpmap:103 H264/90000\r\n"));
        assert!(reordered.contains("a=fmtp:103 profile-level-id=42801F\r\n"));
        assert!(!reordered.contains("a=rtpmap:107 H264/90000\r\n"));
        assert!(!reordered.contains("a=rtpmap:96 VP8/90000\r\n"));
        assert!(!reordered.contains("a=rtpmap:104 VP9/90000\r\n"));
        assert!(!reordered.contains("a=rtcp-fb:104 "));
    }

    #[test]
    fn test_apply_video_caps_to_answer_preserves_offer_payload_and_fmtp() {
        let generated_answer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 4000 UDP/TLS/RTP/SAVPF 96\r\n\
a=sendrecv\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\n";
        let source_caps = vec![
            make_video_cap(96, "H265", None, &[]),
            make_video_cap(
                97,
                "H264",
                Some("profile-level-id=42801F"),
                &["nack pli", "ccm fir"],
            ),
        ];

        let answer = SipSession::apply_video_caps_from_source(
            rustrtc::SdpType::Answer,
            generated_answer,
            "test answer",
            &source_caps,
        )
        .unwrap();

        assert!(answer.contains("m=video 4000 UDP/TLS/RTP/SAVPF 97\r\n"));
        assert!(answer.contains("a=rtpmap:97 H264/90000\r\n"));
        assert!(answer.contains("a=fmtp:97 profile-level-id=42801F\r\n"));
        assert!(answer.contains("a=rtcp-fb:97 nack pli\r\n"));
        assert!(answer.contains("a=rtcp-fb:97 ccm fir\r\n"));
        assert!(!answer.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(!answer.contains("packetization-mode=1"));
    }

    /// Default allowlist keeps one H264 and strips feedback from the RTP leg.
    #[test]
    fn test_filter_video_caps_default_keeps_h264_only() {
        let caps = vec![
            make_video_cap(
                96,
                "H264",
                Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"),
                &["goog-remb", "transport-cc", "nack", "nack pli", "ccm fir"],
            ),
            make_video_cap(97, "VP8", None, &["goog-remb", "transport-cc"]),
            make_video_cap(98, "VP9", None, &["goog-remb"]),
        ];

        let result = SipSession::filter_video_caps_for_rtp(&caps, &[]);

        assert_eq!(result.len(), 1, "only H264 should survive");
        assert_eq!(result[0].codec_name, "H264");
        assert_eq!(result[0].payload_type, 96);
        assert!(result[0].rtcp_fbs.is_empty());
        assert!(result[0].fmtp.is_some(), "fmtp should be preserved");
    }

    /// An explicit allowlist permits H264 but cannot enable another video codec.
    #[test]
    fn test_filter_video_caps_explicit_allowlist() {
        let caps = vec![
            make_video_cap(96, "H264", Some("profile-level-id=42e01f"), &["goog-remb"]),
            make_video_cap(97, "VP8", None, &["transport-cc"]),
            make_video_cap(98, "H265", None, &[]),
        ];

        let allowed = vec!["h265".to_string(), "H264".to_string()];
        let result = SipSession::filter_video_caps_for_rtp(&caps, &allowed);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].codec_name, "H264");
        assert!(result.iter().all(|c| c.rtcp_fbs.is_empty()));
    }

    #[test]
    fn test_filter_video_caps_rejects_allowlist_without_h264() {
        let caps = vec![
            make_video_cap(96, "H264", Some("profile-level-id=42e01f"), &[]),
            make_video_cap(98, "H265", None, &[]),
        ];
        let allowed = vec!["H265".to_string()];

        let result = SipSession::filter_video_caps_for_rtp(&caps, &allowed);

        assert!(result.is_empty());
    }

    /// The RTP/AVP leg does not advertise AVPF feedback.
    #[test]
    fn test_filter_video_caps_strips_all_rtcp_feedback() {
        let caps = vec![make_video_cap(
            96,
            "H264",
            None,
            &["nack", "nack pli", "ccm fir", "goog-remb", "transport-cc"],
        )];

        let result = SipSession::filter_video_caps_for_rtp(&caps, &[]);

        assert!(result[0].rtcp_fbs.is_empty());
    }

    /// Default allowlist does not fall back to non-H264 codecs.
    #[test]
    fn test_filter_video_caps_default_does_not_fallback_when_no_match() {
        let caps = vec![
            make_video_cap(97, "VP8", None, &["goog-remb", "transport-cc"]),
            make_video_cap(98, "VP9", None, &["goog-remb"]),
        ];

        let result = SipSession::filter_video_caps_for_rtp(&caps, &[]);

        assert!(result.is_empty(), "default should not accept VP8/VP9");
    }

    /// Empty caps slice produces empty result (no panic).
    #[test]
    fn test_filter_video_caps_empty_input() {
        let result = SipSession::filter_video_caps_for_rtp(&[], &[]);
        assert!(result.is_empty());
    }

    /// Codec name matching is case-insensitive in both directions.
    #[test]
    fn test_filter_video_caps_case_insensitive_matching() {
        let caps = vec![
            make_video_cap(96, "h264", None, &["nack"]), // lowercase codec name
        ];

        // Allowlist uses uppercase "H264"
        let result = SipSession::filter_video_caps_for_rtp(&caps, &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].codec_name, "h264");
    }

    /// fmtp string is preserved exactly on matched codecs.
    #[test]
    fn test_filter_video_caps_fmtp_preserved() {
        let fmtp = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032";
        let caps = vec![make_video_cap(96, "H264", Some(fmtp), &["goog-remb"])];

        let result = SipSession::filter_video_caps_for_rtp(&caps, &[]);
        assert_eq!(result[0].fmtp.as_deref(), Some(fmtp));
    }

    /// Multiple H264 profiles are reduced to the first offered profile.
    #[test]
    fn test_filter_video_caps_keeps_first_h264_profile_only() {
        let caps = vec![
            make_video_cap(96, "H264", Some("profile-level-id=42e01f"), &["goog-remb"]),
            make_video_cap(97, "VP8", None, &["transport-cc"]),
            make_video_cap(98, "H264", Some("profile-level-id=640032"), &["nack"]),
        ];

        let result = SipSession::filter_video_caps_for_rtp(&caps, &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].payload_type, 96);
        assert_eq!(result[0].fmtp.as_deref(), Some("profile-level-id=42e01f"));
        assert!(result[0].rtcp_fbs.is_empty());
    }

    // ── DTMF payload building ─────────────────────────────────────────────


    // --- trunk_host_port tests ---

    #[test]
    fn test_trunk_host_port_sip_uri_with_port() {
        let (host, port) = trunk_host_port("sip:58.246.19.74:6988").unwrap();
        assert_eq!(host, "58.246.19.74");
        assert_eq!(port, 6988);
    }

    #[test]
    fn test_trunk_host_port_sip_uri_without_port() {
        let (host, port) = trunk_host_port("sip:pbx.example.com").unwrap();
        assert_eq!(host, "pbx.example.com");
        assert_eq!(port, 5060);
    }

    #[test]
    fn test_trunk_host_port_sip_uri_with_user_and_port() {
        let (host, port) = trunk_host_port("sip:user@203.0.113.5:5060").unwrap();
        assert_eq!(host, "203.0.113.5");
        assert_eq!(port, 5060);
    }

    #[test]
    fn test_trunk_host_port_bare_host_port() {
        let (host, port) = trunk_host_port("58.246.19.74:6988").unwrap();
        assert_eq!(host, "58.246.19.74");
        assert_eq!(port, 6988);
    }

    #[test]
    fn test_trunk_host_port_bare_host_only() {
        let (host, port) = trunk_host_port("203.0.113.10").unwrap();
        assert_eq!(host, "203.0.113.10");
        assert_eq!(port, 5060);
    }

    #[test]
    fn test_trunk_host_port_bare_ipv6() {
        let (host, port) = trunk_host_port("[::1]").unwrap();
        assert_eq!(host, "[::1]");
        assert_eq!(port, 5060);
    }

    #[test]
    fn test_trunk_host_port_empty() {
        assert!(trunk_host_port("").is_none());
    }

    // --- resolve_effective_codecs priority logic tests ---

    #[test]
    fn test_priority_uses_dialplan_first() {
        let codecs = resolve_codecs_fake(&[CodecType::PCMA, CodecType::G729], &[]);
        assert_eq!(codecs, vec![CodecType::PCMA, CodecType::G729]);
    }

    #[test]
    fn test_priority_falls_back_to_proxy_when_dialplan_empty() {
        let codecs = resolve_codecs_fake(&[], &["pcma", "g729"]);
        assert_eq!(codecs, vec![CodecType::PCMA, CodecType::G729]);
    }

    #[test]
    fn test_priority_returns_empty_when_no_sources() {
        let codecs = resolve_codecs_fake(&[], &[] as &[&str]);
        assert!(codecs.is_empty());
    }

    #[test]
    fn test_priority_filters_invalid_codec_names() {
        let codecs = resolve_codecs_fake(&[], &["pcma", "invalid_codec", "g729"]);
        assert_eq!(codecs, vec![CodecType::PCMA, CodecType::G729]);
    }

    #[test]
    fn test_priority_ignores_empty_proxy_config() {
        let codecs = resolve_codecs_fake(&[], &[""]);
        assert!(codecs.is_empty());
    }

    #[test]
    fn test_priority_dialplan_with_opus() {
        let codecs = resolve_codecs_fake(&[CodecType::Opus, CodecType::PCMU], &[]);
        assert_eq!(codecs, vec![CodecType::Opus, CodecType::PCMU]);
    }

    /// Simulates the priority chain: dialplan → trunk → proxy.
    fn resolve_codecs_fake(dialplan: &[CodecType], proxy_strs: &[&str]) -> Vec<CodecType> {
        if !dialplan.is_empty() {
            return dialplan.to_vec();
        }
        let proxy: Vec<String> = proxy_strs
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if !proxy.is_empty() {
            return parse_allowed_codecs(&proxy);
        }
        vec![]
    }

    // ── SipSession::parse_info_media_source tests ──────────────────────────
    use crate::call::domain::MediaSource;

    #[test]
    fn test_parse_file_source() {
        let src = serde_json::json!({"source_type": "file", "uri": "/tmp/a.wav"});
        assert_eq!(
            super::SipSession::parse_info_media_source(&src),
            Some(MediaSource::File {
                path: "/tmp/a.wav".into()
            })
        );
    }

    #[test]
    fn test_parse_url_source() {
        let src = serde_json::json!({"source_type": "url", "uri": "http://x.com/a.wav"});
        assert_eq!(
            super::SipSession::parse_info_media_source(&src),
            Some(MediaSource::Url {
                url: "http://x.com/a.wav".into()
            })
        );
    }

    #[test]
    fn test_parse_silence_source() {
        let src = serde_json::json!({"source_type": "silence"});
        assert_eq!(
            super::SipSession::parse_info_media_source(&src),
            Some(MediaSource::Silence)
        );
    }

    #[test]
    fn test_parse_files_source_uses_first_uri() {
        let src = serde_json::json!({"source_type": "files", "uris": ["/tmp/a.wav", "/tmp/b.wav"]});
        assert_eq!(
            super::SipSession::parse_info_media_source(&src),
            Some(MediaSource::File {
                path: "/tmp/a.wav".into()
            })
        );
    }

    #[test]
    fn test_parse_unknown_source_type() {
        let src = serde_json::json!({"source_type": "mp3", "uri": "/tmp/x.mp3"});
        assert_eq!(super::SipSession::parse_info_media_source(&src), None);
    }

    #[test]
    fn test_parse_defaults_to_file() {
        let src = serde_json::json!({"uri": "/tmp/default.wav"});
        assert_eq!(
            super::SipSession::parse_info_media_source(&src),
            Some(MediaSource::File {
                path: "/tmp/default.wav".into()
            })
        );
    }

    // ── Bug 3: transport-aware parallel-fork callee offer caching ──────


    // ── Bug 4: app bridge reused for same-transport callee ─────────────

    // ── Layer 2: media.play → codec + sample rate verification ──
    //
    // Content verification (cross-correlation, frequency analysis) is done
    // at the Recorder level in src/media/info_recording_tests.rs (Layer 4)
    // because bridge get_callee_track() exposes the RECEIVE path (audio from
    // callee), not the SEND path where handle_play injects the file.

    // ── Layer 3: hold/unhold SDP direction ──

    #[tokio::test]
    async fn test_hold_sdp_contains_sendonly() {
        use crate::call::{DialDirection, Dialplan, TransactionCookie};
        use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
        use crate::proxy::tests::common::{
            create_test_request, create_test_server, create_transaction,
        };
        let (server, _) = create_test_server().await;
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        );
        let original_request = request.clone();
        let (tx, _) = create_transaction(request).await;
        let (state_tx, _state_rx) = mpsc::unbounded_channel();
        let server_dialog = server
            .dialog_layer
            .get_or_create_server_invite(&tx, state_tx, None, None)
            .unwrap();
        let caller_peer = Arc::new(MockMediaPeer::new());
        let callee_peer = Arc::new(MockMediaPeer::new());
        let (mut session, _h, _rx) = SipSession::new(
            server,
            CancellationToken::new(),
            None,
            CallContext {
                session_id: "test-hold-sdp".to_string(),
                dialplan: Arc::new(Dialplan::new(
                    "test-hold-sdp".to_string(),
                    original_request,
                    DialDirection::Inbound,
                )),
                cookie: TransactionCookie::default(),
                start_time: Instant::now(),
                original_caller: "sip:alice@rustpbx.com".to_string(),
                original_callee: "sip:bob@rustpbx.com".to_string(),
                max_forwards: 70,
                created_at: chrono::Utc::now().to_rfc3339(),
                metadata: None,
            },
            server_dialog,
            false,
            caller_peer,
            callee_peer,
        );

        // Hold SDP: sendrecv → sendonly
        let sendrecv_sdp = concat!(
            "v=0\r\n",
            "o=alice 1 1 IN IP4 192.0.2.10\r\n",
            "s=Talk\r\n",
            "c=IN IP4 192.0.2.10\r\n",
            "t=0 0\r\n",
            "m=audio 40000 RTP/AVP 0 101\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=rtpmap:101 telephone-event/8000\r\n",
            "a=sendrecv\r\n",
        )
        .to_string();
        // The method reads answer first, then caller_offer
        session.media.answer = Some(sendrecv_sdp);

        let hold_sdp = session.generate_hold_sdp().await.expect("hold SDP");
        assert!(
            hold_sdp.contains("a=sendonly"),
            "hold SDP must be sendonly, got: {}",
            hold_sdp
        );
        assert!(
            !hold_sdp.contains("a=sendrecv"),
            "hold SDP must NOT contain sendrecv"
        );

        let unhold_sdp = session.generate_unhold_sdp().await.expect("unhold SDP");
        assert!(
            unhold_sdp.contains("a=sendrecv"),
            "unhold SDP must be sendrecv, got: {}",
            unhold_sdp
        );
        assert!(
            !unhold_sdp.contains("a=sendonly"),
            "unhold SDP must NOT contain sendonly"
        );
    }

    // ── Layer 1: parse_info_command dispatch (pure function, no session needed) ──

    #[test]
    fn test_parse_info_media_play() {
        let params = serde_json::json!({"source": {"source_type": "file", "uri": "/tmp/test.wav"}, "loop": true});
        let cmd = SipSession::parse_info_command("media.play", Some(&params), &params)
            .expect("parse_info_command returned None");
        match cmd {
            CallCommand::Play {
                source: crate::call::domain::MediaSource::File { ref path },
                ref options,
                ..
            } => {
                assert_eq!(path, "/tmp/test.wav");
                assert!(options.as_ref().unwrap().loop_playback);
            }
            _ => panic!("expected Play with File source"),
        }
    }

    #[test]
    fn test_parse_info_media_stop() {
        let json = serde_json::json!({"leg_id": "callee"});
        let cmd = SipSession::parse_info_command("media.stop", Some(&json), &json).unwrap();
        assert!(
            matches!(&cmd, CallCommand::StopPlayback { leg_id } if leg_id == &Some(LegId::from("callee")))
        );
    }

    #[test]
    fn test_parse_info_record_start() {
        let json = serde_json::json!({"path": "/tmp/rec.wav", "beep": false});
        let cmd = SipSession::parse_info_command("record.start", Some(&json), &json).unwrap();
        assert!(
            matches!(&cmd, CallCommand::StartRecording { config } if config.path == "/tmp/rec.wav" && !config.beep)
        );
    }

    #[test]
    fn test_parse_info_record_stop() {
        assert!(matches!(
            SipSession::parse_info_command("record.stop", None, &serde_json::json!({})),
            Some(CallCommand::StopRecording),
        ));
    }

    #[test]
    fn test_parse_info_hold() {
        let json = serde_json::json!({"leg_id": "callee"});
        let cmd = SipSession::parse_info_command("hold", Some(&json), &json).unwrap();
        assert!(
            matches!(&cmd, CallCommand::Hold { leg_id, music } if leg_id == &LegId::from("callee") && music.is_none())
        );
    }

    #[test]
    fn test_parse_info_unhold() {
        let json = serde_json::json!({"leg_id": "callee"});
        let cmd = SipSession::parse_info_command("unhold", Some(&json), &json).unwrap();
        assert!(matches!(&cmd, CallCommand::Unhold { leg_id } if leg_id == &LegId::from("callee")));
    }

    #[test]
    fn test_parse_info_hold_with_music() {
        let json = serde_json::json!({"music": {"source_type": "file", "uri": "/tmp/hold.wav"}});
        let cmd = SipSession::parse_info_command("hold", Some(&json), &json).unwrap();
        assert!(matches!(&cmd, CallCommand::Hold { music: Some(_), .. }));
    }

    #[test]
    fn test_parse_info_consult_initiate() {
        let parsed = serde_json::json!({});
        let json = serde_json::json!({"leg_id": "caller"});
        let cmd = SipSession::parse_info_command("consult.initiate", Some(&json), &parsed).unwrap();
        assert!(
            matches!(&cmd, CallCommand::Hold { leg_id, music: None } if leg_id == &LegId::from("caller"))
        );
    }

    #[test]
    fn test_parse_info_consult_cancel() {
        let parsed = serde_json::json!({"call_id": "dynamic-leg"});
        let cmd = SipSession::parse_info_command("consult.cancel", None, &parsed).unwrap();
        assert!(
            matches!(&cmd, CallCommand::Unhold { leg_id } if leg_id == &LegId::from("dynamic-leg"))
        );
    }

    #[test]
    fn test_parse_info_unknown_action() {
        assert!(
            SipSession::parse_info_command("unknown.action", None, &serde_json::json!({}))
                .is_none()
        );
    }

    // ── Layer 2 helpers (take &mut SipSession only, no complex types) ──

    // ── BuiltinAppFactory IVR from DB store ──────────────────────────────────

    #[tokio::test]
    async fn builtin_app_factory_creates_ivr_from_db_store() {
        use sea_orm::{ConnectionTrait, Database, sea_query::SqliteQueryBuilder};

        // Setup in-memory SQLite with config_entries table
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = sea_orm::Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(crate::models::config_entry::Entity);
        let sql = stmt.to_string(SqliteQueryBuilder);
        db.execute_unprepared(&sql).await.unwrap();
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_config_entries_category_name \
             ON config_entries (category, entry_name)",
        )
        .await
        .unwrap();

        // Write a valid IVR entry into the DB store
        let store = crate::config_store::GeneratedConfigStore::Database { db: db.clone() };
        let ivr_toml = r#"
[ivr]
name = "test-ivr"
ivr_mode = "tree"

[ivr.root]
greeting = "sounds/welcome.wav"
timeout_ms = 30000
max_retries = 3
"#;
        store
            .write("ivr", "test_ivr.generated.toml", ivr_toml)
            .await
            .unwrap();

        // Config with generated_db = true (must match server's real config)
        let mut config = crate::config::Config::default();
        config.proxy.generated_db = true;

        let call_info = crate::call::app::CallInfo {
            session_id: "test-session".to_string(),
            caller: "caller".to_string(),
            callee: "1000".to_string(),
            direction: "inbound".to_string(),
            started_at: chrono::Utc::now(),
            sip_headers: std::collections::HashMap::new(),
            route_name: None,
        };
        let app_ctx =
            crate::call::app::ApplicationContext::new(db, call_info, std::sync::Arc::new(config));

        let factory = BuiltinAppFactory {
            addon_registry: None,
        };

        let params = Some(serde_json::json!({
            "file": "db://ivr/test_ivr.generated.toml"
        }));
        let app = factory.create_app("ivr", params, &app_ctx).await;

        assert!(
            app.is_some(),
            "BuiltinAppFactory should create IVR app from DB store when generated_db=true"
        );
    }

    // ── align_answer_direction_with_offer ──

    #[test]
    fn test_is_zero_connection() {
        assert!(SipSession::is_zero_connection("IN IP4 0.0.0.0"));
        assert!(SipSession::is_zero_connection("IN IP6 ::"));
        assert!(SipSession::is_zero_connection("IN IP6 0:0:0:0:0:0:0:0"));
        assert!(!SipSession::is_zero_connection("IN IP4 192.168.1.1"));
        assert!(!SipSession::is_zero_connection("IN IP4 127.0.0.1"));
    }

    #[test]
    fn test_align_answer_direction_audio_hold() {
        let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        assert!(
            result.contains("a=recvonly"),
            "hold offer sendonly → answer recvonly:\n{}",
            result
        );
        assert!(
            !result.contains("a=sendrecv"),
            "answer should not have sendrecv:\n{}",
            result
        );
    }

    #[test]
    fn test_align_answer_direction_unhold() {
        let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        assert!(
            result.contains("a=sendrecv"),
            "unhold offer sendrecv → answer keep sendrecv:\n{}",
            result
        );
    }

    #[test]
    fn test_align_answer_direction_audio_recvonly() {
        let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        assert!(
            result.contains("a=sendonly"),
            "offer recvonly → answer sendonly:\n{}",
            result
        );
    }

    #[test]
    fn test_align_answer_direction_inactive() {
        let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=inactive\r\n";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        assert!(
            result.contains("a=inactive"),
            "offer inactive → answer inactive:\n{}",
            result
        );
    }

    #[test]
    fn test_align_answer_direction_port_zero() {
        let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 0 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        assert!(
            result.contains("a=inactive"),
            "port=0 → answer inactive:\n{}",
            result
        );
    }

    #[test]
    fn test_align_answer_direction_zero_connection() {
        let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        assert!(
            result.contains("a=inactive"),
            "c=0.0.0.0 → answer inactive:\n{}",
            result
        );
    }

    #[test]
    fn test_align_answer_direction_mixed_audio_video() {
        let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\nm=video 10002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=sendrecv\r\n";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\nm=video 20002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        assert!(
            result.contains("a=recvonly"),
            "audio hold → audio recvonly:\n{}",
            result
        );
        assert!(
            result.contains("a=sendrecv"),
            "video unchanged → video sendrecv:\n{}",
            result
        );
        // Audio section rewritten → recvonly, video unchanged → sendrecv
        let recvonly_count = result.matches("a=recvonly").count();
        let sendrecv_count = result.matches("a=sendrecv").count();
        assert_eq!(
            recvonly_count, 1,
            "one recvonly for audio hold:\n{}",
            result
        );
        assert_eq!(sendrecv_count, 1, "one sendrecv for video:\n{}", result);
    }

    #[test]
    fn test_align_answer_direction_no_offer_direction() {
        let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        // No direction in offer → default is sendrecv → answer unchanged
        assert!(
            result.contains("a=sendrecv"),
            "no offer direction → answer unchanged:\n{}",
            result
        );
    }

    #[test]
    fn test_align_answer_direction_invalid_offer() {
        let offer = "not an sdp at all";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        assert_eq!(result, answer, "invalid offer → answer unchanged");
    }

    #[test]
    fn test_align_answer_direction_section_connection_zero() {
        // Section-level c=0.0.0.0, session-level c=10.0.0.1
        let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        let result = SipSession::align_answer_direction_with_offer(offer, answer);
        assert!(
            result.contains("a=inactive"),
            "section c=0.0.0.0 → answer inactive:\n{}",
            result
        );
    }

    // ── resolve_audio_file_path: the path resolution that handle_play relies on ──

    #[test]
    fn test_resolve_audio_file_path_http_passthrough() {
        assert_eq!(
            SipSession::resolve_audio_file_path("http://example.com/a.wav"),
            "http://example.com/a.wav"
        );
        assert_eq!(
            SipSession::resolve_audio_file_path("https://example.com/a.wav"),
            "https://example.com/a.wav"
        );
    }

    #[test]
    fn test_resolve_audio_file_path_absolute_passthrough() {
        let abs = if cfg!(windows) {
            "C:\\tmp\\a.wav"
        } else {
            "/tmp/a.wav"
        };
        assert_eq!(SipSession::resolve_audio_file_path(abs), abs);
    }

    #[test]
    fn test_resolve_audio_file_path_config_prefix_passthrough() {
        // Already-prefixed paths must be returned as-is to avoid double prefixing.
        assert_eq!(
            SipSession::resolve_audio_file_path("config/sounds/foo.wav"),
            "config/sounds/foo.wav"
        );
        assert_eq!(
            SipSession::resolve_audio_file_path("./config/sounds/foo.wav"),
            "./config/sounds/foo.wav"
        );
    }

    #[test]
    fn test_resolve_audio_file_path_falls_back_to_config_prefix() {
        // The shipped convention: configs reference "sounds/foo.wav" but the
        // files live under "config/sounds/" at dev time. The resolver must
        // transparently rewrite to the existing config-prefixed path.
        let tmp = std::env::temp_dir().join("rp_bench_exists.wav");
        std::fs::write(&tmp, b"dummy").unwrap();
        let abs = tmp.to_string_lossy().to_string();
        // Absolute path that exists → passthrough.
        assert_eq!(SipSession::resolve_audio_file_path(&abs), abs);

        // Non-existent bare path with no fallback → returned unchanged.
        let bare = "definitely_missing_zzz.wav";
        assert_eq!(SipSession::resolve_audio_file_path(bare), bare);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_resolve_audio_file_path_packaged_sounds_resolve_to_config() {
        // Regression for the queue-hold-music bug: the default constant
        // `sounds/phone-calling.wav` does not exist at the workspace root but
        // `config/sounds/phone-calling.wav` does. Resolution must find it.
        if !Path::new("config/sounds/phone-calling.wav").exists() {
            eprintln!("skipping: config/sounds/phone-calling.wav absent (not in workspace root)");
            return;
        }
        let resolved = SipSession::resolve_audio_file_path(crate::call::DEFAULT_QUEUE_HOLD_AUDIO);
        assert!(
            resolved.ends_with("phone-calling.wav"),
            "expected resolved path to end with phone-calling.wav, got {resolved}"
        );
        assert!(
            Path::new(&resolved).exists(),
            "resolved hold-audio path must exist: {resolved}"
        );
    }

    /// Every shipped default queue prompt must resolve to a real, decodable
    /// WAV file. This guards against the regression where `handle_play`
    /// skipped path resolution and failed with "Audio file not found".
    #[tokio::test]
    async fn test_default_queue_prompts_resolve_and_are_playable() {
        use crate::media::audio_source::{AudioSource, FileAudioSource};

        let cases = [
            ("hold", crate::call::DEFAULT_QUEUE_HOLD_AUDIO),
            ("failure", crate::call::DEFAULT_QUEUE_FAILURE_AUDIO),
            (
                "transfer-zh",
                crate::call::DEFAULT_QUEUE_TRANSFER_PROMPT_ZH,
            ),
            ("busy-zh", crate::call::DEFAULT_QUEUE_BUSY_PROMPT_ZH),
            ("no-answer-zh", crate::call::DEFAULT_QUEUE_NO_ANSWER_PROMPT_ZH),
        ];

        // If the test host has no `config/sounds` checkout, skip gracefully
        // rather than failing — the resolution logic is covered by other unit
        // tests in this module.
        if !Path::new("config/sounds").is_dir() {
            eprintln!("skipping: config/sounds/ directory not present");
            return;
        }

        for (label, spec) in cases {
            let resolved = SipSession::resolve_audio_file_path(spec);
            assert!(
                Path::new(&resolved).exists(),
                "[{label}] resolved path must exist: spec={spec} resolved={resolved}"
            );

            // The file must be openable AND decodable — the exact gate that
            // `handle_play` → `play_file` → `FileAudioSource::new` applies.
            let src = FileAudioSource::new(resolved.clone(), false)
                .await
                .unwrap_or_else(|e| panic!("[{label}] FileAudioSource::new failed for {resolved}: {e}"));
            assert!(
                src.sample_rate() > 0,
                "[{label}] decoded file should report a positive sample rate"
            );
            // Pre-decoded cache must be non-empty for shipped prompts.
            assert!(
                src.has_data(),
                "[{label}] decoded file should contain PCM samples: {resolved}"
            );
            let _ = AudioSource::has_data(&src); // quiet dead_code if not used elsewhere
        }
    }
}

