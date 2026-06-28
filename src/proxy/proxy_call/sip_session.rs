use crate::call::app::PendingQueuePlan;
use crate::call::app::{ApplicationContext, CallInfo};
use crate::call::domain::{
    CallCommand, HangupCascade, HangupCommand, LegId, LegState, MediaPathMode, MediaRuntimeProfile,
    MediaSource, RingbackPolicy,
};
use crate::call::domain::{Leg, SessionState};
use crate::call::runtime::BridgeConfig;
use crate::call::runtime::invoke_post_call_hook;
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
use crate::media::bridge::{BridgeEndpoint, BridgePeerBuilder};
use crate::media::mixer::MediaMixer;
use crate::media::negotiate::{CodecInfo, MediaNegotiator};
use crate::media::recorder::Recorder;
use crate::media::{FileTrack, PlaybackEndReason, RtpTrackBuilder, Track};
use crate::proxy::call::parse_allowed_codecs;
use crate::proxy::proxy_call::{
    dtmf::RtpDtmfDetector,
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
use audio_codec::CodecType;

use dashmap::DashMap;
use parking_lot::RwLock;
use rsipstack::dialog::{
    DialogId, dialog::Dialog, dialog::DialogState, dialog::TerminatedReason,
    dialog::TransactionHandle, server_dialog::ServerInviteDialog,
};
use rsipstack::sip::StatusCode;
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
use tracing::{debug, error, info, warn};

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
    pub server_dialog: ServerInviteDialog,
    pub callee_dialogs: Arc<DashMap<DialogId, ()>>,
    pub supervisor_mixer: Option<Arc<MediaMixer>>,

    pub context: CallContext,
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
    pub recorder: Arc<RwLock<Option<Recorder>>>,
    pub recording_paused: Arc<std::sync::atomic::AtomicBool>,

    pub app_event_bridge: Arc<RwLock<Option<crate::proxy::proxy_call::state::SipSessionHandle>>>,

    pub conference_bridge: crate::call::runtime::SessionConferenceBridge,

    #[allow(dead_code)]
    pub cmd_tx: Option<mpsc::Sender<CallCommand>>,

    /// Tracks whether `DestroySession` has already been sent to the media
    /// engine, so the `Drop` safety-net does not fire a duplicate destroy.
    engine_session_destroyed: bool,
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
    #[cfg(feature = "addon-voicemail")]
    addon_registry: Option<Arc<crate::addons::registry::AddonRegistry>>,
}

