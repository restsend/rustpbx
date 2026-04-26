use crate::call::Location;
use crate::call::app::{ApplicationContext, CallInfo};
use crate::call::domain::{
    CallCommand, HangupCascade, HangupCommand, LegId, LegState, MediaPathMode, MediaRuntimeProfile,
    RingbackPolicy,
};
use crate::call::domain::{Leg, SessionState};
use crate::call::runtime::BridgeConfig;
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
use crate::media::bridge::BridgePeerBuilder;
use crate::media::mixer::MediaMixer;
use crate::media::negotiate::MediaNegotiator;
use crate::media::recorder::Recorder;
use crate::media::{FileTrack, RtpTrackBuilder, Track};
use crate::proxy::proxy_call::{
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RtpDtmfEventKey {
    digit_code: u8,
    rtp_timestamp: u32,
}

#[derive(Debug, Default)]
struct RtpDtmfDetector {
    last_event: Option<RtpDtmfEventKey>,
}

impl RtpDtmfDetector {
    fn observe(&mut self, payload: &[u8], rtp_timestamp: u32) -> Option<char> {
        if payload.len() < 4 {
            return None;
        }

        let digit_code = payload[0];
        let digit = match digit_code {
            0..=9 => (b'0' + digit_code) as char,
            10 => '*',
            11 => '#',
            12..=15 => (b'A' + (digit_code - 12)) as char,
            _ => return None,
        };

        let event = RtpDtmfEventKey {
            digit_code,
            rtp_timestamp,
        };

        if self.last_event == Some(event) {
            return None;
        }

        self.last_event = Some(event);
        Some(digit)
    }
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
    pub caller_peer: Arc<dyn MediaPeer>,
    pub callee_peer: Arc<dyn MediaPeer>,
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
    pub caller_is_webrtc: bool,
    pub callee_is_webrtc: bool,
    pub conference_bridge: crate::call::runtime::SessionConferenceBridge,
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
            cmd_tx,
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
                handle: bridge_handle,
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
            server_dialog,
            callee_dialogs: Arc::new(DashMap::new()),
            pending_hangup: HashSet::new(),
            caller_peer,
            callee_peer,
            supervisor_mixer: None,
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
            caller_is_webrtc: false,
            callee_is_webrtc: false,
            conference_bridge: crate::call::runtime::SessionConferenceBridge::new(),
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

    fn check_media_proxy(context: &CallContext, mode: &MediaProxyMode) -> bool {
        if context.dialplan.recording.enabled {
            return true;
        }
        matches!(mode, MediaProxyMode::All)
    }

    fn is_local_home_proxy(local_addrs: &[SipAddr], home_proxy: &SipAddr) -> bool {
        local_addrs
            .iter()
            .any(|addr| addr.addr.to_string() == home_proxy.addr.to_string())
    }

    fn resolve_outbound_destination(
        target: &Location,
        local_addrs: &[SipAddr],
    ) -> (Option<SipAddr>, bool) {
        if let Some(home_proxy) = target.home_proxy.as_ref() {
            if Self::is_local_home_proxy(local_addrs, home_proxy) {
                (target.destination.clone(), false)
            } else {
                (Some(home_proxy.clone()), true)
            }
        } else {
            (target.destination.clone(), false)
        }
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
                    let body = String::from_utf8_lossy(request.body());
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
                                    "digit": digit.chars().next().unwrap().to_string(),
                                });
                                if let Err(e) = self.app_runtime.inject_event(event) {
                                    warn!(error = %e, "Failed to inject DTMF event");
                                }
                            }
                        }
                    }
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
                    self.pending_hangup.insert(self.server_dialog.id());
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

            match self.try_single_target(target, callee_state_rx).await {
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
            self.try_single_target(target, callee_state_rx).await
        } else {
            Err((
                StatusCode::TemporarilyUnavailable,
                Some("No targets to dial".to_string()),
            ))
        }
    }

    async fn execute_queue(
        &mut self,
        plan: &crate::call::QueuePlan,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        use crate::call::DialStrategy;

        info!("Executing queue plan");

        let agents = match &plan.dial_strategy {
            Some(DialStrategy::Sequential(locations)) => locations.clone(),
            Some(DialStrategy::Parallel(locations)) => locations.clone(),
            None => {
                warn!("No dial strategy in queue plan");
                return Ok(());
            }
        };

        if agents.is_empty() {
            warn!("No agents configured in queue plan");
            return Ok(());
        }

        // Resolve custom targets (e.g., skill-group:) to actual agent locations
        let resolved_agents = self
            .resolve_custom_targets(agents, plan.acd_policy.as_deref())
            .await;

        if resolved_agents.is_empty() {
            warn!("No agents available after resolving skill groups");
            return self.execute_queue_fallback(plan).await;
        }

        if plan.accept_immediately {
            info!("Queue: answering call immediately");
            if let Err(e) = self.accept_call(None, None, None).await {
                warn!(error = %e, "Failed to answer call in queue");
            }
        }

        let hold_handle = if let Some(ref hold) = plan.hold {
            if let Some(ref audio_file) = hold.audio_file {
                info!(file = %audio_file, "Queue: starting hold music");

                self.play_audio_file(audio_file, false, "caller", hold.loop_playback)
                    .await
                    .ok()
            } else {
                None
            }
        } else {
            None
        };

        let result = match &plan.dial_strategy {
            Some(DialStrategy::Sequential(_)) => {
                self.dial_queue_sequential(&resolved_agents, plan.ring_timeout, callee_state_rx)
                    .await
            }
            Some(DialStrategy::Parallel(_)) => {
                self.dial_queue_parallel(&resolved_agents, plan.ring_timeout, callee_state_rx)
                    .await
            }
            None => Ok(()),
        };

        if hold_handle.is_some() {
            info!("Queue: stopping hold music");
        }

        match result {
            Ok(()) => {
                info!("Queue: agent connected successfully");
                Ok(())
            }
            Err(e) => {
                warn!(error = ?e, "Queue: all agents failed, executing fallback");
                self.execute_queue_fallback(plan).await
            }
        }
    }

    /// Resolve custom targets (e.g., skill-group:) to actual agent locations.
    /// Uses the AgentRegistry trait's resolve_target hook, which allows addons
    /// to implement custom routing logic without queue knowing the details.
    async fn resolve_custom_targets(
        &self,
        locations: Vec<crate::call::Location>,
        acd_policy: Option<&str>,
    ) -> Vec<crate::call::Location> {
        let mut resolved = Vec::new();
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
                                        resolved.push(reg_loc);
                                    } else {
                                        // Agent not currently registered; push a bare
                                        // location so downstream dialing can fail cleanly.
                                        let agent_location = crate::call::Location {
                                            aor: uri,
                                            contact_raw: Some(agent_uri),
                                            ..Default::default()
                                        };
                                        resolved.push(agent_location);
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
            resolved.push(location);
        }

        resolved
    }

    async fn dial_queue_sequential(
        &mut self,
        agents: &[crate::call::Location],
        _ring_timeout: Option<Duration>,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let mut last_error = (
            StatusCode::TemporarilyUnavailable,
            Some("All agents unavailable".to_string()),
        );

        for (idx, agent) in agents.iter().enumerate() {
            info!(index = idx, agent = %agent.aor, "Queue: trying agent");

            match self.try_single_target(agent, callee_state_rx).await {
                Ok(()) => {
                    info!(index = idx, "Queue: agent connected");
                    return Ok(());
                }
                Err(e) => {
                    warn!(index = idx, error = ?e, "Queue: agent failed");
                    last_error = e;
                }
            }
        }

        Err(last_error)
    }

    async fn dial_queue_parallel(
        &mut self,
        agents: &[crate::call::Location],
        _ring_timeout: Option<Duration>,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        if let Some(agent) = agents.first() {
            info!(agent = %agent.aor, "Queue: trying parallel agent");
            self.try_single_target(agent, callee_state_rx).await
        } else {
            Err((
                StatusCode::TemporarilyUnavailable,
                Some("No agents available".to_string()),
            ))
        }
    }

    async fn execute_queue_fallback(
        &mut self,
        plan: &crate::call::QueuePlan,
    ) -> Result<(), (StatusCode, Option<String>)> {
        use crate::call::{FailureAction, QueueFallbackAction, TransferEndpoint};

        match &plan.fallback {
            Some(QueueFallbackAction::Failure(FailureAction::Hangup { code, reason })) => {
                info!(?code, ?reason, "Queue fallback - hangup");
                Err((
                    code.clone().unwrap_or(StatusCode::TemporarilyUnavailable),
                    reason.clone(),
                ))
            }
            Some(QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
                audio_file,
                use_early_media: _,
                status_code,
                reason,
            })) => {
                info!(file = %audio_file, "Queue fallback - play then hangup");

                if let Err(e) = self
                    .play_audio_file(audio_file, true, "caller", false)
                    .await
                {
                    warn!(error = %e, "Failed to play fallback audio");
                }
                Err((status_code.clone(), reason.clone()))
            }
            Some(QueueFallbackAction::Failure(FailureAction::Transfer(target))) => {
                info!(target = ?target, "Queue fallback - transfer");

                match target {
                    TransferEndpoint::Uri(uri) => {
                        Box::pin(self.handle_blind_transfer(LegId::from("caller"), uri.clone()))
                            .await
                            .map_err(|e| {
                                (
                                    StatusCode::TemporarilyUnavailable,
                                    Some(format!("Transfer failed: {}", e)),
                                )
                            })
                    }
                    TransferEndpoint::Queue(queue_name) => {
                        Box::pin(self.handle_queue_transfer(LegId::from("caller"), queue_name))
                            .await
                            .map_err(|e| {
                                (
                                    StatusCode::TemporarilyUnavailable,
                                    Some(format!("Transfer failed: {}", e)),
                                )
                            })
                    }
                    TransferEndpoint::Ivr(_) => Err((
                        StatusCode::NotImplemented,
                        Some("IVR transfer not supported here".to_string()),
                    )),
                }
            }
            Some(QueueFallbackAction::Redirect { target }) => {
                info!(target = %target, "Queue fallback - redirecting call");

                // Current dialog API does not expose direct 302 + Contact helper.
                // Use REFER-based transfer to approximate redirect behavior.
                Box::pin(self.handle_blind_transfer(LegId::from("caller"), target.to_string()))
                    .await
                    .map_err(|e| {
                        (
                            StatusCode::TemporarilyUnavailable,
                            Some(format!("Redirect failed: {}", e)),
                        )
                    })
            }
            Some(QueueFallbackAction::Queue { name }) => {
                if name.starts_with("skill-group:") {
                    let skill_group_id = name.strip_prefix("skill-group:").unwrap_or(name).trim();
                    info!(skill_group = %skill_group_id, "Queue fallback - transfer to skill group");

                    // Use AgentRegistry to resolve skill group to agents
                    if let Some(registry) = self.server.agent_registry.clone() {
                        let skill_group_uri = format!("skill-group:{}", skill_group_id);
                        let agents = registry.resolve_target(&skill_group_uri).await;
                        if !agents.is_empty() {
                            info!(agents = ?agents, "Resolved skill group to agents");
                            // Try to transfer to first available agent
                            let target = agents[0].clone();
                            Box::pin(self.handle_blind_transfer(LegId::from("caller"), target))
                                .await
                                .map_err(|e| {
                                    (
                                        StatusCode::TemporarilyUnavailable,
                                        Some(format!("Transfer failed: {}", e)),
                                    )
                                })
                        } else {
                            warn!(skill_group = %skill_group_id, "No agents found for this skill group");
                            Err((
                                StatusCode::TemporarilyUnavailable,
                                Some(format!(
                                    "No agents available for skill group {}",
                                    skill_group_id
                                )),
                            ))
                        }
                    } else {
                        warn!("No agent registry available for skill group resolution");
                        Err((
                            StatusCode::TemporarilyUnavailable,
                            Some("Agent registry not available".to_string()),
                        ))
                    }
                } else {
                    info!(queue = %name, "Queue fallback - transfer to another queue");
                    // Re-enqueue to another queue by starting QueueApp with new plan
                    match Box::pin(self.handle_queue_transfer(LegId::from("caller"), name)).await {
                        Ok(_) => {
                            info!(queue = %name, "Queue fallback - re-enqueue succeeded");
                            Ok(())
                        }
                        Err(e) => {
                            warn!(queue = %name, error = %e, "Queue fallback - re-enqueue operation failed");
                            Err((
                                StatusCode::TemporarilyUnavailable,
                                Some(format!("Re-enqueue failed: {}", e)),
                            ))
                        }
                    }
                }
            }
            None => {
                info!("Queue fallback - default hangup");
                Err((
                    StatusCode::TemporarilyUnavailable,
                    Some("All agents unavailable".to_string()),
                ))
            }
        }
    }

    async fn try_single_target(
        &mut self,
        target: &crate::call::Location,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
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
        let (destination, route_via_home_proxy) =
            Self::resolve_outbound_destination(target, &local_addrs);
        let callee_uri = Self::resolve_outbound_callee_uri(target, route_via_home_proxy);

        let mut headers: Vec<rsipstack::sip::Header> =
            vec![rsipstack::sip::headers::MaxForwards::from(self.context.max_forwards).into()];

        // When routing to another PBX node via home_proxy, anchor dialog route-set.
        if route_via_home_proxy {
            if let Ok(record_route) = self.server.endpoint.inner.get_record_route() {
                headers.push(rsipstack::sip::Header::RecordRoute(record_route.into()));
            } else {
                warn!(
                    session_id = %self.context.session_id,
                    "failed to build Record-Route while routing via home_proxy"
                );
            }
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
        self.caller_is_webrtc = caller_is_webrtc;
        self.callee_is_webrtc = callee_is_webrtc;

        let callee_sdp = if self.bypasses_local_media() && caller_is_webrtc == callee_is_webrtc {
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

        info!(session_id = %self.context.session_id, %caller, %callee_uri, callee_call_id, ?destination, "Sending INVITE to callee");

        let mut invite_option = InviteOption {
            caller_display_name: self.context.dialplan.caller_display_name.clone(),
            callee: callee_uri.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination,
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

        let result = loop {
            tokio::select! {
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
        let callee_is_webrtc = self.callee_is_webrtc;

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
                let codec_info =
                    MediaNegotiator::build_caller_answer_codec_list(caller_offer, caller_is_webrtc);

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

        if self.media_profile.path == MediaPathMode::Anchored && self.media_bridge.is_none() {
            let caller_answer_for_forwarding = self.answer.clone();
            let callee_answer_for_forwarding = callee_sdp.clone();
            self.start_anchored_media_forwarding(
                caller_answer_for_forwarding.as_deref(),
                callee_answer_for_forwarding.as_deref(),
            )
            .await;
        }

        caller_answer
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

        if self.connected_callee.is_some() {
            return;
        }

        let Some(answer_sdp) = self.answer.as_deref() else {
            return;
        };

        let caller_profile = MediaNegotiator::extract_leg_profile(answer_sdp);
        let Some(dtmf_codec) = caller_profile.dtmf else {
            return;
        };

        let Some(caller_pc) = Self::get_peer_pc(&self.caller_peer, Self::CALLER_TRACK_ID).await
        else {
            return;
        };

        let session_id = self.context.session_id.clone();
        let app_runtime = self.app_runtime.clone();
        let cancel_token = self.cancel_token.child_token();
        let monitor_cancel = cancel_token.clone();
        let dtmf_payload_type = dtmf_codec.payload_type;

        let task = tokio::spawn(async move {
            let track = loop {
                if let Some(track) = Self::find_audio_receiver_track(&caller_pc).await {
                    break track;
                }

                tokio::select! {
                    _ = monitor_cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            };

            let mut detector = RtpDtmfDetector::default();

            loop {
                tokio::select! {
                    _ = monitor_cancel.cancelled() => break,
                    sample = track.recv() => {
                        match sample {
                            Ok(rustrtc::media::MediaSample::Audio(frame)) => {
                                if frame.payload_type != Some(dtmf_payload_type) {
                                    continue;
                                }

                                let Some(digit) = detector.observe(&frame.data, frame.rtp_timestamp) else {
                                    continue;
                                };

                                let event = serde_json::json!({
                                    "type": "dtmf",
                                    "digit": digit.to_string(),
                                });

                                if let Err(error) = app_runtime.inject_event(event) {
                                    debug!(
                                        session_id = %session_id,
                                        digit = %digit,
                                        error = %error,
                                        "Caller ingress monitor observed RTP DTMF with no active app receiver"
                                    );
                                } else {
                                    debug!(
                                        session_id = %session_id,
                                        digit = %digit,
                                        "Injected RTP DTMF from caller ingress monitor"
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                debug!(
                                    session_id = %session_id,
                                    error = %error,
                                    "Caller ingress monitor stopped while reading inbound RTP"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        });

        info!(
            session_id = %self.context.session_id,
            payload_type = dtmf_payload_type,
            "Started caller ingress monitor for RTP DTMF"
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

    pub async fn create_callee_track(&mut self, callee_is_webrtc: bool) -> Result<String> {
        let track_id = Self::CALLEE_TRACK_ID.to_string();

        let caller_is_webrtc = self.is_caller_webrtc();
        self.caller_is_webrtc = caller_is_webrtc;
        self.callee_is_webrtc = callee_is_webrtc;

        debug!(
            session_id = %self.id,
            caller_is_webrtc = caller_is_webrtc,
            callee_is_webrtc = callee_is_webrtc,
            "Creating callee track"
        );

        let media_proxy_enabled = self.media_profile.path == MediaPathMode::Anchored;

        let transport_bridge_needed = caller_is_webrtc != callee_is_webrtc;

        let need_transport_bridge = transport_bridge_needed;

        if need_transport_bridge {
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
                );

                let webrtc_side = if caller_is_webrtc {
                    &codec_lists.caller_side
                } else {
                    &codec_lists.callee_side
                };
                let rtp_side = if callee_is_webrtc {
                    &codec_lists.caller_side
                } else {
                    &codec_lists.callee_side
                };

                let webrtc_caps: Vec<_> = webrtc_side
                    .iter()
                    .filter_map(|c| c.to_audio_capability())
                    .collect();
                let rtp_caps: Vec<_> = rtp_side
                    .iter()
                    .filter_map(|c| c.to_audio_capability())
                    .collect();

                bridge_builder = bridge_builder
                    .with_webrtc_audio_capabilities(webrtc_caps)
                    .with_rtp_audio_capabilities(rtp_caps);

                // Sender codecs: first non-DTMF codec for each side
                let webrtc_sender = webrtc_side
                    .iter()
                    .find(|c| !c.is_dtmf())
                    .map(|c| c.to_params());
                let rtp_sender = rtp_side
                    .iter()
                    .find(|c| !c.is_dtmf())
                    .map(|c| c.to_params());

                if let (Some(w), Some(r)) = (webrtc_sender, rtp_sender) {
                    bridge_builder = bridge_builder.with_sender_codecs(w, r);
                }

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
                    webrtc_codecs = ?webrtc_side.iter().map(|c| format!("{:?}", c.codec)).collect::<Vec<_>>(),
                    rtp_codecs = ?rtp_side.iter().map(|c| format!("{:?}", c.codec)).collect::<Vec<_>>(),
                    "Bridge codecs configured from allow_codecs"
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

            bridge.start_bridge().await;

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
                let codecs =
                    MediaNegotiator::build_callee_codec_offer(caller_offer, callee_is_webrtc);
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
                return Some(answer_sdp);
            }
        }

        let caller_offer = self.caller_offer.clone()?;
        let caller_is_webrtc = self.is_caller_webrtc();

        let codec_info =
            MediaNegotiator::build_caller_answer_codec_list(&caller_offer, caller_is_webrtc);

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
                DialogSide::Caller => self.caller_is_webrtc,
                DialogSide::Callee => self.callee_is_webrtc,
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
        let offer_direction = parsed
            .media_sections
            .iter()
            .find(|section| section.kind == rustrtc::MediaKind::Audio)
            .ok_or_else(|| anyhow!("re-INVITE offer has no audio section"))?
            .direction;

        let pc = self
            .get_local_reinvite_pc(side)
            .await
            .ok_or_else(|| anyhow!("No local PeerConnection available for {:?}", side))?;
        let answer_sdp = Self::build_local_answer_from_pc(&pc, offer_sdp).await?;

        match side {
            DialogSide::Caller => {
                self.caller_offer = Some(offer_sdp.to_string());
                self.answer = Some(answer_sdp.clone());
                self.update_leg_state(
                    &LegId::from("caller"),
                    if Self::is_hold_direction(offer_direction) {
                        LegState::Hold
                    } else {
                        LegState::Connected
                    },
                );
            }
            DialogSide::Callee => {
                self.callee_offer = Some(answer_sdp.clone());
                self.callee_answer_sdp = Some(answer_sdp.clone());
                self.update_leg_state(
                    &LegId::from("callee"),
                    if Self::is_hold_direction(offer_direction) {
                        LegState::Hold
                    } else {
                        LegState::Connected
                    },
                );
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
        let resolved_audio_file = Self::resolve_audio_file_path(audio_file);
        info!(
            audio_file = %audio_file,
            resolved_audio_file = %resolved_audio_file,
            track_id = %track_id,
            "Playing audio file"
        );

        let caller_codec = self
            .caller_offer
            .as_ref()
            .map(|offer| MediaNegotiator::extract_codec_params(offer).audio)
            .and_then(|codecs| codecs.first().map(|c| c.codec))
            .unwrap_or(CodecType::PCMU);

        let hold_ssrc = rand::random::<u32>();
        let track = FileTrack::new(track_id.to_string())
            .with_path(resolved_audio_file)
            .with_loop(loop_playback)
            .with_ssrc(hold_ssrc)
            .with_codec_preference(vec![caller_codec]);

        let caller_pc = {
            let mut pc = None;
            for _ in 0..100 {
                if let Some(found_pc) = self.get_caller_peer_connection().await {
                    pc = Some(found_pc);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            pc
        };

        if let Err(e) = track.start_playback_on(caller_pc).await {
            warn!(error = %e, "Failed to start playback");
        }

        let wait_track = if await_completion && !loop_playback {
            Some(track.clone())
        } else {
            None
        };

        self.caller_peer.update_track(Box::new(track), None).await;

        if let Some(track) = wait_track {
            track.wait_for_completion().await;
        }

        Ok(())
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

    fn is_caller_webrtc(&self) -> bool {
        if let Some(ref offer) = self.caller_offer {
            offer.contains("UDP/TLS/RTP/SAVPF")
                || offer.contains("a=fingerprint:")
                || offer.contains("a=ice-ufrag:")
                || offer.contains("a=setup:")
        } else {
            false
        }
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
                    match self.accept_call(None, None, None).await {
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

    async fn handle_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
        attended: bool,
    ) -> Result<()> {
        info!(%leg_id, %target, %attended, "Handling transfer");

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        let leg = self.legs.get(&leg_id).unwrap();
        if !matches!(leg.state, LegState::Connected | LegState::Hold) {
            return Err(anyhow!(
                "Cannot transfer leg {}: invalid state {:?}",
                leg_id,
                leg.state
            ));
        }

        if attended {
            if !target.is_empty() {
                self.handle_replace_transfer(leg_id, target).await?;
            } else {
                self.update_leg_state(&leg_id, LegState::Hold);

                info!(
                    "Attended transfer initiated - consultation call should be created externally"
                );

                if let Some(ref reporter) = self.reporter {
                    let _ = reporter;
                }
            }
        } else {
            self.handle_blind_transfer(leg_id, target).await?;
        }

        Ok(())
    }

    async fn handle_blind_transfer(&mut self, leg_id: LegId, target: String) -> Result<()> {
        // Handle queue: prefix - start QueueApp instead of sending REFER
        if target.starts_with("queue:") {
            let queue_name = target.strip_prefix("queue:").unwrap_or(&target).trim();
            if !queue_name.is_empty() {
                info!(%leg_id, queue = %queue_name, "Handling queue transfer by starting QueueApp");
                return self.handle_queue_transfer(leg_id, queue_name).await;
            }
        }

        let refer_to_str = if target.starts_with("sip:") || target.starts_with("tel:") {
            target.clone()
        } else {
            format!("sip:{}", target)
        };
        let refer_to_uri = rsipstack::sip::Uri::try_from(refer_to_str.as_str())
            .map_err(|e| anyhow!("Invalid transfer target URI: {}", e))?;

        let referred_by = self
            .context
            .dialplan
            .caller_contact
            .clone()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "sip:rustpbx@localhost".to_string());
        let headers = vec![rsipstack::sip::Header::Other(
            "Referred-By".to_string(),
            format!("<{}>", referred_by),
        )];

        info!(%leg_id, target = %refer_to_str, "Sending REFER for blind transfer");

        match self
            .server_dialog
            .refer(refer_to_uri, Some(headers), None)
            .await
        {
            Ok(Some(response)) => {
                let status = response.status_code.code();
                info!(status = %status, "REFER response received");

                match status {
                    202 => {
                        info!("REFER accepted (202), transfer in progress");
                        self.update_leg_state(&leg_id, LegState::Ending);

                        self.emit_transfer_event(&leg_id, "accepted", None, None)
                            .await;
                        self.emit_refer_event(
                            status,
                            None,
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                    }
                    100..=199 => {
                        info!("REFER received provisional response {}", status);
                        self.emit_refer_event(
                            status,
                            None,
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                    }
                    405 | 420 | 501 => {
                        warn!(status = %status, "REFER not supported by peer, needs 3PCC fallback");
                        self.emit_transfer_event(
                            &leg_id,
                            "failed",
                            Some(status),
                            Some("refer_not_supported"),
                        )
                        .await;
                        self.emit_refer_event(
                            status,
                            Some("refer_not_supported".to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        return Err(anyhow!(
                            "REFER not supported by peer ({}), needs 3PCC fallback",
                            status
                        ));
                    }
                    _ if status >= 400 => {
                        warn!(status = %status, "REFER rejected");
                        self.emit_transfer_event(
                            &leg_id,
                            "failed",
                            Some(status),
                            Some("refer_rejected"),
                        )
                        .await;
                        self.emit_refer_event(
                            status,
                            Some("refer_rejected".to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        return Err(anyhow!("REFER rejected with status {}", status));
                    }
                    _ => {
                        warn!(status = %status, "Unexpected REFER response");
                        self.emit_transfer_event(
                            &leg_id,
                            "failed",
                            Some(status),
                            Some("unexpected_response"),
                        )
                        .await;
                        self.emit_refer_event(
                            status,
                            Some("unexpected_response".to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        return Err(anyhow!("Unexpected REFER response: {}", status));
                    }
                }
            }
            Ok(None) => {
                warn!("REFER timed out, no response received");
                self.emit_transfer_event(&leg_id, "failed", None, Some("timeout"))
                    .await;
                self.emit_refer_event(
                    408,
                    Some("timeout".to_string()),
                    crate::call::domain::ReferNotifyEventType::ReferResponse,
                )
                .await;
                return Err(anyhow!("REFER timed out"));
            }
            Err(e) => {
                warn!(error = %e, "Failed to send REFER");
                self.emit_transfer_event(&leg_id, "failed", None, Some(&e.to_string()))
                    .await;
                self.emit_refer_event(
                    500,
                    Some(e.to_string()),
                    crate::call::domain::ReferNotifyEventType::ReferResponse,
                )
                .await;
                return Err(anyhow!("Failed to send REFER: {}", e));
            }
        }

        info!(
            "Blind transfer initiated - call will be transferred to {}",
            target
        );

        Ok(())
    }

    /// Handle queue transfer by loading queue config and executing queue plan.
    /// This is called when a transfer target starts with "queue:".
    async fn handle_queue_transfer(&mut self, leg_id: LegId, queue_name: &str) -> Result<()> {
        info!(%leg_id, queue = %queue_name, "Starting queue transfer");

        // Load queue configuration from data context
        let queue_config = self
            .server
            .data_context
            .resolve_queue_config(queue_name)
            .map_err(|e| anyhow!("Failed to resolve queue config: {}", e))?;

        let queue_config = match queue_config {
            Some(config) => config,
            None => {
                return Err(anyhow!("Queue '{}' not found", queue_name));
            }
        };

        // Convert to queue plan
        let queue_plan = queue_config
            .to_queue_plan()
            .map_err(|e| anyhow!("Invalid queue config: {}", e))?;

        // Execute queue plan
        // Create a channel for callee state (not used in this context)
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        match self.execute_queue(&queue_plan, &mut rx).await {
            Ok(()) => {
                info!(queue = %queue_name, "Queue transfer completed successfully");
                Ok(())
            }
            Err((code, reason)) => {
                warn!(
                    queue = %queue_name,
                    ?code,
                    ?reason,
                    "Queue transfer failed"
                );
                if self.server_dialog.state().is_confirmed() {
                    self.last_error = Some((code.clone(), reason.clone()));
                    self.hangup_reason
                        .get_or_insert(CallRecordHangupReason::Failed);
                    self.pending_hangup.insert(self.server_dialog.id());
                    self.cancel_token.cancel();
                    info!(
                        queue = %queue_name,
                        ?code,
                        ?reason,
                        "Queue transfer failed after caller was answered; hanging up caller dialog"
                    );
                    return Ok(());
                }
                Err(anyhow!("Queue transfer failed: {:?} - {:?}", code, reason))
            }
        }
    }

    fn build_replaces_header(&self) -> Option<String> {
        let dialog_id = self.server_dialog.id();

        let call_id = &dialog_id.call_id;
        let local_tag = &dialog_id.local_tag;
        let remote_tag = &dialog_id.remote_tag;

        if remote_tag.is_empty() {
            return None;
        }

        Some(format!(
            "{};to-tag={};from-tag={}",
            call_id, local_tag, remote_tag
        ))
    }

    async fn handle_replace_transfer(&mut self, leg_id: LegId, target: String) -> Result<()> {
        let replaces = self
            .build_replaces_header()
            .ok_or_else(|| anyhow!("Cannot build Replaces header for current dialog"))?;
        let encoded_replaces = urlencoding::encode(&replaces).into_owned();

        let refer_target = if target.contains('?') {
            format!("{}&Replaces={}", target, encoded_replaces)
        } else {
            format!("{}?Replaces={}", target, encoded_replaces)
        };

        self.handle_blind_transfer(leg_id, refer_target).await
    }

    async fn emit_transfer_event(
        &self,
        leg_id: &LegId,
        event_type: &str,
        sip_status: Option<u16>,
        reason: Option<&str>,
    ) {
        let event_data = serde_json::json!({
            "session_id": self.id.0,
            "leg_id": leg_id.to_string(),
            "event": event_type,
            "sip_status": sip_status,
            "reason": reason,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        info!(?event_data, "Transfer event emitted");
    }

    /// Emit a REFER-related event to all registered transfer controllers.
    async fn emit_refer_event(
        &self,
        sip_status: u16,
        reason: Option<String>,
        event_type: crate::call::domain::ReferNotifyEventType,
    ) {
        let event = crate::call::domain::ReferNotifyEvent {
            call_id: self.id.0.clone(),
            sip_status,
            reason,
            event_type,
        };
        let subscribers = self.server.transfer_notify_subscribers.lock().await;
        for tx in subscribers.iter() {
            let _ = tx.send(event.clone());
        }
    }

    async fn handle_transfer_complete(&mut self, consult_leg: LegId) -> Result<()> {
        info!(%consult_leg, "Completing attended transfer");

        if !self.legs.contains_key(&consult_leg) {
            return Err(anyhow!("Consultation leg not found: {}", consult_leg));
        }

        let original_leg = self
            .legs
            .iter()
            .find(|(_, leg)| leg.state == LegState::Hold)
            .map(|(id, _)| id.clone());

        if let Some(original_leg) = original_leg {
            if self
                .setup_bridge(original_leg.clone(), consult_leg.clone())
                .await
            {
                self.update_leg_state(&original_leg, LegState::Connected);
                self.update_leg_state(&consult_leg, LegState::Connected);
                // Ensure the original leg is properly unheld after bridge
                let _ = self.handle_unhold(original_leg.clone()).await;
                info!("Attended transfer completed successfully");
            } else {
                return Err(anyhow!("Failed to setup bridge for transfer completion"));
            }
        } else {
            return Err(anyhow!("No leg on hold found for transfer completion"));
        }

        Ok(())
    }

    async fn handle_transfer_cancel(&mut self, consult_leg: LegId) -> Result<()> {
        info!(%consult_leg, "Canceling attended transfer");

        if !self.legs.contains_key(&consult_leg) {
            return Err(anyhow!("Consultation leg not found: {}", consult_leg));
        }

        self.update_leg_state(&consult_leg, LegState::Ending);

        let original_leg = self
            .legs
            .iter()
            .find(|(_, leg)| leg.state == LegState::Hold)
            .map(|(id, _)| id.clone());

        if let Some(original_leg) = original_leg {
            self.update_leg_state(&original_leg, LegState::Connected);
            // Ensure the original leg is properly unheld after cancel
            let _ = self.handle_unhold(original_leg.clone()).await;
            info!("Attended transfer canceled, original call resumed");
        }

        Ok(())
    }

    /// Handle cross-session transfer completion by migrating a leg into a conference.
    ///
    /// This is used in the BC -> ABC conference flow where leg_c from session2
    /// needs to be migrated into a conference that also includes legs from session1.
    ///
    /// Flow:
    /// 1. Locate the leg in from_session
    /// 2. Add the leg's media to the target conference
    /// 3. Mark the leg as migrated (don't remove from session yet - session will be cleaned up)
    async fn handle_transfer_complete_cross_session(
        &mut self,
        from_session: String,
        leg_id: LegId,
        into_conference: String,
    ) -> Result<()> {
        info!(
            from_session = %from_session,
            leg_id = %leg_id,
            into_conference = %into_conference,
            "Handling cross-session transfer completion"
        );

        // Check if this is the from_session
        if self.id.to_string() != from_session {
            // This session is not the from_session, forward the command to the correct session
            let registry = &self.server.active_call_registry;
            if let Some(handle) = registry.get_handle(&from_session) {
                let from_session_clone = from_session.clone();
                handle
                    .send_command(CallCommand::TransferCompleteCrossSession {
                        from_session,
                        leg_id,
                        into_conference,
                    })
                    .map_err(|e| anyhow!("Failed to forward cross-session transfer: {}", e))?;
                info!(
                    "Forwarded cross-session transfer command to session {}",
                    from_session_clone
                );
                return Ok(());
            } else {
                return Err(anyhow!(
                    "from_session {} not found in registry",
                    from_session
                ));
            }
        }

        // This is the from_session - find the leg
        let leg = self
            .legs
            .get(&leg_id)
            .ok_or_else(|| anyhow!("Leg {} not found in session {}", leg_id, from_session))?;

        info!(
            session_id = %self.id,
            leg_id = %leg_id,
            leg_state = ?leg.state,
            "Found leg for cross-session migration"
        );

        // Get the conference manager from the server
        let conference_manager = &self.server.conference_manager;
        let conf_id = crate::call::runtime::ConferenceId::from(into_conference.as_str());

        // Add the leg to the conference
        conference_manager
            .add_participant(&conf_id, LegId::new(format!("{}-{}", from_session, leg_id)))
            .await
            .map_err(|e| anyhow!("Failed to add leg to conference: {}", e))?;

        info!(
            session_id = %self.id,
            leg_id = %leg_id,
            conf_id = %into_conference,
            "Successfully migrated leg into conference"
        );

        // Start conference media bridge for this leg
        // This enables the leg to receive mixed audio from all conference participants
        match self
            .start_conference_media_bridge(&into_conference, &leg_id)
            .await
        {
            Ok(handle) => {
                info!(
                    session_id = %self.id,
                    leg_id = %leg_id,
                    "Conference media bridge started"
                );
                self.conference_bridge.bridge_handle = Some(handle);
                self.conference_bridge.conf_id = Some(into_conference);
            }
            Err(e) => {
                warn!(
                    session_id = %self.id,
                    leg_id = %leg_id,
                    error = %e,
                    "Failed to start conference media bridge"
                );
            }
        }

        // Mark the leg as migrated - the session will be cleaned up separately
        // (typically by BYE after confirming migration succeeded)
        self.update_leg_state(&leg_id, LegState::Hold);

        Ok(())
    }

    /// Create a conference audio track and start full-duplex media bridge.
    ///
    /// This creates a rustrtc sample track for output (mixed audio to participant),
    /// and sets up an audio receiver for input (participant audio to mixer).
    /// The bridge runs both forward and reverse loops for full-duplex communication.
    async fn start_conference_media_bridge(
        &mut self,
        conf_id: &str,
        leg_id: &LegId,
    ) -> Result<crate::call::runtime::ConferenceBridgeHandle> {
        use rustrtc::RtpCodecParameters;
        use rustrtc::media::MediaKind;
        use rustrtc::media::MediaSample;
        use rustrtc::media::track::sample_track;

        // Determine which peer to use based on leg_id
        let is_callee = leg_id.0.ends_with("-callee") || leg_id.0 == "callee";
        if !is_callee {
            self.stop_caller_ingress_monitor().await;
        }
        let (peer, track_id) = if is_callee {
            (self.callee_peer.clone(), Self::CALLEE_TRACK_ID)
        } else {
            (self.caller_peer.clone(), Self::CALLER_TRACK_ID)
        };

        // Create a sample track pair (sender -> track) for output
        let (audio_sender, track, _feedback_rx) = sample_track(MediaKind::Audio, 100);

        // Get the existing peer connection from the named RTP track.
        // We search by track_id to avoid picking up ForwardingTrackHandle entries
        // (which return None from get_peer_connection).  During REFER/transfer
        // transitions the track may not be ready yet, so retry briefly.
        let mut pc = None;
        for attempt in 0..150 {
            let tracks = peer.get_tracks().await;
            // First try: look for the named RTP track
            for t in &tracks {
                let guard = t.lock().await;
                if guard.id() == track_id {
                    if let Some(found_pc) = guard.get_peer_connection().await {
                        pc = Some(found_pc);
                        break;
                    }
                }
            }
            if pc.is_some() {
                break;
            }
            // Fallback: accept any track that has a peer connection (e.g. after re-INVITE)
            for t in &tracks {
                let guard = t.lock().await;
                if let Some(found_pc) = guard.get_peer_connection().await {
                    pc = Some(found_pc);
                    break;
                }
            }
            if pc.is_some() {
                break;
            }
            if attempt % 25 == 0 {
                let track_ids: Vec<_> = {
                    let mut ids = Vec::new();
                    for t in &tracks {
                        ids.push(t.lock().await.id().to_string());
                    }
                    ids
                };
                tracing::debug!(
                    session_id = %self.id,
                    leg_id = %leg_id,
                    wanted_track_id = %track_id,
                    available_tracks = ?track_ids,
                    attempt = attempt,
                    "Waiting for peer connection on conference media bridge"
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let pc = pc.ok_or_else(|| {
            anyhow!(
                "No peer connection found for conference audio injection (leg={}, track={}, session={})",
                leg_id,
                track_id,
                self.id
            )
        })?;

        // Add the sample track to the existing peer connection with PCMU params
        let params = RtpCodecParameters {
            payload_type: 0, // PCMU
            clock_rate: 8000,
            channels: 1,
        };

        pc.add_track(track, params)
            .map_err(|e| anyhow!("Failed to add conference track to peer connection: {}", e))?;

        info!(
            session_id = %self.id,
            conf_id = %conf_id,
            leg_id = %leg_id,
            "Conference sample track added to existing peer connection"
        );

        // Create a channel to bridge between ConferenceMediaBridge and audio_sender
        let (tx, mut rx) = tokio::sync::mpsc::channel::<MediaSample>(100);

        // Spawn a forwarder task
        tokio::spawn(async move {
            while let Some(sample) = rx.recv().await {
                if audio_sender.send(sample).await.is_err() {
                    break;
                }
            }
        });

        // Create audio receiver for input from SIP track
        let audio_receiver = if is_callee {
            self.create_audio_receiver_from_peer(&peer).await
        } else {
            self.create_audio_receiver().await
        }
        .map_err(|e| anyhow!("Failed to create audio receiver: {}", e))?;

        // Start full-duplex media bridge
        let bridge = crate::call::runtime::ConferenceMediaBridge::new(
            self.server.conference_manager.clone(),
        );
        bridge
            .start_bridge_full_duplex(conf_id, leg_id, tx, audio_receiver)
            .await
            .map_err(|e| anyhow!("Failed to start conference media bridge: {}", e))
    }

    /// Create an audio receiver that reads decoded PCM from the session's media track.
    ///
    /// This is used to feed participant audio into the conference mixer.
    /// Uses the same pattern as BridgePeer: reads MediaSample from track, decodes to PCM.
    async fn create_audio_receiver(
        &self,
    ) -> Result<Box<dyn crate::call::runtime::conference_media_bridge::AudioReceiver>> {
        // Get the caller's peer connection with a short retry loop for transfer transitions.
        let mut pc = None;
        for _ in 0..100 {
            if let Some(found_pc) = self.get_caller_peer_connection().await {
                pc = Some(found_pc);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let pc = pc.ok_or_else(|| anyhow!("No peer connection found for conference input"))?;

        // Create decoder based on negotiated codec
        let decoder = self
            .create_audio_decoder()
            .ok_or_else(|| anyhow!("Failed to create audio decoder"))?;

        Ok(Box::new(PeerConnectionAudioReceiver::new(pc, decoder)))
    }

    /// Create an audio receiver from a specific peer.
    async fn create_audio_receiver_from_peer(
        &self,
        peer: &Arc<dyn MediaPeer>,
    ) -> Result<Box<dyn crate::call::runtime::conference_media_bridge::AudioReceiver>> {
        // Get peer connection from the given peer with retries during session setup.
        // Iterate all tracks (not just first) to skip ForwardingTrackHandle entries
        // which always return None from get_peer_connection().
        let mut pc = None;
        for _ in 0..150 {
            let tracks = peer.get_tracks().await;
            for t in &tracks {
                let guard = t.lock().await;
                if let Some(found_pc) = guard.get_peer_connection().await {
                    pc = Some(found_pc);
                    break;
                }
            }
            if pc.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let pc = pc.ok_or_else(|| anyhow!("No peer connection found for conference input"))?;

        // Create decoder based on negotiated codec
        let decoder = self
            .create_audio_decoder()
            .ok_or_else(|| anyhow!("Failed to create audio decoder"))?;

        Ok(Box::new(PeerConnectionAudioReceiver::new(pc, decoder)))
    }

    /// Get the caller's peer connection
    async fn get_caller_peer_connection(&self) -> Option<rustrtc::PeerConnection> {
        // Try to get PC from caller_peer's first track
        let tracks = self.caller_peer.get_tracks().await;
        for t in tracks.iter() {
            let guard = t.lock().await;
            if let Some(pc) = guard.get_peer_connection().await {
                return Some(pc);
            }
        }
        None
    }

    /// Create audio decoder based on negotiated codec from answer SDP.
    ///
    /// Extracts the codec from the caller's answer SDP and creates the appropriate decoder.
    /// Falls back to PCMU if no codec can be determined.
    fn create_audio_decoder(&self) -> Option<Box<dyn audio_codec::Decoder>> {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::create_decoder;

        // Try to extract codec from caller's answer SDP
        let codec = if let Some(ref answer_sdp) = self.answer {
            let profile = MediaNegotiator::extract_leg_profile(answer_sdp);
            if let Some(audio) = profile.audio {
                info!(
                    session_id = %self.id,
                    codec = ?audio.codec,
                    payload_type = audio.payload_type,
                    "Using negotiated codec for conference decoder"
                );
                audio.codec
            } else {
                CodecType::PCMU
            }
        } else {
            CodecType::PCMU
        };

        Some(create_decoder(codec))
    }

    /// Handle cross-session P2P bridge by creating a conference with both legs.
    ///
    /// This is called when a consult transfer completes and only A and C remain.
    /// Instead of true P2P media bridging, we create a 2-party conference which
    /// is functionally equivalent but reuses existing infrastructure.
    async fn handle_bridge_cross_session(
        &mut self,
        session_a: String,
        leg_a: LegId,
        session_b: String,
        leg_b: LegId,
    ) -> Result<()> {
        let current_session = self.id.to_string();

        info!(
            current_session = %current_session,
            session_a = %session_a,
            session_b = %session_b,
            "Handling cross-session P2P bridge"
        );

        // Generate a deterministic conference ID
        let conf_id = if session_a < session_b {
            format!("p2p-bridge-{}-{}", session_a, session_b)
        } else {
            format!("p2p-bridge-{}-{}", session_b, session_a)
        };
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());

        // Check if this session is part of the bridge
        let (my_session, my_leg, other_session, _other_leg) = if current_session == session_a {
            (
                session_a.clone(),
                leg_a.clone(),
                session_b.clone(),
                leg_b.clone(),
            )
        } else if current_session == session_b {
            (
                session_b.clone(),
                leg_b.clone(),
                session_a.clone(),
                leg_a.clone(),
            )
        } else {
            // This session is not part of the bridge, forward to session_a
            let registry = &self.server.active_call_registry;
            if let Some(handle) = registry.get_handle(&session_a) {
                let session_a_clone = session_a.clone();
                handle
                    .send_command(CallCommand::BridgeCrossSession {
                        session_a,
                        leg_a: leg_a.clone(),
                        session_b,
                        leg_b: leg_b.clone(),
                    })
                    .map_err(|e| anyhow!("Failed to forward BridgeCrossSession: {}", e))?;
                info!(
                    "Forwarded BridgeCrossSession to session_a {}",
                    session_a_clone
                );
            }
            return Ok(());
        };

        // Create conference if it doesn't exist (idempotent)
        if self
            .server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            info!(conf_id = %conf_id, "Creating P2P bridge conference");
            self.server
                .conference_manager
                .create_conference(conf_id_obj.clone(), None)
                .await
                .map_err(|e| anyhow!("Failed to create P2P conference: {}", e))?;
        }

        // Start conference media bridge for this leg.
        // The bridge runtime adds the participant to the conference.
        let participant_leg = LegId::new(format!("{}-{}", my_session, my_leg));
        match self
            .start_conference_media_bridge(&conf_id, &participant_leg)
            .await
        {
            Ok(handle) => {
                info!(
                    session_id = %current_session,
                    leg_id = %my_leg,
                    "P2P conference media bridge started"
                );
                self.conference_bridge.bridge_handle = Some(handle);
                self.conference_bridge.conf_id = Some(conf_id.clone());
            }
            Err(e) => {
                warn!(
                    session_id = %current_session,
                    leg_id = %my_leg,
                    error = %e,
                    "Failed to start P2P conference media bridge"
                );
            }
        }

        // If this is session_a, notify session_b to also join
        if current_session == session_a {
            let registry = &self.server.active_call_registry;
            if let Some(handle) = registry.get_handle(&other_session) {
                let _ = handle.send_command(CallCommand::BridgeCrossSession {
                    session_a: session_a.clone(),
                    leg_a: leg_a.clone(),
                    session_b: session_b.clone(),
                    leg_b: leg_b.clone(),
                });
                info!(
                    session_a = %session_a,
                    session_b = %session_b,
                    "Notified session_b to join P2P conference"
                );
            }
        }

        Ok(())
    }

    async fn handle_supervisor_listen(
        &mut self,
        supervisor_leg: LegId,
        target_leg: LegId,
        supervisor_session_id: Option<String>,
    ) -> Result<()> {
        // Cross-session supervisor: use conference-based mixing when supervisor is in a different session
        if let Some(ref sup_session_id) = supervisor_session_id
            && sup_session_id != &self.id.0
        {
            return self
                .handle_cross_session_supervisor_listen(sup_session_id, target_leg)
                .await;
        }

        // Same-session supervisor: use existing MediaMixer approach
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        let resolved_target_leg = if self.legs.contains_key(&target_leg) {
            target_leg.clone()
        } else if self.legs.contains_key(&LegId::new("callee")) {
            warn!(
                session_id = %self.id,
                requested_leg = %target_leg,
                "Supervisor listen target leg not found, falling back to callee"
            );
            LegId::new("callee")
        } else if self.legs.contains_key(&LegId::new("caller")) {
            warn!(
                session_id = %self.id,
                requested_leg = %target_leg,
                "Supervisor listen target leg not found, falling back to caller"
            );
            LegId::new("caller")
        } else {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        };

        let mixer = if let Some(ref mixer) = self.supervisor_mixer {
            mixer.clone()
        } else {
            let mixer = MediaMixer::new(format!("supervisor-{}", self.id), 8000);
            let mixer = Arc::new(mixer);
            self.supervisor_mixer = Some(mixer.clone());
            mixer
        };

        use crate::media::mixer::SupervisorMixerMode;
        mixer.set_mode(SupervisorMixerMode::Listen);

        let target_peer = if resolved_target_leg == LegId::new("caller") {
            self.caller_peer.clone()
        } else {
            self.callee_peer.clone()
        };

        let target_input = crate::media::mixer_input::MixerInput::new(
            format!("{}-input", resolved_target_leg),
            target_peer,
            CodecType::PCMU,
        );

        let supervisor_output = crate::media::mixer_output::MixerOutput::new(
            format!("{}-output", supervisor_leg),
            self.callee_peer.clone(),
            CodecType::PCMU,
        );

        mixer.add_mixer_input(target_input);
        mixer.add_mixer_output(supervisor_output);

        mixer.set_output_routing(
            &format!("{}-output", supervisor_leg),
            vec![format!("{}-input", resolved_target_leg)],
        );

        mixer.start();

        self.update_leg_state(&supervisor_leg, LegState::Connected);
        info!(
            session_id = %self.id,
            supervisor = %supervisor_leg,
            target = %resolved_target_leg,
            "Supervisor listen mode activated with MediaMixer"
        );
        Ok(())
    }

    /// Handle cross-session supervisor monitoring using conference-based mixing.
    /// Creates a conference room and adds both the target leg and supervisor session.
    async fn handle_cross_session_supervisor_listen(
        &mut self,
        supervisor_session_id: &str,
        target_leg: LegId,
    ) -> Result<()> {
        let resolved_target_leg = if self.legs.contains_key(&target_leg) {
            target_leg
        } else if self.legs.contains_key(&LegId::new("callee")) {
            warn!(
                session_id = %self.id,
                requested_leg = %target_leg,
                "Cross-session supervisor listen target leg not found, falling back to callee"
            );
            LegId::new("callee")
        } else if self.legs.contains_key(&LegId::new("caller")) {
            warn!(
                session_id = %self.id,
                requested_leg = %target_leg,
                "Cross-session supervisor listen target leg not found, falling back to caller"
            );
            LegId::new("caller")
        } else {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        };

        // Generate a deterministic conference ID for supervisor monitoring
        let conf_id = format!("supervisor-{}-{}", self.id.0, supervisor_session_id);
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());

        // Create conference if it doesn't exist (idempotent)
        if self
            .server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            info!(conf_id = %conf_id, "Creating supervisor conference");
            self.server
                .conference_manager
                .create_conference(conf_id_obj.clone(), Some(3))
                .await
                .map_err(|e| anyhow!("Failed to create supervisor conference: {}", e))?;
        }

        // Start conference media bridge for target session on the resolved target leg.
        let target_participant_leg = LegId::new(format!("{}-{}", self.id.0, resolved_target_leg));
        match self
            .start_conference_media_bridge(&conf_id, &target_participant_leg)
            .await
        {
            Ok(handle) => {
                info!(
                    session_id = %self.id,
                    leg_id = %resolved_target_leg,
                    "Supervisor conference media bridge started for target"
                );
                self.conference_bridge.bridge_handle = Some(handle);
                self.conference_bridge.conf_id = Some(conf_id.clone());
            }
            Err(e) => {
                return Err(anyhow!(
                    "Failed to start supervisor conference media bridge for target {}: {}",
                    resolved_target_leg,
                    e
                ));
            }
        }

        // Notify supervisor session to join the conference
        let registry = &self.server.active_call_registry;
        if let Some(handle) = registry.get_handle(supervisor_session_id) {
            let join_cmd = CallCommand::JoinMixer {
                mixer_id: conf_id.clone(),
            };
            handle
                .send_command(join_cmd)
                .map_err(|e| anyhow!("Failed to notify supervisor session: {}", e))?;
            info!(
                supervisor_session = %supervisor_session_id,
                conf_id = %conf_id,
                "Notified supervisor session to join conference"
            );
        } else {
            return Err(anyhow!(
                "Supervisor session {} not found",
                supervisor_session_id
            ));
        }

        info!(
            session_id = %self.id,
            supervisor_session = %supervisor_session_id,
            conf_id = %conf_id,
            "Cross-session supervisor listen activated via conference"
        );
        Ok(())
    }

    async fn handle_supervisor_whisper(
        &mut self,
        supervisor_leg: LegId,
        target_leg: LegId,
        _supervisor_session_id: Option<String>,
    ) -> Result<()> {
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        if !self.legs.contains_key(&target_leg) {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        }

        let mixer = if let Some(ref mixer) = self.supervisor_mixer {
            mixer.clone()
        } else {
            let mixer = MediaMixer::new(format!("supervisor-{}", self.id), 8000);
            let mixer = Arc::new(mixer);
            self.supervisor_mixer = Some(mixer.clone());
            mixer
        };

        use crate::media::mixer::SupervisorMixerMode;
        mixer.set_mode(SupervisorMixerMode::Whisper);

        let (target_peer, supervisor_peer) = if target_leg == LegId::new("caller") {
            (self.caller_peer.clone(), self.callee_peer.clone())
        } else {
            (self.callee_peer.clone(), self.caller_peer.clone())
        };

        let target_input = crate::media::mixer_input::MixerInput::new(
            format!("{}-input", target_leg),
            target_peer.clone(),
            CodecType::PCMU,
        );
        let supervisor_input = crate::media::mixer_input::MixerInput::new(
            format!("{}-input", supervisor_leg),
            supervisor_peer.clone(),
            CodecType::PCMU,
        );

        let supervisor_output = crate::media::mixer_output::MixerOutput::new(
            format!("{}-output", supervisor_leg),
            supervisor_peer,
            CodecType::PCMU,
        );
        let target_output = crate::media::mixer_output::MixerOutput::new(
            format!("{}-output", target_leg),
            target_peer,
            CodecType::PCMU,
        );

        mixer.add_mixer_input(target_input);
        mixer.add_mixer_input(supervisor_input);
        mixer.add_mixer_output(supervisor_output);
        mixer.add_mixer_output(target_output);

        mixer.set_output_routing(
            &format!("{}-output", supervisor_leg),
            vec![format!("{}-input", target_leg)],
        );
        mixer.set_output_routing(
            &format!("{}-output", target_leg),
            vec![format!("{}-input", supervisor_leg)],
        );

        mixer.start();

        self.update_leg_state(&supervisor_leg, LegState::Connected);
        info!(
            session_id = %self.id,
            supervisor = %supervisor_leg,
            target = %target_leg,
            "Supervisor whisper mode activated with MediaMixer"
        );
        Ok(())
    }

    async fn handle_supervisor_barge(
        &mut self,
        supervisor_leg: LegId,
        target_leg: LegId,
        _supervisor_session_id: Option<String>,
    ) -> Result<()> {
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        if !self.legs.contains_key(&target_leg) {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        }

        let mixer = if let Some(ref mixer) = self.supervisor_mixer {
            mixer.clone()
        } else {
            let mixer = MediaMixer::new(format!("supervisor-{}", self.id), 8000);
            let mixer = Arc::new(mixer);
            self.supervisor_mixer = Some(mixer.clone());
            mixer
        };

        use crate::media::mixer::SupervisorMixerMode;
        mixer.set_mode(SupervisorMixerMode::Barge);

        let (target_peer, supervisor_peer) = if target_leg == LegId::new("caller") {
            (self.caller_peer.clone(), self.callee_peer.clone())
        } else {
            (self.callee_peer.clone(), self.caller_peer.clone())
        };

        let target_input = crate::media::mixer_input::MixerInput::new(
            format!("{}-input", target_leg),
            target_peer.clone(),
            CodecType::PCMU,
        );
        let supervisor_input = crate::media::mixer_input::MixerInput::new(
            format!("{}-input", supervisor_leg),
            supervisor_peer.clone(),
            CodecType::PCMU,
        );

        let supervisor_output = crate::media::mixer_output::MixerOutput::new(
            format!("{}-output", supervisor_leg),
            supervisor_peer,
            CodecType::PCMU,
        );
        let target_output = crate::media::mixer_output::MixerOutput::new(
            format!("{}-output", target_leg),
            target_peer,
            CodecType::PCMU,
        );

        mixer.add_mixer_input(target_input);
        mixer.add_mixer_input(supervisor_input);
        mixer.add_mixer_output(supervisor_output);
        mixer.add_mixer_output(target_output);

        mixer.set_output_routing(
            &format!("{}-output", supervisor_leg),
            vec![
                format!("{}-input", target_leg),
                format!("{}-input", supervisor_leg),
            ],
        );
        mixer.set_output_routing(
            &format!("{}-output", target_leg),
            vec![
                format!("{}-input", target_leg),
                format!("{}-input", supervisor_leg),
            ],
        );

        mixer.start();

        self.update_leg_state(&supervisor_leg, LegState::Connected);
        info!(
            session_id = %self.id,
            supervisor = %supervisor_leg,
            target = %target_leg,
            "Supervisor barge mode activated with MediaMixer"
        );
        Ok(())
    }

    async fn handle_supervisor_takeover(
        &mut self,
        supervisor_leg: LegId,
        target_leg: LegId,
        _supervisor_session_id: Option<String>,
    ) -> Result<()> {
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        if !self.legs.contains_key(&target_leg) {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        }

        // Stop any existing supervisor mixer
        if let Some(ref mixer) = self.supervisor_mixer.take() {
            mixer.stop();
            info!(session_id = %self.id, "Stopped existing supervisor mixer for takeover");
        }

        // Determine the remaining party (the one not being replaced)
        let other_leg = if target_leg == LegId::new("caller") {
            LegId::new("callee")
        } else {
            LegId::new("caller")
        };

        // Update bridge to connect supervisor with the remaining party
        self.bridge = BridgeConfig::bridge(supervisor_leg.clone(), other_leg.clone());

        // Mark target leg as ended and supervisor as connected
        self.update_leg_state(&target_leg, LegState::Ending);
        self.update_leg_state(&supervisor_leg, LegState::Connected);

        info!(
            session_id = %self.id,
            supervisor = %supervisor_leg,
            target = %target_leg,
            other = %other_leg,
            "Supervisor takeover activated"
        );
        Ok(())
    }

    async fn handle_supervisor_stop(&mut self, supervisor_leg: LegId) -> Result<()> {
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }

        if let Some(ref mixer) = self.supervisor_mixer {
            mixer.stop();
            info!(
                session_id = %self.id,
                "Supervisor mixer stopped"
            );
        }

        if self.legs.len() <= 2 {
            self.supervisor_mixer = None;
        }

        self.update_leg_state(&supervisor_leg, LegState::Ended);
        info!("Supervisor mode stopped");
        Ok(())
    }

    async fn handle_play(
        &mut self,
        leg_id: Option<LegId>,
        source: crate::call::domain::MediaSource,
        options: Option<crate::call::domain::PlayOptions>,
    ) -> Result<()> {
        let track_id = options
            .as_ref()
            .and_then(|o| o.track_id.clone())
            .or_else(|| leg_id.as_ref().map(|l| l.to_string()))
            .unwrap_or_else(|| "playback".to_string());
        let file_path = match source {
            crate::call::domain::MediaSource::File { path } => path,
            _ => return Err(anyhow!("Only file playback supported")),
        };

        let caller_codec = self
            .caller_offer
            .as_ref()
            .map(|offer| MediaNegotiator::extract_codec_params(offer).audio)
            .and_then(|codecs| codecs.first().map(|c| c.codec))
            .unwrap_or(CodecType::PCMU);

        let track = FileTrack::new(track_id.clone())
            .with_path(file_path.clone())
            .with_codec_preference(vec![caller_codec]);

        let caller_pc = {
            let mut pc = None;
            for _ in 0..100 {
                if let Some(found_pc) = self.get_caller_peer_connection().await {
                    pc = Some(found_pc);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            pc
        };

        if let Err(e) = track.start_playback_on(caller_pc).await {
            warn!(error = %e, "Failed to start playback");
        }

        self.caller_peer
            .update_track(Box::new(track.clone()), None)
            .await;
        self.playback_tracks.insert(track_id.clone(), track);

        info!(track_id = %track_id, file = %file_path, "Playback started");
        Ok(())
    }

    async fn handle_stop_playback(&mut self, leg_id: Option<LegId>) -> Result<()> {
        let track_id = leg_id
            .as_ref()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "playback".to_string());

        if self.playback_tracks.remove(&track_id).is_some() {
            self.caller_peer.remove_track(&track_id, true).await;
            info!(track_id = %track_id, "Playback stopped");
        }
        Ok(())
    }

    async fn handle_conference_create(
        &mut self,
        conf_id: String,
        options: crate::call::domain::ConferenceOptions,
    ) -> Result<()> {
        info!(%conf_id, "Creating conference");

        let max_participants = options.max_participants.map(|m| m as usize);
        self.server
            .conference_manager
            .create_conference(conf_id.into(), max_participants)
            .await?;

        Ok(())
    }

    async fn handle_conference_add(&mut self, conf_id: String, leg_id: LegId) -> Result<()> {
        info!(%conf_id, %leg_id, "Adding leg to conference");

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        self.server
            .conference_manager
            .add_participant(&conf_id.into(), leg_id)
            .await?;

        Ok(())
    }

    async fn handle_conference_remove(&mut self, conf_id: String, leg_id: LegId) -> Result<()> {
        info!(%conf_id, %leg_id, "Removing leg from conference");

        self.server
            .conference_manager
            .remove_participant(&conf_id.into(), &leg_id)
            .await?;

        Ok(())
    }

    async fn handle_conference_mute(&mut self, conf_id: String, leg_id: LegId) -> Result<()> {
        info!(%conf_id, %leg_id, "Muting leg in conference");

        self.server
            .conference_manager
            .mute_participant(&conf_id.into(), &leg_id)
            .await?;

        Ok(())
    }

    async fn handle_conference_unmute(&mut self, conf_id: String, leg_id: LegId) -> Result<()> {
        info!(%conf_id, %leg_id, "Unmuting leg in conference");

        self.server
            .conference_manager
            .unmute_participant(&conf_id.into(), &leg_id)
            .await?;

        Ok(())
    }

    async fn handle_conference_destroy(&mut self, conf_id: String) -> Result<()> {
        info!(%conf_id, "Destroying conference");

        self.server
            .conference_manager
            .destroy_conference(&conf_id.into())
            .await?;

        Ok(())
    }

    async fn handle_join_mixer(&mut self, mixer_id: String) -> Result<()> {
        info!(%mixer_id, "Joining mixer/conference");

        let conf_id_obj = crate::call::runtime::ConferenceId::from(mixer_id.as_str());

        // Ensure conference exists
        if self
            .server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            return Err(anyhow!("Conference {} not found", mixer_id));
        }

        // Start conference media bridge for supervisor session.
        // The bridge runtime adds the participant to the conference.
        let participant_leg = LegId::new(format!("{}-callee", self.id.0));
        match self
            .start_conference_media_bridge(&mixer_id, &participant_leg)
            .await
        {
            Ok(handle) => {
                info!(
                    session_id = %self.id,
                    conf_id = %mixer_id,
                    "Supervisor conference media bridge started"
                );
                self.conference_bridge.bridge_handle = Some(handle);
                self.conference_bridge.conf_id = Some(mixer_id.clone());
            }
            Err(e) => {
                warn!(
                    session_id = %self.id,
                    conf_id = %mixer_id,
                    error = %e,
                    "Failed to start supervisor conference media bridge"
                );
            }
        }

        Ok(())
    }

    async fn handle_leave_mixer(&mut self) -> Result<()> {
        info!("Leaving mixer/conference");

        if let Some(conf_id) = self.conference_bridge.conf_id.take() {
            let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());
            let participant_leg = LegId::new(format!("{}-callee", self.id.0));

            let _ = self
                .server
                .conference_manager
                .remove_participant(&conf_id_obj, &participant_leg)
                .await;

            if let Some(ref handle) = self.conference_bridge.bridge_handle {
                handle.stop();
            }
            self.conference_bridge.bridge_handle = None;
            info!(session_id = %self.id, conf_id = %conf_id, "Left conference");
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

    async fn handle_send_dtmf(&mut self, leg_id: LegId, digits: String) -> Result<()> {
        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

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

        match self
            .server_dialog
            .info(Some(headers), Some(dtmf_body.into_bytes()))
            .await
        {
            Ok(_) => {
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
struct PeerConnectionAudioReceiver {
    pc: rustrtc::PeerConnection,
    decoder: Box<dyn audio_codec::Decoder>,
    audio_track: Option<Arc<dyn rustrtc::media::MediaStreamTrack>>,
}

impl PeerConnectionAudioReceiver {
    fn new(pc: rustrtc::PeerConnection, decoder: Box<dyn audio_codec::Decoder>) -> Self {
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
    use rustrtc::{MediaKind, PeerConnection, RtcConfiguration};
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
    fn test_resolve_outbound_destination_prefers_remote_home_proxy() {
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

        let (resolved, via_home_proxy) =
            SipSession::resolve_outbound_destination(&target, &local_addrs);

        assert!(via_home_proxy);
        assert_eq!(resolved, Some(home_proxy));
    }

    #[test]
    fn test_resolve_outbound_destination_uses_destination_for_local_home_proxy() {
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

        let (resolved, via_home_proxy) =
            SipSession::resolve_outbound_destination(&target, &local_addrs);

        assert!(!via_home_proxy);
        assert_eq!(resolved, Some(destination));
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
        session.caller_is_webrtc = true;
        session.callee_is_webrtc = false;

        let pc = session.get_local_reinvite_pc(DialogSide::Caller).await;

        assert!(pc.is_some(), "bridge-backed caller leg should resolve a PC");
        assert_eq!(caller_peer.get_tracks_call_count(), 0);
        assert_eq!(callee_peer.get_tracks_call_count(), 0);
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
            target_pc
                .get_transceivers()
                .iter()
                .any(|t| t.kind() == MediaKind::Audio),
            "play_audio_file should bind audio to the caller external PC even when the first track has no PC"
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
}
