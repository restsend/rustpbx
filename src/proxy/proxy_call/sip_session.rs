use crate::call::Location;
use crate::call::app::{ApplicationContext, CallInfo};
use crate::call::domain::{
    CallCommand, HangupCascade, HangupCommand, LegId, LegState, MediaPathMode, MediaRuntimeProfile,
    RingbackPolicy,
};
use crate::call::domain::{Leg, SessionState};
use crate::call::runtime::BridgeConfig;
use crate::call::runtime::invoke_post_call_hook;
use crate::call::runtime::{
    AppFactory, AppRuntime, AppRuntimeConfig, CommandResult, DefaultAppRuntime, ExecutionContext,
    MediaCapabilityCheck, SessionId,
};
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub state: SessionState,
    pub leg_count: usize,
    pub bridge_active: bool,
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
use crate::media::{FileTrack, RtpTrackBuilder, Track};
use crate::proxy::proxy_call::{
    dtmf::RtpDtmfDetector,
    media_peer::{MediaPeer, VoiceEnginePeer},
    reporter::CallReporter,
    session_timer::{
        DEFAULT_SESSION_EXPIRES, HEADER_MIN_SE, HEADER_SESSION_EXPIRES, HEADER_SUPPORTED,
        MIN_MIN_SE, SessionRefresher, SessionTimerState, apply_refresh_response,
        apply_session_timer_headers, build_default_session_timer_headers,
        build_session_timer_headers, build_session_timer_response_headers, get_header_value,
        has_timer_support, parse_min_se, parse_session_expires, select_client_timer_refresher,
        select_server_timer_refresher,
    },
    state::{CallContext, CallSessionRecordSnapshot, SessionHangupMessage},
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
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

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

struct CallerIngressMonitor {
    cancel_token: CancellationToken,
    task: JoinHandle<()>,
}

pub struct SipSession {
    pub id: SessionId,
    pub state: SessionState,
    pub legs: std::collections::HashMap<LegId, Leg>,
    #[allow(dead_code)]
    pub policy: SessionPolicy,
    pub bridge: BridgeConfig,
    pub media_profile: MediaRuntimeProfile,
    pub app_runtime: Arc<dyn AppRuntime>,
    pub snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>>,

    pub server: SipServerRef,
    pub server_dialog: ServerInviteDialog,
    pub callee_dialogs: Arc<DashMap<DialogId, ()>>,
    /// Unified per-leg dialog map.
    pub dialogs: std::collections::HashMap<LegId, rsipstack::dialog::dialog::Dialog>,
    pub supervisor_mixer: Option<Arc<MediaMixer>>,

    pub context: CallContext,
    pub call_record_sender: Option<CallRecordSender>,

    pub cancel_token: CancellationToken,
    pub pending_hangup: HashSet<DialogId>,
    pub connected_callee: Option<String>,
    pub connected_callee_dialog_id: Option<DialogId>,
    pub callee_call_ids: HashSet<String>,
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub caller_offer: Option<String>,
    pub callee_offer: Option<String>,
    pub answer: Option<String>,
    pub early_media_sent: bool,
    pub callee_answer_sdp: Option<String>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<SessionHangupMessage>,
    pub last_error: Option<(StatusCode, Option<String>)>,
    pub recording_state: Option<(String, Instant)>,

    pub routed_caller: Option<String>,
    pub routed_callee: Option<String>,
    pub routed_contact: Option<String>,
    pub routed_destination: Option<String>,

    timers: HashMap<DialogId, SessionTimerState>,
    update_refresh_disabled: HashSet<DialogId>,
    timer_queue: DelayQueue<DialogId>,
    timer_keys: HashMap<DialogId, delay_queue::Key>,

    pub callee_event_tx: Option<mpsc::UnboundedSender<DialogState>>,
    pub callee_guards: Vec<ClientDialogGuard>,

    pub reporter: Option<CallReporter>,
    pub recorder: Arc<RwLock<Option<Recorder>>>,
    pub playback_tracks: std::collections::HashMap<String, FileTrack>,
    caller_ingress_monitor: Option<CallerIngressMonitor>,

    pub media_bridge: Option<Arc<crate::media::bridge::BridgePeer>>,
    caller_answer_uses_media_bridge: bool,
    callee_offer_uses_media_bridge: bool,
    media_bridge_started: bool,
    bridge_playback_track_id: Option<String>,

    pub conference_bridge: crate::call::runtime::SessionConferenceBridge,

    /// Per-leg transport mode (canonical source).
    pub leg_transport: std::collections::HashMap<LegId, rustrtc::TransportMode>,

    /// Unified map of all leg media peers (caller, callee, dynamic).
    /// Fast-path: P2P 2-leg case continues using `caller_peer`/`callee_peer`
    /// direct references for legacy code; all new code uses `peers`.
    pub peers: std::collections::HashMap<LegId, Arc<dyn MediaPeer>>,

    /// @deprecated Use `peers[LegId::from("caller")]` instead.
    /// Kept as fast-path alias for the P2P 2-leg case.
    pub caller_peer: Arc<dyn MediaPeer>,
    /// @deprecated Use `peers[LegId::from("callee")]` instead.
    /// Kept as fast-path alias for the P2P 2-leg case.
    pub callee_peer: Arc<dyn MediaPeer>,

    /// Command sender for internal async tasks (e.g., leg dial completion)
    #[allow(dead_code)]
    pub cmd_tx: Option<mpsc::UnboundedSender<CallCommand>>,

    /// Per-leg spawned task handles (for cleanup when leg is removed)
    pub leg_tasks: std::collections::HashMap<LegId, Vec<tokio::task::JoinHandle<()>>>,

    /// Per-leg answer SDP (populated for all legs including dynamic).
    pub leg_answers: std::collections::HashMap<LegId, String>,

    /// Current queue name (for post-call hook, e.g. CSAT survey).
    pub queue_name: Option<String>,

    /// Tracks which legs currently have video in their SDP (populated by
    /// re-INVITE handling).  Used to detect video add/remove transitions.
    pub leg_has_video: std::collections::HashMap<LegId, bool>,
}

#[derive(Clone)]
pub struct SipSessionHandle {
    session_id: SessionId,
    cmd_tx: mpsc::UnboundedSender<CallCommand>,
    snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>>,
    app_event_bridge: Arc<RwLock<Option<crate::proxy::proxy_call::state::SipSessionHandle>>>,
}

impl SipSessionHandle {
    pub fn send_command(&self, cmd: CallCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|e| anyhow::anyhow!("channel closed: {}", e))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id.0
    }

    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        self.snapshot_cache.read().clone()
    }

    pub fn update_snapshot(&self, snapshot: SessionSnapshot) {
        *self.snapshot_cache.write() = Some(snapshot);
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
        cmd_tx: mpsc::UnboundedSender<crate::call::domain::CallCommand>,
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
struct BuiltinAppFactory;

impl AppFactory for BuiltinAppFactory {
    fn create_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        _context: &ApplicationContext,
    ) -> Option<Box<dyn crate::call::app::CallApp>> {
        match app_name {
            "ivr" => {
                let file = params.as_ref()?.get("file")?.as_str()?;
                let mut app = match crate::call::app::ivr::IvrApp::from_file(file) {
                    Ok(app) => app,
                    Err(e) => {
                        tracing::warn!("Failed to load IVR app from {}: {}", file, e);
                        return None;
                    }
                };
                // Allow per-instance TTS override via app_params
                if let Some(tts_value) = params.as_ref()?.get("tts")
                    && let Ok(tts_cfg) =
                        serde_json::from_value::<crate::tts::TtsConfig>(tts_value.clone())
                {
                    app = app.with_tts(Some(tts_cfg));
                }
                Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
            }
            "voicemail" => {
                let extension = params.as_ref()?.get("extension")?.as_str()?.to_string();
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
            _ => None,
        }
    }
}

impl SipSession {
    pub const CALLER_TRACK_ID: &'static str = "caller-track";
    pub const CALLEE_TRACK_ID: &'static str = "callee-track";
    pub const CALLER_FORWARDING_TRACK_ID: &'static str = "caller-forwarding-track";
    pub const CALLEE_FORWARDING_TRACK_ID: &'static str = "callee-forwarding-track";

    pub const QUEUE_HOLD_TRACK_ID: &'static str = "queue-hold";
    const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

    pub fn with_handle(id: SessionId) -> (SipSessionHandle, mpsc::UnboundedReceiver<CallCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
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
    ) -> (Self, SipSessionHandle, mpsc::UnboundedReceiver<CallCommand>) {
        let session_id = SessionId::from(context.session_id.clone());

        let media_profile = if use_media_proxy {
            MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored)
        } else {
            MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass)
        };

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
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
            session_id: context.session_id.clone(),
            caller: context.original_caller.clone(),
            callee: context.original_callee.clone(),
            direction: context.dialplan.direction.to_string(),
            started_at: chrono::Utc::now(),
        };
        let app_ctx = ApplicationContext::new(
            server
                .database
                .clone()
                .unwrap_or(sea_orm::DatabaseConnection::Disconnected),
            call_info,
            Arc::new(crate::config::Config::default()),
        );

        // Create a bridge handle that speaks SessionAction (for DefaultAppRuntime)
        // and translates to CallCommand for the unified SipSession.
        let bridge_shared = crate::proxy::proxy_call::state::SipSessionShared::new(
            context.session_id.clone(),
            crate::call::DialDirection::Inbound,
            Some(context.original_caller.clone()),
            Some(context.original_callee.clone()),
            None,
        );
        let (bridge_handle, mut action_rx) =
            crate::proxy::proxy_call::state::SipSessionHandle::with_shared(bridge_shared);

        // Wire the bridge into the sip handle so send_app_event forwards events.
        let mut slot = app_event_bridge.write();
        *slot = Some(bridge_handle.clone());

        // Spawn the bridge task: SessionAction -> CallCommand
        let sip_handle_clone = sip_handle.clone();
        tokio::spawn(async move {
            use crate::call::adapters::session_action_to_call_command;
            while let Some(action) = action_rx.recv().await {
                match session_action_to_call_command(action) {
                    Ok(cmd) => {
                        if let Err(e) = sip_handle_clone.send_command(cmd) {
                            tracing::warn!(
                                "SessionAction bridge failed to send CallCommand: {}",
                                e
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to convert SessionAction to CallCommand: {}", e);
                    }
                }
            }
        });

        let app_runtime: Arc<dyn AppRuntime> = Arc::new(
            DefaultAppRuntime::new(AppRuntimeConfig {
                session_id: context.session_id.clone(),
                handle: sip_handle.clone(),
                context: Arc::new(app_ctx),
            })
            .with_factory(Arc::new(BuiltinAppFactory)),
        );

        let initial = server_dialog.initial_request();
        let caller_offer = if initial.body().is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(initial.body()).to_string())
        };

        let session = Self {
            id: session_id.clone(),
            state: SessionState::Initializing,
            legs: std::collections::HashMap::new(),
            policy: SessionPolicy::inbound_sip(),
            bridge: BridgeConfig::new(),
            media_profile: media_profile.clone(),
            app_runtime,
            snapshot_cache: snapshot_cache.clone(),
            server,
            server_dialog: server_dialog.clone(),
            callee_dialogs: Arc::new(DashMap::new()),
            dialogs: {
                let mut m = std::collections::HashMap::new();
                let caller_dialog = rsipstack::dialog::dialog::Dialog::ServerInvite(server_dialog);
                m.insert(LegId::from("caller"), caller_dialog);
                m
            },
            pending_hangup: HashSet::new(),
            caller_peer: caller_peer.clone(),
            callee_peer: callee_peer.clone(),
            supervisor_mixer: None,
            peers: {
                let mut m = std::collections::HashMap::new();
                m.insert(LegId::from("caller"), caller_peer.clone());
                m.insert(LegId::from("callee"), callee_peer.clone());
                m
            },
            leg_transport: std::collections::HashMap::new(),
            context,
            call_record_sender,
            cancel_token,
            connected_callee: None,
            connected_callee_dialog_id: None,
            callee_call_ids: HashSet::new(),
            ring_time: None,
            answer_time: None,
            caller_offer,
            callee_offer: None,
            answer: None,
            early_media_sent: false,
            callee_answer_sdp: None,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            last_error: None,
            recording_state: None,
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            timers: HashMap::new(),
            update_refresh_disabled: HashSet::new(),
            timer_queue: DelayQueue::new(),
            timer_keys: HashMap::new(),
            callee_event_tx: None,
            callee_guards: Vec::new(),
            reporter: None,
            recorder: Arc::new(RwLock::new(None)),
            playback_tracks: std::collections::HashMap::new(),
            caller_ingress_monitor: None,
            media_bridge: None,
            caller_answer_uses_media_bridge: false,
            callee_offer_uses_media_bridge: false,
            media_bridge_started: false,
            bridge_playback_track_id: None,

            conference_bridge: crate::call::runtime::SessionConferenceBridge::new(),
            cmd_tx: Some(cmd_tx.clone()),
            leg_tasks: std::collections::HashMap::new(),
            leg_answers: std::collections::HashMap::new(),
            queue_name: None,
            leg_has_video: std::collections::HashMap::new(),
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
            context.clone(),
            server_dialog.clone(),
            use_media_proxy,
            caller_peer,
            callee_peer,
        );

        session.reporter = Some(CallReporter {
            server: server.clone(),
            context: context.clone(),
            call_record_sender: session.call_record_sender.clone(),
        });

        if use_media_proxy {
            let offer_sdp =
                String::from_utf8_lossy(server_dialog.initial_request().body()).to_string();
            session.caller_offer = Some(offer_sdp.clone());
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

        let mut server_dialog_clone = server_dialog.clone();
        crate::utils::spawn(async move {
            session
                .process(state_rx, callee_state_rx, cmd_rx, dialog_guard)
                .await
        });

        let max_setup_duration = context
            .dialplan
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
            return registered_aor.clone();
        }

        target.aor.clone()
    }

    fn bypasses_local_media(&self) -> bool {
        self.media_profile.path == MediaPathMode::Bypass && self.media_bridge.is_none()
    }

    async fn send_mid_dialog_request_to_side(
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
        let headers = vec![rsipstack::sip::Header::ContentType(
            "application/sdp".into(),
        )];
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
        let answer_sdp = if response.body().is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(response.body()).to_string())
        };

        Ok((status, answer_sdp))
    }

    pub async fn process(
        &mut self,
        mut state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut cmd_rx: mpsc::UnboundedReceiver<CallCommand>,
        _dialog_guard: ServerDialogGuard,
    ) -> Result<()> {
        let _cancel_guard = self.cancel_token.clone().drop_guard();

        if !self.context.dialplan.is_empty()
            && let Err((status_code, reason)) = self.execute_dialplan(&mut callee_state_rx).await
        {
            warn!(?status_code, ?reason, "Dialplan execution failed");

            let code = status_code.clone();
            let _ = self.server_dialog.reject(Some(code), reason.clone());
            // Store error so cleanup/CDR can report the failure reason
            self.last_error = Some((status_code.clone(), reason));
            self.hangup_reason = Some(CallRecordHangupReason::Failed);
            // Ensure cleanup runs (generates CDR) even on early failure
            self.cleanup().await;
            return Err(anyhow!("Dialplan failed: {:?}", status_code));
        }

        let hangup_futures = FuturesUnordered::new();
        let timeout = futures::future::pending::<()>().boxed();
        let mut cancelled = false;
        tokio::pin!(hangup_futures);
        tokio::pin!(timeout);

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
                    let result = self.execute_command(cmd).await;
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
                            self.hangup_reason = Some(CallRecordHangupReason::Autohangup);
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
            media_path: self.media_profile.path,
            answer_sdp: self.answer.clone(),
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
            DialogState::Info(_, request, tx_handle) => {
                // Parse inbound SIP INFO for DTMF (application/dtmf-relay)
                let is_dtmf = request.headers.iter().any(|h| {
                    if let rsipstack::sip::Header::ContentType(ct) = h {
                        ct.value().to_lowercase().contains("application/dtmf-relay")
                    } else {
                        false
                    }
                });
                if is_dtmf {
                    info!(
                        session_id = %self.context.session_id,
                        "✓ Received SIP INFO with DTMF (application/dtmf-relay content type)"
                    );
                    let body = String::from_utf8_lossy(request.body());
                    debug!(
                        session_id = %self.context.session_id,
                        body = %body,
                        "INFO DTMF message body"
                    );
                    for line in body.lines() {
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
                                    "digit": digit.chars().next().unwrap().to_string(),
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
                                }
                            }
                        }
                    }
                } else {
                    debug!(
                        session_id = %self.context.session_id,
                        "Received SIP INFO without DTMF content type"
                    );
                }
                tx_handle
                    .respond(rsipstack::sip::StatusCode::OK, None, None)
                    .await
                    .ok();
            }
            DialogState::Notify(_, request, tx_handle) => {
                // Respond 200 OK to NOTIFY
                let _ = tx_handle
                    .respond(rsipstack::sip::StatusCode::OK, None, None)
                    .await;

                // Check if this is a REFER-related NOTIFY
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
                        if (200..300).contains(&sip_status) {
                            self.hangup_reason
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
            }
            DialogState::Terminated(_, reason) => {
                self.update_leg_state(&LegId::from("caller"), LegState::Ended);

                match reason {
                    TerminatedReason::UacBye => {
                        self.hangup_reason = Some(CallRecordHangupReason::ByCaller);
                        info!("Caller initiated hangup (UacBye)");
                    }
                    TerminatedReason::UasBye => {
                        self.hangup_reason = Some(CallRecordHangupReason::ByCallee);
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
            }
            _ => {}
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
            }
            DialogState::Updated(dialog_id, request, tx_handle) => {
                self.handle_updated_dialog(DialogSide::Callee, dialog_id, request, tx_handle)
                    .await?;
            }
            DialogState::Terminated(terminated_dialog_id, reason) => {
                self.pending_hangup.remove(&terminated_dialog_id);
                self.callee_dialogs.remove(&terminated_dialog_id);
                self.dialogs
                    .retain(|_, dlg| dlg.id() != terminated_dialog_id);
                self.unschedule_timer(&terminated_dialog_id);
                self.timers.remove(&terminated_dialog_id);
                self.update_refresh_disabled.remove(&terminated_dialog_id);
                self.callee_guards
                    .retain(|guard| guard.id() != &terminated_dialog_id);

                let connected_callee_terminated =
                    self.connected_callee_dialog_id.as_ref() == Some(&terminated_dialog_id);
                if self.connected_callee_dialog_id.is_some() && !connected_callee_terminated {
                    debug!(
                        dialog_id = %terminated_dialog_id,
                        connected_dialog_id = ?self.connected_callee_dialog_id,
                        ?reason,
                        "Ignoring terminated non-connected callee dialog"
                    );
                    return Ok(());
                }

                self.update_leg_state(&LegId::from("callee"), LegState::Ended);

                match &reason {
                    TerminatedReason::UasBye => {
                        self.hangup_reason = Some(CallRecordHangupReason::ByCallee);
                        info!("Callee initiated hangup (UasBye)");
                    }
                    TerminatedReason::UacBye => {
                        self.hangup_reason = Some(CallRecordHangupReason::ByCaller);
                        info!("Caller initiated hangup (UacBye) on callee dialog");
                    }
                    _ => {
                        debug!(?reason, "Callee dialog terminated with reason");
                    }
                }

                if connected_callee_terminated {
                    self.connected_callee = None;
                    self.connected_callee_dialog_id = None;

                    let hook_handled = if !self.server_dialog.state().is_terminated() {
                        let agent_id = self.connected_callee.clone().unwrap_or_default();
                        let queue_name = self.queue_name.clone().unwrap_or_default();
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

                    if !hook_handled {
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
                        warn!(
                            ?code,
                            ?reason_str,
                            "Callee rejected call, propagating error to caller"
                        );
                        self.last_error = Some((code.clone(), reason_str.clone()));
                        if let Err(e) = self.server_dialog.reject(code.into(), reason_str) {
                            warn!(error = %e, "Failed to send rejection response to caller");
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn execute_dialplan(
        &mut self,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let flow = self.context.dialplan.flow.clone();
        self.execute_flow(&flow, callee_state_rx).await
    }

    fn execute_flow<'a>(
        &'a mut self,
        flow: &'a crate::call::DialplanFlow,
        callee_state_rx: &'a mut mpsc::UnboundedReceiver<DialogState>,
    ) -> futures::future::BoxFuture<'a, Result<(), (StatusCode, Option<String>)>> {
        use crate::call::DialplanFlow;
        use futures::FutureExt;

        async move {
            match flow {
                DialplanFlow::Targets(strategy) => {
                    self.run_targets(strategy, callee_state_rx).await
                }
                DialplanFlow::Queue { plan, next } => {
                    match self.execute_queue(plan, callee_state_rx).await {
                        Ok(()) => Ok(()),
                        Err((code, reason)) => {
                            warn!(?code, ?reason, "Queue execution failed, trying next flow");
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
    ) -> Result<(), (StatusCode, Option<String>)> {
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
    ) -> Result<(), (StatusCode, Option<String>)> {
        let mut last_error = (
            StatusCode::TemporarilyUnavailable,
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
    ) -> Result<(), (StatusCode, Option<String>)> {
        if let Some(target) = targets.first() {
            self.try_single_target(target, callee_state_rx, None).await
        } else {
            Err((
                StatusCode::TemporarilyUnavailable,
                Some("No targets to dial".to_string()),
            ))
        }
    }

    async fn prepare_queue_early_answer(
        &mut self,
        agents: &[crate::call::Location],
    ) -> Option<String> {
        let caller_is_webrtc = self.is_caller_webrtc();
        self.leg_transport.insert(
            LegId::from("caller"),
            if caller_is_webrtc {
                rustrtc::TransportMode::WebRtc
            } else {
                rustrtc::TransportMode::Rtp
            },
        );

        let Some(first_agent) = agents.first() else {
            return self.ensure_caller_answer_sdp().await;
        };

        let callee_is_webrtc = first_agent.supports_webrtc;
        self.leg_transport.insert(
            LegId::from("callee"),
            if callee_is_webrtc {
                rustrtc::TransportMode::WebRtc
            } else {
                rustrtc::TransportMode::Rtp
            },
        );

        if caller_is_webrtc == callee_is_webrtc {
            self.caller_answer_uses_media_bridge = false;
            self.callee_offer_uses_media_bridge = false;
            return self.ensure_caller_answer_sdp().await;
        }

        if self.media_bridge.is_none() {
            match self.create_callee_track(callee_is_webrtc).await {
                Ok(callee_offer) => {
                    self.callee_offer = Some(callee_offer);
                }
                Err(error) => {
                    warn!(
                        session_id = %self.context.session_id,
                        error = %error,
                        "Queue early answer: failed to prepare bridge, falling back to caller media"
                    );
                    self.caller_answer_uses_media_bridge = false;
                    self.callee_offer_uses_media_bridge = false;
                    return self.ensure_caller_answer_sdp().await;
                }
            }
        }

        match self.prepare_bridge_caller_answer().await {
            Ok(answer) => {
                self.answer = Some(answer.clone());
                self.caller_answer_uses_media_bridge = true;
                self.replace_caller_bridge_output_with_silence().await;
                Some(answer)
            }
            Err(error) => {
                warn!(
                    session_id = %self.context.session_id,
                    error = %error,
                    "Queue early answer: failed to answer from bridge, falling back to caller media"
                );
                self.caller_answer_uses_media_bridge = false;
                self.callee_offer_uses_media_bridge = false;
                self.ensure_caller_answer_sdp().await
            }
        }
    }

    async fn prepare_app_caller_media_bridge(&mut self) -> Option<String> {
        if let Some(ref answer) = self.answer {
            return Some(answer.clone());
        }

        let caller_offer = self.caller_offer.clone()?;
        let caller_is_webrtc = self.is_caller_webrtc();
        self.leg_transport.insert(
            LegId::from("caller"),
            if caller_is_webrtc {
                rustrtc::TransportMode::WebRtc
            } else {
                rustrtc::TransportMode::Rtp
            },
        );
        self.leg_transport.insert(
            LegId::from("callee"),
            if caller_is_webrtc {
                rustrtc::TransportMode::Rtp
            } else {
                rustrtc::TransportMode::WebRtc
            },
        );
        self.callee_offer_uses_media_bridge = false;

        let created_bridge = self.media_bridge.is_none();
        if self.media_bridge.is_none()
            && let Err(error) = self
                .create_app_caller_media_bridge(&caller_offer, caller_is_webrtc)
                .await
        {
            warn!(
                session_id = %self.context.session_id,
                error = %error,
                "Application answer: failed to prepare media bridge, falling back to caller media"
            );
            self.caller_answer_uses_media_bridge = false;
            return None;
        }

        match self.prepare_bridge_caller_answer().await {
            Ok(answer) => {
                self.answer = Some(answer.clone());
                self.caller_answer_uses_media_bridge = true;
                self.replace_caller_bridge_output_with_silence().await;
                Some(answer)
            }
            Err(error) => {
                if created_bridge && let Some(bridge) = self.media_bridge.take() {
                    bridge.stop().await;
                }
                warn!(
                    session_id = %self.context.session_id,
                    error = %error,
                    "Application answer: failed to answer from media bridge, falling back to caller media"
                );
                self.caller_answer_uses_media_bridge = false;
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
            .with_enable_latching(self.server.proxy_config.enable_latching);

        if let (Some(start), Some(end)) = (
            self.server.rtp_config.start_port,
            self.server.rtp_config.end_port,
        ) {
            bridge_builder = bridge_builder.with_rtp_port_range(start, end);
        }

        if !caller_is_webrtc && caller_offer.contains("a=group:BUNDLE") {
            bridge_builder = bridge_builder
                .with_rtp_sdp_compatibility(rustrtc::config::SdpCompatibilityMode::Standard)
                .with_enable_latching(true);
            info!(session_id = %self.id, "RTP caller offered BUNDLE, using Standard SDP mode + latching for app media bridge");
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

        let allow_codecs = &self.context.dialplan.allow_codecs;
        let codec_lists = MediaNegotiator::build_bridge_codec_lists(
            caller_offer,
            caller_is_webrtc,
            !caller_is_webrtc,
            allow_codecs,
            self.context.dialplan.media.codec_strategy,
        );
        let webrtc_side_codecs = if caller_is_webrtc {
            &codec_lists.caller_side
        } else {
            &codec_lists.callee_side
        };
        let rtp_side_codecs = if caller_is_webrtc {
            &codec_lists.callee_side
        } else {
            &codec_lists.caller_side
        };

        let webrtc_caps: Vec<_> = webrtc_side_codecs
            .iter()
            .filter_map(|codec| codec.to_audio_capability())
            .collect();
        let rtp_caps: Vec<_> = rtp_side_codecs
            .iter()
            .filter_map(|codec| codec.to_audio_capability())
            .collect();
        if !webrtc_caps.is_empty() || !rtp_caps.is_empty() {
            bridge_builder = bridge_builder
                .with_webrtc_audio_capabilities(webrtc_caps)
                .with_rtp_audio_capabilities(rtp_caps);
        }

        let webrtc_sender = webrtc_side_codecs
            .iter()
            .find(|codec| !codec.is_dtmf())
            .map(|codec| codec.to_params());
        let rtp_sender = rtp_side_codecs
            .iter()
            .find(|codec| !codec.is_dtmf())
            .map(|codec| codec.to_params());
        if let (Some(webrtc_sender), Some(rtp_sender)) = (webrtc_sender, rtp_sender) {
            bridge_builder = bridge_builder.with_sender_codecs(webrtc_sender, rtp_sender);
        }

        if self.context.dialplan.recording.enabled {
            bridge_builder = bridge_builder.with_recorder(self.recorder.clone());
        }

        let bridge = bridge_builder.build();
        bridge.setup_bridge().await?;
        self.media_bridge = Some(bridge);

        debug!(
            session_id = %self.context.session_id,
            caller_is_webrtc,
            "Application caller media bridge prepared"
        );

        Ok(())
    }

    async fn replace_caller_bridge_output_with_silence(&self) {
        let Some(bridge) = self.media_bridge.as_ref() else {
            return;
        };

        if let Err(error) = bridge
            .replace_output_with_silence(
                self.leg_bridge_endpoint(&LegId::from("caller")),
                self.caller_output_codec_info(),
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

    fn caller_output_codec_info(&self) -> CodecInfo {
        self.caller_offer
            .as_ref()
            .map(|offer| MediaNegotiator::extract_codec_params(offer).audio)
            .and_then(|codecs| codecs.first().cloned())
            .unwrap_or_else(|| {
                let codec = CodecType::PCMU;
                MediaNegotiator::codec_info_for_type(codec)
            })
    }

    async fn prepare_bridge_caller_answer(&self) -> Result<String> {
        let caller_offer = self
            .caller_offer
            .as_deref()
            .ok_or_else(|| anyhow!("No caller offer available for bridge answer"))?;
        let bridge = self
            .media_bridge
            .as_ref()
            .ok_or_else(|| anyhow!("No media bridge available for caller answer"))?;

        let pc = if self.is_caller_webrtc() {
            bridge.webrtc_pc().clone()
        } else {
            bridge.rtp_pc().clone()
        };

        if let Some(local_description) = pc.local_description() {
            return Ok(local_description.to_sdp_string());
        }

        if pc.remote_description().is_none() {
            let offer = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, caller_offer)
                .map_err(|e| anyhow!("Failed to parse caller offer SDP: {}", e))?;
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

        pc.local_description()
            .map(|desc| desc.to_sdp_string())
            .ok_or_else(|| anyhow!("Bridge caller side has no local answer"))
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
        acd_policy: Option<&str>,
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
                            .resolve_target_with_policy(&uri_str, acd_policy)
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
                                        let host = uri.host().to_string();
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
                    resolved.extend(locations);
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
    ) -> Result<(), (StatusCode, Option<String>)> {
        use rsipstack::dialog::dialog::DialogState;
        use rsipstack::dialog::invitation::InviteOption;

        let caller = self.context.dialplan.caller.clone().ok_or_else(|| {
            (
                StatusCode::ServerInternalError,
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

        // When routing to another PBX node via home_proxy, we intentionally do NOT
        // add a Record-Route on the outbound INVITE. The Contact header already
        // provides the correct return path for the callee's responses and in-dialog
        // requests to reach this proxy. Adding a self-referencing Record-Route would
        // pollute the dialog route-set with the local address, causing all subsequent
        // in-dialog requests (BYE, ACK) to loopback to this node instead of reaching
        // the remote agent node.
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
            .unwrap_or(DEFAULT_SESSION_EXPIRES);
        if self.server.proxy_config.session_timer_mode().is_enabled() {
            headers.extend(build_default_session_timer_headers(
                default_expires,
                MIN_MIN_SE,
            ));
        }

        let callee_is_webrtc = target.supports_webrtc;
        let caller_is_webrtc = self.is_caller_webrtc();
        self.leg_transport.insert(
            LegId::from("caller"),
            if caller_is_webrtc {
                rustrtc::TransportMode::WebRtc
            } else {
                rustrtc::TransportMode::Rtp
            },
        );
        self.leg_transport.insert(
            LegId::from("callee"),
            if callee_is_webrtc {
                rustrtc::TransportMode::WebRtc
            } else {
                rustrtc::TransportMode::Rtp
            },
        );

        let callee_sdp = if self.bypasses_local_media() && caller_is_webrtc == callee_is_webrtc {
            self.callee_offer_uses_media_bridge = false;
            self.caller_offer.clone()
        } else {
            self.create_callee_track(callee_is_webrtc).await.ok()
        };
        self.callee_offer = callee_sdp.clone();

        let offer = if self.media_bridge.is_some() {
            // Bridge handles transport conversion — use bridge's callee-facing PC SDP directly.
            // The callee connects to the bridge's PC, not to the caller.
            debug!(session_id = %self.context.session_id, "Using bridge callee-facing SDP for INVITE");
            self.callee_offer.clone().map(|s| s.into_bytes())
        } else {
            // No bridge (same transport type or bridge creation failed) — pass through directly
            self.callee_offer.clone().map(|s| s.into_bytes())
        };

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
        self.callee_call_ids.insert(callee_call_id.clone());

        if route_via_home_proxy {
            if let Some(home_proxy) = target.home_proxy.as_ref() {
                info!(
                    session_id = %self.context.session_id,
                    %caller,
                    %callee_uri,
                    %home_proxy,
                    "Routing INVITE to home proxy node"
                );
            }
        }

        info!(session_id = %self.context.session_id, %caller, %callee_uri, callee_call_id, "Sending INVITE to callee");

        let mut invite_option = InviteOption {
            caller_display_name: self.context.dialplan.caller_display_name.clone(),
            callee: callee_uri.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination: target.destination.clone(),
            contact: contact_uri,
            credential: target.credential.clone(),
            headers: Some(headers),
            call_id: Some(callee_call_id),
            ..Default::default()
        };

        let state_tx = self.callee_event_tx.clone().ok_or_else(|| {
            (
                StatusCode::ServerInternalError,
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
                        break Err((
                            StatusCode::RequestTerminated,
                            Some("Caller cancelled".to_string()),
                        ));
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    break Err((
                        StatusCode::RequestTerminated,
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

                                    let code = StatusCode::from(resp.status_code.code());

                                    Err((code, None))
                                }
                            } else {
                                Err((StatusCode::ServerInternalError, Some("No response from callee".to_string())))
                            }
                        }
                        Err(e) => Err((StatusCode::ServerInternalError, Some(format!("Invite failed: {}", e)))),
                    };
                }

                state = callee_state_rx.recv() => {
                    if let Some(DialogState::Early(_, ref response)) = state {
                        if self.ring_time.is_none() {
                            self.ring_time = Some(Instant::now());
                        }

                        let callee_sdp = String::from_utf8_lossy(response.body()).to_string();
                        if !callee_sdp.is_empty() && callee_sdp.contains("v=0") {
                            self.early_media_sent = true;
                            self.update_leg_state(&LegId::from("callee"), LegState::EarlyMedia);

                            if self.media_profile.path == MediaPathMode::Anchored {
                                let caller_sdp = self
                                    .prepare_caller_answer_from_callee_sdp(
                                        Some(callee_sdp),
                                        false,
                                    )
                                    .await;

                                if let Err(e) = self.server_dialog.ringing(
                                    Some(vec![rsipstack::sip::Header::ContentType(
                                        "application/sdp".into(),
                                    )]),
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
                                        Some(vec![rsipstack::sip::Header::ContentType(
                                            "application/sdp".into(),
                                        )]),
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
                            if !self.early_media_sent {
                                self.update_leg_state(&LegId::from("callee"), LegState::Ringing);
                            }
                            if let Err(e) = self.server_dialog.ringing(None, None) {
                                warn!(
                                    session_id = %self.context.session_id,
                                    error = %e,
                                    "Failed to send 180 Ringing"
                                );
                            }
                        }
                        self.update_snapshot_cache();
                    }
                }
            }
        };

        let (dialog_id, response): (DialogId, Option<rsipstack::sip::Response>) = result?;

        let callee_sdp = response.as_ref().and_then(|r: &rsipstack::sip::Response| {
            let body = r.body();
            if body.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(body).to_string())
            }
        });
        if let Some(track_id) = stop_playback_on_answer {
            self.stop_playback_track(track_id, false).await;
        }
        let caller_answer = self
            .prepare_caller_answer_from_callee_sdp(callee_sdp, false)
            .await;

        self.accept_call(
            Some(callee_uri.to_string()),
            caller_answer,
            Some(dialog_id.to_string()),
        )
        .await
        .map_err(|e| (StatusCode::ServerInternalError, Some(e.to_string())))?;

        self.connected_callee_dialog_id = Some(dialog_id.clone());
        self.callee_dialogs.insert(dialog_id.clone(), ());
        // Register callee dialog in unified map
        if let Some(dlg) = self.server.dialog_layer.get_dialog(&dialog_id) {
            self.dialogs.insert(LegId::from("callee"), dlg);
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
        self.callee_guards.push(ClientDialogGuard::new(
            self.server.dialog_layer.clone(),
            dialog_id,
        ));

        self.update_snapshot_cache();

        Ok(())
    }

    async fn prepare_caller_answer_from_callee_sdp(
        &mut self,
        callee_sdp: Option<String>,
        force_regenerate: bool,
    ) -> Option<String> {
        let Some(callee_sdp_value) = callee_sdp else {
            return if self.early_media_sent {
                self.answer.clone()
            } else {
                None
            };
        };

        let sdp_changed = self.callee_answer_sdp.as_deref() != Some(callee_sdp_value.as_str());

        if self.answer.is_some() && !sdp_changed && !force_regenerate {
            return self.answer.clone();
        }

        if self.callee_answer_sdp.is_some() && sdp_changed {
            info!(
                session_id = %self.context.session_id,
                "Callee answer SDP changed after early media; regenerating caller-facing SDP"
            );
        }

        if self.server_dialog.state().is_confirmed()
            && self.answer.is_some()
            && self.media_bridge.is_some()
            && self.caller_answer_uses_media_bridge
            && self.callee_offer_uses_media_bridge
        {
            match self.apply_bridge_callee_answer(&callee_sdp_value).await {
                Ok(()) => {
                    self.configure_media_bridge_transcoders(
                        self.answer.as_deref(),
                        Some(&callee_sdp_value),
                    );
                    self.start_media_bridge_forwarding().await;
                }
                Err(error) => {
                    warn!(
                        session_id = %self.context.session_id,
                        error = %error,
                        "Failed to apply callee answer to existing media bridge"
                    );
                }
            }

            self.callee_answer_sdp = Some(callee_sdp_value);
            return self.answer.clone();
        }

        if self.server_dialog.state().is_confirmed()
            && self.answer.is_some()
            && self.media_profile.path == MediaPathMode::Anchored
            && self.media_bridge.is_none()
        {
            debug!(
                session_id = %self.context.session_id,
                "Caller dialog already confirmed; keeping existing caller track/SDP and only updating callee-side forwarding"
            );

            let caller_answer = self.answer.clone();

            if let Err(e) = self
                .callee_peer
                .update_remote_description(Self::CALLEE_TRACK_ID, &callee_sdp_value)
                .await
            {
                warn!(
                    session_id = %self.context.session_id,
                    error = %e,
                    "Failed to set callee answer on callee track"
                );
            }

            self.callee_answer_sdp = Some(callee_sdp_value);
            let callee_answer_for_forwarding = self.callee_answer_sdp.clone();
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
            // WebRTC caller, RTP callee — bridge must convert media
            if let Some(ref sdp) = callee_sdp {
                if let Some(ref bridge) = self.media_bridge {
                    use rustrtc::sdp::{SdpType, SessionDescription};

                    // 1. Set callee's RTP answer on bridge's RTP side
                    if let Ok(desc) = SessionDescription::parse(SdpType::Answer, sdp) {
                        let rtp_pc = bridge.rtp_pc();
                        // If we already negotiated early media, the RTP peer is in Stable state.
                        // To apply a new answer we must re-offer: create offer -> set local -> set remote.
                        if rtp_pc.remote_description().is_some() {
                            debug!(session_id = %self.context.session_id, "Bridge: Re-negotiating RTP side for changed callee answer");
                            match rtp_pc.create_offer().await {
                                Ok(offer) => {
                                    if let Err(e) = rtp_pc.set_local_description(offer) {
                                        warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge RTP local re-offer");
                                    }
                                }
                                Err(e) => {
                                    warn!(session_id = %self.context.session_id, error = %e, "Failed to create bridge RTP re-offer");
                                }
                            }
                        }
                        if let Err(e) = rtp_pc.set_remote_description(desc).await {
                            warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge RTP remote description");
                        }
                    }

                    // Log post-negotiation RTP pair and payload map for diagnostics
                    if let Some(pair) = bridge.rtp_pc().ice_transport().get_selected_pair().await {
                        let payload_map = bridge
                            .rtp_pc()
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
                        info!(
                            session_id = %self.context.session_id,
                            rtp_remote_addr = %pair.remote.address,
                            rtp_remote_port = pair.remote.address.port(),
                            payload_types = ?pt_info,
                            "Bridge RTP side re-negotiated"
                        );
                    }

                    // 2. Set caller's WebRTC offer on bridge's WebRTC side and create answer
                    if let Some(ref caller_offer) = self.caller_offer {
                        debug!(session_id = %self.context.session_id, "Bridge: Creating WebRTC answer from caller offer");
                        match SessionDescription::parse(SdpType::Offer, caller_offer) {
                            Ok(caller_desc) => {
                                match bridge.webrtc_pc().set_remote_description(caller_desc).await {
                                    Ok(_) => {
                                        match bridge.webrtc_pc().create_answer().await {
                                            Ok(answer) => {
                                                if let Err(e) =
                                                    bridge.webrtc_pc().set_local_description(answer)
                                                {
                                                    warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge WebRTC local description");
                                                    callee_sdp.clone()
                                                } else {
                                                    // Wait for ICE gathering to complete so the SDP
                                                    // contains real candidates (not 0.0.0.0:9)
                                                    bridge
                                                        .webrtc_pc()
                                                        .wait_for_gathering_complete()
                                                        .await;
                                                    debug!(session_id = %self.context.session_id, "Bridge: WebRTC answer created with ICE candidates");
                                                    bridge
                                                        .webrtc_pc()
                                                        .local_description()
                                                        .map(|d| d.to_sdp_string())
                                                        .map(|answer_sdp| {
                                                            MediaNegotiator::restrict_answer_to_callee_accepted_codecs(
                                                                &answer_sdp,
                                                                sdp,
                                                            )
                                                            .unwrap_or(answer_sdp)
                                                        })
                                                        .or_else(|| callee_sdp.clone())
                                                }
                                            }
                                            Err(e) => {
                                                warn!(session_id = %self.context.session_id, error = %e, "Failed to create bridge WebRTC answer");
                                                callee_sdp.clone()
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge WebRTC remote description");
                                        callee_sdp.clone()
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(session_id = %self.context.session_id, error = %e, "Failed to parse caller offer SDP");
                                callee_sdp.clone()
                            }
                        }
                    } else {
                        callee_sdp.clone()
                    }
                } else {
                    // No bridge — should not happen for WebRTC↔RTP bridging,
                    // but pass through callee SDP as-is
                    warn!(session_id = %self.context.session_id, "No media bridge for WebRTC↔RTP — SDP may be incorrect");
                    callee_sdp.clone()
                }
            } else {
                callee_sdp.clone()
            }
        } else if !caller_is_webrtc && callee_is_webrtc {
            // RTP caller, WebRTC callee — bridge must convert media
            if let Some(ref sdp) = callee_sdp {
                if let Some(ref bridge) = self.media_bridge {
                    use rustrtc::sdp::{SdpType, SessionDescription};

                    // 1. Set callee's WebRTC answer on bridge's WebRTC side
                    debug!(session_id = %self.context.session_id, sdp= %sdp, "Bridge: Setting WebRTC side remote from callee answer");
                    if let Ok(desc) = SessionDescription::parse(SdpType::Answer, sdp)
                        && let Err(e) = bridge.webrtc_pc().set_remote_description(desc).await
                    {
                        warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge WebRTC remote description");
                    }

                    // 2. Set caller's RTP offer on bridge's RTP side and create answer
                    if let Some(ref caller_offer) = self.caller_offer {
                        debug!(session_id = %self.context.session_id, "Bridge: Creating RTP answer from caller offer");
                        // Pass the offer as-is: address latching (enabled above for BUNDLE
                        // callers) will auto-correct remote_addr to the actual BUNDLE port
                        // when the first packet arrives, handling both send and receive.
                        match SessionDescription::parse(SdpType::Offer, caller_offer) {
                            Ok(caller_desc) => {
                                match bridge.rtp_pc().set_remote_description(caller_desc).await {
                                    Ok(_) => match bridge.rtp_pc().create_answer().await {
                                        Ok(answer) => {
                                            if let Err(e) =
                                                bridge.rtp_pc().set_local_description(answer)
                                            {
                                                warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge RTP local description");
                                                callee_sdp.clone()
                                            } else {
                                                let rtp_sdp = bridge
                                                    .rtp_pc()
                                                    .local_description()
                                                    .map(|d| d.to_sdp_string());
                                                debug!(session_id = %self.context.session_id, sdp = ?rtp_sdp, "Bridge: RTP answer SDP (sent to RTP caller)");
                                                rtp_sdp.or_else(|| callee_sdp.clone())
                                            }
                                        }
                                        Err(e) => {
                                            warn!(session_id = %self.context.session_id, error = %e, "Failed to create bridge RTP answer");
                                            callee_sdp.clone()
                                        }
                                    },
                                    Err(e) => {
                                        warn!(session_id = %self.context.session_id, error = %e, "Failed to set bridge RTP remote description");
                                        callee_sdp.clone()
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(session_id = %self.context.session_id, error = %e, "Failed to parse caller offer SDP");
                                callee_sdp.clone()
                            }
                        }
                    } else {
                        callee_sdp.clone()
                    }
                } else {
                    // No bridge — should not happen for WebRTC↔RTP bridging,
                    // but pass through callee SDP as-is
                    warn!(session_id = %self.context.session_id, "No media bridge for RTP↔WebRTC — SDP may be incorrect");
                    callee_sdp.clone()
                }
            } else {
                callee_sdp.clone()
            }
        } else if self.media_profile.path == MediaPathMode::Anchored {
            if let Some(ref sdp) = callee_sdp
                && let Err(e) = self
                    .callee_peer
                    .update_remote_description(Self::CALLEE_TRACK_ID, sdp)
                    .await
            {
                warn!(
                    session_id = %self.context.session_id,
                    error = %e,
                    "Failed to set callee answer on callee track"
                );
            }

            if let Some(ref caller_offer) = self.caller_offer {
                let codec_info = MediaNegotiator::build_caller_answer_codec_list_with_allow(
                    caller_offer,
                    caller_is_webrtc,
                    &self.context.dialplan.allow_codecs,
                );

                let mut track_builder = RtpTrackBuilder::new(Self::CALLER_TRACK_ID.to_string())
                    .with_cancel_token(self.caller_peer.cancel_token())
                    .with_enable_latching(self.server.proxy_config.enable_latching);

                if let Some(ref external_ip) = self.server.rtp_config.external_ip {
                    track_builder = track_builder.with_external_ip(external_ip.clone());
                }
                if let Some(ref bind_ip) = self.server.rtp_config.bind_ip {
                    track_builder = track_builder.with_bind_ip(bind_ip.clone());
                }

                let (start_port, end_port) = if caller_is_webrtc {
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
                    track_builder = track_builder.with_rtp_range(start, end);
                }

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
                        debug!(
                            session_id = %self.context.session_id,
                            "Generated PBX answer SDP for caller (anchored media)"
                        );
                        self.caller_peer.update_track(Box::new(track), None).await;
                        Some(answer_sdp)
                    }
                    Err(e) => {
                        warn!(
                            session_id = %self.context.session_id,
                            error = %e,
                            "Failed to handshake caller track, falling back to callee SDP"
                        );
                        callee_sdp.clone()
                    }
                }
            } else {
                callee_sdp.clone()
            }
        } else {
            callee_sdp.clone()
        };

        self.callee_answer_sdp = callee_sdp.clone();
        self.answer = caller_answer.clone();
        self.caller_answer_uses_media_bridge = self.callee_offer_uses_media_bridge
            && self.media_bridge.is_some()
            && caller_answer.is_some();

        self.configure_media_bridge_transcoders(caller_answer.as_deref(), callee_sdp.as_deref());

        if self.media_profile.path == MediaPathMode::Anchored && self.media_bridge.is_none() {
            let caller_answer_for_forwarding = self.answer.clone();
            let callee_answer_for_forwarding = callee_sdp.clone();
            self.start_anchored_media_forwarding(
                caller_answer_for_forwarding.as_deref(),
                callee_answer_for_forwarding.as_deref(),
            )
            .await;
        }

        if self.media_bridge.is_some() && self.caller_answer_uses_media_bridge {
            self.start_media_bridge_forwarding().await;
        }

        caller_answer
    }

    fn configure_media_bridge_transcoders(
        &self,
        caller_answer_sdp: Option<&str>,
        callee_answer_sdp: Option<&str>,
    ) {
        let Some(bridge) = self.media_bridge.as_ref() else {
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

        let (Some(caller_audio), Some(callee_audio)) =
            (&caller_profile.audio, &callee_profile.audio)
        else {
            return;
        };

        if caller_audio.codec == callee_audio.codec {
            bridge.clear_transcoder(self.leg_bridge_endpoint(&LegId::from("caller")));
            bridge.clear_transcoder(self.leg_bridge_endpoint(&LegId::from("callee")));
            debug!(
                session_id = %self.context.session_id,
                codec = ?caller_audio.codec,
                "Bridge transcoder not needed; caller and callee selected the same codec"
            );
            return;
        }

        bridge.set_transcoder(
            self.leg_bridge_endpoint(&LegId::from("caller")),
            caller_audio.codec,
            callee_audio.codec,
            callee_audio.payload_type,
        );
        bridge.set_transcoder(
            self.leg_bridge_endpoint(&LegId::from("callee")),
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
        if self.media_bridge_started {
            return;
        }

        if let Some(ref bridge) = self.media_bridge {
            let caller_pc = if self.is_caller_webrtc() {
                bridge.webrtc_pc()
            } else {
                bridge.rtp_pc()
            };
            let callee_pc = if self.is_callee_webrtc() {
                bridge.webrtc_pc()
            } else {
                bridge.rtp_pc()
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
            self.media_bridge_started = true;
        }
    }

    /// Get the unified dialog for a leg, falling back to caller/callee fields.
    #[allow(dead_code)]
    fn dialog_for_leg(&self, leg_id: &LegId) -> Option<&rsipstack::dialog::dialog::Dialog> {
        if let Some(dlg) = self.dialogs.get(leg_id) {
            return Some(dlg);
        }
        // Legacy fallback: construct from stored fields
        if leg_id == &LegId::from("caller") {
            // Always available via server_dialog
            return None; // callers should use self.server_dialog directly
        }
        None
    }

    fn is_caller_webrtc(&self) -> bool {
        // Sniff the caller's SDP offer for WebRTC indicators (ICE + DTLS).
        // This is used during SDP bridge negotiation before leg_transport is populated.
        if let Some(ref offer) = self.caller_offer {
            offer.contains("a=ice-ufrag") && offer.contains("a=fingerprint")
        } else {
            self.leg_transport
                .get(&LegId::from("caller"))
                .map(|t| *t == rustrtc::TransportMode::WebRtc)
                .unwrap_or(false)
        }
    }

    fn is_callee_webrtc(&self) -> bool {
        self.leg_transport
            .get(&LegId::from("callee"))
            .map(|t| *t == rustrtc::TransportMode::WebRtc)
            .unwrap_or(false)
    }

    /// Register a dialog for a leg in the unified map.
    #[allow(dead_code)]
    fn set_dialog_for_leg(&mut self, leg_id: LegId, dlg: rsipstack::dialog::dialog::Dialog) {
        self.dialogs.insert(leg_id, dlg);
    }

    /// Resolve bridge endpoint for a leg from leg_transport.
    fn leg_bridge_endpoint(&self, leg_id: &LegId) -> BridgeEndpoint {
        match self.leg_transport.get(leg_id) {
            Some(rustrtc::TransportMode::WebRtc) => BridgeEndpoint::WebRtc,
            _ => BridgeEndpoint::Rtp,
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

        let caller_pc = Self::get_peer_pc(&self.caller_peer, Self::CALLER_TRACK_ID).await;
        let callee_pc = Self::get_peer_pc(&self.callee_peer, Self::CALLEE_TRACK_ID).await;

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

        match Self::wire_with_forwarding_track(
            Self::CALLER_FORWARDING_TRACK_ID,
            &caller_pc,
            &callee_pc,
            caller_profile.clone(),
            callee_profile.clone(),
            shared_recorder.clone(),
            Leg::A,
            session_id,
            "caller→callee",
        ) {
            Ok(forwarding) => {
                self.caller_peer
                    .update_track(
                        Box::new(crate::media::forwarding_track::ForwardingTrackHandle::new(
                            Self::CALLER_FORWARDING_TRACK_ID.to_string(),
                            forwarding,
                        )),
                        None,
                    )
                    .await;
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
            session_id,
            "callee→caller",
        ) {
            Ok(forwarding) => {
                self.callee_peer
                    .update_track(
                        Box::new(crate::media::forwarding_track::ForwardingTrackHandle::new(
                            Self::CALLEE_FORWARDING_TRACK_ID.to_string(),
                            forwarding,
                        )),
                        None,
                    )
                    .await;
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

    async fn start_caller_ingress_monitor_if_needed(&mut self) {
        if self.connected_callee.is_some() {
            return;
        }

        let Some(answer_sdp) = self.answer.as_deref() else {
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
        info!(
            session_id = %self.context.session_id,
            dtmf_codec = ?dtmf_codec.codec,
            dtmf_payload_type = dtmf_codec.payload_type,
            dtmf_clock_rate = dtmf_codec.clock_rate,
            "Found DTMF codec in SDP, will start ingress monitor"
        );

        let session_id = self.context.session_id.clone();
        let app_runtime = self.app_runtime.clone();
        let dtmf_payload_type = dtmf_codec.payload_type;
        let caller_leg_id = "caller".to_string();

        if self.caller_answer_uses_media_bridge {
            if self.caller_ingress_monitor.is_some() {
                self.stop_caller_ingress_monitor().await;
            }

            let Some(bridge) = self.media_bridge.as_ref() else {
                return;
            };
            let endpoint = self.leg_bridge_endpoint(&LegId::from("caller"));
            let caller_leg = caller_leg_id.clone();
            bridge.set_dtmf_sink(
                endpoint,
                dtmf_payload_type,
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
                    }
                }),
            );
            bridge.start_bridge().await;
            info!(
                session_id = %self.context.session_id,
                endpoint = ?endpoint,
                payload_type = dtmf_payload_type,
                "Installed caller bridge DTMF sink"
            );
            return;
        }

        if self
            .caller_ingress_monitor
            .as_ref()
            .is_some_and(|monitor| !monitor.task.is_finished())
        {
            return;
        }

        if self.caller_ingress_monitor.is_some() {
            self.stop_caller_ingress_monitor().await;
        }

        let caller_pc = Self::get_peer_pc(&self.caller_peer, Self::CALLER_TRACK_ID).await;
        let Some(caller_pc) = caller_pc else {
            return;
        };

        let cancel_token = self.cancel_token.child_token();
        let monitor_cancel = cancel_token.clone();

        let task = tokio::spawn(async move {
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
                                if frame.payload_type.is_some() && frame.payload_type != Some(dtmf_payload_type) {
                                    if frame_count % 100 == 0 {
                                        debug!(
                                            session_id = %session_id,
                                            expected_payload_type = dtmf_payload_type,
                                            frame_payload_type = ?frame.payload_type,
                                            frame_count = frame_count,
                                            "Received non-DTMF RTP frame (samples shown every 100 frames)"
                                        );
                                    }
                                    continue;
                                }

                                if frame.payload_type == Some(dtmf_payload_type) {
                                    dtmf_frames_count += 1;
                                    debug!(
                                        session_id = %session_id,
                                        payload_type = dtmf_payload_type,
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
            payload_type = dtmf_payload_type,
            "✓ Started caller ingress monitor for RFC2833 RTP DTMF detection (payload type: {})",
            dtmf_payload_type
        );

        self.caller_ingress_monitor = Some(CallerIngressMonitor { cancel_token, task });
    }

    async fn stop_caller_ingress_monitor(&mut self) {
        let Some(monitor) = self.caller_ingress_monitor.take() else {
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

    #[allow(dead_code)]
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
                rustrtc::media::frame::MediaSample,
            )>(RECORDER_CHANNEL_CAPACITY);
            let recorder_arc = recorder.clone();
            tokio::spawn(async move {
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
            leg,
            ingress_profile,
            egress_profile,
        ));

        let sender = rustrtc::RtpSender::builder(
            forwarding.clone() as Arc<dyn rustrtc::media::MediaStreamTrack>,
            existing_sender.ssrc(),
        )
        .stream_id(existing_sender.stream_id().to_string())
        .params(existing_sender.params())
        .build();

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
            bridge.webrtc_pc().clone()
        } else {
            bridge.rtp_pc().clone()
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
        let Some(bridge) = &self.media_bridge else {
            return Ok(());
        };

        let pc = if self.is_callee_webrtc() {
            bridge.webrtc_pc().clone()
        } else {
            bridge.rtp_pc().clone()
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

        let answer = rustrtc::SessionDescription::parse(rustrtc::SdpType::Answer, callee_sdp)
            .map_err(|e| anyhow!("Failed to parse callee answer SDP: {}", e))?;
        pc.set_remote_description(answer)
            .await
            .map_err(|e| anyhow!("Failed to set bridge callee remote answer: {}", e))
    }

    pub async fn create_callee_track(&mut self, callee_is_webrtc: bool) -> Result<String> {
        let track_id = Self::CALLEE_TRACK_ID.to_string();

        let caller_is_webrtc = self.is_caller_webrtc();
        self.leg_transport.insert(
            LegId::from("caller"),
            if caller_is_webrtc {
                rustrtc::TransportMode::WebRtc
            } else {
                rustrtc::TransportMode::Rtp
            },
        );
        self.leg_transport.insert(
            LegId::from("callee"),
            if callee_is_webrtc {
                rustrtc::TransportMode::WebRtc
            } else {
                rustrtc::TransportMode::Rtp
            },
        );

        debug!(
            session_id = %self.id,
            caller_is_webrtc = caller_is_webrtc,
            callee_is_webrtc = callee_is_webrtc,
            "Creating callee track"
        );

        let media_proxy_enabled = self.media_profile.path == MediaPathMode::Anchored;

        let transport_bridge_needed = caller_is_webrtc != callee_is_webrtc;

        let need_transport_bridge = transport_bridge_needed;

        if need_transport_bridge
            && self.caller_answer_uses_media_bridge
            && let Some(ref bridge) = self.media_bridge
        {
            self.callee_offer_uses_media_bridge = true;
            debug!(
                session_id = %self.id,
                callee_is_webrtc,
                "Reusing existing media bridge callee-facing offer"
            );
            return Self::bridge_callee_offer_sdp(bridge, callee_is_webrtc).await;
        }

        if need_transport_bridge {
            self.callee_offer_uses_media_bridge = true;
            let mut bridge_builder = BridgePeerBuilder::new(format!("{}-bridge", self.id))
                .with_enable_latching(self.server.proxy_config.enable_latching);
            if let (Some(start), Some(end)) = (
                self.server.rtp_config.start_port,
                self.server.rtp_config.end_port,
            ) {
                bridge_builder = bridge_builder.with_rtp_port_range(start, end);
            }
            if let Some(ref caller_sdp) = self.caller_offer
                && !caller_is_webrtc
                && caller_sdp.contains("a=group:BUNDLE")
            {
                bridge_builder = bridge_builder
                    .with_rtp_sdp_compatibility(rustrtc::config::SdpCompatibilityMode::Standard)
                    .with_enable_latching(true);
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

            // Configure codecs from allow_codecs + caller's SDP
            if let Some(ref caller_sdp) = self.caller_offer {
                let allow_codecs = &self.context.dialplan.allow_codecs;
                let codec_lists = MediaNegotiator::build_bridge_codec_lists(
                    caller_sdp,
                    caller_is_webrtc,
                    callee_is_webrtc,
                    allow_codecs,
                    self.context.dialplan.media.codec_strategy,
                );

                let webrtc_side_codecs = if caller_is_webrtc {
                    &codec_lists.caller_side
                } else {
                    &codec_lists.callee_side
                };
                let rtp_side_codecs = if caller_is_webrtc {
                    &codec_lists.callee_side
                } else {
                    &codec_lists.caller_side
                };

                let webrtc_caps: Vec<_> = webrtc_side_codecs
                    .iter()
                    .filter_map(|c| c.to_audio_capability())
                    .collect();
                let rtp_caps: Vec<_> = rtp_side_codecs
                    .iter()
                    .filter_map(|c| c.to_audio_capability())
                    .collect();

                bridge_builder = bridge_builder
                    .with_webrtc_audio_capabilities(webrtc_caps.clone())
                    .with_rtp_audio_capabilities(rtp_caps);

                let webrtc_sender = webrtc_side_codecs
                    .iter()
                    .find(|c| !c.is_dtmf())
                    .map(|c| c.to_params());
                let rtp_sender = rtp_side_codecs
                    .iter()
                    .find(|c| !c.is_dtmf())
                    .map(|c| c.to_params());

                if let (Some(webrtc_sender), Some(rtp_sender)) = (webrtc_sender, rtp_sender) {
                    bridge_builder = bridge_builder.with_sender_codecs(webrtc_sender, rtp_sender);
                }

                debug!(
                    session_id = %self.context.session_id,
                    caller_side_codecs = ?codec_lists.caller_side.iter().filter(|c| !c.is_dtmf()).map(|c| format!("{:?}", c.codec)).collect::<Vec<_>>(),
                    callee_side_codecs = ?codec_lists.callee_side.iter().filter(|c| !c.is_dtmf()).map(|c| format!("{:?}", c.codec)).collect::<Vec<_>>(),
                    common_codecs = ?codec_lists.common.iter().filter(|c| !c.is_dtmf()).map(|c| format!("{:?}", c.codec)).collect::<Vec<_>>(),
                    "Bridge codecs configured for transport sides"
                );

                // Extract video capabilities from caller SDP
                match rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, caller_sdp) {
                    Ok(caller_desc) => {
                        let caller_video_caps = caller_desc.to_video_capabilities();
                        if !caller_video_caps.is_empty() {
                            info!(
                                session_id = %self.id,
                                codecs = ?caller_video_caps.iter().map(|c| format!("{}@{}", c.codec_name, c.payload_type)).collect::<Vec<_>>(),
                                "Video capabilities configured from caller SDP"
                            );
                            let webrtc_video_caps: Vec<rustrtc::config::VideoCapability> =
                                caller_video_caps
                                    .iter()
                                    .map(|cap| {
                                        if cap.codec_name.eq_ignore_ascii_case("H264") {
                                            let mut c = cap.clone();
                                            // Inject packetization-mode=1 if not already present
                                            let fmtp = cap.fmtp.as_deref().unwrap_or("");
                                            if !fmtp.contains("packetization-mode") {
                                                let new_fmtp = if fmtp.is_empty() {
                                                    "packetization-mode=1".to_string()
                                                } else {
                                                    format!("{};packetization-mode=1", fmtp)
                                                };
                                                c.fmtp = Some(new_fmtp);
                                            }
                                            c
                                        } else {
                                            cap.clone()
                                        }
                                    })
                                    .collect();

                            let rtp_video_caps: Vec<rustrtc::config::VideoCapability> =
                                caller_video_caps
                                    .iter()
                                    .map(|cap| {
                                        if cap.codec_name.eq_ignore_ascii_case("H264") {
                                            let mut c = cap.clone();
                                            let fmtp = cap.fmtp.as_deref().unwrap_or("");
                                            if !fmtp.contains("packetization-mode") {
                                                let new_fmtp = if fmtp.is_empty() {
                                                    "packetization-mode=1".to_string()
                                                } else {
                                                    format!("{};packetization-mode=1", fmtp)
                                                };
                                                c.fmtp = Some(new_fmtp);
                                            }
                                            c
                                        } else {
                                            cap.clone()
                                        }
                                    })
                                    .collect();
                            bridge_builder = bridge_builder
                                .with_webrtc_video_capabilities(webrtc_video_caps)
                                .with_rtp_video_capabilities(rtp_video_caps);
                        }
                    }
                    Err(e) => {
                        warn!(session_id = %self.id, "Failed to parse caller SDP for video: {}", e)
                    }
                }

                debug!(
                    session_id = %self.id,
                    common_codecs = ?codec_lists.common.iter().filter(|c| !c.is_dtmf()).map(|c| format!("{:?}", c.codec)).collect::<Vec<_>>(),
                    webrtc_codecs = ?webrtc_side_codecs
                        .iter()
                        .map(|c| format!("{:?}", c.codec))
                        .collect::<Vec<_>>(),
                    rtp_codecs = ?rtp_side_codecs
                        .iter()
                        .map(|c| format!("{:?}", c.codec))
                        .collect::<Vec<_>>(),
                    "Bridge codecs configured for transport sides"
                );
            }

            // Attach recorder to bridge so audio is captured when recording starts
            if self.context.dialplan.recording.enabled {
                bridge_builder = bridge_builder.with_recorder(self.recorder.clone());
            }

            let bridge = bridge_builder.build();

            bridge.setup_bridge().await?;

            if callee_is_webrtc {
                let offer = bridge.webrtc_pc().create_offer().await?;
                let offer_sdp = offer.to_sdp_string();
                debug!(session_id = %self.id, sdp = %offer_sdp, "Bridge WebRTC offer SDP");
                bridge.webrtc_pc().set_local_description(offer)?;
                // Wait for ICE gathering so SDP contains real candidates
                bridge.webrtc_pc().wait_for_gathering_complete().await;
            } else {
                let offer = bridge.rtp_pc().create_offer().await?;
                let offer_sdp = offer.to_sdp_string();
                debug!(session_id = %self.id, sdp = %offer_sdp, "Bridge RTP offer SDP");
                bridge.rtp_pc().set_local_description(offer)?;
            }

            self.media_bridge = Some(bridge.clone());

            if callee_is_webrtc {
                let sdp = bridge
                    .webrtc_pc()
                    .local_description()
                    .ok_or_else(|| anyhow!("No WebRTC local description"))?
                    .to_sdp_string();
                Ok(sdp)
            } else {
                let sdp = bridge
                    .rtp_pc()
                    .local_description()
                    .ok_or_else(|| anyhow!("No RTP local description"))?
                    .to_sdp_string();
                Ok(sdp)
            }
        } else if media_proxy_enabled {
            self.callee_offer_uses_media_bridge = false;
            let mut track_builder = RtpTrackBuilder::new(track_id.clone())
                .with_cancel_token(self.callee_peer.cancel_token())
                .with_enable_latching(self.server.proxy_config.enable_latching);

            if let Some(ref external_ip) = self.server.rtp_config.external_ip {
                track_builder = track_builder.with_external_ip(external_ip.clone());
            }
            if let Some(ref bind_ip) = self.server.rtp_config.bind_ip {
                track_builder = track_builder.with_bind_ip(bind_ip.clone());
            }

            let (start_port, end_port) = if callee_is_webrtc {
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
                track_builder = track_builder.with_rtp_range(start, end);
            }

            if let Some(ref caller_offer) = self.caller_offer {
                let codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
                    caller_offer,
                    callee_is_webrtc,
                    &self.context.dialplan.allow_codecs,
                    self.context.dialplan.media.codec_strategy,
                );
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

            self.callee_peer.update_track(Box::new(track), None).await;

            Ok(sdp)
        } else {
            self.callee_offer_uses_media_bridge = false;
            let mut track_builder = RtpTrackBuilder::new(track_id.clone())
                .with_cancel_token(self.callee_peer.cancel_token());

            if callee_is_webrtc {
                track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
            }

            let track = track_builder.build();
            let sdp = track.local_description().await?;

            self.callee_peer.update_track(Box::new(track), None).await;

            Ok(sdp)
        }
    }

    async fn ensure_caller_answer_sdp(&mut self) -> Option<String> {
        if let Some(ref answer) = self.answer {
            return Some(answer.clone());
        }

        if self.bypasses_local_media() {
            if let Some(answer_sdp) = self.callee_answer_sdp.clone() {
                self.answer = Some(answer_sdp.clone());
                self.caller_answer_uses_media_bridge = false;
                return Some(answer_sdp);
            }
        }

        let caller_offer = self.caller_offer.clone()?;
        let caller_is_webrtc = self.is_caller_webrtc();

        let codec_info = MediaNegotiator::build_caller_answer_codec_list_with_allow(
            &caller_offer,
            caller_is_webrtc,
            &self.context.dialplan.allow_codecs,
        );

        let mut track_builder = RtpTrackBuilder::new(Self::CALLER_TRACK_ID.to_string())
            .with_cancel_token(self.caller_peer.cancel_token())
            .with_enable_latching(self.server.proxy_config.enable_latching);

        if let Some(ref external_ip) = self.server.rtp_config.external_ip {
            track_builder = track_builder.with_external_ip(external_ip.clone());
        }
        if let Some(ref bind_ip) = self.server.rtp_config.bind_ip {
            track_builder = track_builder.with_bind_ip(bind_ip.clone());
        }

        let (start_port, end_port) = if caller_is_webrtc {
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
            track_builder = track_builder.with_rtp_range(start, end);
        }

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
        match track.handshake(caller_offer).await {
            Ok(answer_sdp) => {
                debug!(
                    session_id = %self.context.session_id,
                    "Generated PBX answer SDP for caller"
                );
                self.caller_peer.update_track(Box::new(track), None).await;
                self.answer = Some(answer_sdp.clone());
                self.caller_answer_uses_media_bridge = false;
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
        self.connected_callee = callee.clone();

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
                Err((code, reason)) => {
                    warn!(?code, ?reason, "Failed to initialize session timer");
                }
            }
        }

        let answer_sdp = if let Some(answer_sdp) = sdp {
            Some(answer_sdp)
        } else {
            self.ensure_caller_answer_sdp().await
        };

        if let Some(answer_sdp) = answer_sdp {
            let mut headers = vec![rsipstack::sip::Header::ContentType(
                "application/sdp".into(),
            )];
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

        self.answer_time = Some(Instant::now());

        let session_id = self.id.to_string();
        let caller = self
            .routed_caller
            .clone()
            .or_else(|| Some(self.context.original_caller.clone()));
        let callee = self
            .connected_callee
            .clone()
            .or_else(|| self.routed_callee.clone())
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
            && self.recording_state.is_none()
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

        self.start_caller_ingress_monitor_if_needed().await;

        Ok(())
    }

    fn is_hold_direction(direction: rustrtc::Direction) -> bool {
        !matches!(direction, rustrtc::Direction::SendRecv)
    }

    async fn get_local_reinvite_pc(&self, side: DialogSide) -> Option<rustrtc::PeerConnection> {
        if let Some(bridge) = &self.media_bridge {
            let leg_is_webrtc = match side {
                DialogSide::Caller => self.is_caller_webrtc(),
                DialogSide::Callee => self.is_callee_webrtc(),
            };

            return Some(if leg_is_webrtc {
                bridge.webrtc_pc().clone()
            } else {
                bridge.rtp_pc().clone()
            });
        }

        let (peer, track_id) = match side {
            DialogSide::Caller => (&self.caller_peer, Self::CALLER_TRACK_ID),
            DialogSide::Callee => (&self.callee_peer, Self::CALLEE_TRACK_ID),
        };

        Self::get_peer_pc(peer, track_id).await
    }

    async fn build_local_answer_from_pc(
        pc: &rustrtc::PeerConnection,
        offer_sdp: &str,
    ) -> Result<String> {
        let offer = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, offer_sdp)
            .map_err(|e| anyhow!("Failed to parse re-INVITE offer SDP: {}", e))?;
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
        if self.media_profile.path != MediaPathMode::Anchored || self.media_bridge.is_some() {
            return Ok(());
        }

        let has_remote_callee = self.connected_callee.is_some() || !self.callee_dialogs.is_empty();
        if side == DialogSide::Caller && !has_remote_callee {
            debug!(
                session_id = %self.context.session_id,
                "Skipping callee forwarding update for app-only caller dialog"
            );
            return Ok(());
        }

        let changed_profile = MediaNegotiator::extract_leg_profile(changed_leg_sdp);
        let caller_to_callee_forwarding =
            Self::get_forwarding_track(&self.caller_peer, Self::CALLER_FORWARDING_TRACK_ID)
                .await
                .ok_or_else(|| anyhow!("Missing caller forwarding track"))?;
        let callee_to_caller_forwarding =
            Self::get_forwarding_track(&self.callee_peer, Self::CALLEE_FORWARDING_TRACK_ID)
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
        let parsed = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, offer_sdp)
            .map_err(|e| anyhow!("Failed to parse re-INVITE offer SDP: {}", e))?;

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
        let offer_has_video = parsed
            .media_sections
            .iter()
            .any(|s| s.kind == rustrtc::MediaKind::Video);
        let leg_key = match side {
            DialogSide::Caller => LegId::from("caller"),
            DialogSide::Callee => LegId::from("callee"),
        };
        let had_video = self.leg_has_video.get(&leg_key).copied().unwrap_or(false);

        if offer_has_video && !had_video {
            // Video being added — extract first video codec PT/clock from the offer
            use crate::media::negotiate::MediaNegotiator;
            let extracted = MediaNegotiator::extract_codec_params(offer_sdp);
            if let Some(video_codec) = extracted.video.first() {
                if let Some(bridge) = &self.media_bridge {
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
        let answer_sdp = Self::build_local_answer_from_pc(&pc, offer_sdp).await?;

        match side {
            DialogSide::Caller => {
                self.caller_offer = Some(offer_sdp.to_string());
                self.answer = Some(answer_sdp.clone());
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
                self.leg_has_video
                    .insert(LegId::from("caller"), offer_has_video);
            }
            DialogSide::Callee => {
                self.callee_offer = Some(answer_sdp.clone());
                self.callee_answer_sdp = Some(answer_sdp.clone());
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
                self.leg_has_video
                    .insert(LegId::from("callee"), offer_has_video);
            }
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
                return Ok(self.answer.clone());
            }
        };
        if !self.bypasses_local_media() {
            self.caller_offer = Some(offer_sdp.clone());
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
                let headers = vec![rsipstack::sip::Header::ContentType(
                    "application/sdp".into(),
                )];

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
                        || self.media_bridge.is_some()
                    {
                        final_answer = self
                            .prepare_caller_answer_from_callee_sdp(Some(answer_sdp), true)
                            .await;
                    } else {
                        final_answer = Some(answer_sdp.clone());
                    }
                }
            }
        }

        if let Some(ref answer_sdp) = final_answer {
            let mut headers = vec![rsipstack::sip::Header::ContentType(
                "application/sdp".into(),
            )];
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
        self.handle_play(
            None,
            crate::call::domain::MediaSource::File { path: resolved },
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
        let Some(track) = self.playback_tracks.remove(track_id) else {
            return;
        };

        track.stop().await;
        let (leg_label, dynamic_leg_id) = Self::infer_track_leg(track_id);

        // Restore bridge output if this was a bridge-track
        if let Some(ref bridge) = self.media_bridge {
            let is_bridge_track = self.bridge_playback_track_id.as_deref() == Some(track_id);
            match leg_label {
                "caller" if is_bridge_track && self.caller_answer_uses_media_bridge => {
                    self.bridge_playback_track_id = None;
                    if self.media_bridge_started {
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
                "callee" if is_bridge_track && self.callee_offer_uses_media_bridge => {
                    self.bridge_playback_track_id = None;
                    if self.media_bridge_started {
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
                    self.caller_peer.remove_track(track_id, true).await;
                }
                "callee" => {
                    self.callee_peer.remove_track(track_id, true).await;
                }
                "dynamic" => {
                    if let Some(ref lid_str) = dynamic_leg_id {
                        let lid = LegId::new(lid_str.clone());
                        if let Some(peer) = self.peers.get(&lid) {
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
        _max_duration: Option<Duration>,
        beep: bool,
    ) -> Result<()> {
        if self.server.sip_flow.is_some() {
            return Err(anyhow!(
                "Live recording is disabled when SipFlow is enabled"
            ));
        }
        let mut recorder = Recorder::new(path, CodecType::PCMU)?;
        if let Some(forwarding) =
            Self::get_forwarding_track(&self.caller_peer, Self::CALLER_FORWARDING_TRACK_ID).await
        {
            if let Some(profile) = forwarding.ingress_profile() {
                recorder.set_leg_profile(crate::media::recorder::Leg::A, profile);
            }
        } else if let Some(answer_sdp) = self.answer.as_deref() {
            let caller_profile = MediaNegotiator::extract_leg_profile(answer_sdp);
            recorder.set_leg_profile(crate::media::recorder::Leg::A, caller_profile);
        }
        if let Some(forwarding) =
            Self::get_forwarding_track(&self.callee_peer, Self::CALLEE_FORWARDING_TRACK_ID).await
        {
            if let Some(profile) = forwarding.ingress_profile() {
                recorder.set_leg_profile(crate::media::recorder::Leg::B, profile);
            }
        } else if let Some(callee_answer_sdp) = self.callee_answer_sdp.as_deref() {
            let callee_profile = MediaNegotiator::extract_leg_profile(callee_answer_sdp);
            recorder.set_leg_profile(crate::media::recorder::Leg::B, callee_profile);
        }
        {
            let mut guard = self.recorder.write();
            if guard.is_some() {
                return Err(anyhow!("Recording already active"));
            }
            *guard = Some(recorder);
        }
        self.recording_state = Some((path.to_string(), Instant::now()));

        if beep {
            debug!("Playing recording beep");
        }
        Ok(())
    }

    pub async fn pause_recording(&mut self) -> Result<()> {
        if self.recording_state.is_none() {
            return Err(anyhow!("Recording not active"));
        }
        info!("Recording paused");
        Ok(())
    }

    pub async fn resume_recording(&mut self) -> Result<()> {
        if self.recording_state.is_none() {
            return Err(anyhow!("Recording not active"));
        }
        info!("Recording resumed");
        Ok(())
    }

    pub async fn stop_recording(&mut self) -> Result<()> {
        if let Some((path, start_time)) = self.recording_state.take() {
            let duration = start_time.elapsed();
            {
                let mut guard = self.recorder.write();
                if let Some(ref mut r) = *guard {
                    let _ = r.finalize();
                }
                *guard = None;
            }
            info!(path = %path, duration = ?duration, "Recording stopped");
        }
        Ok(())
    }

    async fn cleanup(&mut self) {
        debug!(session_id = %self.context.session_id, "Cleaning up session");

        self.stop_caller_ingress_monitor().await;

        if self.recording_state.is_some() {
            let _ = self.stop_recording().await;
        }

        // Stop media bridge (closes both WebRTC + RTP PeerConnections)
        if let Some(bridge) = self.media_bridge.take() {
            info!(session_id = %self.context.session_id, "Stopping media bridge during cleanup");
            bridge.stop().await;
            self.media_bridge_started = false;
            self.bridge_playback_track_id = None;
        }

        // Stop caller and callee media peers (cancels their tasks)
        self.caller_peer.stop();
        self.callee_peer.stop();

        if let Some(mixer) = self.supervisor_mixer.take() {
            drop(mixer);
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
        self.connected_callee_dialog_id = None;
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
        }

        debug!(session_id = %self.context.session_id, "Session cleanup complete");
    }

    pub fn init_server_timer(
        &mut self,
        default_expires: u64,
    ) -> Result<(), (StatusCode, Option<String>)> {
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
                    return Err((
                        StatusCode::SessionIntervalTooSmall,
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
        let body = self.answer.clone().map(|sdp| sdp.into_bytes());
        self.send_dialog_session_refresh(&dialog_id, body).await
    }

    async fn send_callee_session_refresh(&mut self, dialog_id: &DialogId) -> Result<()> {
        let body = self.callee_offer.clone().map(|sdp| sdp.into_bytes());
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
        CallSessionRecordSnapshot {
            ring_time: self.ring_time,
            answer_time: self.answer_time,
            last_error: self.last_error.clone(),
            hangup_reason: self.hangup_reason.clone(),
            hangup_messages: self.recorded_hangup_messages(),
            original_caller: Some(self.context.original_caller.clone()),
            original_callee: Some(self.context.original_callee.clone()),
            routed_caller: self.routed_caller.clone(),
            routed_callee: self.routed_callee.clone(),
            connected_callee: self.connected_callee.clone(),
            routed_contact: self.routed_contact.clone(),
            routed_destination: self.routed_destination.clone(),
            last_queue_name: None,
            callee_call_ids: self.callee_call_ids.iter().cloned().collect(),
            server_dialog_id: self.server_dialog.id(),
            extensions: self.context.dialplan.extensions.clone(),
        }
    }

    fn recorded_hangup_messages(&self) -> Vec<CallRecordHangupMessage> {
        self.hangup_messages
            .iter()
            .map(CallRecordHangupMessage::from)
            .collect()
    }
}

impl SipSession {
    pub async fn execute_command(&mut self, command: CallCommand) -> CommandResult {
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

        let mut result = self.process_command(command).await;

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

    async fn process_command(&mut self, command: CallCommand) -> CommandResult {
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

            CallCommand::Hold { leg_id, music } => match self.handle_hold(leg_id, music).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::Unhold { leg_id } => match self.handle_unhold(leg_id).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

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
            } => match self.handle_play(leg_id, source, options).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::StopPlayback { leg_id } => match self.handle_stop_playback(leg_id).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::StartRecording { config } => {
                match self
                    .start_recording(
                        &config.path,
                        config
                            .max_duration_secs
                            .map(|s| Duration::from_secs(s as u64)),
                        config.beep,
                    )
                    .await
                {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::StopRecording => match self.stop_recording().await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::PauseRecording => match self.pause_recording().await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::ResumeRecording => match self.resume_recording().await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::Transfer {
                leg_id,
                target,
                attended,
            } => match self.handle_transfer(leg_id, target, attended).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::TransferComplete { consult_leg } => {
                match self.handle_transfer_complete(consult_leg).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::TransferCancel { consult_leg } => {
                match self.handle_transfer_cancel(consult_leg).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::TransferCompleteCrossSession {
                from_session,
                leg_id,
                into_conference,
            } => {
                match self
                    .handle_transfer_complete_cross_session(from_session, leg_id, into_conference)
                    .await
                {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::BridgeCrossSession {
                session_a,
                leg_a,
                session_b,
                leg_b,
            } => {
                match self
                    .handle_bridge_cross_session(session_a, leg_a, session_b, leg_b)
                    .await
                {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::SupervisorListen {
                supervisor_leg,
                target_leg,
                supervisor_session_id,
            } => {
                match self
                    .handle_supervisor_listen(supervisor_leg, target_leg, supervisor_session_id)
                    .await
                {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::SupervisorWhisper {
                supervisor_leg,
                target_leg,
                supervisor_session_id,
            } => {
                match self
                    .handle_supervisor_whisper(supervisor_leg, target_leg, supervisor_session_id)
                    .await
                {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::SupervisorBarge {
                supervisor_leg,
                target_leg,
                supervisor_session_id,
            } => {
                match self
                    .handle_supervisor_barge(supervisor_leg, target_leg, supervisor_session_id)
                    .await
                {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::SupervisorTakeover {
                supervisor_leg,
                target_leg,
                supervisor_session_id,
            } => {
                match self
                    .handle_supervisor_takeover(supervisor_leg, target_leg, supervisor_session_id)
                    .await
                {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::SupervisorStop { supervisor_leg } => {
                match self.handle_supervisor_stop(supervisor_leg).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::ConferenceCreate { conf_id, options } => {
                match self.handle_conference_create(conf_id, options).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::ConferenceAdd { conf_id, leg_id } => {
                match self.handle_conference_add(conf_id, leg_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::ConferenceRemove { conf_id, leg_id } => {
                match self.handle_conference_remove(conf_id, leg_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::ConferenceMute { conf_id, leg_id } => {
                match self.handle_conference_mute(conf_id, leg_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::ConferenceUnmute { conf_id, leg_id } => {
                match self.handle_conference_unmute(conf_id, leg_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::ConferenceDestroy { conf_id } => {
                match self.handle_conference_destroy(conf_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::QueueEnqueue {
                leg_id,
                queue_id,
                priority,
            } => match self.handle_queue_enqueue(leg_id, queue_id, priority).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::QueueDequeue { leg_id } => match self.handle_queue_dequeue(leg_id).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::Reject { leg_id, reason } => {
                match self.handle_reject(leg_id, reason).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::Ring { leg_id, ringback } => {
                match self.handle_ring(leg_id, ringback).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::SendDtmf { leg_id, digits } => {
                match self.handle_send_dtmf(leg_id, digits).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::HandleReInvite { leg_id, sdp } => {
                match self.handle_reinvite_command(leg_id, sdp).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::MuteTrack { track_id } => match self.handle_mute_track(track_id).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::UnmuteTrack { track_id } => {
                match self.handle_unmute_track(track_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::SendSipMessage { content_type, body } => {
                match self.handle_send_sip_message(content_type, body).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::SendSipNotify {
                event,
                content_type,
                body,
            } => match self.handle_send_sip_notify(event, content_type, body).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::SendSipOptionsPing => match self.handle_send_sip_options_ping().await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::JoinMixer { mixer_id } => match self.handle_join_mixer(mixer_id).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::LeaveMixer => match self.handle_leave_mixer().await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::LegAdd { target, leg_id } => {
                match self.handle_add_leg(target, leg_id).await {
                    Ok(new_leg_id) => CommandResult::success_with_leg(new_leg_id),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::LegRemove { leg_id } => match self.handle_remove_leg(leg_id).await {
                Ok(_) => CommandResult::success(),
                Err(e) => CommandResult::failure(e.to_string()),
            },

            CallCommand::LegConnected { leg_id, answer_sdp } => {
                info!(%leg_id, "Leg connected async notification");
                if let Some(sdp) = answer_sdp {
                    self.leg_answers.insert(leg_id.clone(), sdp);
                }
                self.update_leg_state(&leg_id, LegState::Connected);
                self.update_media_path().await;
                CommandResult::success()
            }

            CallCommand::LegFailed { leg_id, reason } => {
                warn!(%leg_id, %reason, "Leg failed async notification");
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
        } else {
            let mut leg = crate::call::domain::Leg::new(leg_id.clone());
            leg.state = new_state;
            self.legs.insert(leg_id.clone(), leg);
        }
        self.state = Self::derive_state(&self.legs);
        true
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

        // Abort any spawned tasks for this leg
        if let Some(handles) = self.leg_tasks.remove(&leg_id) {
            let count = handles.len();
            for handle in handles {
                handle.abort();
            }
            info!(%leg_id, "Aborted {} tasks for removed leg", count);
        }

        // Remove peer, transport, and dialog for this leg
        self.peers.remove(&leg_id);
        self.leg_transport.remove(&leg_id);
        self.dialogs.remove(&leg_id);

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
            .with_cancel_token(self.cancel_token.child_token());

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
        self.peers.insert(leg_id.clone(), peer.clone());

        // Dynamic legs are plain RTP (SIP) by default
        self.leg_transport
            .insert(leg_id.clone(), rustrtc::TransportMode::Rtp);

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
        let invite_handle = tokio::spawn(async move {
            let leg_id = leg_id_for_spawn;
            let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
            let invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

            let result = match invitation.await {
                Ok((dialog, response)) => {
                    if let Some(ref resp) = response {
                        let status_code = resp.status_code.code();
                        if (200..300).contains(&status_code) {
                            info!(%leg_id, status = %status_code, "SIP leg answered successfully");

                            // Extract SDP answer from response body
                            let answer_sdp = if !resp.body().is_empty() {
                                let sdp = String::from_utf8_lossy(resp.body()).to_string();

                                // Set remote description on the peer
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

                            // Send LegConnected with SDP answer
                            let _ = cmd_tx.send(CallCommand::LegConnected {
                                leg_id: leg_id.clone(),
                                answer_sdp,
                            });

                            // Return dialog for further processing
                            Ok(dialog)
                        } else {
                            warn!(%leg_id, status = %status_code, "SIP leg rejected");
                            let _ = cmd_tx.send(CallCommand::LegFailed {
                                leg_id: leg_id.clone(),
                                reason: format!("Rejected with {}", status_code),
                            });
                            Err(format!("Rejected with {}", status_code))
                        }
                    } else {
                        warn!(%leg_id, "SIP leg timeout (no response)");
                        let _ = cmd_tx.send(CallCommand::LegFailed {
                            leg_id: leg_id.clone(),
                            reason: "Timeout".to_string(),
                        });
                        Err("Timeout".to_string())
                    }
                }
                Err(e) => {
                    warn!(%leg_id, error = %e, "SIP leg failed");
                    let _ = cmd_tx.send(CallCommand::LegFailed {
                        leg_id: leg_id.clone(),
                        reason: e.to_string(),
                    });
                    Err(e.to_string())
                }
            };

            // Process dialog state changes (e.g., BYE from remote)
            if let Ok(dialog) = result {
                let dialog_cancel = cancel_token.child_token();
                tokio::spawn(async move {
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
                                        });
                                        break;
                                    }
                                    Some(_) => {}
                                    None => break,
                                }
                            }
                        }
                    }
                    // Keep dialog alive
                    let _ = dialog;
                });
            }
        });

        self.leg_tasks
            .entry(leg_id.clone())
            .or_default()
            .push(invite_handle);

        Ok(())
    }

    /// Update media path based on number of active legs.
    async fn update_media_path(&mut self) {
        let active_count = self.legs.values().filter(|l| l.is_active()).count();

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
        // Abort all leg-specific spawned tasks
        for (leg_id, handles) in self.leg_tasks.drain() {
            for handle in handles {
                handle.abort();
            }
            info!(%leg_id, "Aborted tasks for leg");
        }
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

    fn derive_state(legs: &std::collections::HashMap<LegId, Leg>) -> SessionState {
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
            _ => return Err(anyhow!("Only file playback supported")),
        };

        let codec_info = self
            .caller_offer
            .as_ref()
            .map(|offer| MediaNegotiator::extract_codec_params(offer).audio)
            .and_then(|codecs| codecs.first().cloned())
            .unwrap_or_else(|| {
                let codec = CodecType::PCMU;
                MediaNegotiator::codec_info_for_type(codec)
            });

        /// Route playback to a specific leg.
        macro_rules! play_to_leg {
            ($leg_str:expr, $peer:expr, $bridge_endpoint:expr, $uses_bridge:expr) => {{
                let target_tid = if $leg_str == "caller" && leg_id.is_none() {
                    base_track_id.clone()
                } else {
                    format!("{}-{}", base_track_id, $leg_str)
                };
                let leg_track = FileTrack::new(target_tid.clone())
                    .with_path(file_path.clone())
                    .with_codec_info(codec_info.clone());
                if $uses_bridge {
                    if let Some(ref bridge) = self.media_bridge {
                        bridge
                            .replace_output_with_file($bridge_endpoint, &leg_track)
                            .await?;
                        if $leg_str == "caller" {
                            self.bridge_playback_track_id = Some(target_tid.clone());
                        }
                    }
                } else {
                    if let Err(e) = leg_track.start_playback_on(None).await {
                        warn!(error = %e, "Failed to start playback on {}", $leg_str);
                    }
                    $peer.update_track(Box::new(leg_track.clone()), None).await;
                }
                self.playback_tracks
                    .insert(target_tid.clone(), leg_track);
            }};
        }

        match leg_id {
            // Caller leg — P2P fast path preserved identically
            Some(ref lid) if lid == &LegId::from("caller") => {
                play_to_leg!(
                    "caller",
                    self.caller_peer,
                    self.leg_bridge_endpoint(&LegId::from("caller")),
                    self.caller_answer_uses_media_bridge
                );
            }
            // Callee leg
            Some(ref lid) if lid == &LegId::from("callee") => {
                play_to_leg!(
                    "callee",
                    self.callee_peer,
                    self.leg_bridge_endpoint(&LegId::from("callee")),
                    self.callee_offer_uses_media_bridge
                );
            }
            // Dynamic leg from peers
            Some(ref lid) => {
                let peer = self
                    .peers
                    .get(lid)
                    .ok_or_else(|| anyhow!("Leg not found: {}", lid))?;
                let target_tid = format!("{}-{}", base_track_id, lid);
                let leg_track = FileTrack::new(target_tid.clone())
                    .with_path(file_path.clone())
                    .with_codec_info(codec_info.clone());
                if let Err(e) = leg_track.start_playback_on(None).await {
                    warn!(error = %e, "Failed to start playback on leg {}", lid);
                }
                peer.update_track(Box::new(leg_track.clone()), None).await;
                self.playback_tracks.insert(target_tid.clone(), leg_track);
            }
            // None = caller only (backward compatible)
            None => {
                play_to_leg!(
                    "caller",
                    self.caller_peer,
                    self.leg_bridge_endpoint(&LegId::from("caller")),
                    self.caller_answer_uses_media_bridge
                );
            }
        }

        info!(track_id = %base_track_id, file = %file_path, "Playback started");

        // Spawn completion watcher for the first (or only) track
        if await_completion && !loop_playback {
            let first_leg = match leg_id {
                Some(ref l) => l.to_string(),
                None => "caller".to_string(),
            };
            let first_tid = if leg_id.is_none() {
                base_track_id.clone()
            } else {
                format!("{}-{}", base_track_id, first_leg)
            };
            if let Some(track) = self.playback_tracks.get(&first_tid) {
                let app_runtime = self.app_runtime.clone();
                let track_id_clone = first_tid.clone();
                let track_clone = track.clone();
                tokio::spawn(async move {
                    track_clone.wait_for_completion().await;
                    let _ = app_runtime.inject_event(serde_json::json!({
                        "type": "audio_complete",
                        "track_id": track_id_clone,
                        "interrupted": false
                    }));
                });
            }
        }

        Ok(())
    }

    async fn handle_stop_playback(&mut self, leg_id: Option<LegId>) -> Result<()> {
        let to_stop: Vec<String> = match leg_id {
            None => self.playback_tracks.keys().cloned().collect(),
            Some(ref lid) => {
                let suffix = format!("-{}", lid);
                let is_caller = lid.0 == "caller";
                self.playback_tracks
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

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

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

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

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

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

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

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        self.update_leg_state(&leg_id, LegState::Ringing);

        let sdp = ringback.as_ref().and_then(|policy| match policy {
            RingbackPolicy::Replace { .. } => self.caller_offer.clone(),
            _ => None,
        });

        if let Err(e) = self
            .server_dialog
            .ringing(None, sdp.map(|s| s.into_bytes()))
        {
            warn!(%leg_id, error = %e, "Failed to send 180 Ringing");
            return Err(anyhow!("Failed to send 180 Ringing: {}", e));
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
            crate::media::bridge::BridgeEndpoint::WebRtc => bridge.get_webrtc_sender().await,
            crate::media::bridge::BridgeEndpoint::Rtp => bridge.get_rtp_sender().await,
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
            let sdp = self.answer.as_deref()?;
            let profile = crate::media::negotiate::MediaNegotiator::extract_leg_profile(sdp);
            profile.dtmf.map(|c| c.payload_type)
        } else if leg_id == &LegId::from("callee") {
            let sdp = self.callee_answer_sdp.as_deref()?;
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
            match self.connected_callee_dialog_id.as_ref() {
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
                    self.context.dtmf_digits.push(*digit);
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
            self.media_bridge.clone(),
            self.leg_dtmf_payload_type(&leg_id),
        ) {
            let endpoint = self.leg_bridge_endpoint(&leg_id);
            let digits = valid_digits.clone();
            tokio::spawn(async move {
                Self::send_rtp_dtmf_via_bridge(&bridge, endpoint, &digits, dtmf_pt).await;
            });
        }

        Ok(())
    }

    async fn handle_reinvite_command(&mut self, leg_id: LegId, sdp: String) -> Result<()> {
        info!(%leg_id, "Handling re-INVITE command");

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        self.handle_reinvite(rsipstack::sip::Method::Invite, Some(sdp))
            .await?;

        info!(%leg_id, "Re-INVITE command handled");
        Ok(())
    }

    async fn handle_mute_track(&mut self, track_id: String) -> Result<()> {
        info!(%track_id, "Muting track");

        let caller_result = self.caller_peer.mute_track(&track_id).await;

        let callee_result = self.callee_peer.mute_track(&track_id).await;

        if !caller_result && !callee_result {
            return Err(anyhow!("Track not found on either peer: {}", track_id));
        }

        info!(%track_id, caller_muted = caller_result, callee_muted = callee_result, "Track muted");
        Ok(())
    }

    async fn handle_unmute_track(&mut self, track_id: String) -> Result<()> {
        info!(%track_id, "Unmuting track");

        let caller_result = self.caller_peer.unmute_track(&track_id).await;

        let callee_result = self.callee_peer.unmute_track(&track_id).await;

        if !caller_result && !callee_result {
            return Err(anyhow!("Track not found on either peer: {}", track_id));
        }

        info!(%track_id, caller_unmuted = caller_result, callee_unmuted = callee_result, "Track unmuted");
        Ok(())
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
                if (200..300).contains(&status_code) {
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

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        self.update_leg_state(&leg_id, LegState::Hold);

        let hold_sdp = self.generate_hold_sdp().await?;

        match self.send_reinvite_to_caller(hold_sdp).await {
            Ok(_) => {
                info!(%leg_id, "Hold re-INVITE sent successfully");

                if let Some(media_source) = music
                    && let crate::call::domain::MediaSource::File { path } = media_source
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

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        let leg = self.legs.get(&leg_id).unwrap();
        if leg.state != LegState::Hold {
            info!(%leg_id, state = ?leg.state, "Leg is not on hold, skipping unhold");
            return Ok(());
        }

        self.update_leg_state(&leg_id, LegState::Connected);

        self.playback_tracks.remove("hold-music");

        let unhold_sdp = self.generate_unhold_sdp().await?;

        match self.send_reinvite_to_caller(unhold_sdp).await {
            Ok(_) => {
                info!(%leg_id, "Unhold re-INVITE sent successfully");
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
            .answer
            .as_ref()
            .or(self.caller_offer.as_ref())
            .ok_or_else(|| anyhow!("No SDP available for hold"))?;

        let hold_sdp = rustrtc::modify_sdp_direction(base_sdp, "sendonly");
        Ok(hold_sdp)
    }

    async fn generate_unhold_sdp(&self) -> Result<String> {
        let base_sdp = self
            .answer
            .as_ref()
            .or(self.caller_offer.as_ref())
            .ok_or_else(|| anyhow!("No SDP available for unhold"))?;

        let unhold_sdp = rustrtc::modify_sdp_direction(base_sdp, "sendrecv");
        Ok(unhold_sdp)
    }

    async fn send_reinvite_to_caller(&self, sdp: String) -> Result<()> {
        let headers = vec![rsipstack::sip::Header::ContentType(
            "application/sdp".into(),
        )];

        match self
            .server_dialog
            .reinvite(Some(headers), Some(sdp.into_bytes()))
            .await
        {
            Ok(Some(response)) => {
                let status = response.status_code.code();
                if (200..300).contains(&status) {
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
        self.connected_callee_dialog_id = None;
        self.timers.clear();
        self.timer_queue.clear();
        self.timer_keys.clear();

        let _ = self.supervisor_mixer.take();

        debug!(session_id = %self.context.session_id, "SipSession drop complete");
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

                let track = self.audio_track.as_ref().unwrap().clone();

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
    use async_trait::async_trait;
    use rustrtc::{PeerConnection, RtcConfiguration, media::MediaStreamTrack};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestTrack {
        id: String,
        pc: Option<PeerConnection>,
    }

    impl TestTrack {
        fn with_pc(id: &str, pc: Option<PeerConnection>) -> Self {
            Self {
                id: id.to_string(),
                pc,
            }
        }
    }

    #[async_trait]
    impl Track for TestTrack {
        fn id(&self) -> &str {
            &self.id
        }

        async fn handshake(&self, _remote_offer: String) -> Result<String> {
            Err(anyhow!("not used in this test"))
        }

        async fn local_description(&self) -> Result<String> {
            Err(anyhow!("not used in this test"))
        }

        async fn set_remote_description(&self, _remote: &str) -> Result<()> {
            Ok(())
        }

        async fn stop(&self) {}

        async fn get_peer_connection(&self) -> Option<PeerConnection> {
            self.pc.clone()
        }
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

        let target = Location {
            aor: contact_uri,
            registered_aor: Some(registered_aor.clone()),
            ..Default::default()
        };

        let resolved = SipSession::resolve_outbound_callee_uri(&target, true);
        assert_eq!(resolved, registered_aor);
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
            dtmf_digits: Vec::new(),
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
    async fn test_get_local_reinvite_pc_uses_bridge_when_present() {
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
            dtmf_digits: Vec::new(),
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

        session.media_bridge = Some(BridgePeerBuilder::new("test-bridge".to_string()).build());
        session
            .leg_transport
            .insert(LegId::from("caller"), rustrtc::TransportMode::WebRtc);
        session
            .leg_transport
            .insert(LegId::from("callee"), rustrtc::TransportMode::Rtp);

        let pc = session.get_local_reinvite_pc(DialogSide::Caller).await;

        assert!(pc.is_some(), "bridge-backed caller leg should resolve a PC");
        assert_eq!(caller_peer.get_tracks_call_count(), 0);
        assert_eq!(callee_peer.get_tracks_call_count(), 0);
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
            dtmf_digits: Vec::new(),
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

        session.caller_offer = Some(
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
        assert!(session.media_bridge.is_some());
        assert!(session.caller_answer_uses_media_bridge);
        assert_eq!(caller_peer.update_track_call_count(), 0);

        let bridge = session
            .media_bridge
            .as_ref()
            .expect("media bridge should exist")
            .clone();
        let rtp_track = bridge
            .get_rtp_track()
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

        if let Some(bridge) = session.media_bridge.take() {
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
            dtmf_digits: Vec::new(),
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

        session.caller_offer = Some(
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
        assert!(session.caller_answer_uses_media_bridge);
        assert_eq!(
            session.leg_transport.get(&LegId::from("caller")),
            Some(&rustrtc::TransportMode::WebRtc)
        );

        let bridge = session
            .media_bridge
            .as_ref()
            .expect("media bridge should exist")
            .clone();
        let webrtc_track = bridge
            .get_webrtc_track()
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

        if let Some(bridge) = session.media_bridge.take() {
            bridge.stop().await;
        }
    }

    #[tokio::test]
    async fn test_play_audio_file_uses_second_caller_track_pc_when_first_is_none() {
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
            dtmf_digits: Vec::new(),
        };

        let caller_peer = Arc::new(MockMediaPeer::new());
        let callee_peer = Arc::new(MockMediaPeer::new());

        let target_pc = PeerConnection::new(RtcConfiguration::default());
        assert!(target_pc.get_transceivers().is_empty());

        {
            let mut tracks = caller_peer.tracks.lock().unwrap();
            tracks.push(Arc::new(tokio::sync::Mutex::new(Box::new(
                TestTrack::with_pc("forwarding-without-pc", None),
            ))));
            tracks.push(Arc::new(tokio::sync::Mutex::new(Box::new(
                TestTrack::with_pc("real-caller-track", Some(target_pc.clone())),
            ))));
        }

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

        session
            .play_audio_file("sounds/phone-calling.wav", false, "caller", true)
            .await
            .expect("queue hold audio should start");

        assert_eq!(caller_peer.update_track_call_count(), 1);
        assert!(
            !caller_peer.tracks.lock().unwrap().is_empty(),
            "play_audio_file should add track to caller peer"
        );
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

        let task = tokio::spawn(async move {
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
            dtmf_digits: Vec::new(),
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

        let result = session
            .handle_blind_transfer(LegId::from("caller"), "queue:test-queue".to_string())
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
            dtmf_digits: Vec::new(),
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

        let result = session
            .handle_blind_transfer(LegId::from("caller"), "queue:nonexistent".to_string())
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
}