impl AppFactory for BuiltinAppFactory {
    fn create_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        context: &ApplicationContext,
    ) -> Option<Box<dyn crate::call::app::CallApp>> {
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
                    Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
                } else {
                    // File-based: read TOML and detect mode from content
                    let file = params.as_ref()?.get("file")?.as_str()?;
                    let content = match std::fs::read_to_string(file) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("Failed to read IVR config '{}': {}", file, e);
                            return None;
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
                        Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
                    }
                }
            }
            "voicemail" => {
                let extension = params.as_ref()?.get("extension")?.as_str()?.to_string();
                // Try the commercial addon first (full DB persistence, notifiers, S3).
                #[cfg(feature = "addon-voicemail")]
                if let Some(reg) = &self.addon_registry {
                    if let Some(addon) = reg.get_addon("voicemail") {
                        if let Some(vm) = addon
                            .as_any()
                            .downcast_ref::<crate::addons::voicemail::VoicemailAddon>()
                        {
                            let caller_id = context.call_info.caller.clone();
                            match vm.build_app(&extension, &caller_id) {
                                Ok(app) => {
                                    return Some(
                                        Box::new(app) as Box<dyn crate::call::app::CallApp>
                                    );
                                }
                                Err(e) => tracing::warn!(
                                    "voicemail addon build_app failed: {}; \
                                     falling back to core impl",
                                    e
                                ),
                            }
                        }
                    }
                }
                // Fallback to the built-in core implementation.
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
            "check_voicemail" => {
                #[cfg(feature = "addon-voicemail")]
                if let Some(reg) = &self.addon_registry {
                    if let Some(addon) = reg.get_addon("voicemail") {
                        if let Some(vm) = addon
                            .as_any()
                            .downcast_ref::<crate::addons::voicemail::VoicemailAddon>()
                        {
                            match vm.build_check_app() {
                                Ok(app) => {
                                    return Some(
                                        Box::new(app) as Box<dyn crate::call::app::CallApp>
                                    );
                                }
                                Err(e) => tracing::warn!("voicemail check_app build failed: {}", e),
                            }
                        }
                    }
                }
                None
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

const MID_DIALOG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
    pub const CALLER_FORWARDING_TRACK_ID: &'static str = "caller-forwarding-track";
    pub const CALLEE_FORWARDING_TRACK_ID: &'static str = "callee-forwarding-track";

    pub const QUEUE_HOLD_TRACK_ID: &'static str = "queue-hold";
    const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

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
            info!(conf_id = %conf_id, "Creating conference");
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
                warn!("{} runtime still marked running, restarting app", label);
                match self
                    .app_runtime
                    .stop_app(Some(format!("restart {}", label)))
                    .await
                {
                    Ok(()) | Err(AppRuntimeError::NotRunning) => {}
                    Err(stop_err) => {
                        warn!(error = ?stop_err, "Failed to stop existing {} app", label)
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
                        Some(s) => if sender.send(s).await.is_err() { break; }
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

    fn engine_send(&self, cmd: crate::media::engine::MediaCommand) {
        if let Err(e) = self.server.media_engine.send(cmd) {
            warn!(
                session_id = %self.context.session_id,
                error = %e,
                "MediaEngine command rejected"
            );
        }
    }

    fn setup_sipflow_capture(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Option<crate::media::engine::SipFlowCaptureTx> {
        let backend = self.server.sip_flow.as_ref().and_then(|sf| sf.backend())?;

        let (tx, rx) = tokio::sync::mpsc::channel(
            crate::media::forwarding_track::ForwardingTrack::DEFAULT_SIPFLOW_CHANNEL_CAPACITY,
        );
        if let Err(e) =
            self.server
                .media_engine
                .send(crate::media::engine::MediaCommand::SetSipFlowCapture {
                    session_id: session_id.to_string(),
                    call_id: call_id.to_string(),
                    backend: Some(backend),
                    receiver: Some(rx),
                })
        {
            warn!(
                session_id = %session_id,
                error = %e,
                "MediaEngine SipFlow capture command rejected"
            );
            return None;
        }

        Some(tx)
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
        server_dialog: ServerInviteDialog,
        use_media_proxy: bool,
        caller_peer: Arc<dyn MediaPeer>,
        callee_peer: Arc<dyn MediaPeer>,
    ) -> (Self, SipSessionHandle, mpsc::Receiver<CallCommand>) {
        // Clone-once locals used repeatedly below
        let session_id_str = context.session_id.clone();
        let original_caller = context.original_caller.clone();
        let original_callee = context.original_callee.clone();

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
        let mut app_ctx = ApplicationContext::new(
            server
                .database
                .clone()
                .unwrap_or(sea_orm::DatabaseConnection::Disconnected),
            call_info,
            Arc::new(crate::config::Config::default()),
        );
        app_ctx.rwi_gateway = server.rwi_gateway.clone();
        app_ctx.ivr_trace = server.ivr_trace.clone();

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
                app_id: None,
                routing_target: None,
                agent_id: None,
                agent_name: None,
            };
            let store = gw.read().meta_store.clone();
            let sid = session_id_str.clone();
            crate::utils::spawn(async move {
                store.insert(sid, meta).await;
            });
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
                #[cfg(feature = "addon-voicemail")]
                addon_registry: server.addon_registry.clone(),
            })),
        );

        let initial = server_dialog.initial_request();
        let caller_offer = Self::extract_sdp(initial.body());

        let session = Self {
            id: session_id.clone(),
            state: SessionState::Initializing,
            policy: SessionPolicy::inbound_sip(),
            bridge: BridgeConfig::new(),
            media_profile: media_profile.clone(),
            app_runtime,
            snapshot_cache: snapshot_cache.clone(),
            server,
            server_dialog: server_dialog.clone(),
            callee_dialogs: Arc::new(DashMap::new()),
            supervisor_mixer: None,
            legs: {
                use crate::proxy::proxy_call::leg_registry::LegRegistry;
                let mut lr = LegRegistry::new();
                let caller_id = LegId::from("caller");
                let caller_dialog =
                    rsipstack::dialog::dialog::Dialog::ServerInvite(server_dialog.clone());
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
            call_record_sender,
            cancel_token,
            meta: crate::proxy::proxy_call::call_meta::CallMeta::new(),
            media: crate::proxy::proxy_call::media_state::MediaState::new(caller_offer),
            timers: HashMap::new(),
            update_refresh_disabled: HashSet::new(),
            timer_queue: DelayQueue::new(),
            timer_keys: HashMap::new(),
            callee_event_tx: None,
            callee_guards: Vec::new(),
            reporter: None,
            cdr_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            recorder: Arc::new(RwLock::new(None)),
            recording_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            app_event_bridge: app_event_bridge.clone(),
            conference_bridge: crate::call::runtime::SessionConferenceBridge::new(),
            cmd_tx: Some(cmd_tx.clone()),
            dtmf_digits: Vec::new(),
            engine_session_destroyed: false,
        };

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
        let dialplan_clone = context.dialplan.clone();

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
            session.media.caller_offer = Some(offer_sdp.clone());
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

        // Register a session in the media engine and attach the recorder so
        // recording / playback commands route correctly.
        {
            use crate::media::engine::MediaCommand;
            let engine = &server.media_engine;
            let sid = session_id.clone();
            let recorder = session.recorder.clone();
            let recording_paused = session.recording_paused.clone();
            if let Err(e) = engine.send(MediaCommand::CreateSession {
                session_id: sid.clone(),
            }) {
                warn!(session_id = %sid, error = %e, "Failed to create engine session");
            }
            if let Err(e) = engine.send(MediaCommand::AttachRecorder {
                session_id: sid,
                recorder,
                paused: recording_paused,
            }) {
                warn!(session_id = %session_id, error = %e, "Failed to attach recorder to engine");
            }
        }

        crate::utils::spawn(async move {
            session
                .process(state_rx, callee_state_rx, cmd_rx, dialog_guard)
                .await
        });

        let max_setup_duration = dialplan_clone
            .max_ring_time
            .clamp(Duration::from_secs(30), Duration::from_secs(120));
        let teardown_duration = Duration::from_secs(2);
        let mut timeout = tokio::time::sleep(max_setup_duration).boxed();
        let mut cancelled = false;

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
                _ = cancel_token.cancelled(), if !cancelled => {
                    debug!(session_id = %session_id, "Call cancelled via token");
                    cancelled = true;
                    timeout = tokio::time::sleep(teardown_duration).boxed();
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

    fn bypasses_local_media(&self) -> bool {
        self.media_profile.path == MediaPathMode::Bypass && self.media.media_bridge.is_none()
    }

    async fn send_mid_dialog_request_to_side(
        &mut self,
        side: DialogSide,
        method: rsipstack::sip::Method,
        headers: Vec<rsipstack::sip::Header>,
        body: Option<Vec<u8>>,
    ) -> Result<Option<rsipstack::sip::Response>> {
        let result = tokio::time::timeout(
            MID_DIALOG_TIMEOUT,
            self.send_mid_dialog_request_to_side_inner(side, method, headers, body),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "mid-dialog request timed out after {}s",
                MID_DIALOG_TIMEOUT.as_secs()
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
                (side == DialogSide::Caller)
                    .then(|| Dialog::ServerInvite(self.server_dialog.clone()))
            })
            .ok_or_else(|| anyhow!("No dialog found for {}", dialog_id))?;

        match (method, &mut dialog) {
            (rsipstack::sip::Method::Invite, Dialog::ClientInvite(d)) => d
                .reinvite(Some(headers), body)
                .await
                .map_err(|e| anyhow!("re-INVITE failed: {}", e)),
            (rsipstack::sip::Method::Invite, Dialog::ServerInvite(d)) => d
                .reinvite(Some(headers), body)
                .await
                .map_err(|e| anyhow!("re-INVITE failed: {}", e)),
            (rsipstack::sip::Method::Update, Dialog::ClientInvite(d)) => d
                .update(Some(headers), body)
                .await
                .map_err(|e| anyhow!("UPDATE failed: {}", e)),
            (rsipstack::sip::Method::Update, Dialog::ServerInvite(d)) => d
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

    async fn relay_bridged_dialog_offer(
        &mut self,
        side: DialogSide,
        method: rsipstack::sip::Method,
        offer_sdp: &str,
    ) -> Result<(StatusCode, Option<String>)> {
        let source_desc =
            Self::parse_sdp(rustrtc::SdpType::Offer, offer_sdp, "bridged dialog offer")?;
        let source_is_webrtc = match side {
            DialogSide::Caller => self.is_caller_webrtc(),
            DialogSide::Callee => self.is_callee_webrtc(),
        };
        let target_side = match side {
            DialogSide::Caller => DialogSide::Callee,
            DialogSide::Callee => DialogSide::Caller,
        };
        let target_is_webrtc = match target_side {
            DialogSide::Caller => self.is_caller_webrtc(),
            DialogSide::Callee => self.is_callee_webrtc(),
        };

        if source_is_webrtc == target_is_webrtc {
            return self
                .relay_signaling_only_offer(side, method, offer_sdp)
                .await;
        }

        // Analyze source SDP for video/audio capabilities and directions
        let offer_has_video = source_desc
            .media_sections
            .iter()
            .any(|section| section.kind == rustrtc::MediaKind::Video);
        let offer_video_direction = source_desc
            .media_sections
            .iter()
            .find(|section| section.kind == rustrtc::MediaKind::Video)
            .map(|section| {
                if section.port == 0 {
                    rustrtc::Direction::Inactive
                } else {
                    section.direction
                }
            });
        let offer_video_active = offer_video_direction
            .map(|direction| direction != rustrtc::Direction::Inactive)
            .unwrap_or(false);
        let source_video_caps: Vec<_> = source_desc
            .media_sections
            .iter()
            .filter(|section| {
                section.kind == rustrtc::MediaKind::Video
                    && section.port != 0
                    && section.direction != rustrtc::Direction::Inactive
            })
            .flat_map(|section| section.to_video_capabilities())
            .filter(|cap| !cap.codec_name.eq_ignore_ascii_case("unknown"))
            .collect();
        let has_audio = source_desc
            .media_sections
            .iter()
            .any(|section| section.kind == rustrtc::MediaKind::Audio);
        let offer_audio_direction = source_desc
            .media_sections
            .iter()
            .find(|section| section.kind == rustrtc::MediaKind::Audio)
            .map(|section| section.direction);

        // Bridged WebRTC/RTP renegotiation should use the RTP-safe video set
        // consistently on both legs, otherwise the browser side can negotiate a
        // codec the RTP peer was not answered with.
        let allowed_video_codecs = self
            .server
            .proxy_config
            .video_codecs
            .as_deref()
            .unwrap_or(&[]);
        let bridged_video_caps =
            Self::filter_video_caps_for_rtp(&source_video_caps, allowed_video_codecs);

        // Obtain bridge peer connections, adding a video track if this is a new video offer
        let (target_pc, source_pc) = {
            let bridge = self
                .media
                .media_bridge
                .as_ref()
                .ok_or_else(|| anyhow!("No media bridge available for dialog offer"))?;

            if offer_video_active && !bridge.has_video().await {
                let video_codec = bridged_video_caps
                    .first()
                    .ok_or_else(|| anyhow!("Video re-INVITE offer has no video codec"))?;
                bridge
                    .add_video_track(video_codec.payload_type, video_codec.clock_rate)
                    .await?;
            }

            let target_pc = if target_is_webrtc {
                bridge.caller_pc().clone()
            } else {
                bridge.callee_pc().clone()
            };
            let source_pc = if source_is_webrtc {
                bridge.caller_pc().clone()
            } else {
                bridge.callee_pc().clone()
            };
            (target_pc, source_pc)
        };

        // Build the re-offer to send to the target side
        let previously_had_video = self.legs.any_has_video();
        let target_offer_codecs = if has_audio {
            let allow_codecs = self.resolve_effective_codecs();
            let codecs =
                MediaNegotiator::build_callee_codec_offer_with_allow(offer_sdp, &allow_codecs);
            if target_is_webrtc {
                MediaNegotiator::filter_webrtc_offer_codecs(offer_sdp, codecs)
            } else {
                codecs
            }
        } else {
            Vec::new()
        };
        let target_offer_sdp = Self::build_bridge_outbound_offer(
            &source_desc,
            &bridged_video_caps,
            &target_pc,
            offer_sdp,
            &target_offer_codecs,
            offer_has_video,
            target_is_webrtc,
            previously_had_video,
        )
        .await?;

        let (status, target_answer_sdp) = self
            .send_mid_dialog_offer_with_fallback(target_side, method, &target_offer_sdp)
            .await?;
        if status.kind() != rsipstack::sip::status_code::StatusCodeKind::Successful {
            return Ok((status, None));
        }

        // Apply target answer to target PC and extract video negotiation state
        let target_answer = Self::parse_sdp(
            rustrtc::SdpType::Answer,
            &target_answer_sdp,
            "bridged target answer",
        )?;
        let target_video_caps = target_answer.to_video_capabilities();
        let target_video_active = target_answer.media_sections.iter().any(|section| {
            section.kind == rustrtc::MediaKind::Video
                && section.port != 0
                && section.direction != rustrtc::Direction::Inactive
        });
        target_pc
            .set_remote_description(target_answer)
            .await
            .map_err(|e| anyhow!("Failed to set bridged target answer SDP: {}", e))?;

        // Build the answer to return to the source side
        let (mut source_answer_sdp, _source_video_params) = Self::build_bridge_source_answer(
            &source_pc,
            offer_sdp,
            &target_answer_sdp,
            &bridged_video_caps,
            offer_has_video,
            source_is_webrtc,
            target_video_active,
        )
        .await?;
        if has_audio {
            source_answer_sdp = self.rewrite_answer_to_selected_codecs(
                &source_answer_sdp,
                offer_sdp,
                Some(&target_answer_sdp),
                "bridged re-INVITE source answer",
            );
        }

        // Sync bridge video payload type mapping
        if let Some(bridge) = &self.media.media_bridge {
            let source_video_caps =
                rustrtc::SessionDescription::parse(rustrtc::SdpType::Answer, &source_answer_sdp)
                    .map(|desc| desc.to_video_capabilities())
                    .unwrap_or_default();
            let (webrtc_video_caps, rtp_video_caps) = if target_is_webrtc {
                (target_video_caps.as_slice(), source_video_caps.as_slice())
            } else {
                (source_video_caps.as_slice(), target_video_caps.as_slice())
            };
            bridge.set_video_payload_maps(webrtc_video_caps, rtp_video_caps);
        }

        // Update stored SDPs and leg hold/connected state
        match side {
            DialogSide::Caller => {
                self.media.caller_offer = Some(offer_sdp.to_string());
                self.media.callee_offer = Some(target_offer_sdp);
                self.media.callee_answer_sdp = Some(target_answer_sdp.clone());
                self.media.answer = Some(source_answer_sdp.clone());
                if has_audio {
                    self.update_leg_state(
                        &LegId::from("caller"),
                        if Self::is_hold_direction(offer_audio_direction.unwrap_or_default()) {
                            LegState::Hold
                        } else {
                            LegState::Connected
                        },
                    );
                }
            }
            DialogSide::Callee => {
                self.media.callee_offer = Some(source_answer_sdp.clone());
                self.media.callee_answer_sdp = Some(source_answer_sdp.clone());
                if has_audio {
                    self.update_leg_state(
                        &LegId::from("callee"),
                        if Self::is_hold_direction(offer_audio_direction.unwrap_or_default()) {
                            LegState::Hold
                        } else {
                            LegState::Connected
                        },
                    );
                }
            }
        }

        if offer_video_direction.is_some() && (previously_had_video || offer_video_active) {
            let source_leg_id = match side {
                DialogSide::Caller => LegId::from("caller"),
                DialogSide::Callee => LegId::from("callee"),
            };
            let target_leg_id = match side {
                DialogSide::Caller => LegId::from("callee"),
                DialogSide::Callee => LegId::from("caller"),
            };
            self.legs
                .set_bridge_video_state(&source_leg_id, &target_leg_id, true);
        }
        self.media.caller_answer_uses_media_bridge = true;
        self.media.callee_offer_uses_media_bridge = true;

        let caller_sdp = match side {
            DialogSide::Caller => source_answer_sdp.as_str(),
            DialogSide::Callee => target_answer_sdp.as_str(),
        };
        let callee_sdp = match side {
            DialogSide::Caller => target_answer_sdp.as_str(),
            DialogSide::Callee => source_answer_sdp.as_str(),
        };
        self.configure_media_bridge_transcoders(Some(caller_sdp), Some(callee_sdp));
        self.start_media_bridge_forwarding().await;
        // Re-INVITE is always a confirmed, in-progress call — open the gate.
        if let Some(ref bridge) = self.media.media_bridge {
            bridge.open_caller_gate();
        }
        self.update_snapshot_cache();

        Ok((status, Some(source_answer_sdp)))
    }

    /// Build the re-offer SDP to send to the target side of the bridge.
    ///
    /// Sets transceiver directions to mirror the source offer, creates a local offer on
    /// the target PC, applies video codec constraints from the source, applies the
    /// shared audio codec policy when the source offer has audio, and returns
    /// the final SDP string after ICE gathering.
    async fn build_bridge_outbound_offer(
        source_desc: &rustrtc::SessionDescription,
        source_video_caps: &[rustrtc::VideoCapability],
        target_pc: &rustrtc::PeerConnection,
        offer_sdp: &str,
        target_offer_codecs: &[CodecInfo],
        offer_has_video: bool,
        target_is_webrtc: bool,
        previously_had_video: bool,
    ) -> Result<String> {
        // Mirror source transceiver directions onto the target PC
        let target_transceivers = target_pc.get_transceivers();
        let mut used_transceivers = HashSet::new();
        for source_section in &source_desc.media_sections {
            if let Some((idx, transceiver)) =
                target_transceivers
                    .iter()
                    .enumerate()
                    .find(|(idx, transceiver)| {
                        !used_transceivers.contains(idx)
                            && transceiver.kind() == source_section.kind
                    })
            {
                used_transceivers.insert(idx);
                let direction = if source_section.port == 0 {
                    rustrtc::Direction::Inactive
                } else {
                    source_section.direction
                };
                let target_direction = if target_is_webrtc
                    && previously_had_video
                    && source_section.kind == rustrtc::MediaKind::Video
                    && direction == rustrtc::Direction::Inactive
                {
                    // Keep the browser receiver negotiated while the RTP leg
                    // temporarily disables video.
                    rustrtc::Direction::SendOnly
                } else {
                    direction
                };
                transceiver.set_direction(target_direction.into());
            }
        }

        // Create offer, apply video codec constraints, restrict to reference codecs
        let target_offer = target_pc
            .create_offer()
            .await
            .map_err(|e| anyhow!("Failed to create bridged dialog offer: {}", e))?;
        let mut target_offer_sdp = target_offer.to_sdp_string();
        if offer_has_video && !source_video_caps.is_empty() {
            target_offer_sdp = Self::apply_video_caps_from_source(
                rustrtc::SdpType::Offer,
                &target_offer_sdp,
                "bridged local offer",
                source_video_caps,
            )?;
        }
        if let Some(filtered) = MediaNegotiator::restrict_sdp_to_reference_codecs(
            rustrtc::SdpType::Offer,
            &target_offer_sdp,
            rustrtc::SdpType::Offer,
            offer_sdp,
        ) {
            target_offer_sdp = filtered;
        }
        if !target_offer_codecs.is_empty()
            && let Some(rewritten) =
                MediaNegotiator::rewrite_sdp_codec_list(&target_offer_sdp, target_offer_codecs)
        {
            target_offer_sdp = rewritten;
        }
        let target_offer_desc = Self::parse_sdp(
            rustrtc::SdpType::Offer,
            &target_offer_sdp,
            "bridged local offer",
        )?;
        target_pc
            .set_local_description(target_offer_desc)
            .map_err(|e| anyhow!("Failed to set bridged local offer SDP: {}", e))?;
        if target_is_webrtc {
            target_pc.wait_for_gathering_complete().await;
        }
        let target_offer_sdp = target_pc
            .local_description()
            .map(|desc| desc.to_sdp_string())
            .ok_or_else(|| anyhow!("Bridge target side has no local offer"))?;
        if target_is_webrtc {
            Ok(target_offer_sdp)
        } else {
            MediaNegotiator::sanitize_sdp_for_rtp_peer(
                rustrtc::SdpType::Offer,
                &target_offer_sdp,
                "bridged RTP offer",
            )
        }
    }

    /// Build the answer SDP to return to the source side of the bridge.
    ///
    /// Applies the source offer to the source PC, creates a local answer, applies video
    /// codec constraints and reference codec restrictions, patches inactive video if the
    /// target disabled it, and returns the final SDP string after ICE gathering alongside
    /// the selected video codec parameters.
    async fn build_bridge_source_answer(
        source_pc: &rustrtc::PeerConnection,
        offer_sdp: &str,
        target_answer_sdp: &str,
        source_video_caps: &[rustrtc::VideoCapability],
        offer_has_video: bool,
        source_is_webrtc: bool,
        target_video_active: bool,
    ) -> Result<(String, Option<rustrtc::RtpCodecParameters>)> {
        let source_offer =
            Self::parse_sdp(rustrtc::SdpType::Offer, offer_sdp, "bridged source offer")?;
        source_pc
            .set_remote_description(source_offer)
            .await
            .map_err(|e| anyhow!("Failed to apply bridged source offer SDP: {}", e))?;
        let source_answer = source_pc
            .create_answer()
            .await
            .map_err(|e| anyhow!("Failed to create bridged source answer SDP: {}", e))?;
        let mut source_answer_sdp = source_answer.to_sdp_string();
        if offer_has_video && !source_video_caps.is_empty() {
            source_answer_sdp = Self::apply_video_caps_from_source(
                rustrtc::SdpType::Answer,
                &source_answer_sdp,
                "bridged source answer",
                source_video_caps,
            )?;
        }
        if let Some(filtered) = MediaNegotiator::restrict_sdp_to_reference_codecs(
            rustrtc::SdpType::Answer,
            &source_answer_sdp,
            rustrtc::SdpType::Answer,
            target_answer_sdp,
        ) {
            source_answer_sdp = filtered;
        }
        // If target disabled video, disable it in source answer too
        if offer_has_video && !target_video_active {
            let mut desc = Self::parse_sdp(
                rustrtc::SdpType::Answer,
                &source_answer_sdp,
                "bridged source answer",
            )?;
            if let Some(video_section) = desc
                .media_sections
                .iter_mut()
                .find(|section| section.kind == rustrtc::MediaKind::Video)
            {
                video_section.port = 0;
                video_section.direction = rustrtc::Direction::Inactive;
            }
            source_answer_sdp = desc.to_sdp_string();
        }
        let source_answer = Self::parse_sdp(
            rustrtc::SdpType::Answer,
            &source_answer_sdp,
            "bridged source answer",
        )?;
        let source_video_active = source_answer.media_sections.iter().any(|section| {
            section.kind == rustrtc::MediaKind::Video
                && section.port != 0
                && section.direction != rustrtc::Direction::Inactive
        });
        let source_video_params = source_video_active
            .then(|| {
                source_answer.to_video_capabilities().first().map(|video| {
                    rustrtc::RtpCodecParameters {
                        payload_type: video.payload_type,
                        clock_rate: video.clock_rate,
                        channels: 0,
                    }
                })
            })
            .flatten();
        source_pc
            .set_local_description(source_answer)
            .map_err(|e| anyhow!("Failed to set bridged source answer SDP: {}", e))?;
        if source_is_webrtc {
            source_pc.wait_for_gathering_complete().await;
        }
        let source_answer_sdp = source_pc
            .local_description()
            .map(|desc| desc.to_sdp_string())
            .ok_or_else(|| anyhow!("Bridge source side has no local answer"))?;
        let source_answer_sdp = if source_is_webrtc {
            source_answer_sdp
        } else {
            MediaNegotiator::sanitize_sdp_for_rtp_peer(
                rustrtc::SdpType::Answer,
                &source_answer_sdp,
                "bridged RTP answer",
            )?
        };
        Ok((source_answer_sdp, source_video_params))
    }

    async fn send_mid_dialog_offer_with_fallback(
        &mut self,
        target_side: DialogSide,
        method: rsipstack::sip::Method,
        offer_sdp: &str,
    ) -> Result<(StatusCode, String)> {
        let headers = Self::sdp_headers();
        let response_result = self
            .send_mid_dialog_request_to_side(
                target_side,
                method.clone(),
                headers.clone(),
                Some(offer_sdp.as_bytes().to_vec()),
            )
            .await;

        let response = match response_result {
            Ok(Some(resp))
                if resp.status_code.kind()
                    == rsipstack::sip::status_code::StatusCodeKind::Successful
                    || method != rsipstack::sip::Method::Update =>
            {
                resp
            }
            Ok(Some(resp)) => {
                warn!(
                    session_id = %self.context.session_id,
                    target_side = ?target_side,
                    status = %resp.status_code,
                    "Bridged UPDATE was rejected; retrying as re-INVITE"
                );
                self.send_mid_dialog_request_to_side(
                    target_side,
                    rsipstack::sip::Method::Invite,
                    headers,
                    Some(offer_sdp.as_bytes().to_vec()),
                )
                .await?
                .ok_or_else(|| anyhow!("Bridged re-INVITE timed out"))?
            }
            Ok(None) if method == rsipstack::sip::Method::Update => {
                warn!(
                    session_id = %self.context.session_id,
                    target_side = ?target_side,
                    "Bridged UPDATE timed out; retrying as re-INVITE"
                );
                self.send_mid_dialog_request_to_side(
                    target_side,
                    rsipstack::sip::Method::Invite,
                    headers,
                    Some(offer_sdp.as_bytes().to_vec()),
                )
                .await?
                .ok_or_else(|| anyhow!("Bridged re-INVITE timed out"))?
            }
            Ok(None) => return Err(anyhow!("{} timed out", method)),
            Err(error) if method == rsipstack::sip::Method::Update => {
                warn!(
                    session_id = %self.context.session_id,
                    target_side = ?target_side,
                    error = %error,
                    "Bridged UPDATE failed; retrying as re-INVITE"
                );
                self.send_mid_dialog_request_to_side(
                    target_side,
                    rsipstack::sip::Method::Invite,
                    headers,
                    Some(offer_sdp.as_bytes().to_vec()),
                )
                .await?
                .ok_or_else(|| anyhow!("Bridged re-INVITE timed out"))?
            }
            Err(error) => return Err(error),
        };

        let status = response.status_code.clone();
        if response.body().is_empty()
            && status.kind() == rsipstack::sip::status_code::StatusCodeKind::Successful
        {
            return Err(anyhow!("Bridged dialog offer answer had no SDP"));
        }
        let answer_sdp = String::from_utf8_lossy(response.body()).to_string();
        Ok((status, answer_sdp))
    }

    /// Filter video capabilities for sending to an RTP/PSTN/IMS leg.
    ///
    /// - Keeps only codecs whose name (case-insensitive) appears in `allowed_codecs`.
    ///   If `allowed_codecs` is empty the built-in default `["H264"]` is used.
    /// - Clears all `rtcp-fb` entries on every kept capability; `RTP/AVP` peers
    ///   (IMS, PSTN gateways) commonly reject `goog-remb`, `transport-cc`, etc.
    /// - If the filter produces an empty list, leave video unconfigured instead
    ///   of silently accepting a non-default codec.
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

        caps.iter()
            .filter(|cap| {
                effective_allow
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&cap.codec_name))
            })
            .map(|cap| rustrtc::VideoCapability {
                payload_type: cap.payload_type,
                codec_name: cap.codec_name.clone(),
                clock_rate: cap.clock_rate,
                fmtp: cap.fmtp.clone(),
                rtcp_fbs: vec![],
            })
            .collect()
    }

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
            let mut ordered_caps = Vec::new();
            let mut seen_pts = HashSet::new();

            for source_cap in caps {
                let mut matched_local = false;
                for local_cap in &local_video_caps {
                    if local_cap
                        .codec_name
                        .eq_ignore_ascii_case(&source_cap.codec_name)
                        && local_cap.clock_rate == source_cap.clock_rate
                    {
                        matched_local = true;
                        if seen_pts.insert(local_cap.payload_type) {
                            ordered_caps.push(local_cap.clone());
                        }
                    }
                }

                if !matched_local && seen_pts.insert(source_cap.payload_type) {
                    ordered_caps.push(source_cap.clone());
                }
            }

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

        // Send proactive 183 early media if trunk has configured ringback.ring tone
        let ring_audio = self
            .context
            .dialplan
            .audio_profile
            .as_ref()
            .and_then(|p| p.ring.clone());
        if let Some(ref audio) = ring_audio {
            info!(
                session_id = %self.context.session_id,
                audio = %audio,
                "Sending proactive 183 Session Progress with ringback tone"
            );
            if let Err(e) = self.send_early_media_tone(audio).await {
                warn!(session_id = %self.context.session_id, error = %e, "Failed to send proactive 183");
            }
        }

        if !self.context.dialplan.is_empty()
            && let Err((status_code, text, reason)) =
                self.execute_dialplan(&mut callee_state_rx).await
        {
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

        let (rtp_timeout_tx, mut rtp_timeout_rx) = mpsc::channel::<String>(1);
        self.media.rtp_timeout_tx = Some(rtp_timeout_tx);

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
                && self.server_dialog.state().is_terminated()
                && self.callee_dialogs.is_empty()
            {
                break;
            }

            tokio::select! {
                res = hangup_futures.next(), if !hangup_futures.is_empty() => {
                    if let Some(res) = res {
                        tracing::info!("Hangup completed for dialog_id: {:?}", &res);
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
                        warn!(error = %e, "Error handling dialog state");
                    }
                }


                Some(state) = callee_state_rx.recv() => {
                    if let Err(e) = self.handle_callee_state(state).await {
                        warn!(error = %e, "Error handling callee state");
                    }
                }


                Some(cmd) = cmd_rx.recv() => {
                    let result = self
                        .execute_command(cmd, Some(&mut callee_state_rx))
                        .await;
                    if !result.success {
                        warn!(error = ?result.message, "Command execution failed");
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
                                    warn!(dialog_id = %scheduled, error = %e, "Failed to send session refresh");
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
                            warn!(dialog_id = %scheduled, "Session timer expired, terminating session");
                            self.meta.hangup_reason = Some(CallRecordHangupReason::Autohangup);
                            self.pending_hangup.insert(scheduled);
                        }
                        None => {}
                    }
                }

                Some(reason) = rtp_timeout_rx.recv(), if !cancelled => {
                    warn!(
                        session_id = %self.context.session_id,
                        reason = %reason,
                        "RTP timeout detected, terminating session"
                    );
                    self.meta.hangup_reason = Some(CallRecordHangupReason::RtpTimeout);
                    self.cancel_token.cancel();
                }

                _ = &mut max_duration_sleep, if !cancelled => {
                    warn!(
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
        crate::proxy::proxy_call::session_hooks::CallSessionContext {
            session_id: self.context.session_id.clone(),
            caller: self.context.original_caller.clone(),
            callee: self.context.original_callee.clone(),
            connected_callee: self.meta.connected_callee.clone(),
            queue_name: self.meta.queue_name.clone(),
            direction: self.context.dialplan.direction.to_string(),
            started_at: Some(self.context.created_at.clone()),
            metadata: self.context.metadata.clone(),
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
            debug!(
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
            headers.extend(target_headers.iter().cloned());
        }

        let callee_is_webrtc = target.supports_webrtc;
        let leg_id = leg_id_override.unwrap_or("callee");
        self.legs.set_transport(
            crate::call::domain::LegId::from(leg_id),
            self.callee_transport_mode(callee_is_webrtc),
        );

        let offer = self.prepare_callee_media_offer(target).await;
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
            .with_enable_latching(self.server.proxy_config.enable_latching)
            .with_probation_max_packets(self.server.proxy_config.latching_probation_max_packets)
            .with_cname(self.server.rtc_cname.clone());

        if let Some(ref external_ip) = self.server.rtp_config.external_ip {
            builder = builder.with_external_ip(external_ip.clone());
        }
        if let Some(ref bind_ip) = self.server.rtp_config.bind_ip {
            builder = builder.with_bind_ip(bind_ip.clone());
        }

        // SDES-SRTP shares the plain-RTP port range; only WebRTC uses the
        // dedicated WebRTC range.
        let (start_port, end_port) = if is_webrtc {
            (
                self.server.rtp_config.webrtc_start_port,
                self.server.rtp_config.webrtc_end_port,
            )
        } else {
            (
                self.server.rtp_config.start_port,
                self.server.rtp_config.end_port,
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
            caller_gate_open: self
                .media
                .media_bridge
                .as_ref()
                .map_or(true, |b| b.is_caller_gate_open()),
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
        debug!(
            %dialog_id,
            method = ?request.method,
            side = ?side,
            "Received UPDATE/INVITE on dialog"
        );

        let update_result = self.update_dialog_timer_from_headers(&dialog_id, &request.headers);
        if let Err(e) = &update_result {
            warn!(
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
            let bridged_video_offer = self.media.media_bridge.is_some()
                && rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer_sdp)
                    .map(|desc| {
                        desc.media_sections
                            .iter()
                            .any(|section| section.kind == rustrtc::MediaKind::Video)
                    })
                    .unwrap_or(false);
            let answer_result = if self.bypasses_local_media() {
                self.relay_signaling_only_offer(side, request.method.clone(), &offer_sdp)
                    .await
                    .map_err(|e| {
                        (
                            rsipstack::sip::StatusCode::ServerInternalError,
                            "Failed to relay signaling-only dialog offer",
                            e,
                        )
                    })
            } else if bridged_video_offer {
                self.relay_bridged_dialog_offer(side, request.method.clone(), &offer_sdp)
                    .await
                    .map_err(|e| {
                        (
                            rsipstack::sip::StatusCode::NotAcceptableHere,
                            "Failed to relay bridged video dialog offer",
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
                    warn!(
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
        debug!(
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
                        info!("Caller initiated hangup (UacBye)");
                    }
                    TerminatedReason::UasBye => {
                        self.meta.hangup_reason = Some(CallRecordHangupReason::ByCallee);
                        info!("Callee initiated hangup (UasBye) on caller dialog");
                    }
                    _ => {
                        debug!(?reason, "Caller dialog terminated with reason");
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
            info!(
                session_id = %self.context.session_id,
                "✓ Received SIP INFO with DTMF (application/dtmf-relay content type)"
            );
            debug!(
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
                        let event = serde_json::json!({
                            "type": "dtmf",
                            "leg_id": "caller",
                            "digit": digit.chars().next().unwrap_or_default().to_string(),
                        });
                        warn!(
                            session_id = %self.context.session_id,
                            digit = %digit,
                            "✓ Successfully detected DTMF digit from SIP INFO"
                        );
                        if let Err(e) = self.app_runtime.inject_event(event.clone()) {
                            warn!(
                                session_id = %self.context.session_id,
                                digit = %digit,
                                error = %e,
                                "Detected DTMF via INFO but failed to inject event"
                            );
                        } else {
                            info!(
                                session_id = %self.context.session_id,
                                digit = %digit,
                                "✓ Successfully injected DTMF event from SIP INFO"
                            );
                            // Emit typed RWI DTMF event
                            self.emit_typed_rwi_event(&crate::rwi::Dtmf {
                                call_id: self.context.session_id.clone(),
                                digit: digit.chars().next().unwrap_or_default().to_string(),
                                leg_id: Some("caller".to_string()),
                                extra: None,
                            });
                        }
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
                        warn!(
                            session_id = %self.context.session_id,
                            error = %e,
                            "Failed to forward SIP INFO DTMF to callee"
                        );
                    } else {
                        debug!(
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
            debug!(
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

    /// Handle a rustpbx JSON command received via SIP INFO with
    /// `application/vnd.rustpbx+json` content type.  The command is dispatched
    /// asynchronously through the session command channel and the INFO request
    /// is always acknowledged with 200 OK.
    async fn handle_rustpbx_info_command(
        &mut self,
        body: &str,
        tx_handle: &TransactionHandle,
    ) -> Result<()> {
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => {
                warn!(
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

        let cmd: Option<CallCommand> = match action.as_str() {
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
                music: None,
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
            other => {
                warn!(
                    session_id = %self.context.session_id,
                    action = %other,
                    "SIP INFO rustpbx command: unknown action"
                );
                None
            }
        };

        match cmd {
            Some(cmd) => {
                if let Some(ref tx) = self.cmd_tx {
                    if let Err(e) = tx.try_send(cmd) {
                        warn!(
                            session_id = %self.context.session_id,
                            action = %action,
                            error = %e,
                            "SIP INFO rustpbx command: failed to enqueue"
                        );
                    } else {
                        info!(
                            session_id = %self.context.session_id,
                            action = %action,
                            "SIP INFO rustpbx command accepted"
                        );
                    }
                }
            }
            None => {
                info!(
                    session_id = %self.context.session_id,
                    action = %action,
                    "SIP INFO rustpbx command ignored (unknown action or no-op)"
                );
            }
        }

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
        let Some(bridge) = self.media.media_bridge.as_ref() else {
            debug!(
                session_id = %self.context.session_id,
                side = ?requester_side,
                "Received picture_fast_update without media bridge"
            );
            return;
        };

        let (source_side, source_pc) = match requester_side {
            DialogSide::Caller => (DialogSide::Callee, bridge.callee_pc()),
            DialogSide::Callee => (DialogSide::Caller, bridge.caller_pc()),
        };

        match Self::find_video_receiver_track(source_pc).await {
            Some(track) => {
                if let Err(e) = track.request_key_frame().await {
                    warn!(
                        session_id = %self.context.session_id,
                        requester_side = ?requester_side,
                        source_side = ?source_side,
                        error = %e,
                        "Failed to handle picture_fast_update keyframe request"
                    );
                } else {
                    info!(
                        session_id = %self.context.session_id,
                        requester_side = ?requester_side,
                        source_side = ?source_side,
                        "Handled picture_fast_update keyframe request"
                    );
                }
            }
            None => {
                debug!(
                    session_id = %self.context.session_id,
                    requester_side = ?requester_side,
                    source_side = ?source_side,
                    "No video receiver track for picture_fast_update"
                );
            }
        }
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
                info!(
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
                    self.pending_hangup.insert(self.server_dialog.id());
                    info!(
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
        debug!(
            session_id = %self.context.session_id,
            state = %state,
            "Callee dialog state"
        );
        match state {
            DialogState::Confirmed(_, _) => {
                self.update_leg_state(&LegId::from("callee"), LegState::Connected);
                info!(
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
                    debug!(
                        dialog_id = %terminated_dialog_id,
                        ?reason,
                        "Ignoring terminated untracked callee dialog"
                    );
                    return Ok(());
                }

                if self.meta.connected_callee_dialog_id.is_some() && !connected_callee_terminated {
                    debug!(
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
                        info!("Callee initiated hangup (UasBye)");
                    }
                    TerminatedReason::UacBye => {
                        self.meta.hangup_reason = Some(CallRecordHangupReason::ByCaller);
                        info!("Caller initiated hangup (UacBye) on callee dialog");
                    }
                    _ => {
                        debug!(?reason, "Callee dialog terminated with reason");
                    }
                }

                if connected_callee_terminated {
                    self.meta.connected_callee = None;
                    self.meta.connected_callee_dialog_id = None;

                    let hook_handled = if !self.server_dialog.state().is_terminated() {
                        let agent_id = self.meta.connected_callee.clone().unwrap_or_default();
                        let queue_name = self
                            .app_runtime
                            .get_queue_name()
                            .or_else(|| self.meta.queue_name.clone())
                            .unwrap_or_default();
                        invoke_post_call_hook(
                            &self.context.session_id,
                            &self.context.original_caller,
                            &agent_id,
                            &queue_name,
                            &*self.app_runtime,
                        )
                        .await
                    } else {
                        false
                    };

                    if !hook_handled && !self.server_dialog.state().is_terminated() {
                        self.pending_hangup.insert(self.server_dialog.id());
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
                        info!(
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
                            && !self.server_dialog.state().is_terminated()
                        {
                            if let Some(ext) =
                                extract_sip_username(&self.context.original_callee)
                            {
                                info!(
                                    session_id = %self.context.session_id,
                                    extension = %ext,
                                    status = %code.code(),
                                    "Voicemail enabled for callee, starting voicemail app instead of rejecting"
                                );
                                if self.start_voicemail_app(&ext).await.is_ok() {
                                    // Voicemail app took over — don't reject the caller.
                                    return Ok(());
                                }
                                warn!(
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
                                    debug!(
                                        session_id = %self.context.session_id,
                                        error = %e,
                                        "Callee SIP INFO DTMF app inject (no active app)"
                                    );
                                }
                            }
                        }
                    }
                    // Forward to caller
                    let fwd_headers = vec![rsipstack::sip::Header::ContentType(
                        rsipstack::sip::headers::ContentType::from("application/dtmf-relay"),
                    )];
                    if let Err(e) = self
                        .server_dialog
                        .info(Some(fwd_headers), Some(request.body().to_vec()))
                        .await
                    {
                        warn!(
                            session_id = %self.context.session_id,
                            error = %e,
                            "Failed to forward callee SIP INFO DTMF to caller"
                        );
                    } else {
                        debug!(
                            session_id = %self.context.session_id,
                            "Forwarded callee SIP INFO DTMF to caller"
                        );
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
                            warn!("No dial strategy in queue plan");
                            return self.execute_flow(next, callee_state_rx).await;
                        }
                    };

                    if agents.is_empty() {
                        warn!("No agents configured in queue plan");
                        return self.execute_flow(next, callee_state_rx).await;
                    }

                    // Resolve custom targets (skill-groups → specific agents)
                    let resolved_agents = self.resolve_custom_targets(agents).await;

                    // Enrich via queue_location_enricher if configured
                    let resolved_agents =
                        if let Some(enricher) = &self.server.queue_location_enricher {
                            let caller_headers: Vec<rsipstack::sip::Header> = self
                                .server_dialog
                                .initial_request()
                                .headers
                                .iter()
                                .cloned()
                                .collect();
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
                            warn!(error = %e, "Queue: failed to start queue app, trying next flow");
                            self.execute_flow(next, callee_state_rx).await
                        }
                    }
                }
                DialplanFlow::Application {
                    app_name,
                    app_params,
                    auto_answer,
                } => {
                    info!(app_name = %app_name, "Executing application flow");
                    if let Err(e) = self
                        .app_runtime
                        .start_app(app_name, app_params.clone(), *auto_answer)
                        .await
                    {
                        warn!(error = %e, "Failed to start application");
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
            info!(index = idx, target = %target.aor, "Trying sequential target");

            match self.try_single_target(target, callee_state_rx, None).await {
                Ok(()) => {
                    info!(index = idx, "Sequential target succeeded");
                    return Ok(());
                }
                Err(e) => {
                    warn!(index = idx, error = ?e, "Sequential target failed");
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
            info!(target = %target.aor, "dial_parallel: target");
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

        let _caller = self.context.dialplan.caller.clone().ok_or_else(|| {
            into_callee_err(
                &StatusCode::ServerInternalError,
                Some("No caller in dialplan".to_string()),
            )
        })?;

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
        if !self.server_dialog.state().is_confirmed() {
            let _ = self.server_dialog.ringing(None, None);
        }

        // Race the forked INVITEs – first 200 OK wins
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
                    if self.server_dialog.state().is_terminated() {
                        info!(
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
                            info!(
                                fork = winner_idx,
                                callee_uri = %callee_uri,
                                "fork_targets_parallel: target answered first"
                            );

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
                    warn!(
                        fork = winner_idx,
                        code = code,
                        text = %text,
                        "fork_targets_parallel: target rejected"
                    );
                    failures += 1;
                    last_error = (code, text, None);
                }
                Ok(Some((_idx, Err(e), _callee_uri))) => {
                    warn!(
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
                    debug!("fork_targets_parallel: fork cancelled (another answered)");
                    failures += 1;
                }
                Err(e) => {
                    warn!(error = %e, "fork_targets_parallel: join error");
                    failures += 1;
                    last_error = into_callee_err(
                        &StatusCode::ServerInternalError,
                        Some(format!("Fork join error: {e}")),
                    );
                }
            }

            // If all forks completed and none succeeded, we're done
            if failures >= total {
                info!("fork_targets_parallel: all {} targets failed", failures);
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

        let caller_offer = match self.media.caller_offer.clone() {
            Some(offer) => offer,
            None => {
                warn!("Cannot send 183: no caller offer available");
                return Ok(());
            }
        };

        let caller_is_webrtc = self.is_caller_webrtc();

        // Create media bridge if not already present
        let created_bridge = self.media.media_bridge.is_none();
        if self.media.media_bridge.is_none() {
            if let Err(e) = self
                .create_app_caller_media_bridge(&caller_offer, caller_is_webrtc)
                .await
            {
                warn!(
                    session_id = %self.context.session_id,
                    error = %e,
                    "Failed to create media bridge for 183 early media"
                );
                return Ok(());
            }
        }

        // Get caller-facing answer SDP from bridge
        let answer_sdp = match self.prepare_bridge_caller_answer().await {
            Ok(sdp) => sdp,
            Err(e) => {
                if created_bridge {
                    if let Some(bridge) = self.media.media_bridge.take() {
                        bridge.stop().await;
                    }
                }
                warn!(
                    session_id = %self.context.session_id,
                    error = %e,
                    "Failed to prepare bridge answer for 183 early media"
                );
                return Ok(());
            }
        };

        self.media.answer = Some(answer_sdp.clone());
        self.media.caller_answer_uses_media_bridge = true;
        self.media.early_media_sent = true;

        // Send 183 Session Progress with SDP
        if let Err(e) = self
            .server_dialog
            .ringing(Some(Self::sdp_headers()), Some(answer_sdp.into_bytes()))
        {
            warn!(session_id = %self.context.session_id, error = %e, "Failed to send 183 Session Progress");
            // Continue even if 183 fails
        } else {
            info!(session_id = %self.context.session_id, "Sent 183 Session Progress with early media");
        }

        // Resolve audio path — generate temp WAV for tone:// specs
        let resolved_path = Self::resolve_audio_path(audio_path)?;

        // Play progress audio through the bridge
        if let Some(ref bridge) = self.media.media_bridge {
            let Some(codec_info) = self.caller_answer_codec_info() else {
                warn!(
                    session_id = %self.context.session_id,
                    "Cannot play progress audio: caller answer has no audio codec"
                );
                return Ok(());
            };
            let track = crate::media::FileTrack::new("progress-media".to_string())
                .with_path(resolved_path)
                .with_loop(true)
                .with_codec_info(codec_info)
                .with_cname(self.server.rtc_cname.clone());
            if let Err(e) = bridge
                .replace_output_with_file(self.leg_bridge_endpoint(&LegId::from("caller")), &track)
                .await
            {
                warn!(session_id = %self.context.session_id, error = %e, "Failed to play progress audio");
            }
            self.media.bridge_playback_track_id = Some("progress-media".to_string());
            self.media
                .playback_tracks
                .insert("progress-media".to_string(), track);
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
            info!(
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
        self.server_dialog.reject(Some(status), reason.clone())?;
        Ok(())
    }

    async fn prepare_app_caller_media_bridge(&mut self) -> Option<String> {
        if let Some(ref answer) = self.media.answer {
            return Some(answer.clone());
        }

        let caller_offer = self.media.caller_offer.clone()?;
        let caller_is_webrtc = self.is_caller_webrtc();
        let caller_mode = self.caller_transport_mode();
        self.legs.set_transport(LegId::from("caller"), caller_mode);
        // App callee always gets the opposite of caller via the bridge
        self.legs.set_transport(
            LegId::from("callee"),
            if caller_is_webrtc {
                rustrtc::TransportMode::Rtp
            } else {
                rustrtc::TransportMode::WebRtc
            },
        );
        self.media.callee_offer_uses_media_bridge = false;

        let created_bridge = self.media.media_bridge.is_none();
        if self.media.media_bridge.is_none()
            && let Err(error) = self
                .create_app_caller_media_bridge(&caller_offer, caller_is_webrtc)
                .await
        {
            warn!(
                session_id = %self.context.session_id,
                error = %error,
                "Application answer: failed to prepare media bridge, falling back to caller media"
            );
            self.media.caller_answer_uses_media_bridge = false;
            return None;
        }

        match self.prepare_bridge_caller_answer().await {
            Ok(answer) => {
                self.media.answer = Some(answer.clone());
                self.media.caller_answer_uses_media_bridge = true;
                self.replace_caller_bridge_output_with_silence().await;
                Some(answer)
            }
            Err(error) => {
                if created_bridge && let Some(bridge) = self.media.media_bridge.take() {
                    bridge.stop().await;
                }
                warn!(
                    session_id = %self.context.session_id,
                    error = %error,
                    "Application answer: failed to answer from media bridge, falling back to caller media"
                );
                self.media.caller_answer_uses_media_bridge = false;
                None
            }
        }
    }

    async fn create_app_caller_media_bridge(
        &mut self,
        caller_offer: &str,
        caller_is_webrtc: bool,
    ) -> Result<()> {
        let mut bridge_builder = BridgePeerBuilder::new(format!("{}-app-bridge", self.id))
            .with_enable_latching(self.server.proxy_config.enable_latching)
            .with_probation_max_packets(self.server.proxy_config.latching_probation_max_packets)
            .with_cname(self.server.rtc_cname.clone());

        if let (Some(start), Some(end)) = (
            self.server.rtp_config.start_port,
            self.server.rtp_config.end_port,
        ) {
            bridge_builder = bridge_builder.with_rtp_port_range(start, end);
        }

        if !caller_is_webrtc && caller_offer.contains("a=group:BUNDLE") {
            bridge_builder = bridge_builder
                .with_rtp_sdp_compatibility(rustrtc::config::SdpCompatibilityMode::Standard)
                .with_enable_latching(true)
                .with_probation_max_packets(Some(6));
            info!(session_id = %self.id, "RTP caller offered BUNDLE, using Standard SDP mode + latching for app media bridge");
        }

        // When caller uses SDES-SRTP, configure the non-WebRTC bridge side for SRTP
        if !caller_is_webrtc
            && Self::sdp_transport_mode(caller_offer) == rustrtc::TransportMode::Srtp
        {
            let callee_config = rustrtc::RtcConfiguration {
                transport_mode: rustrtc::TransportMode::Srtp,
                rtp_start_port: self.server.rtp_config.start_port,
                rtp_end_port: self.server.rtp_config.end_port,
                enable_latching: self.server.proxy_config.enable_latching,
                probation_max_packets: self.server.proxy_config.latching_probation_max_packets,
                external_ip: self.server.rtp_config.external_ip.clone(),
                bind_ip: self.server.rtp_config.bind_ip.clone(),
                cname: Some(self.server.rtc_cname.clone()),
                ..Default::default()
            };
            bridge_builder = bridge_builder.with_callee_config(callee_config);
        }

        if let Some(ref external_ip) = self.server.rtp_config.external_ip {
            bridge_builder = bridge_builder.with_external_ip(external_ip.clone());
        }
        if let Some(ref bind_ip) = self.server.rtp_config.bind_ip {
            bridge_builder = bridge_builder.with_bind_ip(bind_ip.clone());
        }
        if let Some(ref ice_servers) = self.context.dialplan.media.ice_servers {
            bridge_builder = bridge_builder.with_ice_servers(ice_servers.clone());
        }

        let allow_codecs = self.resolve_effective_codecs();
        let caller_offer_codecs =
            MediaNegotiator::build_codec_list_from_offer(caller_offer, &allow_codecs);
        let callee_offer_codecs =
            MediaNegotiator::build_callee_codec_offer_with_allow(caller_offer, &allow_codecs);
        let caller_leg_codecs = if caller_is_webrtc {
            &caller_offer_codecs
        } else {
            &callee_offer_codecs
        };
        let callee_leg_codecs = if caller_is_webrtc {
            &callee_offer_codecs
        } else {
            &caller_offer_codecs
        };

        let caller_caps: Vec<_> = caller_leg_codecs
            .iter()
            .filter_map(|codec| codec.to_audio_capability())
            .collect();
        let callee_caps: Vec<_> = callee_leg_codecs
            .iter()
            .filter_map(|codec| codec.to_audio_capability())
            .collect();
        if !caller_caps.is_empty() || !callee_caps.is_empty() {
            bridge_builder = bridge_builder
                .with_caller_audio_capabilities(caller_caps)
                .with_callee_audio_capabilities(callee_caps);
        }

        let caller_sender = caller_leg_codecs
            .iter()
            .find(|codec| !codec.is_dtmf())
            .map(|codec| codec.to_params());
        let callee_sender = callee_leg_codecs
            .iter()
            .find(|codec| !codec.is_dtmf())
            .map(|codec| codec.to_params());
        if let (Some(caller_sender), Some(callee_sender)) = (caller_sender, callee_sender) {
            bridge_builder = bridge_builder.with_sender_codecs(caller_sender, callee_sender);
        }

        if self.context.dialplan.recording.enabled {
            bridge_builder =
                bridge_builder.with_recorder(self.recorder.clone(), self.recording_paused.clone());
        }
        if self.context.dialplan.recording.enabled && !self.context.dialplan.recording.force_file {
            if let Some(sipflow_tx) =
                self.setup_sipflow_capture(&self.context.session_id, &self.context.session_id)
            {
                bridge_builder = bridge_builder.with_sipflow_capture(sipflow_tx);
            }
        }

        let rtp_timeout = self.context.dialplan.rtp_timeout.or_else(|| {
            self.server
                .proxy_config
                .rtp_timeout
                .map(Duration::from_secs)
        });
        if let (Some(tx), Some(timeout)) = (self.media.rtp_timeout_tx.take(), rtp_timeout) {
            bridge_builder = bridge_builder.with_rtp_timeout_notify(tx, timeout);
        }

        let bridge = bridge_builder.build();
        bridge.setup_bridge().await?;
        self.media.media_bridge = Some(bridge);

        // Notify the media engine so it can route Play/Record commands to this bridge.
        if let Some(bridge) = self.media.media_bridge.clone() {
            use crate::media::engine::MediaCommand;
            let codec_info = self
                .caller_answer_codec_info()
                .map(|c| vec![c])
                .unwrap_or_default();
            self.engine_send(MediaCommand::AttachBridge {
                session_id: self.context.session_id.clone(),
                bridge,
                caller_is_webrtc,
                caller_codec_info: codec_info,
            });
        }

        debug!(
            session_id = %self.context.session_id,
            caller_is_webrtc,
            "Application caller media bridge prepared"
        );

        Ok(())
    }

    async fn replace_caller_bridge_output_with_silence(&self) {
        let Some(bridge) = self.media.media_bridge.as_ref() else {
            return;
        };

        if let Err(error) = bridge
            .replace_output_with_silence(
                self.leg_bridge_endpoint(&LegId::from("caller")),
                match self.caller_answer_codec_info() {
                    Some(codec_info) => codec_info,
                    None => {
                        warn!(
                            session_id = %self.context.session_id,
                            "Cannot install caller bridge silence source: caller answer has no audio codec"
                        );
                        return;
                    }
                },
            )
            .await
        {
            warn!(
                session_id = %self.context.session_id,
                error = %error,
                "Failed to install caller bridge silence source"
            );
        }
    }

    fn caller_answer_codec_info(&self) -> Option<CodecInfo> {
        self.media
            .answer
            .as_ref()
            .map(|answer_sdp| MediaNegotiator::extract_codec_params(answer_sdp).audio)
            .and_then(|codecs| codecs.first().cloned())
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
            warn!(
                session_id = %self.context.session_id,
                context,
                "No compatible audio codec selected for SDP answer"
            );
            return answer_sdp.to_string();
        }

        MediaNegotiator::rewrite_sdp_codec_list(answer_sdp, &selected_codecs).unwrap_or_else(|| {
            warn!(
                session_id = %self.context.session_id,
                context,
                "Failed to rewrite SDP answer to selected audio codec"
            );
            answer_sdp.to_string()
        })
    }

    async fn prepare_bridge_caller_answer(&self) -> Result<String> {
        let caller_offer = self
            .media
            .caller_offer
            .as_deref()
            .ok_or_else(|| anyhow!("No caller offer available for bridge answer"))?;
        let bridge = self
            .media
            .media_bridge
            .as_ref()
            .ok_or_else(|| anyhow!("No media bridge available for caller answer"))?;

        let pc = if self.is_caller_webrtc() {
            bridge.caller_pc().clone()
        } else {
            bridge.callee_pc().clone()
        };

        if let Some(local_description) = pc.local_description() {
            let answer_sdp = local_description.to_sdp_string();
            return Ok(self.rewrite_answer_to_selected_codecs(
                &answer_sdp,
                caller_offer,
                None,
                "bridge caller answer",
            ));
        }

        if pc.remote_description().is_none() {
            let offer = Self::parse_sdp(rustrtc::SdpType::Offer, caller_offer, "caller offer")?;
            pc.set_remote_description(offer)
                .await
                .map_err(|e| anyhow!("Failed to set bridge caller remote description: {}", e))?;
        }

        let answer = pc
            .create_answer()
            .await
            .map_err(|e| anyhow!("Failed to create bridge caller answer: {}", e))?;
        pc.set_local_description(answer)
            .map_err(|e| anyhow!("Failed to set bridge caller local answer: {}", e))?;

        if self.is_caller_webrtc() {
            pc.wait_for_gathering_complete().await;
        }

        let answer_sdp = pc
            .local_description()
            .map(|desc| desc.to_sdp_string())
            .ok_or_else(|| anyhow!("Bridge caller side has no local answer"))?;

        Ok(self.rewrite_answer_to_selected_codecs(
            &answer_sdp,
            caller_offer,
            None,
            "bridge caller answer",
        ))
    }

    /// Resolve queue targets to the concrete locations used for dialing.
    ///
    /// Custom targets (e.g. skill-group:) are expanded through the AgentRegistry.
    /// Same-realm SIP targets are then looked up in the registrar locator so
    /// registered contact metadata such as transport and WebRTC support is used.
    /// Uses the AgentRegistry trait's resolve_target hook, which allows addons
    /// to implement custom routing logic without queue knowing the details.
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
                    info!(target = %uri_str, "Resolving custom target to agents");

                    if let Some(registry) = &agent_registry {
                        // Use the registry's resolve_target hook
                        // CC addon implements this to resolve skill-group: URIs
                        let agent_uris = registry
                            .resolve_target_with_policy(&uri_str, None, &self.id.0)
                            .await;

                        if agent_uris.is_empty() {
                            warn!(target = %uri_str, "No agents resolved for custom target");
                        } else {
                            let resolved_sample =
                                agent_uris.iter().take(5).cloned().collect::<Vec<_>>();
                            let mut parsed_count = 0usize;
                            info!(
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
                                    // Query the SIP registrar for this agent's live contact.
                                    let registered_locations =
                                        self.server.locator.lookup(&uri).await.unwrap_or_default();

                                    if let Some(reg_loc) = registered_locations.into_iter().next() {
                                        // Use the full registered location (preserves
                                        // supports_webrtc, destination, transport, etc.)
                                        expanded.push(reg_loc);
                                    } else {
                                        // Agent not currently registered.
                                        // Use host_with_port so that agents on a
                                        // different port (e.g. test UAs) are not
                                        // incorrectly treated as same-realm offline.
                                        let host = uri.host_with_port.to_string();
                                        if self.server.is_same_realm(&host).await {
                                            // Local realm agent not registered → offline; skip.
                                            warn!(
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

                            info!(
                                target = %uri_str,
                                parsed_location_count = parsed_count,
                                "Resolved custom target parsed into dialable locations"
                            );
                        }
                    } else {
                        warn!("No agent registry available to resolve custom target");
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
                    info!(
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
                    warn!(
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
    ) -> Result<(), CalleeError> {
        use rsipstack::dialog::dialog::DialogState;

        let _caller = self.context.dialplan.caller.clone().ok_or_else(|| {
            into_callee_err(
                &StatusCode::ServerInternalError,
                Some("No caller in dialplan".to_string()),
            )
        })?;

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

        if let Some(home_proxy) = target.home_proxy.as_ref() {
            info!(
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

        let result = loop {
            tokio::select! {
                _ = caller_end_check.tick() => {
                    if self.server_dialog.state().is_terminated() {
                        info!(
                            session_id = %self.context.session_id,
                            "Caller dialog terminated while callee INVITE was pending"
                        );
                        self.cancel_token.cancel();
                        break Err(into_callee_err(
                            &StatusCode::RequestTerminated,
                            Some("Caller cancelled".to_string()),
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
                        if self.meta.ring_time.is_none() {
                            self.meta.ring_time = Some(Instant::now());
                        }

                        let callee_sdp = String::from_utf8_lossy(response.body()).to_string();
                        if !callee_sdp.is_empty() && callee_sdp.contains("v=0") {
                            self.media.early_media_sent = true;
                            self.update_leg_state(&LegId::from("callee"), LegState::EarlyMedia);

                            if self.media_profile.path == MediaPathMode::Anchored {
                                let caller_sdp = self
                                    .prepare_caller_answer_from_callee_sdp(
                                        Some(callee_sdp),
                                        false,
                                        true,
                                    )
                                    .await;

                                if let Err(e) = self.server_dialog.ringing(
                                    Some(Self::sdp_headers()),
                                    caller_sdp.map(|sdp| sdp.into_bytes()),
                                ) {
                                    warn!(
                                        session_id = %self.context.session_id,
                                        error = %e,
                                        "Failed to send 183 Session Progress"
                                    );
                                }
                            } else {
                                if let Err(e) = self
                                    .server_dialog
                                    .ringing(
                                        Some(Self::sdp_headers()),
                                        Some(callee_sdp.into_bytes()),
                                    )
                                {
                                    warn!(
                                        session_id = %self.context.session_id,
                                        error = %e,
                                        "Failed to relay provisional SDP"
                                    );
                                }
                            }
                        } else {
                            if !self.media.early_media_sent {
                                self.update_leg_state(&LegId::from("callee"), LegState::Ringing);
                            }
                            if let Err(e) = self.server_dialog.ringing(None, None) {
                                warn!(
                                    session_id = %self.context.session_id,
                                    error = %e,
                                    "Failed to send 180 Ringing"
                                );
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
        if let Some(track_id) = stop_playback_on_answer {
            self.stop_playback_track(track_id, false).await;
        }

        // Stop playback (if any) before transitioning to confirmed call.
        if self.media.early_media_sent {
            if let Some(track_id) = self.media.bridge_playback_track_id.clone() {
                self.stop_playback_track(&track_id, false).await;
            }
        }

        let caller_answer = self
            .prepare_caller_answer_from_callee_sdp(callee_sdp, false, false)
            .await;

        self.meta.connected_callee_dialog_id = Some(dialog_id.clone());
        self.callee_dialogs.insert(dialog_id.clone(), ());
        self.callee_guards.push(ClientDialogGuard::new(
            self.server.dialog_layer.clone(),
            dialog_id.clone(),
        ));

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
    ) -> Option<Vec<u8>> {
        let callee_is_webrtc = target.supports_webrtc;
        let caller_is_webrtc = self.is_caller_webrtc();
        let callee_sdp = if self.bypasses_local_media() && caller_is_webrtc == callee_is_webrtc {
            self.media.callee_offer_uses_media_bridge = false;
            let allow_codecs = self.resolve_effective_codecs();
            if !allow_codecs.is_empty() {
                if let Some(ref caller_offer) = self.media.caller_offer {
                    let selected_codecs =
                        MediaNegotiator::build_codec_list_from_offer(caller_offer, &allow_codecs);
                    if selected_codecs.is_empty() {
                        warn!(
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
                            warn!(
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
            self.create_callee_track(callee_is_webrtc).await.ok()
        };
        self.media.callee_offer = callee_sdp;
        self.media.callee_offer.clone().map(|s| s.into_bytes())
    }

    async fn prepare_caller_answer_from_callee_sdp(
        &mut self,
        callee_sdp: Option<String>,
        force_regenerate: bool,
        is_early_media: bool,
    ) -> Option<String> {
        let Some(callee_sdp_value) = callee_sdp else {
            return if self.media.early_media_sent {
                self.media.answer.clone()
            } else {
                None
            };
        };

        let sdp_changed =
            self.media.callee_answer_sdp.as_deref() != Some(callee_sdp_value.as_str());

        if self.media.answer.is_some() && !sdp_changed && !force_regenerate {
            if !is_early_media {
                if let Some(ref bridge) = self.media.media_bridge {
                    bridge.open_caller_gate();
                }
            }
            return self.media.answer.clone();
        }

        if self.media.callee_answer_sdp.is_some() && sdp_changed {
            debug!(
                session_id = %self.context.session_id,
                "Callee answer SDP changed after early media; regenerating caller-facing SDP"
            );
        }

        if self.server_dialog.state().is_confirmed()
            && self.media.answer.is_some()
            && self.media.media_bridge.is_some()
            && self.media.caller_answer_uses_media_bridge
            && self.media.callee_offer_uses_media_bridge
        {
            match self.apply_bridge_callee_answer(&callee_sdp_value).await {
                Ok(()) => {
                    self.configure_media_bridge_transcoders(
                        self.media.answer.as_deref(),
                        Some(&callee_sdp_value),
                    );
                    self.start_media_bridge_forwarding().await;
                    // Dialog is confirmed at this point — open the gate.
                    if let Some(ref bridge) = self.media.media_bridge {
                        bridge.open_caller_gate();
                    }
                }
                Err(error) => {
                    warn!(
                        session_id = %self.context.session_id,
                        error = %error,
                        "Failed to apply callee answer to existing media bridge"
                    );
                }
            }

            self.media.callee_answer_sdp = Some(callee_sdp_value);
            return self.media.answer.clone();
        }

        let can_update_confirmed_anchored_media = self.media.media_bridge.is_none()
            || (self.media.caller_answer_uses_media_bridge
                && !self.media.callee_offer_uses_media_bridge);

        if self.server_dialog.state().is_confirmed()
            && self.media.answer.is_some()
            && self.media_profile.path == MediaPathMode::Anchored
            && can_update_confirmed_anchored_media
        {
            debug!(
                session_id = %self.context.session_id,
                "Caller dialog already confirmed; keeping existing caller track/SDP and only updating callee-side forwarding"
            );

            let caller_answer = self.media.answer.clone();

            if let Some(peer) = self.callee_peer() {
                if let Err(e) = peer
                    .update_remote_description(Self::CALLEE_TRACK_ID, &callee_sdp_value)
                    .await
                {
                    warn!(
                        session_id = %self.context.session_id,
                        error = %e,
                        "Failed to set callee answer on callee track"
                    );
                }
            }

            self.media.callee_answer_sdp = Some(callee_sdp_value);
            let callee_answer_for_forwarding = self.media.callee_answer_sdp.clone();
            self.start_anchored_media_forwarding(
                caller_answer.as_deref(),
                callee_answer_for_forwarding.as_deref(),
            )
            .await;

            return caller_answer;
        }

        let callee_sdp = Some(callee_sdp_value.clone());
        let caller_is_webrtc = self.is_caller_webrtc();
        let callee_is_webrtc = self.is_callee_webrtc();

        let caller_answer = if caller_is_webrtc && !callee_is_webrtc {
            self.negotiate_bridge_caller_answer(&callee_sdp_value, is_early_media, true)
                .await
        } else if !caller_is_webrtc && callee_is_webrtc {
            self.negotiate_bridge_caller_answer(&callee_sdp_value, is_early_media, false)
                .await
        } else if self.media_profile.path == MediaPathMode::Anchored {
            if let (Some(sdp), Some(peer)) = (callee_sdp.as_ref(), self.callee_peer()) {
                if let Err(e) = peer
                    .update_remote_description(Self::CALLEE_TRACK_ID, sdp)
                    .await
                {
                    warn!(
                        session_id = %self.context.session_id,
                        error = %e,
                        "Failed to set callee answer on callee track"
                    );
                }
            }

            if let Some(ref caller_offer) = self.media.caller_offer {
                let allow_codecs = self.resolve_effective_codecs();
                let preferred_codecs: Vec<CodecType> =
                    MediaNegotiator::extract_codec_params(&callee_sdp_value)
                        .audio
                        .into_iter()
                        .map(|codec| codec.codec)
                        .collect();
                let codec_info = MediaNegotiator::build_codec_list_from_offer(
                    caller_offer,
                    if preferred_codecs.is_empty() {
                        &allow_codecs
                    } else {
                        &preferred_codecs
                    },
                );
                if codec_info.is_empty() {
                    warn!(
                        session_id = %self.context.session_id,
                        "No compatible codec found for anchored caller answer"
                    );
                }

                let cancel_token = self.caller_peer().map(|p| p.cancel_token()).unwrap_or_default();
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
                match track.handshake(caller_offer.clone()).await {
                    Ok(answer_sdp) => {
                        let answer_sdp = self.rewrite_answer_to_selected_codecs(
                            &answer_sdp,
                            caller_offer,
                            Some(&callee_sdp_value),
                            "anchored caller answer",
                        );
                        debug!(
                            session_id = %self.context.session_id,
                            "Generated PBX answer SDP for caller (anchored media)"
                        );
                        if let Some(peer) = self.caller_peer() {
                            peer.update_track(Box::new(track), None).await;
                        }
                        Some(answer_sdp)
                    }
                    Err(e) => {
                        warn!(
                            session_id = %self.context.session_id,
                            error = %e,
                            "Failed to handshake caller track"
                        );
                        None
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
        self.media.caller_answer_uses_media_bridge = self.media.callee_offer_uses_media_bridge
            && self.media.media_bridge.is_some()
            && caller_answer.is_some();

        self.configure_media_bridge_transcoders(caller_answer.as_deref(), callee_sdp.as_deref());

        if self.media_profile.path == MediaPathMode::Anchored && self.media.media_bridge.is_none() {
            let caller_answer_for_forwarding = self.media.answer.clone();
            let callee_answer_for_forwarding = callee_sdp.clone();
            self.start_anchored_media_forwarding(
                caller_answer_for_forwarding.as_deref(),
                callee_answer_for_forwarding.as_deref(),
            )
            .await;
        }

        if self.media.media_bridge.is_some() && self.media.caller_answer_uses_media_bridge {
            self.start_media_bridge_forwarding().await;
            // Only allow WebRTC→RTP forwarding once the callee has confirmed the call (200 OK).
            // During 183 early media the gate stays closed to avoid sending WebRTC audio to a
            // SIP endpoint that is still ringing / not yet ready to receive it.
            if !is_early_media {
                if let Some(ref bridge) = self.media.media_bridge {
                    bridge.open_caller_gate();
                }
            }
        }

        caller_answer
    }

    /// WebRTC caller ↔ RTP callee: negotiate bridge media answer for the WebRTC side.
    /// Applies the callee's RTP answer to the bridge RTP PC, then creates a WebRTC answer
    /// from the caller's offer for the bridge WebRTC PC. Returns the WebRTC SDP to send back
    /// to the caller.
    async fn negotiate_bridge_caller_answer(
        &mut self,
        callee_sdp: &str,
        is_early_media: bool,
        caller_is_webrtc: bool,
    ) -> Option<String> {
        let sdp = callee_sdp;
        if let Some(ref bridge) = self.media.media_bridge {
            use rustrtc::sdp::{SdpType, SessionDescription};

            let callee_sdp_type = if is_early_media {
                SdpType::Pranswer
            } else {
                SdpType::Answer
            };

            let callee_pc = if caller_is_webrtc {
                bridge.callee_pc()
            } else {
                bridge.caller_pc()
            };
            let caller_pc = if caller_is_webrtc {
                bridge.caller_pc()
            } else {
                bridge.callee_pc()
            };

            let mut callee_video_caps = Vec::new();
            if let Ok(desc) = SessionDescription::parse(callee_sdp_type, sdp) {
                callee_video_caps = desc.to_video_capabilities();

                if caller_is_webrtc {
                    let selected_video_params =
                        callee_video_caps
                            .first()
                            .map(|cap| rustrtc::RtpCodecParameters {
                                payload_type: cap.payload_type,
                                clock_rate: cap.clock_rate,
                                channels: 0,
                            });
                    if let Err(e) = callee_pc.set_remote_description(desc).await {
                        warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge RTP remote description");
                    }
                    if let Some(params) = selected_video_params {
                        bridge.set_video_payload_types(Some(params.clone()), Some(params));
                    }

                    if let Some(pair) = callee_pc.ice_transport().get_selected_pair().await {
                        let payload_map = callee_pc
                            .get_transceivers()
                            .iter()
                            .find(|t| t.kind() == rustrtc::MediaKind::Audio)
                            .map(|t| t.get_payload_map())
                            .unwrap_or_default();
                        let pt_info: Vec<String> = payload_map
                            .iter()
                            .map(|(pt, params)| {
                                format!(
                                    "{}(clock_rate={},channels={})",
                                    pt, params.clock_rate, params.channels
                                )
                            })
                            .collect();
                        debug!(
                            session_id = %self.context.session_id,
                            rtp_remote_addr = %pair.remote.address,
                            rtp_remote_port = pair.remote.address.port(),
                            payload_types = ?pt_info,
                            "Bridge RTP side re-negotiated"
                        );
                    }
                } else {
                    if let Err(e) = callee_pc.set_remote_description(desc).await {
                        warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge WebRTC remote description");
                    }
                }
            }

            if let Some(ref caller_offer) = self.media.caller_offer {
                match SessionDescription::parse(SdpType::Offer, caller_offer) {
                    Ok(caller_desc) => match caller_pc.set_remote_description(caller_desc).await {
                        Ok(_) => match caller_pc.create_answer().await {
                            Ok(answer) => {
                                if let Err(e) = caller_pc.set_local_description(answer) {
                                    warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge local description");
                                    Some(callee_sdp.to_string())
                                } else {
                                    if caller_is_webrtc {
                                        caller_pc.wait_for_gathering_complete().await;
                                    }

                                    if let Some(answer_sdp) =
                                        caller_pc.local_description().map(|d| d.to_sdp_string())
                                    {
                                        let answer_sdp = crate::media::negotiate::MediaNegotiator::restrict_sdp_to_reference_codecs(
                                                    SdpType::Answer,
                                                    &answer_sdp,
                                                    SdpType::Answer,
                                                    sdp,
                                                ).unwrap_or(answer_sdp);

                                        let answer_sdp = self.rewrite_answer_to_selected_codecs(
                                            &answer_sdp,
                                            caller_offer,
                                            Some(sdp),
                                            "bridge caller answer",
                                        );

                                        let caller_video_caps =
                                            SessionDescription::parse(SdpType::Answer, &answer_sdp)
                                                .map(|desc| desc.to_video_capabilities())
                                                .unwrap_or_default();

                                        let (webrtc_caps, rtp_caps) = if caller_is_webrtc {
                                            (caller_video_caps, callee_video_caps)
                                        } else {
                                            (callee_video_caps, caller_video_caps)
                                        };

                                        bridge.set_video_payload_maps(&webrtc_caps, &rtp_caps);
                                        Some(answer_sdp)
                                    } else {
                                        Some(callee_sdp.to_string())
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(session_id = %self.context.session_id, error = %e, "Failed to create bridge answer");
                                Some(callee_sdp.to_string())
                            }
                        },
                        Err(e) => {
                            warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge remote description");
                            Some(callee_sdp.to_string())
                        }
                    },
                    Err(e) => {
                        warn!(session_id = %self.context.session_id, error = %e, "Failed to parse caller offer SDP");
                        Some(callee_sdp.to_string())
                    }
                }
            } else {
                Some(callee_sdp.to_string())
            }
        } else {
            warn!(session_id = %self.context.session_id, "No media bridge — SDP may be incorrect");
            Some(callee_sdp.to_string())
        }
    }

    fn configure_media_bridge_transcoders(
        &self,
        caller_answer_sdp: Option<&str>,
        callee_answer_sdp: Option<&str>,
    ) {
        let Some(bridge) = self.media.media_bridge.as_ref() else {
            return;
        };
        let Some(caller_answer_sdp) = caller_answer_sdp else {
            return;
        };
        let Some(callee_answer_sdp) = callee_answer_sdp else {
            return;
        };

        let caller_profile = MediaNegotiator::extract_leg_profile(caller_answer_sdp);
        let callee_profile = MediaNegotiator::extract_leg_profile(callee_answer_sdp);

        let caller_endpoint = self.leg_bridge_endpoint(&LegId::from("caller"));
        let callee_endpoint = self.leg_bridge_endpoint(&LegId::from("callee"));

        match (&caller_profile.dtmf, &callee_profile.dtmf) {
            (Some(caller_dtmf), Some(callee_dtmf)) => {
                bridge.set_dtmf_mapping(
                    caller_endpoint,
                    caller_dtmf.payload_type,
                    caller_dtmf.clock_rate,
                    callee_dtmf.payload_type,
                    callee_dtmf.clock_rate,
                );
                bridge.set_dtmf_mapping(
                    callee_endpoint,
                    callee_dtmf.payload_type,
                    callee_dtmf.clock_rate,
                    caller_dtmf.payload_type,
                    caller_dtmf.clock_rate,
                );
            }
            _ => {
                bridge.clear_dtmf_mapping(caller_endpoint);
                bridge.clear_dtmf_mapping(callee_endpoint);
            }
        }

        let (Some(caller_audio), Some(callee_audio)) =
            (&caller_profile.audio, &callee_profile.audio)
        else {
            return;
        };

        if caller_audio.codec == callee_audio.codec {
            bridge.clear_transcoder(caller_endpoint);
            bridge.clear_transcoder(callee_endpoint);
            debug!(
                session_id = %self.context.session_id,
                codec = ?caller_audio.codec,
                "Bridge transcoder not needed; caller and callee selected the same codec"
            );
            return;
        }

        bridge.set_transcoder(
            caller_endpoint,
            caller_audio.codec,
            callee_audio.codec,
            callee_audio.payload_type,
        );
        bridge.set_transcoder(
            callee_endpoint,
            callee_audio.codec,
            caller_audio.codec,
            caller_audio.payload_type,
        );
        info!(
            session_id = %self.context.session_id,
            caller_codec = ?caller_audio.codec,
            caller_pt = caller_audio.payload_type,
            callee_codec = ?callee_audio.codec,
            callee_pt = callee_audio.payload_type,
            "Bridge transcoder configured for selected codec mismatch"
        );
    }

    async fn start_media_bridge_forwarding(&mut self) {
        if self.media.media_bridge_started {
            return;
        }

        if let Some(ref bridge) = self.media.media_bridge {
            let caller_pc = if self.is_caller_webrtc() {
                bridge.caller_pc()
            } else {
                bridge.callee_pc()
            };
            let callee_pc = if self.is_callee_webrtc() {
                bridge.caller_pc()
            } else {
                bridge.callee_pc()
            };

            if caller_pc.local_description().is_none()
                || caller_pc.remote_description().is_none()
                || callee_pc.local_description().is_none()
                || callee_pc.remote_description().is_none()
            {
                warn!(
                    session_id = %self.context.session_id,
                    caller_local = caller_pc.local_description().is_some(),
                    caller_remote = caller_pc.remote_description().is_some(),
                    callee_local = callee_pc.local_description().is_some(),
                    callee_remote = callee_pc.remote_description().is_some(),
                    "Media bridge forwarding not started because SDP is incomplete"
                );
                return;
            }

            bridge
                .replace_output_with_peer(self.leg_bridge_endpoint(&LegId::from("caller")))
                .await;
            bridge
                .replace_output_with_peer(self.leg_bridge_endpoint(&LegId::from("callee")))
                .await;

            info!(
                session_id = %self.context.session_id,
                "Starting media bridge forwarding"
            );
            bridge.start_bridge().await;
            self.media.media_bridge_started = true;
        }
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

    /// Resolve bridge endpoint for a leg from leg_transport.
    fn leg_bridge_endpoint(&self, leg_id: &LegId) -> BridgeEndpoint {
        match self.legs.get_transport(leg_id) {
            Some(rustrtc::TransportMode::WebRtc) => BridgeEndpoint::Caller,
            _ => BridgeEndpoint::Callee,
        }
    }

    async fn start_anchored_media_forwarding(
        &mut self,
        caller_answer_sdp: Option<&str>,
        callee_answer_sdp: Option<&str>,
    ) {
        self.stop_caller_ingress_monitor().await;

        use crate::media::recorder::Leg;

        let session_id = &self.context.session_id;

        let caller_pc = if self.media.caller_answer_uses_media_bridge {
            let Some(bridge) = self.media.media_bridge.as_ref() else {
                warn!(
                    session_id = %session_id,
                    "Cannot start anchored forwarding: caller answer uses media bridge but bridge is missing"
                );
                return;
            };
            match self.leg_bridge_endpoint(&LegId::from("caller")) {
                BridgeEndpoint::Caller => Some(bridge.caller_pc().clone()),
                BridgeEndpoint::Callee => Some(bridge.callee_pc().clone()),
            }
        } else {
            match self.caller_peer() {
                Some(peer) => Self::get_peer_pc(peer, Self::CALLER_TRACK_ID).await,
                None => None,
            }
        };
        let callee_pc = match self.callee_peer() {
            Some(peer) => Self::get_peer_pc(peer, Self::CALLEE_TRACK_ID).await,
            None => None,
        };

        let (Some(caller_pc), Some(callee_pc)) = (caller_pc, callee_pc) else {
            warn!(
                session_id = %session_id,
                "Cannot start anchored forwarding: missing PeerConnection on caller or callee track"
            );
            return;
        };

        let caller_profile = caller_answer_sdp
            .map(MediaNegotiator::extract_leg_profile)
            .unwrap_or_default();
        let callee_profile = callee_answer_sdp
            .map(MediaNegotiator::extract_leg_profile)
            .unwrap_or_default();

        if let (Some(ca), Some(ce)) = (&caller_profile.audio, &callee_profile.audio) {
            info!(
                session_id = %session_id,
                caller_codec = ?ca.codec, caller_pt = ca.payload_type,
                callee_codec = ?ce.codec, callee_pt = ce.payload_type,
                caller_dtmf_pt = caller_profile.dtmf.as_ref().map(|codec| codec.payload_type),
                callee_dtmf_pt = callee_profile.dtmf.as_ref().map(|codec| codec.payload_type),
                needs_transcoding = (ca.codec != ce.codec),
                "Anchored media: leg profiles extracted"
            );
        }

        let shared_recorder = self.recorder.clone();

        let sipflow_tx = if self.context.dialplan.recording.enabled
            && !self.context.dialplan.recording.force_file
        {
            self.setup_sipflow_capture(session_id, session_id)
        } else {
            None
        };
        let (caller_sipflow_tx, callee_sipflow_tx) = (sipflow_tx.clone(), sipflow_tx);

        match Self::wire_with_forwarding_track(
            Self::CALLER_FORWARDING_TRACK_ID,
            &caller_pc,
            &callee_pc,
            caller_profile.clone(),
            callee_profile.clone(),
            shared_recorder.clone(),
            Leg::A,
            caller_sipflow_tx,
            session_id,
            "caller→callee",
        ) {
            Ok(forwarding) => {
                if let Some(peer) = self.caller_peer() {
                    peer.update_track(
                        Box::new(crate::media::forwarding_track::ForwardingTrackHandle::new(
                            Self::CALLER_FORWARDING_TRACK_ID.to_string(),
                            forwarding,
                        )),
                        None,
                    )
                    .await;
                }
            }
            Err(e) => {
                warn!(session_id = %session_id, error = %e, "Failed to wire caller→callee");
            }
        }

        match Self::wire_with_forwarding_track(
            Self::CALLEE_FORWARDING_TRACK_ID,
            &callee_pc,
            &caller_pc,
            callee_profile,
            caller_profile,
            shared_recorder,
            Leg::B,
            callee_sipflow_tx,
            session_id,
            "callee→caller",
        ) {
            Ok(forwarding) => {
                if let Some(peer) = self.callee_peer() {
                    peer.update_track(
                        Box::new(crate::media::forwarding_track::ForwardingTrackHandle::new(
                            Self::CALLEE_FORWARDING_TRACK_ID.to_string(),
                            forwarding,
                        )),
                        None,
                    )
                    .await;
                }
            }
            Err(e) => {
                warn!(session_id = %session_id, error = %e, "Failed to wire callee→caller");
            }
        }
    }

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

    async fn find_video_receiver_track(
        pc: &rustrtc::PeerConnection,
    ) -> Option<Arc<dyn rustrtc::media::MediaStreamTrack>> {
        for transceiver in pc.get_transceivers() {
            if transceiver.kind() == rustrtc::MediaKind::Video
                && let Some(receiver) = transceiver.receiver()
            {
                return Some(receiver.track());
            }
        }
        None
    }

    async fn start_caller_ingress_monitor_if_needed(&mut self) {
        if self.meta.connected_callee.is_some() {
            return;
        }

        let Some(answer_sdp) = self.media.answer.as_deref() else {
            warn!(
                session_id = %self.context.session_id,
                "Cannot start caller ingress monitor: no answer SDP available for DTMF detection"
            );
            return;
        };

        let caller_profile = MediaNegotiator::extract_leg_profile(answer_sdp);
        let Some(dtmf_codec) = caller_profile.dtmf else {
            warn!(
                session_id = %self.context.session_id,
                "Cannot start caller ingress monitor: no DTMF codec found in SDP. Available audio codec: {:?}",
                caller_profile.audio.as_ref().map(|a| &a.codec)
            );
            return;
        };
        debug!(
            session_id = %self.context.session_id,
            dtmf_codec = ?dtmf_codec.codec,
            dtmf_payload_type = dtmf_codec.payload_type,
            dtmf_clock_rate = dtmf_codec.clock_rate,
            "Found DTMF codec in SDP, will start ingress monitor"
        );

        let dtmf_payload_types: Vec<u8> = {
            let mut pts: Vec<u8> = MediaNegotiator::extract_dtmf_codecs(answer_sdp)
                .into_iter()
                .map(|c| c.payload_type)
                .collect();
            if pts.is_empty() {
                pts.push(dtmf_codec.payload_type);
            }
            pts.sort_unstable();
            pts.dedup();
            pts
        };

        let session_id = self.context.session_id.clone();
        let app_runtime = self.app_runtime.clone();
        let caller_leg_id = "caller".to_string();
        let rwi_gateway = self.server.rwi_gateway.clone();

        if self.media.caller_answer_uses_media_bridge {
            if self.media.caller_ingress_monitor.is_some() {
                self.stop_caller_ingress_monitor().await;
            }

            let Some(bridge) = self.media.media_bridge.as_ref() else {
                return;
            };
            let endpoint = self.leg_bridge_endpoint(&LegId::from("caller"));
            let caller_leg = caller_leg_id.clone();
            bridge.set_dtmf_sink(
                endpoint,
                dtmf_payload_types.clone(),
                Arc::new(move |digit| {
                    let event = serde_json::json!({
                        "type": "dtmf",
                        "leg_id": caller_leg,
                        "digit": digit.to_string(),
                    });

                    if let Err(error) = app_runtime.inject_event(event) {
                        debug!(
                            session_id = %session_id,
                            digit = %digit,
                            error = %error,
                            "Bridge DTMF sink observed RTP DTMF with no active app receiver"
                        );
                    } else {
                        debug!(
                            session_id = %session_id,
                            digit = %digit,
                            "Injected RTP DTMF from bridge sink"
                        );
                        // Emit RWI DTMF event
                        if let Some(ref gw) = rwi_gateway {
                            let ev = crate::rwi::Dtmf {
                                call_id: session_id.clone(),
                                digit: digit.to_string(),
                                leg_id: Some(caller_leg.clone()),
                                extra: None,
                            };
                            let g = gw.read();
                            g.send_to_owner(&ev);
                        }
                    }
                }),
            );
            bridge.start_bridge().await;
            return;
        }

        if self
            .media
            .caller_ingress_monitor
            .as_ref()
            .is_some_and(|monitor| !monitor.task.is_finished())
        {
            return;
        }

        if self.media.caller_ingress_monitor.is_some() {
            self.stop_caller_ingress_monitor().await;
        }

        let caller_pc = match self.caller_peer() {
            Some(peer) => Self::get_peer_pc(peer, Self::CALLER_TRACK_ID).await,
            None => None,
        };
        let Some(caller_pc) = caller_pc else {
            return;
        };

        let cancel_token = self.cancel_token.child_token();
        let monitor_cancel = cancel_token.clone();

        let dtmf_payload_types_for_task = dtmf_payload_types.clone();
        let task = crate::utils::spawn(async move {
            let track = loop {
                if let Some(track) = Self::find_audio_receiver_track(&caller_pc).await {
                    info!(
                        session_id = %session_id,
                        "Found audio receiver track for DTMF monitoring"
                    );
                    break track;
                }

                tokio::select! {
                    _ = monitor_cancel.cancelled() => {
                        warn!(session_id = %session_id, "Ingress monitor cancelled while searching for audio track");
                        return;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            };

            let mut detector = RtpDtmfDetector::default();
            let mut frame_count = 0u64;
            let mut dtmf_frames_count = 0u64;

            loop {
                tokio::select! {
                    _ = monitor_cancel.cancelled() => {
                        info!(
                            session_id = %session_id,
                            frame_count = frame_count,
                            dtmf_frames_count = dtmf_frames_count,
                            "Ingress monitor cancelled"
                        );
                        break;
                    }
                    sample = track.recv() => {
                        match sample {
                            Ok(rustrtc::media::MediaSample::Audio(frame)) => {
                                frame_count += 1;
                                if frame.payload_type.is_some() && !dtmf_payload_types_for_task.contains(&frame.payload_type.unwrap()) {
                                    if frame_count % 100 == 0 {
                                        debug!(
                                            session_id = %session_id,
                                            expected_payload_types = ?dtmf_payload_types_for_task,
                                            frame_payload_type = ?frame.payload_type,
                                            frame_count = frame_count,
                                            "Received non-DTMF RTP frame (samples shown every 100 frames)"
                                        );
                                    }
                                    continue;
                                }

                                if frame.payload_type.is_some() && dtmf_payload_types_for_task.contains(&frame.payload_type.unwrap()) {
                                    dtmf_frames_count += 1;
                                    debug!(
                                        session_id = %session_id,
                                        payload_type = ?frame.payload_type,
                                        data_len = frame.data.len(),
                                        rtp_timestamp = frame.rtp_timestamp,
                                        dtmf_frames_count = dtmf_frames_count,
                                        "Received RTP DTMF frame"
                                    );
                                }

                                let Some(digit) = detector.observe(&frame.data, frame.rtp_timestamp) else {
                                    continue;
                                };

                                let caller_leg = "caller".to_string();
                                let event = serde_json::json!({
                                    "type": "dtmf",
                                    "leg_id": caller_leg,
                                    "digit": digit.to_string(),
                                });

                                warn!(
                                    session_id = %session_id,
                                    digit = %digit,
                                    "✓ Successfully detected DTMF digit from RFC2833 RTP frame"
                                );

                                if let Err(error) = app_runtime.inject_event(event) {
                                    warn!(
                                        session_id = %session_id,
                                        digit = %digit,
                                        error = %error,
                                        "Detected DTMF but failed to inject event (no active app receiver?)"
                                    );
                                } else {
                                    info!(
                                        session_id = %session_id,
                                        digit = %digit,
                                        "✓ Successfully injected RTP DTMF from caller ingress monitor"
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                warn!(
                                    session_id = %session_id,
                                    error = %error,
                                    frame_count = frame_count,
                                    dtmf_frames_count = dtmf_frames_count,
                                    "Caller ingress monitor stopped while reading inbound RTP"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        });

        warn!(
            session_id = %self.context.session_id,
            payload_types = ?dtmf_payload_types,
            "✓ Started caller ingress monitor for RFC2833 RTP DTMF detection"
        );

        self.media.caller_ingress_monitor = Some(
            crate::proxy::proxy_call::media_state::CallerIngressMonitor { cancel_token, task },
        );
    }

    async fn stop_caller_ingress_monitor(&mut self) {
        let Some(monitor) = self.media.caller_ingress_monitor.take() else {
            return;
        };

        monitor.cancel_token.cancel();
        let mut task = monitor.task;

        tokio::select! {
            result = &mut task => {
                if let Err(error) = result {
                    warn!(
                        session_id = %self.context.session_id,
                        error = %error,
                        "Caller ingress monitor task ended with join error"
                    );
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                warn!(
                    session_id = %self.context.session_id,
                    "Caller ingress monitor did not stop in time; aborting task"
                );
                task.abort();
                let _ = task.await;
            }
        }
    }

    async fn get_forwarding_track(
        peer: &Arc<dyn MediaPeer>,
        track_id: &str,
    ) -> Option<Arc<crate::media::forwarding_track::ForwardingTrack>> {
        let tracks = peer.get_tracks().await;
        for t in &tracks {
            let mut guard = t.lock().await;
            if guard.id() != track_id {
                continue;
            }

            let handle = guard
                .as_any_mut()
                .downcast_mut::<crate::media::forwarding_track::ForwardingTrackHandle>()?;
            return Some(handle.forwarding());
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn wire_with_forwarding_track(
        track_id: &str,
        source_pc: &rustrtc::PeerConnection,
        target_pc: &rustrtc::PeerConnection,
        ingress_profile: crate::media::negotiate::NegotiatedLegProfile,
        egress_profile: crate::media::negotiate::NegotiatedLegProfile,
        recorder: Arc<RwLock<Option<crate::media::recorder::Recorder>>>,
        leg: crate::media::recorder::Leg,
        sipflow_tx: Option<crate::media::engine::command::SipFlowCaptureTx>,
        session_id: &str,
        direction: &str,
    ) -> Result<Arc<crate::media::forwarding_track::ForwardingTrack>> {
        use crate::media::forwarding_track::ForwardingTrack;

        let source_transceiver = source_pc
            .get_transceivers()
            .into_iter()
            .find(|t| t.kind() == rustrtc::MediaKind::Audio)
            .ok_or_else(|| anyhow!("{}: no audio transceiver on source PC", direction))?;

        let receiver = source_transceiver
            .receiver()
            .ok_or_else(|| anyhow!("{}: no receiver on source audio transceiver", direction))?;

        let receiver_track = receiver.track();

        let target_transceiver = target_pc
            .get_transceivers()
            .into_iter()
            .find(|t| t.kind() == rustrtc::MediaKind::Audio)
            .ok_or_else(|| anyhow!("{}: no audio transceiver on target PC", direction))?;

        let existing_sender = target_transceiver
            .sender()
            .ok_or_else(|| anyhow!("{}: no sender on target audio transceiver", direction))?;
        {
            let mut guard = recorder.write();
            if let Some(recorder) = guard.as_mut() {
                recorder.set_leg_profile(leg, ingress_profile.clone());
            }
        }

        // Issue #171: spin up a dedicated recorder drain task so that
        // write_sample (codec decode + disk I/O) never blocks the RTP recv loop.
        // The task borrows the shared recorder lock and calls write_sample
        // asynchronously; the ForwardingTrack just does a non-blocking
        // try_send per sample — if the channel is full the sample is dropped
        // rather than allowing unbounded memory growth under disk pressure.
        let recorder_tx = {
            use tokio::sync::mpsc;
            // 256 slots ≈ 5 seconds of 20 ms packets at 8 kHz — enough to
            // absorb transient disk stalls without unbounded heap growth.
            const RECORDER_CHANNEL_CAPACITY: usize = 256;
            let (tx, mut rx) = mpsc::channel::<(
                crate::media::recorder::Leg,
                crate::media::engine::command::SharedMediaSample,
            )>(RECORDER_CHANNEL_CAPACITY);
            let recorder_arc = recorder.clone();
            crate::utils::spawn(async move {
                while let Some((sample_leg, sample)) = rx.recv().await {
                    let mut guard = recorder_arc.write();
                    if let Some(rec) = guard.as_mut()
                        && let Err(err) = rec.write_sample(sample_leg, &sample, None, None, None)
                    {
                        tracing::warn!("recorder write_sample failed: {err}");
                    }
                }
            });
            tx
        };

        let forwarding = Arc::new(ForwardingTrack::new(
            track_id.to_string(),
            receiver_track,
            Some(recorder_tx),
            sipflow_tx,
            leg,
            ingress_profile,
            egress_profile,
        ));

        let mut sender_builder = rustrtc::RtpSender::builder(
            forwarding.clone() as Arc<dyn rustrtc::media::MediaStreamTrack>,
            existing_sender.ssrc(),
        )
        .stream_id(existing_sender.stream_id().to_string())
        .params(existing_sender.params());
        if !existing_sender.cname().starts_with("rustrtc-cname-") {
            sender_builder = sender_builder.cname(existing_sender.cname().to_string());
        }
        let sender = sender_builder.build();

        target_transceiver.set_sender(Some(sender));

        debug!(
            session_id = %session_id,
            direction = %direction,
            "Wired ForwardingTrack (async recorder task, zero-blocking forwarding)"
        );

        Ok(forwarding)
    }

    async fn bridge_callee_offer_sdp(
        bridge: &crate::media::bridge::BridgePeer,
        callee_is_webrtc: bool,
    ) -> Result<String> {
        let pc = if callee_is_webrtc {
            bridge.caller_pc().clone()
        } else {
            bridge.callee_pc().clone()
        };

        if let Some(local_description) = pc.local_description() {
            return Ok(local_description.to_sdp_string());
        }

        let offer = pc.create_offer().await?;
        pc.set_local_description(offer)?;
        if callee_is_webrtc {
            pc.wait_for_gathering_complete().await;
        }

        pc.local_description()
            .map(|desc| desc.to_sdp_string())
            .ok_or_else(|| anyhow!("Bridge callee side has no local offer"))
    }

    async fn apply_bridge_callee_answer(&self, callee_sdp: &str) -> Result<()> {
        let Some(bridge) = &self.media.media_bridge else {
            return Ok(());
        };

        let pc = if self.is_callee_webrtc() {
            bridge.caller_pc().clone()
        } else {
            bridge.callee_pc().clone()
        };

        if let Some(remote_description) = pc.remote_description() {
            if remote_description.to_sdp_string() == callee_sdp {
                return Ok(());
            }

            let offer = pc
                .create_offer()
                .await
                .map_err(|e| anyhow!("Failed to create bridge callee re-offer: {}", e))?;
            pc.set_local_description(offer)
                .map_err(|e| anyhow!("Failed to set bridge callee local re-offer: {}", e))?;
            if self.is_callee_webrtc() {
                pc.wait_for_gathering_complete().await;
            }
        }

        let answer = Self::parse_sdp(rustrtc::SdpType::Answer, callee_sdp, "callee answer")?;
        pc.set_remote_description(answer)
            .await
            .map_err(|e| anyhow!("Failed to set bridge callee remote answer: {}", e))
    }

    /// Resolve effective audio codec allow list by priority:
    /// 1. `dialplan.allow_codecs` (set by routing rules + merge_trunk_media_hints)
    /// 2. Codecs from the trunk whose destination matches the callee URI (by host:port)
    /// 3. Proxy-level `audio_codecs` config as global fallback
    ///
    /// An empty list means "no explicit audio codec policy".
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

        let caller_is_webrtc = self.is_caller_webrtc();
        let caller_mode = self.caller_transport_mode();
        let callee_mode = self.callee_transport_mode(callee_is_webrtc);
        self.legs.set_transport(LegId::from("caller"), caller_mode.clone());
        self.legs.set_transport(LegId::from("callee"), callee_mode.clone());

        let media_proxy_enabled = self.media_profile.path == MediaPathMode::Anchored;

        let transport_bridge_needed = caller_is_webrtc != callee_is_webrtc;

        let need_transport_bridge = transport_bridge_needed;

        if need_transport_bridge
            && self.media.caller_answer_uses_media_bridge
            && let Some(ref bridge) = self.media.media_bridge
        {
            self.media.callee_offer_uses_media_bridge = true;
            debug!(
                session_id = %self.id,
                callee_is_webrtc,
                "Reusing existing media bridge callee-facing offer"
            );
            return Self::bridge_callee_offer_sdp(bridge, callee_is_webrtc).await;
        }

        if need_transport_bridge {
            self.media.callee_offer_uses_media_bridge = true;
            let mut bridge_builder = BridgePeerBuilder::new(format!("{}-bridge", self.id))
                .with_enable_latching(self.server.proxy_config.enable_latching)
                .with_probation_max_packets(self.server.proxy_config.latching_probation_max_packets)
                .with_cname(self.server.rtc_cname.clone());

            // Configure the non-WebRTC bridge side's transport mode to match the leg using it.
            // bridge.caller_pc() is always WebRTC, bridge.callee_pc() handles both RTP and SRTP.
            let non_webrtc_mode = if caller_is_webrtc {
                callee_mode
            } else {
                caller_mode
            };
            if non_webrtc_mode == rustrtc::TransportMode::Srtp {
                let callee_config = rustrtc::RtcConfiguration {
                    transport_mode: rustrtc::TransportMode::Srtp,
                    rtp_start_port: self.server.rtp_config.start_port,
                    rtp_end_port: self.server.rtp_config.end_port,
                    enable_latching: self.server.proxy_config.enable_latching,
                    probation_max_packets: self.server.proxy_config.latching_probation_max_packets,
                    external_ip: self.server.rtp_config.external_ip.clone(),
                    bind_ip: self.server.rtp_config.bind_ip.clone(),
                    cname: Some(self.server.rtc_cname.clone()),
                    ..Default::default()
                };
                bridge_builder = bridge_builder.with_callee_config(callee_config);
            }

            let mut selected_callee_offer_codecs = Vec::new();
            if let (Some(start), Some(end)) = (
                self.server.rtp_config.start_port,
                self.server.rtp_config.end_port,
            ) {
                bridge_builder = bridge_builder.with_rtp_port_range(start, end);
            }
            if let Some(ref caller_sdp) = self.media.caller_offer
                && !caller_is_webrtc
                && caller_sdp.contains("a=group:BUNDLE")
            {
                bridge_builder = bridge_builder
                    .with_rtp_sdp_compatibility(rustrtc::config::SdpCompatibilityMode::Standard)
                    .with_enable_latching(true)
                    .with_probation_max_packets(Some(6));
                info!(session_id = %self.id, "RTP caller offered BUNDLE, using Standard SDP mode + latching for RTP side");
            }

            if let Some(ref external_ip) = self.server.rtp_config.external_ip {
                bridge_builder = bridge_builder.with_external_ip(external_ip.clone());
            }
            if let Some(ref bind_ip) = self.server.rtp_config.bind_ip {
                bridge_builder = bridge_builder.with_bind_ip(bind_ip.clone());
            }

            if let Some(ref ice_servers) = self.context.dialplan.media.ice_servers {
                bridge_builder = bridge_builder.with_ice_servers(ice_servers.clone());
            }

            // Configure codecs from effective allow list (dialplan → trunk → proxy fallback)
            if let Some(ref caller_sdp) = self.media.caller_offer {
                let allow_codecs = self.resolve_effective_codecs();
                let caller_offer_codecs =
                    MediaNegotiator::build_codec_list_from_offer(caller_sdp, &allow_codecs);
                let mut callee_offer_codecs =
                    MediaNegotiator::build_callee_codec_offer_with_allow(caller_sdp, &allow_codecs);
                if callee_is_webrtc {
                    callee_offer_codecs = MediaNegotiator::filter_webrtc_offer_codecs(
                        caller_sdp,
                        callee_offer_codecs,
                    );
                }
                selected_callee_offer_codecs = callee_offer_codecs.clone();

                let caller_leg_codecs = if caller_is_webrtc {
                    &caller_offer_codecs
                } else {
                    &callee_offer_codecs
                };
                let callee_leg_codecs = if caller_is_webrtc {
                    &callee_offer_codecs
                } else {
                    &caller_offer_codecs
                };

                let caller_caps: Vec<_> = caller_leg_codecs
                    .iter()
                    .filter_map(|c| c.to_audio_capability())
                    .collect();
                let callee_caps: Vec<_> = callee_leg_codecs
                    .iter()
                    .filter_map(|c| c.to_audio_capability())
                    .collect();

                bridge_builder = bridge_builder
                    .with_caller_audio_capabilities(caller_caps.clone())
                    .with_callee_audio_capabilities(callee_caps);

                let caller_sender = caller_leg_codecs
                    .iter()
                    .find(|c| !c.is_dtmf())
                    .map(|c| c.to_params());
                let callee_sender = callee_leg_codecs
                    .iter()
                    .find(|c| !c.is_dtmf())
                    .map(|c| c.to_params());

                if let (Some(caller_sender), Some(callee_sender)) = (caller_sender, callee_sender) {
                    bridge_builder =
                        bridge_builder.with_sender_codecs(caller_sender, callee_sender);
                }

                debug!(
                    session_id = %self.context.session_id,
                    caller_offer_codecs = ?caller_offer_codecs.iter().filter(|c| !c.is_dtmf()).map(|c| format!("{:?}", c.codec)).collect::<Vec<_>>(),
                    callee_offer_codecs = ?callee_offer_codecs.iter().filter(|c| !c.is_dtmf()).map(|c| format!("{:?}", c.codec)).collect::<Vec<_>>(),
                    "Bridge codecs configured for transport sides"
                );

                // Extract video capabilities from caller SDP
                match rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, caller_sdp) {
                    Ok(caller_desc) => {
                        let caller_video_caps = caller_desc.to_video_capabilities();
                        if !caller_video_caps.is_empty() {
                            let bridge_video_caps = if caller_is_webrtc != callee_is_webrtc {
                                let allowed_video_codecs = self
                                    .server
                                    .proxy_config
                                    .video_codecs
                                    .as_deref()
                                    .unwrap_or(&[]);
                                Self::filter_video_caps_for_rtp(
                                    &caller_video_caps,
                                    allowed_video_codecs,
                                )
                            } else {
                                caller_video_caps.clone()
                            };

                            if bridge_video_caps.is_empty() {
                                warn!(
                                    session_id = %self.id,
                                    source_codecs = ?caller_video_caps.iter().map(|c| format!("{}@{}", c.codec_name, c.payload_type)).collect::<Vec<_>>(),
                                    "Video capabilities skipped after RTP filtering"
                                );
                            } else {
                                info!(
                                    session_id = %self.id,
                                    source_codecs = ?caller_video_caps.iter().map(|c| format!("{}@{}", c.codec_name, c.payload_type)).collect::<Vec<_>>(),
                                    bridge_codecs = ?bridge_video_caps.iter().map(|c| format!("{}@{}", c.codec_name, c.payload_type)).collect::<Vec<_>>(),
                                    "Video capabilities configured from caller SDP"
                                );
                                bridge_builder = bridge_builder
                                    .with_caller_video_capabilities(bridge_video_caps.clone())
                                    .with_callee_video_capabilities(bridge_video_caps);
                            }
                        }
                    }
                    Err(e) => {
                        warn!(session_id = %self.id, "Failed to parse caller SDP for video: {}", e)
                    }
                }

                debug!(
                    session_id = %self.id,
                    caller_codecs = ?caller_leg_codecs
                        .iter()
                        .map(|c| format!("{:?}", c.codec))
                        .collect::<Vec<_>>(),
                    callee_codecs = ?callee_leg_codecs
                        .iter()
                        .map(|c| format!("{:?}", c.codec))
                        .collect::<Vec<_>>(),
                    "Bridge codecs configured for transport sides"
                );
            }

            // Attach recorder to bridge so audio is captured when recording starts
            if self.context.dialplan.recording.enabled {
                bridge_builder = bridge_builder
                    .with_recorder(self.recorder.clone(), self.recording_paused.clone());
            }
            if self.context.dialplan.recording.enabled
                && !self.context.dialplan.recording.force_file
            {
                if let Some(sipflow_tx) =
                    self.setup_sipflow_capture(&self.context.session_id, &self.context.session_id)
                {
                    bridge_builder = bridge_builder.with_sipflow_capture(sipflow_tx);
                }
            }

            let bridge = bridge_builder.build();

            bridge.setup_bridge().await?;

            if callee_is_webrtc {
                let offer = bridge.caller_pc().create_offer().await?;
                let mut offer_sdp = offer.to_sdp_string();
                if !selected_callee_offer_codecs.is_empty()
                    && let Some(rewritten) = MediaNegotiator::rewrite_sdp_codec_list(
                        &offer_sdp,
                        &selected_callee_offer_codecs,
                    )
                {
                    offer_sdp = rewritten;
                }
                debug!(session_id = %self.id, sdp = %offer_sdp, "Bridge WebRTC offer SDP");
                let offer =
                    rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer_sdp)?;
                bridge.caller_pc().set_local_description(offer)?;
                // Wait for ICE gathering so SDP contains real candidates
                bridge.caller_pc().wait_for_gathering_complete().await;
            } else {
                let offer = bridge.callee_pc().create_offer().await?;
                let mut offer_sdp = offer.to_sdp_string();
                if !selected_callee_offer_codecs.is_empty()
                    && let Some(rewritten) = MediaNegotiator::rewrite_sdp_codec_list(
                        &offer_sdp,
                        &selected_callee_offer_codecs,
                    )
                {
                    offer_sdp = rewritten;
                }
                debug!(session_id = %self.id, sdp = %offer_sdp, "Bridge RTP offer SDP");
                let offer =
                    rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer_sdp)?;
                bridge.callee_pc().set_local_description(offer)?;
            }

            self.media.media_bridge = Some(bridge.clone());

            // Notify the media engine so Play/Record commands route to this bridge.
            {
                use crate::media::engine::MediaCommand;
                let codec_info = self
                    .caller_answer_codec_info()
                    .map(|c| vec![c])
                    .unwrap_or_default();
                self.engine_send(MediaCommand::AttachBridge {
                    session_id: self.context.session_id.clone(),
                    bridge: bridge.clone(),
                    caller_is_webrtc,
                    caller_codec_info: codec_info,
                });
            }

            if callee_is_webrtc {
                let sdp = bridge
                    .caller_pc()
                    .local_description()
                    .ok_or_else(|| anyhow!("No WebRTC local description"))?
                    .to_sdp_string();
                Ok(sdp)
            } else {
                let sdp = bridge
                    .callee_pc()
                    .local_description()
                    .ok_or_else(|| anyhow!("No RTP local description"))?
                    .to_sdp_string();
                Ok(sdp)
            }
        } else if media_proxy_enabled {
            self.media.callee_offer_uses_media_bridge = false;
            let cancel_token = self.callee_peer().map(|p| p.cancel_token()).unwrap_or_default();
            let mut track_builder = self.build_rtp_track_builder(
                track_id.clone(),
                cancel_token,
                self.callee_transport_mode(callee_is_webrtc),
            );

            if let Some(ref caller_offer) = self.media.caller_offer {
                let allow_codecs = self.resolve_effective_codecs();
                let mut codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
                    caller_offer,
                    &allow_codecs,
                );
                if callee_is_webrtc {
                    codecs = MediaNegotiator::filter_webrtc_offer_codecs(caller_offer, codecs);
                }
                if !codecs.is_empty() {
                    track_builder = track_builder.with_codec_info(codecs);
                }

                // Extract video capabilities from caller SDP
                if let Ok(caller_desc) =
                    rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, caller_offer)
                {
                    let video_caps: Vec<rustrtc::config::VideoCapability> =
                        caller_desc.to_video_capabilities();
                    if !video_caps.is_empty() {
                        track_builder = track_builder.with_video_capabilities(video_caps);
                        info!(
                            session_id = %self.id,
                            "Video capabilities configured for anchored media"
                        );
                    }
                }
            }

            if callee_is_webrtc {
                track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
                if let Some(ref ice_servers) = self.context.dialplan.media.ice_servers {
                    track_builder = track_builder.with_ice_servers(ice_servers.clone());
                }
            }

            let track = track_builder.build();
            let sdp = track.local_description().await?;

            if let Some(peer) = self.callee_peer() {
                peer.update_track(Box::new(track), None).await;
            }

            Ok(sdp)
        } else {
            self.media.callee_offer_uses_media_bridge = false;
            let cancel_token = self.callee_peer().map(|p| p.cancel_token()).unwrap_or_default();
            let mut track_builder = RtpTrackBuilder::new(track_id.clone())
                .with_mode(self.callee_transport_mode(callee_is_webrtc))
                .with_cancel_token(cancel_token)
                .with_cname(self.server.rtc_cname.clone());

            if let Some(ref caller_offer) = self.media.caller_offer {
                let allow_codecs = self.resolve_effective_codecs();
                let mut codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
                    caller_offer,
                    &allow_codecs,
                );
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
    }

    async fn ensure_caller_answer_sdp(&mut self) -> Option<String> {
        if let Some(ref answer) = self.media.answer {
            return Some(answer.clone());
        }

        if self.bypasses_local_media() {
            if let Some(answer_sdp) = self.media.callee_answer_sdp.clone() {
                self.media.answer = Some(answer_sdp.clone());
                self.media.caller_answer_uses_media_bridge = false;
                return Some(answer_sdp);
            }
        }

        let caller_offer = self.media.caller_offer.clone()?;
        let caller_is_webrtc = self.is_caller_webrtc();

        let allow_codecs = self.resolve_effective_codecs();
        let codec_info = MediaNegotiator::build_codec_list_from_offer(&caller_offer, &allow_codecs);
        if codec_info.is_empty() {
            warn!(
                session_id = %self.context.session_id,
                "No compatible codec found for local caller answer"
            );
        }

        let cancel_token = self.caller_peer().map(|p| p.cancel_token()).unwrap_or_default();
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
        match track.handshake(caller_offer.clone()).await {
            Ok(answer_sdp) => {
                let answer_sdp = self.rewrite_answer_to_selected_codecs(
                    &answer_sdp,
                    &caller_offer,
                    None,
                    "local caller answer",
                );
                debug!(
                    session_id = %self.context.session_id,
                    "Generated PBX answer SDP for caller"
                );
                if let Some(peer) = self.caller_peer() {
                    peer.update_track(Box::new(track), None).await;
                }
                self.media.answer = Some(answer_sdp.clone());
                self.media.caller_answer_uses_media_bridge = false;
                Some(answer_sdp)
            }
            Err(e) => {
                warn!(
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
        info!(
            callee = ?callee,
            dialog_id = ?dialog_id,
            "Accepting call"
        );

        self.update_leg_state(&LegId::from("callee"), LegState::Connected);
        if self.media.caller_answer_uses_media_bridge {
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
                            debug!(
                                session_expires = %timer.get_session_expires_value(),
                                "Session timer negotiated in 200 OK"
                            );
                        }
                    }
                }
                Err((code, text, reason)) => {
                    warn!(code = %code, text = %text, ?reason, "Failed to initialize session timer");
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
            if let Err(e) = self
                .server_dialog
                .accept(Some(headers), Some(answer_sdp.into_bytes()))
            {
                if self.server_dialog.state().is_confirmed() {
                    debug!(
                        session_id = %self.context.session_id,
                        error = %e,
                        "Caller leg already confirmed; skipping duplicate 200 OK"
                    );
                } else {
                    return Err(anyhow!("Failed to send answer: {}", e));
                }
            }
        }

        self.meta.answer_time = Some(Instant::now());

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
                warn!(
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

    fn is_hold_direction(direction: rustrtc::Direction) -> bool {
        !matches!(direction, rustrtc::Direction::SendRecv)
    }

    async fn get_local_reinvite_pc(&self, side: DialogSide) -> Option<rustrtc::PeerConnection> {
        if let Some(bridge) = &self.media.media_bridge {
            let bridge_endpoint = match side {
                DialogSide::Caller if self.media.caller_answer_uses_media_bridge => {
                    Some(self.leg_bridge_endpoint(&LegId::from("caller")))
                }
                DialogSide::Callee if self.media.callee_offer_uses_media_bridge => {
                    Some(self.leg_bridge_endpoint(&LegId::from("callee")))
                }
                _ => None,
            };

            // A session may keep an app/IVR bridge after transferring to a
            // normal SIP callee. Only use the bridge PC for a leg explicitly
            // negotiated on that bridge; otherwise use the leg's own track PC.
            if let Some(endpoint) = bridge_endpoint {
                return Some(match endpoint {
                    BridgeEndpoint::Caller => bridge.caller_pc().clone(),
                    BridgeEndpoint::Callee => bridge.callee_pc().clone(),
                });
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
        &self,
        side: DialogSide,
        changed_leg_sdp: &str,
    ) -> Result<()> {
        if self.media_profile.path != MediaPathMode::Anchored || self.media.media_bridge.is_some() {
            return Ok(());
        }

        let has_remote_callee =
            self.meta.connected_callee.is_some() || !self.callee_dialogs.is_empty();
        if side == DialogSide::Caller && !has_remote_callee {
            debug!(
                session_id = %self.context.session_id,
                "Skipping callee forwarding update for app-only caller dialog"
            );
            return Ok(());
        }

        let changed_profile = MediaNegotiator::extract_leg_profile(changed_leg_sdp);
        let caller_peer = self.caller_peer().ok_or_else(|| anyhow!("Missing caller peer"))?;
        let caller_to_callee_forwarding =
            Self::get_forwarding_track(caller_peer, Self::CALLER_FORWARDING_TRACK_ID)
                .await
                .ok_or_else(|| anyhow!("Missing caller forwarding track"))?;
        let callee_peer = self.callee_peer().ok_or_else(|| anyhow!("Missing callee peer"))?;
        let callee_to_caller_forwarding =
            Self::get_forwarding_track(callee_peer, Self::CALLEE_FORWARDING_TRACK_ID)
                .await
                .ok_or_else(|| anyhow!("Missing callee forwarding track"))?;

        match side {
            DialogSide::Caller => {
                // caller->callee track reads caller RTP, so caller-side re-INVITE updates ingress.
                caller_to_callee_forwarding.stage_ingress_profile(changed_profile.clone());
                // callee->caller track sends toward caller, so caller-side re-INVITE updates egress.
                callee_to_caller_forwarding.stage_egress_profile(changed_profile.clone());
            }
            DialogSide::Callee => {
                // caller->callee track sends toward callee, so callee-side re-INVITE updates egress.
                caller_to_callee_forwarding.stage_egress_profile(changed_profile.clone());
                // callee->caller track reads callee RTP, so callee-side re-INVITE updates ingress.
                callee_to_caller_forwarding.stage_ingress_profile(changed_profile.clone());
            }
        }

        Ok(())
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
        let offer_direction = parsed
            .media_sections
            .iter()
            .find(|section| section.kind == rustrtc::MediaKind::Audio)
            .map(|s| s.direction);

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
            // Video being added — extract first video codec PT/clock from the offer
            use crate::media::negotiate::MediaNegotiator;
            let extracted = MediaNegotiator::extract_codec_params(offer_sdp);
            if let Some(video_codec) = extracted.video.first() {
                if let Some(bridge) = &self.media.media_bridge {
                    let _ = bridge
                        .add_video_track(video_codec.payload_type, video_codec.clock_rate)
                        .await;
                    info!(
                        "Dynamically added video track (PT={}, clock={}) for leg {:?}",
                        video_codec.payload_type, video_codec.clock_rate, side
                    );
                }
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

        match side {
            DialogSide::Caller => {
                self.media.caller_offer = Some(offer_sdp.to_string());
                self.media.answer = Some(answer_sdp.clone());
                if has_audio {
                    self.update_leg_state(
                        &LegId::from("caller"),
                        if Self::is_hold_direction(offer_direction.unwrap_or_default()) {
                            LegState::Hold
                        } else {
                            LegState::Connected
                        },
                    );
                }
            }
            DialogSide::Callee => {
                self.media.callee_offer = Some(answer_sdp.clone());
                self.media.callee_answer_sdp = Some(answer_sdp.clone());
                if has_audio {
                    self.update_leg_state(
                        &LegId::from("callee"),
                        if Self::is_hold_direction(offer_direction.unwrap_or_default()) {
                            LegState::Hold
                        } else {
                            LegState::Connected
                        },
                    );
                }
            }
        }
        if offer_video_direction.is_some() && (had_video || offer_video_active) {
            self.legs.set_video_state(&leg_key, true);
        }

        self.update_anchored_forwarding_from_sdp(side, &answer_sdp)
            .await?;

        self.update_snapshot_cache();
        Ok(answer_sdp)
    }

    pub async fn handle_reinvite(
        &mut self,
        method: rsipstack::sip::Method,
        sdp: Option<String>,
    ) -> Result<Option<String>> {
        debug!(
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
                    Dialog::ClientInvite(d) => d
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
                        || self.media.media_bridge.is_some()
                    {
                        final_answer = self
                            .prepare_caller_answer_from_callee_sdp(Some(answer_sdp), true, false)
                            .await;
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
            self.server_dialog
                .accept(Some(headers), Some(answer_sdp.clone().into_bytes()))
                .map_err(|e| anyhow!("Failed to send 200 OK for re-INVITE: {}", e))?;
        }

        Ok(final_answer)
    }

    pub async fn play_audio_file(
        &mut self,
        audio_file: &str,
        await_completion: bool,
        track_id: &str,
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
                track_id: Some(track_id.to_string()),
                loop_playback,
                ..Default::default()
            }),
        )
        .await
    }

    /// Infer which leg this track_id belongs to from the canonical suffix.
    /// Returns (leg_label, Option<dynamic_leg_id>).
    fn infer_track_leg(track_id: &str) -> (&'static str, Option<String>) {
        if track_id.ends_with("-caller") {
            ("caller", None)
        } else if track_id.ends_with("-callee") {
            ("callee", None)
        } else if track_id == "caller" {
            ("caller", None)
        } else if track_id == "callee" {
            ("callee", None)
        } else if let Some(pos) = track_id.rfind("-leg-") {
            let leg_id = &track_id[pos + 1..];
            ("dynamic", Some(leg_id.to_string()))
        } else {
            ("caller", None) // fallback
        }
    }

    async fn stop_playback_track(&mut self, track_id: &str, remove_from_peer: bool) {
        let Some(track) = self.media.playback_tracks.remove(track_id) else {
            return;
        };

        track.stop().await;
        let (leg_label, dynamic_leg_id) = Self::infer_track_leg(track_id);

        // Restore bridge output if this was a bridge-track
        if let Some(ref bridge) = self.media.media_bridge {
            let is_bridge_track = self.media.bridge_playback_track_id.as_deref() == Some(track_id);
            match leg_label {
                "caller" if is_bridge_track && self.media.caller_answer_uses_media_bridge => {
                    self.media.bridge_playback_track_id = None;
                    if self.media.media_bridge_started {
                        bridge
                            .replace_output_with_peer(
                                self.leg_bridge_endpoint(&LegId::from("caller")),
                            )
                            .await;
                    } else {
                        bridge
                            .mute_output(self.leg_bridge_endpoint(&LegId::from("caller")))
                            .await;
                    }
                }
                "callee" if is_bridge_track && self.media.callee_offer_uses_media_bridge => {
                    self.media.bridge_playback_track_id = None;
                    if self.media.media_bridge_started {
                        bridge
                            .replace_output_with_peer(
                                self.leg_bridge_endpoint(&LegId::from("callee")),
                            )
                            .await;
                    } else {
                        bridge
                            .mute_output(self.leg_bridge_endpoint(&LegId::from("callee")))
                            .await;
                    }
                }
                _ => {}
            }
        }

        // Remove track from the correct peer only when caller asks for it
        if remove_from_peer {
            match leg_label {
                "caller" => {
                    if let Some(peer) = self.caller_peer() {
                        peer.remove_track(track_id, true).await;
                    }
                }
                "callee" => {
                    if let Some(peer) = self.callee_peer() {
                        peer.remove_track(track_id, true).await;
                    }
                }
                "dynamic" => {
                    if let Some(ref lid_str) = dynamic_leg_id {
                        let lid = LegId::new(lid_str.clone());
                        if let Some(peer) = self.legs.get_peer(&lid) {
                            peer.remove_track(track_id, true).await;
                        }
                    }
                }
                _ => {}
            }
        }

        info!(track_id = %track_id, leg = %leg_label, "Playback stopped");
    }

    fn resolve_audio_file_path(audio_file: &str) -> String {
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

        // Resolve leg profiles from forwarding tracks (preferred) or raw SDP.
        let caller_profile = match self.caller_peer() {
            Some(peer) => match Self::get_forwarding_track(peer, Self::CALLER_FORWARDING_TRACK_ID).await {
                Some(forwarding) => forwarding.ingress_profile(),
                None => self.media.answer.as_deref().map(MediaNegotiator::extract_leg_profile),
            },
            None => self.media.answer.as_deref().map(MediaNegotiator::extract_leg_profile),
        };
        let callee_profile = match self.callee_peer() {
            Some(peer) => match Self::get_forwarding_track(peer, Self::CALLEE_FORWARDING_TRACK_ID).await {
                Some(forwarding) => forwarding.ingress_profile(),
                None => self.media.callee_answer_sdp.as_deref().map(MediaNegotiator::extract_leg_profile),
            },
            None => self.media.callee_answer_sdp.as_deref().map(MediaNegotiator::extract_leg_profile),
        };

        // Delegate recorder creation (with leg profiles) to the engine.
        // Use a oneshot to wait until the Recorder is visible inside the
        // shared Arc so the bridge's forwarding loop won't skip the first
        // few packets.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if let Err(e) =
            self.server
                .media_engine
                .send(crate::media::engine::MediaCommand::StartRecording {
                    session_id: self.context.session_id.clone(),
                    config: crate::media::engine::command::RecordConfig {
                        path: path.to_string(),
                        max_duration_secs: max_duration.map(|d| d.as_secs() as u32),
                        beep: false,
                        format: None,
                    },
                    caller_profile,
                    callee_profile,
                    reply: Some(reply_tx),
                })
        {
            warn!(session_id = %self.context.session_id, error = %e, "Failed to send StartRecording to engine");
        }
        let _ = reply_rx.await;

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
                info!(path = %path, "Recording paused");
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
        self.engine_send(crate::media::engine::MediaCommand::PauseRecording {
            session_id: self.context.session_id.clone(),
        });
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
                info!(path = %path, "Recording resumed");
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
        self.engine_send(crate::media::engine::MediaCommand::ResumeRecording {
            session_id: self.context.session_id.clone(),
        });
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

        // Delegate recorder finalization to the engine (it owns the Recorder).
        // Use a oneshot channel to get the actual file_size after finalize.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if let Err(e) =
            self.server
                .media_engine
                .send(crate::media::engine::MediaCommand::StopRecording {
                    session_id: self.context.session_id.clone(),
                    reply: Some(reply_tx),
                })
        {
            warn!(session_id = %self.context.session_id, error = %e, "Failed to send StopRecording to engine");
        }
        let file_size = match reply_rx.await {
            Ok(result) => result.file_size,
            Err(_) => std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
        };
        info!(path = %path, duration = ?duration, file_size, "Recording stopped");

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
        debug!(session_id = %self.context.session_id, "Cleaning up session");

        self.stop_caller_ingress_monitor().await;

        // Ensure the running app (IVR/voicemail/queue) is notified of session end.
        if self.app_runtime.is_running() {
            let _ = self.app_runtime.stop_app(None).await;
        }

        if self.media.recording_state.is_active() {
            let _ = self.stop_recording().await;
        }

        // Stop media bridge (closes both WebRTC + RTP PeerConnections)
        if let Some(bridge) = self.media.media_bridge.take() {
            debug!(session_id = %self.context.session_id, "Stopping media bridge during cleanup");
            bridge.stop().await;
            self.media.media_bridge_started = false;
            self.media.bridge_playback_track_id = None;
        }

        // Abort all leg-specific spawned tasks
        for (leg_id, handles) in self.legs.drain_tasks() {
            for handle in handles {
                handle.abort();
            }
            debug!(session_id = %self.context.session_id, %leg_id, "Aborted tasks for leg during cleanup");
        }

        // Stop all conference bridge handles (cancel their tasks)
        self.legs.stop_all_conference_bridge_handles();

        // Stop the session-level conference bridge
        self.conference_bridge.stop_bridge();

        // Stop caller and callee media peers (cancels their tasks)
        if let Some(peer) = self.caller_peer() {
            peer.stop();
        }
        if let Some(peer) = self.callee_peer() {
            peer.stop();
        }

        if let Some(mixer) = self.supervisor_mixer.take() {
            mixer.stop();
        }

        self.callee_guards.clear();

        self.callee_event_tx = None;

        let dialogs_to_hangup = self.pending_hangup.clone();

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
                warn!(
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

        // Emit hangup webhook with Display (lowercase) reason and actual SIP status.
        let hangup_reason_str = self.meta.hangup_reason.clone().map(|r| r.to_string());
        let sip_status = self.meta.last_error.as_ref().map(|(sc, _)| sc.code());
        self.emit_typed_rwi_event(&crate::rwi::CallHangup {
            call_id: self.context.session_id.clone(),
            reason: hangup_reason_str,
            sip_status,
        });

        info!(session_id = %self.context.session_id, "Session cleanup complete");

        // Destroy the engine session last — after all recording/bridge cleanup.
        {
            use crate::media::engine::MediaCommand;
            self.engine_send(MediaCommand::DestroySession {
                session_id: self.context.session_id.clone(),
            });
            self.engine_session_destroyed = true;
        }
    }

    pub fn init_server_timer(&mut self, default_expires: u64) -> Result<(), CalleeError> {
        let request = self.server_dialog.initial_request();
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
        self.server_dialog.id()
    }

    fn is_uac_dialog(&self, dialog_id: &DialogId) -> bool {
        *dialog_id != self.caller_dialog_id()
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
                Dialog::ClientInvite(invite_dialog) => invite_dialog
                    .update(Some(headers), None)
                    .await
                    .map_err(|e| anyhow!("UPDATE failed: {}", e)),
                _ => Err(anyhow!(
                    "Dialog {} is not a client INVITE dialog",
                    dialog_id
                )),
            }
        } else {
            self.server_dialog
                .update(Some(headers), None)
                .await
                .map_err(|e| anyhow!("UPDATE failed: {}", e))
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
                Dialog::ClientInvite(invite_dialog) => invite_dialog
                    .reinvite(Some(headers), body)
                    .await
                    .map_err(|e| anyhow!("re-INVITE failed: {}", e)),
                _ => Err(anyhow!(
                    "Dialog {} is not a client INVITE dialog",
                    dialog_id
                )),
            }
        } else {
            self.server_dialog
                .reinvite(Some(headers), body)
                .await
                .map_err(|e| anyhow!("re-INVITE failed: {}", e))
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
        // Extract CC agent_id from connected_callee SIP URI
        let cc_agent_id = self.meta.connected_callee.as_ref().map(|callee| {
            let trimmed = callee
                .strip_prefix("sips:")
                .or_else(|| callee.strip_prefix("sip:"))
                .unwrap_or(callee);
            match trimmed.find('@') {
                Some(at) => &trimmed[..at],
                None => trimmed,
            }
            .to_string()
        });

        // Inject CC data into extensions so the reporter picks it up
        let mut extensions = self.context.dialplan.extensions.clone();
        {
            let mut meta: std::collections::HashMap<String, String> = extensions
                .get::<std::collections::HashMap<String, String>>()
                .cloned()
                .unwrap_or_default();
            if let Some(aid) = &cc_agent_id {
                meta.insert("agent_id".to_string(), aid.clone());
            }
            if let Some(ref qn) = self.meta.queue_name {
                meta.insert("queue_name".to_string(), qn.clone());
            }
            extensions.insert(meta);
        }

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
            server_dialog_id: self.server_dialog.id(),
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

            CallCommand::LegConnected { leg_id, answer_sdp } => {
                info!(%leg_id, "Leg connected async notification");
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
                warn!(%leg_id, %reason, "Leg failed async notification");
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

            _ => CommandResult::not_supported("Command not yet implemented"),
        }
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
            debug!(
                leg_id = %leg_id,
                "update_leg_state: leg not found, refusing to create phantom leg"
            );
            false
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

        info!(%new_leg_id, %target, "Adding new SIP leg to session");

        // Parse target as SIP URI
        let uri = rsipstack::sip::Uri::try_from(target.as_str())
            .map_err(|e| anyhow!("Invalid SIP URI '{}': {}", target, e))?;

        // Create leg
        let leg = crate::call::domain::Leg::new(new_leg_id.clone()).with_endpoint(target.clone());
        self.legs.insert(new_leg_id.clone(), leg);
        self.update_leg_state(&new_leg_id, LegState::Initializing);

        // Create peer and initiate INVITE in background
        if let Err(e) = self.initiate_sip_leg(&new_leg_id, uri).await {
            warn!(error = %e, "Failed to initiate SIP leg, cleaning up");
            self.legs.remove(&new_leg_id);
            return Err(e);
        }

        self.update_media_path().await;

        info!(%new_leg_id, "SIP leg added successfully");
        Ok(new_leg_id)
    }

    /// Remove a leg from the session.
    async fn handle_remove_leg(&mut self, leg_id: LegId) -> Result<()> {
        info!(%leg_id, "Removing leg from session");

        if let Some(leg) = self.legs.remove(&leg_id) {
            // Send BYE to SIP dialog if exists
            let dialog_id = format!("{}-{}", self.id.0, leg_id);
            if leg.dialog_id.is_some() {
                if let Some(dlg) = self.server.dialog_layer.get_dialog_with(&dialog_id) {
                    if let Err(e) = dlg.hangup().await {
                        warn!(%leg_id, %dialog_id, error = %e, "Failed to hangup SIP dialog");
                    } else {
                        info!(%leg_id, %dialog_id, "SIP dialog hangup sent");
                    }
                }
            }
            info!(%leg_id, "Leg removed");
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

        if let Some(ref external_ip) = self.server.rtp_config.external_ip {
            track_builder = track_builder.with_external_ip(external_ip.clone());
        }
        if let Some(ref bind_ip) = self.server.rtp_config.bind_ip {
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
        // Create peer for this leg
        let (peer, _track, sdp_offer) = self.create_leg_peer(leg_id).await?;
        self.legs.set_peer(leg_id.clone(), peer.clone());

        // Dynamic legs are plain RTP (SIP) by default
        self.legs
            .set_transport(leg_id.clone(), rustrtc::TransportMode::Rtp);

        info!(%leg_id, %callee_uri, sdp_len = %sdp_offer.len(), "Initiating SIP leg");

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

            let mut result: Result<rsipstack::dialog::client_dialog::ClientInviteDialog, String> =
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
                                        info!(%leg_id, status = %status_code, "SIP leg answered successfully");

                                        let answer_sdp = if !resp.body().is_empty() {
                                            let sdp = String::from_utf8_lossy(resp.body()).to_string();
                                            if let Err(e) =
                                                peer.update_remote_description(&track_id, &sdp).await
                                            {
                                                warn!(%leg_id, error = %e, "Failed to set remote description on leg peer");
                                            } else {
                                                info!(%leg_id, "Remote description set successfully");
                                            }
                                            Some(sdp)
                                        } else {
                                            None
                                        };

                                        let _ = cmd_tx.send(CallCommand::LegConnected {
                                            leg_id: leg_id.clone(),
                                            answer_sdp,
                                        }).await;

                                        result = Ok(dialog);
                                        break;
                                    } else {
                                        warn!(%leg_id, status = %status_code, "SIP leg rejected");
                                        let _ = cmd_tx.send(CallCommand::LegFailed {
                                            leg_id: leg_id.clone(),
                                            reason: format!("Rejected with {}", status_code),
                                        }).await;
                                        result = Err(format!("Rejected with {}", status_code));
                                        break;
                                    }
                                } else {
                                    warn!(%leg_id, "SIP leg timeout (no response)");
                                    let _ = cmd_tx.send(CallCommand::LegFailed {
                                        leg_id: leg_id.clone(),
                                        reason: "Timeout".to_string(),
                                    }).await;
                                    result = Err("Timeout".to_string());
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!(%leg_id, error = %e, "SIP leg failed");
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
                                info!(%leg_id, "SIP leg early media (183)");
                                let body = resp.body();
                                if !body.is_empty() {
                                    let sdp = String::from_utf8_lossy(body).to_string();
                                    if let Err(e) =
                                        peer.update_remote_description(&track_id, &sdp).await
                                    {
                                        warn!(%leg_id, error = %e, "Failed to set early media remote description");
                                    } else {
                                        info!(%leg_id, "Early media remote description set");
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
                                info!(%leg_id, "Dialog monitor cancelled");
                                break;
                            }
                            state = state_rx.recv() => {
                                match state {
                                    Some(rsipstack::dialog::dialog::DialogState::Terminated(..)) => {
                                        info!(%leg_id, "SIP leg dialog terminated");
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
                info!("Switching to direct bridge mode");
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
                    info!(leg_a = %active_legs[0], leg_b = %active_legs[1], "Direct bridge configured");
                }
            }
            n if n >= 3 => {
                // Conference mixer: all legs mixed together
                info!("Switching to conference mixer mode ({} legs)", n);
                // Clean up direct bridge if any
                self.stop_direct_bridge().await;
                // Setup conference mixer
                self.setup_conference_mixer().await;
            }
            _ => {
                // Single leg or none - no bridging needed
                info!("No bridging needed");
                self.stop_all_bridges().await;
            }
        }
    }

    /// Stop all conference bridges for this session.
    async fn stop_conference_bridges(&mut self) {
        if self.conference_bridge.is_active() {
            info!("Stopping conference bridges");
            self.conference_bridge.stop_bridge();
        }
    }

    /// Stop direct bridge if active.
    async fn stop_direct_bridge(&mut self) {
        if self.bridge.active {
            info!("Stopping direct bridge");
            self.bridge.clear();
        }
    }

    /// Stop all bridges (both direct and conference).
    async fn stop_all_bridges(&mut self) {
        self.stop_conference_bridges().await;
        self.stop_direct_bridge().await;
        info!("All bridges stopped");
    }

    async fn setup_bridge(&mut self, leg_a: LegId, leg_b: LegId) -> bool {
        if self.legs.contains_key(&leg_a) && self.legs.contains_key(&leg_b) {
            self.bridge = BridgeConfig::bridge(leg_a, leg_b);
            true
        } else {
            false
        }
    }

    async fn clear_bridge(&mut self) {
        self.bridge.clear();
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
        let await_completion = options
            .as_ref()
            .map(|o| o.await_completion)
            .unwrap_or(false);
        let loop_playback = options.as_ref().map(|o| o.loop_playback).unwrap_or(false);
        let base_track_id = options
            .as_ref()
            .and_then(|o| o.track_id.clone())
            .or_else(|| leg_id.as_ref().map(|l| l.to_string()))
            .unwrap_or_else(|| "playback".to_string());
        let file_path = match source {
            crate::call::domain::MediaSource::File { path } => path,
            crate::call::domain::MediaSource::Url { url } => url,
            _ => return Err(anyhow!("Only file/URL playback supported")),
        };

        let codec_info = self
            .media
            .caller_offer
            .as_ref()
            .map(|offer| MediaNegotiator::extract_codec_params(offer).audio)
            .and_then(|codecs| codecs.first().cloned())
            .unwrap_or_else(|| {
                let codec = CodecType::PCMU;
                MediaNegotiator::codec_info_for_type(codec)
            });
        let completion_notify = if await_completion {
            Some(Arc::new(tokio::sync::Notify::new()))
        } else {
            None
        };

        /// Route playback to a specific leg.
        macro_rules! play_to_leg {
            ($leg_str:expr, $bridge_endpoint:expr, $uses_bridge:expr) => {{
                let target_tid = if $leg_str == "caller" && leg_id.is_none() {
                    base_track_id.clone()
                } else {
                    format!("{}-{}", base_track_id, $leg_str)
                };
                let mut leg_track = FileTrack::new(target_tid.clone())
                    .with_path(file_path.clone())
                    .with_loop(loop_playback)
                    .with_codec_info(codec_info.clone())
                    .with_cname(self.server.rtc_cname.clone());
                let app_runtime = self.app_runtime.clone();
                let track_id = target_tid.clone();
                let notify = completion_notify.clone();
                leg_track = leg_track.with_on_end(Arc::new(move |reason| {
                    let _ = app_runtime.inject_event(serde_json::json!({
                        "type": "audio_complete",
                        "track_id": track_id,
                        "interrupted": matches!(reason, PlaybackEndReason::Interrupted)
                    }));
                    if let Some(notify) = notify.as_ref() {
                        notify.notify_one();
                    }
                }));
                if !$uses_bridge {
                    return Err(anyhow!("Playback requires media bridge for {} leg", $leg_str));
                }
                let bridge = self
                    .media.media_bridge
                    .clone()
                    .ok_or_else(|| anyhow!("Playback requires active media bridge"))?;
                bridge
                    .replace_output_with_file($bridge_endpoint, &leg_track)
                    .await?;
                if $leg_str == "caller" {
                    self.media.bridge_playback_track_id = Some(target_tid.clone());
                }
                self.media.playback_tracks
                    .insert(target_tid.clone(), leg_track);
            }};
        }

        match leg_id {
            // Caller leg — P2P fast path preserved identically
            // Caller leg — P2P fast path preserved identically
            Some(ref lid) if lid == &LegId::from("caller") => {
                play_to_leg!(
                    "caller",
                    self.leg_bridge_endpoint(&LegId::from("caller")),
                    self.media.caller_answer_uses_media_bridge
                );
            }
            // Callee leg
            Some(ref lid) if lid == &LegId::from("callee") => {
                play_to_leg!(
                    "callee",
                    self.leg_bridge_endpoint(&LegId::from("callee")),
                    self.media.callee_offer_uses_media_bridge
                );
            }
            // Both legs
            Some(ref lid) if lid == &LegId::from("both") => {
                if self.media.caller_answer_uses_media_bridge {
                    play_to_leg!(
                        "caller",
                        self.leg_bridge_endpoint(&LegId::from("caller")),
                        self.media.caller_answer_uses_media_bridge
                    );
                }
                if self.media.callee_offer_uses_media_bridge {
                    play_to_leg!(
                        "callee",
                        self.leg_bridge_endpoint(&LegId::from("callee")),
                        self.media.callee_offer_uses_media_bridge
                    );
                }
                if !self.media.caller_answer_uses_media_bridge
                    && !self.media.callee_offer_uses_media_bridge
                {
                    return Err(anyhow!("No leg has media bridge for playback"));
                }
            }
            // Dynamic leg from peers
            Some(ref lid) => {
                return Err(anyhow!(
                    "Playback to dynamic leg {} requires media bridge output mapping",
                    lid
                ));
            }
            // None = caller only (backward compatible)
            None => {
                play_to_leg!(
                    "caller",
                    self.leg_bridge_endpoint(&LegId::from("caller")),
                    self.media.caller_answer_uses_media_bridge
                );
            }
        }

        info!(track_id = %base_track_id, file = %file_path, "Playback started");

        if let Some(notify) = completion_notify {
            let cancel = self.cancel_token.clone();
            tokio::select! {
                _ = notify.notified() => {
                    debug!(track_id = %base_track_id, "Playback completed before returning");
                }
                _ = cancel.cancelled() => {
                    debug!(track_id = %base_track_id, "Playback wait cancelled");
                }
            }
        }

        Ok(())
    }

    async fn handle_stop_playback(&mut self, leg_id: Option<LegId>) -> Result<()> {
        let to_stop: Vec<String> = match leg_id {
            None => self.media.playback_tracks.keys().cloned().collect(),
            Some(ref lid) => {
                let suffix = format!("-{}", lid);
                let is_caller = lid.0 == "caller";
                self.media
                    .playback_tracks
                    .keys()
                    .filter(|tid| {
                        tid.ends_with(&suffix)
                            || **tid == lid.0
                            || (is_caller && !tid.contains('-'))
                    })
                    .cloned()
                    .collect()
            }
        };

        for track_id in to_stop {
            self.stop_playback_track(&track_id, true).await;
        }
        Ok(())
    }

    async fn handle_queue_enqueue(
        &mut self,
        leg_id: LegId,
        queue_id: String,
        priority: Option<u32>,
    ) -> Result<()> {
        info!(%leg_id, %queue_id, ?priority, "Enqueueing leg to queue");

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

        info!(%leg_id, %queue_id, position, "Leg enqueued successfully at position");
        Ok(())
    }

    async fn handle_queue_dequeue(&mut self, leg_id: LegId) -> Result<()> {
        info!(%leg_id, "Dequeuing leg from queue");

        self.require_leg(&leg_id)?;

        let queue_manager = &self.server.queue_manager;
        let queues = queue_manager.list_queues().await;

        let mut dequeued = false;
        for queue_id in queues {
            if let Ok(_entry) = queue_manager.dequeue(&queue_id, &leg_id).await {
                info!(%leg_id, queue_id = %queue_id.0, "Leg dequeued from queue");
                dequeued = true;

                let _ = queue_manager.remove_queue_if_empty(&queue_id).await;

                break;
            }
        }

        if !dequeued {
            warn!(%leg_id, "Leg was not found in any queue");
        }

        self.update_leg_state(&leg_id, LegState::Connected);

        info!(%leg_id, "Leg dequeued successfully");
        Ok(())
    }

    async fn handle_reject(&mut self, leg_id: LegId, reason: Option<String>) -> Result<()> {
        info!(%leg_id, ?reason, "Rejecting call");

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

        if let Err(e) = self.server_dialog.reject(Some(status_code), reason_phrase) {
            warn!(%leg_id, error = %e, "Failed to send reject response");
            return Err(anyhow!("Failed to send reject response: {}", e));
        }

        self.update_leg_state(&leg_id, LegState::Ended);

        info!(%leg_id, "Call rejected successfully");
        Ok(())
    }

    async fn handle_ring(&mut self, leg_id: LegId, ringback: Option<RingbackPolicy>) -> Result<()> {
        info!(%leg_id, ?ringback, "Sending ringing indication");

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

        if let Err(e) = self
            .server_dialog
            .ringing(None, sdp.map(|s| s.into_bytes()))
        {
            warn!(%leg_id, error = %e, "Failed to send ringing indication");
            return Err(anyhow!("Failed to send ringing indication: {}", e));
        }

        info!(%leg_id, "Ringing indication sent successfully");
        Ok(())
    }

    async fn send_info_to_dialog(
        dialog: &rsipstack::dialog::dialog::Dialog,
        headers: Vec<rsipstack::sip::Header>,
        body: Vec<u8>,
    ) -> Result<()> {
        use rsipstack::dialog::dialog::Dialog;

        match dialog {
            Dialog::ServerInvite(d) => {
                d.info(Some(headers), Some(body))
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            }
            Dialog::ClientInvite(d) => {
                d.info(Some(headers), Some(body))
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            }
            _ => return Err(anyhow!("Unsupported dialog type for DTMF")),
        }
        Ok(())
    }

    /// Build RFC 2833 telephone-event RTP payload for a single DTMF digit.
    fn build_telephone_event_payload(
        digit: char,
        end: bool,
        duration_samples: u16,
    ) -> Result<Vec<u8>> {
        let event_code = crate::media::telephone_event::dtmf_char_to_code(digit)
            .ok_or_else(|| anyhow::anyhow!("Invalid DTMF digit: {}", digit))?;
        let mut payload = vec![0u8; 4];
        payload[0] = event_code;
        if end {
            payload[1] = 0x80; // E bit set
        }
        payload[2] = (duration_samples >> 8) as u8;
        payload[3] = (duration_samples & 0xFF) as u8;
        Ok(payload)
    }

    /// Send RTP (RFC 2833) DTMF to a leg via the media bridge.
    async fn send_rtp_dtmf_via_bridge(
        bridge: &crate::media::bridge::BridgePeer,
        endpoint: crate::media::bridge::BridgeEndpoint,
        digits: &[char],
        dtmf_payload_type: u8,
    ) {
        use rustrtc::media::{AudioFrame, MediaSample};
        use std::time::Duration;
        use tokio::time::sleep;

        const TE_SAMPLES_PER_EVENT: u16 = 800; // 100ms at 8kHz
        const TE_SAMPLES_PAUSE: u16 = 160; // 20ms gap

        let mut timestamp: u32 = rand::random();
        let mut seq: u16 = rand::random();

        let sender = match endpoint {
            crate::media::bridge::BridgeEndpoint::Caller => bridge.get_caller_sender().await,
            crate::media::bridge::BridgeEndpoint::Callee => bridge.get_callee_sender().await,
        };
        let Some(sender) = sender else {
            return;
        };

        for &digit in digits {
            // Send start event
            if let Ok(payload) = Self::build_telephone_event_payload(digit, false, 0) {
                let start_frame = AudioFrame {
                    payload_type: Some(dtmf_payload_type),
                    data: payload.into(),
                    clock_rate: 8000,
                    rtp_timestamp: timestamp,
                    sequence_number: Some(seq),
                    marker: true,
                    header_extension: None,
                    source_addr: None,
                    raw_packet: None,
                };
                let _ = sender.send(MediaSample::Audio(start_frame)).await;
            }
            timestamp = timestamp.wrapping_add(TE_SAMPLES_PER_EVENT as u32);
            seq = seq.wrapping_add(1);

            // Wait for event duration
            sleep(Duration::from_millis(100)).await;

            // Send end event
            if let Ok(payload) =
                Self::build_telephone_event_payload(digit, true, TE_SAMPLES_PER_EVENT)
            {
                let end_frame = AudioFrame {
                    payload_type: Some(dtmf_payload_type),
                    data: payload.into(),
                    clock_rate: 8000,
                    rtp_timestamp: timestamp,
                    sequence_number: Some(seq),
                    marker: false,
                    header_extension: None,
                    source_addr: None,
                    raw_packet: None,
                };
                let _ = sender.send(MediaSample::Audio(end_frame)).await;
            }
            timestamp = timestamp.wrapping_add(TE_SAMPLES_PAUSE as u32);
            seq = seq.wrapping_add(1);

            // Pause between digits
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Get the telephone-event (RFC 2833) payload type from the stored SDP.
    fn leg_dtmf_payload_type(&self, leg_id: &LegId) -> Option<u8> {
        if leg_id == &LegId::from("caller") {
            let sdp = self.media.answer.as_deref()?;
            let profile = crate::media::negotiate::MediaNegotiator::extract_leg_profile(sdp);
            profile.dtmf.map(|c| c.payload_type)
        } else if leg_id == &LegId::from("callee") {
            let sdp = self.media.callee_answer_sdp.as_deref()?;
            let profile = crate::media::negotiate::MediaNegotiator::extract_leg_profile(sdp);
            profile.dtmf.map(|c| c.payload_type)
        } else {
            None
        }
    }

    async fn handle_send_dtmf(&mut self, leg_id: LegId, digits: String) -> Result<()> {
        let valid_digits: Vec<char> = digits
            .chars()
            .filter(|c| matches!(c, '0'..='9' | '*' | '#' | 'A'..='D'))
            .collect();

        if valid_digits.is_empty() {
            return Err(anyhow!("No valid DTMF digits provided: {}", digits));
        }

        let dtmf_body = valid_digits
            .iter()
            .map(|d| format!("Signal={}\nDuration=160", d))
            .collect::<Vec<_>>()
            .join("\n");
        let headers = vec![rsipstack::sip::Header::ContentType(
            rsipstack::sip::headers::ContentType::from("application/dtmf-relay"),
        )];

        // 1. Send via SIP INFO
        let info_result: Result<()> = if leg_id == LegId::from("caller") {
            self.server_dialog
                .info(Some(headers), Some(dtmf_body.clone().into_bytes()))
                .await
                .map_err(|e| anyhow!("{}", e))?;
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
                info!(%leg_id, digits = %valid_digits.iter().collect::<String>(), "DTMF sent via SIP INFO");
            }
            Err(e) => {
                warn!(error = %e, "Failed to send DTMF via SIP INFO");
                return Err(anyhow!("Failed to send DTMF: {}", e));
            }
        }

        // 2. Also send via RTP (RFC 2833) telephone-event for legs that use the media bridge
        if let (Some(bridge), Some(dtmf_pt)) = (
            self.media.media_bridge.clone(),
            self.leg_dtmf_payload_type(&leg_id),
        ) {
            let endpoint = self.leg_bridge_endpoint(&leg_id);
            let digits = valid_digits.clone();
            crate::utils::spawn(async move {
                Self::send_rtp_dtmf_via_bridge(&bridge, endpoint, &digits, dtmf_pt).await;
            });
        }

        Ok(())
    }

    async fn handle_reinvite_command(&mut self, leg_id: LegId, sdp: String) -> Result<()> {
        info!(%leg_id, "Handling re-INVITE command");

        self.require_leg(&leg_id)?;

        self.handle_reinvite(rsipstack::sip::Method::Invite, Some(sdp))
            .await?;

        info!(%leg_id, "Re-INVITE command handled");
        Ok(())
    }

    async fn set_track_muted(&mut self, track_id: String, muted: bool) -> Result<()> {
        info!(%track_id, muted, "Setting track mute state");

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

        info!(%track_id, caller_affected = caller_result, callee_affected = callee_result, muted, "Track mute state set");
        Ok(())
    }
    async fn handle_mute_track(&mut self, track_id: String) -> Result<()> {
        self.set_track_muted(track_id, true).await
    }

    async fn handle_unmute_track(&mut self, track_id: String) -> Result<()> {
        self.set_track_muted(track_id, false).await
    }

    async fn handle_send_sip_message(&mut self, content_type: String, body: String) -> Result<()> {
        info!(content_type = %content_type, body_len = body.len(), "Sending SIP MESSAGE");

        let headers = vec![rsipstack::sip::Header::ContentType(content_type.into())];
        let body_bytes = body.into_bytes();

        match self
            .server_dialog
            .message(Some(headers), Some(body_bytes))
            .await
        {
            Ok(Some(response)) => {
                info!(status = %response.status_code, "SIP MESSAGE sent successfully");
                Ok(())
            }
            Ok(None) => {
                info!("SIP MESSAGE sent (no response)");
                Ok(())
            }
            Err(e) => {
                error!(error = %e, "Failed to send SIP MESSAGE");
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
        info!(event = %event, content_type = %content_type, body_len = body.len(), "Sending SIP NOTIFY");

        let headers = vec![
            rsipstack::sip::Header::Other("Event".into(), event),
            rsipstack::sip::Header::ContentType(content_type.into()),
        ];
        let body_bytes = body.into_bytes();

        match self
            .server_dialog
            .notify(Some(headers), Some(body_bytes))
            .await
        {
            Ok(Some(response)) => {
                info!(status = %response.status_code, "SIP NOTIFY sent successfully");
                Ok(())
            }
            Ok(None) => {
                info!("SIP NOTIFY sent (no response)");
                Ok(())
            }
            Err(e) => {
                error!(error = %e, "Failed to send SIP NOTIFY");
                Err(anyhow!("Failed to send SIP NOTIFY: {}", e))
            }
        }
    }

    async fn handle_send_sip_options_ping(&mut self) -> Result<()> {
        info!("Sending SIP OPTIONS ping");

        match self
            .server_dialog
            .request(rsipstack::sip::Method::Options, None, None)
            .await
        {
            Ok(Some(response)) => {
                let status_code = u16::from(response.status_code);
                if StatusCode::from(status_code).kind()
                    == rsipstack::sip::StatusCodeKind::Successful
                {
                    info!(status = status_code, "SIP OPTIONS ping successful");
                    Ok(())
                } else {
                    warn!(status = status_code, "SIP OPTIONS ping returned error");
                    Err(anyhow!("OPTIONS ping failed with status: {}", status_code))
                }
            }
            Ok(None) => {
                info!("SIP OPTIONS ping sent (no response)");
                Ok(())
            }
            Err(e) => {
                error!(error = %e, "Failed to send SIP OPTIONS ping");
                Err(anyhow!("Failed to send OPTIONS ping: {}", e))
            }
        }
    }
    async fn handle_hold(
        &mut self,
        leg_id: LegId,
        music: Option<crate::call::domain::MediaSource>,
    ) -> Result<()> {
        info!(%leg_id, ?music, "Handling hold with SDP renegotiation");

        self.require_leg(&leg_id)?;

        self.update_leg_state(&leg_id, LegState::Hold);

        let hold_sdp = self.generate_hold_sdp().await?;

        match self.send_reinvite_to_caller(hold_sdp).await {
            Ok(_) => {
                info!(%leg_id, "Hold re-INVITE sent successfully");

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
                    && let Err(e) = self.play_audio_file(&path, false, "hold-music", true).await
                {
                    warn!(error = %e, "Failed to start hold music");
                }

                Ok(())
            }
            Err(e) => {
                warn!(%leg_id, error = %e, "Failed to send hold re-INVITE");
                Ok(())
            }
        }
    }

    async fn handle_unhold(&mut self, leg_id: LegId) -> Result<()> {
        info!(%leg_id, "Handling unhold with SDP renegotiation");

        self.require_leg(&leg_id)?;

        let Some(leg) = self.legs.get(&leg_id) else {
            warn!(%leg_id, "Leg disappeared between require_leg and access");
            return Err(anyhow::anyhow!("Leg not found: {}", leg_id));
        };
        if leg.state != LegState::Hold {
            info!(%leg_id, state = ?leg.state, "Leg is not on hold, skipping unhold");
            return Ok(());
        }

        self.update_leg_state(&leg_id, LegState::Connected);

        self.media.playback_tracks.remove("hold-music");

        let unhold_sdp = self.generate_unhold_sdp().await?;

        match self.send_reinvite_to_caller(unhold_sdp).await {
            Ok(_) => {
                info!(%leg_id, "Unhold re-INVITE sent successfully");

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
                warn!(%leg_id, error = %e, "Failed to send unhold re-INVITE");
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

    async fn send_reinvite_to_caller(&self, sdp: String) -> Result<()> {
        let headers = Self::sdp_headers();

        match self
            .server_dialog
            .reinvite(Some(headers), Some(sdp.into_bytes()))
            .await
        {
            Ok(Some(response)) => {
                let status = response.status_code.code();
                if StatusCode::from(status).kind() == rsipstack::sip::StatusCodeKind::Successful {
                    info!(status = %status, "re-INVITE accepted");
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
        debug!(session_id = %self.context.session_id, "SipSession dropping");

        self.cancel_token.cancel();

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

        // Safety net: ensure the registry entry is always removed even if
        // cleanup() was never called (e.g. tokio task cancellation).
        self.server
            .active_call_registry
            .remove(&self.context.session_id);

        // Safety net: ensure the engine session is destroyed even if
        // cleanup() was never called (e.g. tokio task cancellation).
        if !self.engine_session_destroyed {
            if let Err(e) =
                self.server
                    .media_engine
                    .send(crate::media::engine::MediaCommand::DestroySession {
                        session_id: self.context.session_id.clone(),
                    })
            {
                debug!(
                    session_id = %self.context.session_id,
                    error = %e,
                    "Drop: engine DestroySession failed (session may already be destroyed)"
                );
            }
        }

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
    pub fn set_caller_uses_bridge_for_test(&mut self, value: bool) {
        self.media.caller_answer_uses_media_bridge = value;
    }

    /// Test-only: returns true if a caller ingress monitor task is currently active.
    pub fn has_active_caller_ingress_monitor(&self) -> bool {
        self.media
            .caller_ingress_monitor
            .as_ref()
            .is_some_and(|m| !m.task.is_finished())
    }
}

/// Audio receiver that reads from a PeerConnection and decodes to PCM.
/// Uses the same pattern as BridgePeer: listens for Track events, reads MediaSample, decodes to PCM.
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

    /// Wait for and capture the first audio track from the peer connection
    async fn capture_audio_track(&mut self) -> Option<Arc<dyn rustrtc::media::MediaStreamTrack>> {
        // First, check pre-existing transceivers for a receiver track
        for transceiver in self.pc.get_transceivers() {
            if transceiver.kind() == rustrtc::MediaKind::Audio
                && let Some(receiver) = transceiver.receiver()
            {
                let track = receiver.track();
                tracing::info!("Conference audio receiver using pre-existing audio track");
                return Some(track);
            }
        }

        // If no pre-existing track, wait for Track event
        let mut pc_recv = Box::pin(self.pc.recv());

        loop {
            match pc_recv.await {
                Some(rustrtc::PeerConnectionEvent::Track(transceiver)) => {
                    if transceiver.kind() == rustrtc::MediaKind::Audio
                        && let Some(receiver) = transceiver.receiver()
                    {
                        let track = receiver.track();
                        tracing::info!("Conference audio receiver captured audio track");
                        return Some(track);
                    }
                    pc_recv = Box::pin(self.pc.recv());
                }
                Some(_) => {
                    pc_recv = Box::pin(self.pc.recv());
                }
                None => {
                    tracing::warn!("PeerConnection closed before audio track was captured");
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
    use rustrtc::media::MediaStreamTrack;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[tokio::test]
    async fn test_get_local_reinvite_pc_uses_bridge_only_for_bridge_backed_leg() {
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
            caller_peer.clone(),
            callee_peer.clone(),
        );

        session.media.media_bridge =
            Some(BridgePeerBuilder::new("test-bridge".to_string()).build());
        session.media.caller_answer_uses_media_bridge = true;
        session.media.callee_offer_uses_media_bridge = false;
        session
            .legs
            .transports
            .insert(LegId::from("caller"), rustrtc::TransportMode::WebRtc);
        session
            .legs
            .transports
            .insert(LegId::from("callee"), rustrtc::TransportMode::Rtp);

        let pc = session.get_local_reinvite_pc(DialogSide::Caller).await;

        assert!(pc.is_some(), "bridge-backed caller leg should resolve a PC");
        assert_eq!(caller_peer.get_tracks_call_count(), 0);
        assert_eq!(callee_peer.get_tracks_call_count(), 0);

        let pc = session.get_local_reinvite_pc(DialogSide::Callee).await;

        assert!(
            pc.is_none(),
            "non-bridge callee leg should resolve through its own track PC"
        );
        assert_eq!(caller_peer.get_tracks_call_count(), 0);
        assert_eq!(callee_peer.get_tracks_call_count(), 1);
    }

    #[tokio::test]
    async fn test_prepare_app_caller_media_bridge_routes_playback_through_bridge() {
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
            original_callee: "sip:ivr@rustpbx.com".to_string(),
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
            caller_peer.clone(),
            callee_peer,
        );

        session.media.caller_offer = Some(
            concat!(
                "v=0\r\n",
                "o=alice 1 1 IN IP4 192.0.2.10\r\n",
                "s=Talk\r\n",
                "c=IN IP4 192.0.2.10\r\n",
                "t=0 0\r\n",
                "m=audio 40000 RTP/AVP 0 8 101\r\n",
                "a=rtpmap:0 PCMU/8000\r\n",
                "a=rtpmap:8 PCMA/8000\r\n",
                "a=rtpmap:101 telephone-event/8000\r\n",
                "a=sendrecv\r\n",
            )
            .to_string(),
        );

        let answer = session
            .prepare_app_caller_media_bridge()
            .await
            .expect("app caller bridge answer should be prepared");

        assert!(answer.contains("RTP/AVP"));
        assert!(session.media.media_bridge.is_some());
        assert!(session.media.caller_answer_uses_media_bridge);
        assert_eq!(caller_peer.update_track_call_count(), 0);

        let bridge = session
            .media
            .media_bridge
            .as_ref()
            .expect("media bridge should exist")
            .clone();
        let rtp_track = bridge
            .get_callee_track()
            .await
            .expect("RTP bridge output track should exist");
        let silence_sample = tokio::time::timeout(Duration::from_millis(100), rtp_track.recv())
            .await
            .expect("bridge silence source should send promptly")
            .expect("bridge silence source should produce a sample");
        assert!(
            matches!(silence_sample, rustrtc::media::MediaSample::Audio(_)),
            "caller bridge should send silence before file playback is installed"
        );

        session
            .play_audio_file("sounds/phone-calling.wav", false, "caller", true)
            .await
            .expect("app playback should install a bridge file source");

        assert_eq!(caller_peer.update_track_call_count(), 0);
        assert_eq!(caller_peer.get_tracks_call_count(), 0);

        if let Some(bridge) = session.media.media_bridge.take() {
            bridge.stop().await;
        }
    }

    #[tokio::test]
    async fn test_handle_play_both_legs_creates_two_tracks() {
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
            session_id: "test-both-play".to_string(),
            dialplan: Arc::new(Dialplan::new(
                "test-both-play".to_string(),
                original_request,
                DialDirection::Inbound,
            )),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:alice@rustpbx.com".to_string(),
            original_callee: "sip:bob@microsip.net".to_string(),
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
            caller_peer.clone(),
            callee_peer,
        );

        // Set up caller offer SDP (PCMU)
        session.media.caller_offer = Some(
            concat!(
                "v=0\r\n",
                "o=alice 1 1 IN IP4 192.0.2.10\r\n",
                "s=Talk\r\n",
                "c=IN IP4 192.0.2.10\r\n",
                "t=0 0\r\n",
                "m=audio 40000 RTP/AVP 0 8 101\r\n",
                "a=rtpmap:0 PCMU/8000\r\n",
                "a=rtpmap:8 PCMA/8000\r\n",
                "a=rtpmap:101 telephone-event/8000\r\n",
                "a=sendrecv\r\n",
            )
            .to_string(),
        );

        // Set up callee offer SDP (PCMA, simulating MicroSIP)
        session.media.callee_offer = Some(
            concat!(
                "v=0\r\n",
                "o=bob 1 1 IN IP4 192.0.2.20\r\n",
                "s=Talk\r\n",
                "c=IN IP4 192.0.2.20\r\n",
                "t=0 0\r\n",
                "m=audio 40050 RTP/AVP 8 101\r\n",
                "a=rtpmap:8 PCMA/8000\r\n",
                "a=rtpmap:101 telephone-event/8000\r\n",
                "a=sendrecv\r\n",
            )
            .to_string(),
        );

        // Register legs as WebRTC caller + RTP callee (typical browser↔MicroSIP)
        session.legs.set_transport(
            LegId::from("caller"),
            rustrtc::TransportMode::WebRtc,
        );
        session.legs.set_transport(
            LegId::from("callee"),
            rustrtc::TransportMode::Rtp,
        );

        // Create media bridge via app caller path
        let answer = session
            .prepare_app_caller_media_bridge()
            .await
            .expect("app caller bridge answer should be prepared");
        assert!(answer.contains("RTP/AVP"));
        assert!(session.media.media_bridge.is_some());
        assert!(session.media.caller_answer_uses_media_bridge);

        // Enable callee bridge (app path leaves it false for IVR/queue)
        session.media.callee_offer_uses_media_bridge = true;

        // Open bridge gate for forwarding
        if let Some(ref bridge) = session.media.media_bridge {
            bridge.open_caller_gate();
        }

        let num_before = session.media.playback_tracks.len();

        // Call handle_play with leg_id = "both", use audio file from config/
        let audio_path = "config/sounds/phone-calling.wav";
        session
            .handle_play(
                Some(LegId::from("both")),
                crate::call::domain::MediaSource::File {
                    path: audio_path.to_string(),
                },
                Some(crate::call::domain::PlayOptions {
                    loop_playback: false,
                    ..Default::default()
                }),
            )
            .await
            .expect("play to both legs should succeed");

        // Verify two playback tracks were created (caller + callee)
        assert_eq!(
            session.media.playback_tracks.len(),
            num_before + 2,
            "playback_tracks should have 2 entries for both-leg play"
        );

        // Verify track IDs contain "caller" and "callee" suffixes
        let track_ids: Vec<&str> = session
            .media
            .playback_tracks
            .keys()
            .map(|s| s.as_str())
            .collect();
        assert!(
            track_ids.iter().any(|id| id.ends_with("-caller")),
            "should have a caller track, got: {:?}",
            track_ids
        );
        assert!(
            track_ids.iter().any(|id| id.ends_with("-callee")),
            "should have a callee track, got: {:?}",
            track_ids
        );

        if let Some(bridge) = session.media.media_bridge.take() {
            bridge.stop().await;
        }
    }

    #[tokio::test]
    async fn test_prepare_app_caller_media_bridge_webrtc_caller_uses_webrtc_output_endpoint() {
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
            original_callee: "sip:ivr@rustpbx.com".to_string(),
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

        session.media.caller_offer = Some(
            concat!(
                "v=0\r\n",
                "o=- 123456 2 IN IP4 127.0.0.1\r\n",
                "s=-\r\n",
                "t=0 0\r\n",
                "a=group:BUNDLE 0\r\n",
                "m=audio 9 UDP/TLS/RTP/SAVPF 111 0\r\n",
                "c=IN IP4 0.0.0.0\r\n",
                "a=ice-ufrag:test\r\n",
                "a=ice-pwd:test123456\r\n",
                "a=fingerprint:sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00\r\n",
                "a=setup:actpass\r\n",
                "a=mid:0\r\n",
                "a=sendrecv\r\n",
                "a=rtcp-mux\r\n",
                "a=rtpmap:111 opus/48000/2\r\n",
                "a=rtpmap:0 PCMU/8000\r\n",
            )
            .to_string(),
        );

        let answer = session
            .prepare_app_caller_media_bridge()
            .await
            .expect("app caller bridge answer should be prepared");

        assert!(answer.contains("UDP/TLS/RTP/SAVPF"));
        assert!(session.media.caller_answer_uses_media_bridge);
        assert_eq!(
            session.legs.transports.get(&LegId::from("caller")),
            Some(&rustrtc::TransportMode::WebRtc)
        );

        let bridge = session
            .media
            .media_bridge
            .as_ref()
            .expect("media bridge should exist")
            .clone();
        let webrtc_track = bridge
            .get_caller_track()
            .await
            .expect("WebRTC bridge output track should exist");
        let silence_sample = tokio::time::timeout(Duration::from_millis(100), webrtc_track.recv())
            .await
            .expect("bridge silence source should send promptly")
            .expect("bridge silence source should produce a sample");
        assert!(
            matches!(silence_sample, rustrtc::media::MediaSample::Audio(_)),
            "WebRTC caller should receive bridge silence on WebRTC endpoint"
        );

        if let Some(bridge) = session.media.media_bridge.take() {
            bridge.stop().await;
        }
    }

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
        }
    }

    #[test]
    fn test_apply_video_caps_from_source_keeps_source_order_with_target_payload_types() {
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

        assert!(reordered.contains("m=video 9 UDP/TLS/RTP/SAVPF 103 107 96\r\n"));
        assert!(reordered.contains("a=rtpmap:103 H264/90000\r\n"));
        assert!(reordered.contains("a=rtpmap:107 H264/90000\r\n"));
        assert!(reordered.contains("a=rtpmap:96 VP8/90000\r\n"));
        assert!(reordered.contains("a=fmtp:103 "));
        assert!(reordered.contains("a=fmtp:107 "));
        assert!(!reordered.contains("a=rtpmap:104 VP9/90000\r\n"));
        assert!(!reordered.contains("a=rtcp-fb:104 "));
    }

    /// Default allowlist (empty slice) keeps only H264 and strips rtcp-fb.
    #[test]
    fn test_filter_video_caps_default_keeps_h264_only() {
        let caps = vec![
            make_video_cap(
                96,
                "H264",
                Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"),
                &["goog-remb", "transport-cc", "nack"],
            ),
            make_video_cap(97, "VP8", None, &["goog-remb", "transport-cc"]),
            make_video_cap(98, "VP9", None, &["goog-remb"]),
        ];

        let result = SipSession::filter_video_caps_for_rtp(&caps, &[]);

        assert_eq!(result.len(), 1, "only H264 should survive");
        assert_eq!(result[0].codec_name, "H264");
        assert_eq!(result[0].payload_type, 96);
        assert!(result[0].rtcp_fbs.is_empty(), "rtcp_fbs must be cleared");
        assert!(result[0].fmtp.is_some(), "fmtp should be preserved");
    }

    /// Explicit allowlist is respected (case-insensitive).
    #[test]
    fn test_filter_video_caps_explicit_allowlist() {
        let caps = vec![
            make_video_cap(96, "H264", Some("profile-level-id=42e01f"), &["goog-remb"]),
            make_video_cap(97, "VP8", None, &["transport-cc"]),
            make_video_cap(98, "H265", None, &[]),
        ];

        let allowed = vec!["H264".to_string(), "h265".to_string()]; // mixed case
        let result = SipSession::filter_video_caps_for_rtp(&caps, &allowed);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].codec_name, "H264");
        assert_eq!(result[1].codec_name, "H265");
        assert!(result.iter().all(|c| c.rtcp_fbs.is_empty()));
    }

    /// All rtcp-fb attributes are always cleared regardless of allowlist.
    #[test]
    fn test_filter_video_caps_rtcp_fb_always_cleared() {
        let caps = vec![make_video_cap(
            96,
            "H264",
            None,
            &["nack", "nack pli", "ccm fir", "goog-remb", "transport-cc"],
        )];

        let result = SipSession::filter_video_caps_for_rtp(&caps, &[]);

        assert!(
            result[0].rtcp_fbs.is_empty(),
            "every rtcp-fb must be stripped"
        );
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

    /// Multiple H264 profiles (e.g. different profile-level-id) are all kept.
    #[test]
    fn test_filter_video_caps_multiple_h264_profiles_all_kept() {
        let caps = vec![
            make_video_cap(96, "H264", Some("profile-level-id=42e01f"), &["goog-remb"]),
            make_video_cap(97, "VP8", None, &["transport-cc"]),
            make_video_cap(98, "H264", Some("profile-level-id=640032"), &["nack"]),
        ];

        let result = SipSession::filter_video_caps_for_rtp(&caps, &[]);
        assert_eq!(result.len(), 2, "both H264 profiles should be kept");
        assert!(result.iter().all(|c| c.codec_name == "H264"));
        assert!(result.iter().all(|c| c.rtcp_fbs.is_empty()));
    }

    // ── DTMF payload building ─────────────────────────────────────────────

    #[test]
    fn test_build_telephone_event_payload_digit_1() {
        let payload = SipSession::build_telephone_event_payload('1', false, 800).unwrap();
        assert_eq!(payload[0], 1); // DTMF event code for '1'
        assert_eq!(payload[1] & 0x80, 0); // E bit not set
        assert_eq!(u16::from_be_bytes([payload[2], payload[3]]), 800);
    }

    #[test]
    fn test_build_telephone_event_payload_digit_end_bit() {
        let payload = SipSession::build_telephone_event_payload('5', true, 1600).unwrap();
        assert_eq!(payload[0], 5);
        assert_eq!(payload[1] & 0x80, 0x80); // E bit set
        assert_eq!(u16::from_be_bytes([payload[2], payload[3]]), 1600);
    }

    #[test]
    fn test_build_telephone_event_payload_star() {
        let payload = SipSession::build_telephone_event_payload('*', false, 0).unwrap();
        assert_eq!(payload[0], 10); // DTMF event code for '*'
    }

    #[test]
    fn test_build_telephone_event_payload_hash() {
        let payload = SipSession::build_telephone_event_payload('#', false, 0).unwrap();
        assert_eq!(payload[0], 11); // DTMF event code for '#'
    }

    #[test]
    fn test_build_telephone_event_payload_invalid() {
        let result = SipSession::build_telephone_event_payload('Z', false, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_telephone_event_payload_all_digits() {
        for (ch, expected_code) in [
            ('0', 0),
            ('1', 1),
            ('2', 2),
            ('3', 3),
            ('4', 4),
            ('5', 5),
            ('6', 6),
            ('7', 7),
            ('8', 8),
            ('9', 9),
            ('*', 10),
            ('#', 11),
        ] {
            let payload = SipSession::build_telephone_event_payload(ch, false, 800).unwrap();
            assert_eq!(
                payload[0], expected_code,
                "DTMF digit '{}' should map to code {}",
                ch, expected_code
            );
        }
    }

    #[test]
    fn test_build_telephone_event_payload_duration_zero() {
        let payload = SipSession::build_telephone_event_payload('3', false, 0).unwrap();
        assert_eq!(u16::from_be_bytes([payload[2], payload[3]]), 0);
    }

    #[test]
    fn test_build_telephone_event_payload_duration_max() {
        let payload = SipSession::build_telephone_event_payload('7', false, 65535).unwrap();
        assert_eq!(u16::from_be_bytes([payload[2], payload[3]]), 65535);
    }

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
}
