use super::builtin_app_factory::BuiltinAppFactory;
use super::prelude::*;
use super::util::{
    CalleeError, format_duration_ms, forward_dtmf_event, inject_dtmf_into_app, into_callee_err,
    normalize_call_hangup_by, other_header_ci, parse_dial_target, parse_dtmf_digit,
    parse_sipfrag_status, route_outbound_leg, sip_status_to_hangup_reason, trunk_host_port,
};
use super::{live_transcription, transfer};
use crate::proxy::call::parse_allowed_codecs;

const CMD_CHANNEL_CAPACITY: usize = 256;
const RUSTPBX_COMMAND_CT: &str = "application/vnd.rustpbx+json";

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
pub(super) enum TimerAction {
    Refresh,
    Expired,
}
pub(super) enum UpdateRefreshOutcome {
    Refreshed,
    Retry,
    FallbackToReinvite,
    Failed(anyhow::Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DialogSide {
    Caller,
    Callee,
}
pub struct SipSession {
    pub id: SessionId,
    pub state: SessionState,
    pub legs: crate::proxy::proxy_call::leg_registry::LegRegistry,
    pub bridge: BridgeConfig,
    pub media_profile: MediaRuntimeProfile,
    pub app_runtime: Arc<dyn AppRuntime>,
    pub snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>>,

    pub server: SipServerRef,
    /// The primary caller dialog (A leg). `None` while a UAC/outbound session
    /// (for example RWI originate) prepares and sends its first INVITE, then
    /// populated when that answered dialog is attached.
    pub caller_dialog: Option<InviteDialog>,
    pub callee_dialogs: Arc<DashMap<DialogId, ()>>,

    pub context: CallContext,
    /// Shared owner for every concurrent-call permit held by this
    /// call. Cleanup releases the complete set; Drop is the safety net.
    pub concurrent_call_lease: crate::call::concurrent_call_limiter::ConcurrentCallLease,
    /// Concurrent-call leases acquired by routing app/transfer/originate legs
    /// through `match_invite`. Stored separately (permits are private to
    /// `ConcurrentCallLease`) and released during cleanup/Drop alongside the
    /// primary lease so route-acquired trunk/tenant slots are never leaked.
    transient_leases: Vec<crate::call::concurrent_call_limiter::ConcurrentCallLease>,
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
    /// One-shot latch for the core `call_answered` event: `accept_call`
    /// (direct UAS answer) and the queue-agent `LegConnected` branch can both
    /// observe an answer moment for the same logical call (app-startup race);
    /// only the first emits. Mirrors the dedup the former CC-addon
    /// `cc_answered` got from the agent state machine (Ringing/Idle → Busy
    /// fired exactly once per call). A transfer to a new agent runs in a NEW
    /// session with its own latch, so per-agent attribution is preserved.
    answered_event_emitted: bool,

    pub app_event_bridge: crate::proxy::proxy_call::state::AppEventBridge,

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

    /// Originating IVR node context of the active bridge (step_id / extra).
    /// Armed by `connect_bridge()` from `_rst_*` URI params injected by the
    /// step executor; used to report bridge DTMF as `ivr_step_trace`.
    pub(crate) bridge_trace_context:
        Arc<parking_lot::Mutex<Option<super::transfer::BridgeTraceContext>>>,

    /// DTMF digits captured while a bridge was active, replayed into the
    /// return-app IVR params when the bridge disconnects.
    pub(crate) bridge_dtmf_digits: Arc<parking_lot::Mutex<Vec<String>>>,

    /// Active realtime (AI voice) WS bridge handle — set by
    /// `CallCommand::RealtimeStart`, cleared by `RealtimeStop` / teardown.
    pub(crate) realtime_bridge: Option<super::realtime_bridge::RealtimeBridgeHandle>,

    /// Active TTS/voip WebSocket bridge handle — set by `connect_bridge()`,
    /// cleared on disconnect (`CallCommand::VoipBridgeClosed`) / teardown.
    /// Kept separate from `conference_bridge`: storing the voip handle there
    /// set a fake `conf_id` that permanently gated `update_media_path()`, so a
    /// queue agent leg connecting after any TTS bridge flow was never bridged
    /// (silent one-way audio).
    pub(crate) voip_bridge: Option<crate::call::runtime::ConferenceBridgeHandle>,

    pub cmd_tx: Option<mpsc::Sender<CallCommand>>,

    /// Cluster session-registry RAII guard: registers this session's owning
    /// node at birth and unregisters on drop (any exit path). `None` when the
    /// registry is a no-op backend (single-node) or registration failed.
    session_registry_guard: Option<crate::call::runtime::SessionGuard>,

    /// This session's own handle (used to send commands back into the session
    /// from spawned tasks, e.g. restore the media route after playback).
    pub handle: SipSessionHandle,

    /// In-flight `media.play` bookkeeping for the call trace: track_id → what
    /// is playing and when it started. Used to emit `Play` trace events with
    /// duration + interruption. `record_play_end` is idempotent via `remove`.
    active_plays: std::collections::HashMap<String, crate::proxy::proxy_call::state::ActivePlay>,

    /// Live transcription state; `None` while nobody subscribes.
    pub(crate) live_transcription: Option<live_transcription::LiveTranscription>,

    /// Currently active mid-call / auto recording (if any).
    active_recording: Option<crate::callrecord::ActiveRecording>,
    /// Completed recording segments for this leg (full-call + mid-call slices).
    completed_recording_segments: Vec<crate::callrecord::RecordingSegment>,
    /// 1-based counter feeding auto-generated segment file names
    /// (`{session_id}_{seq}_{label}.wav`). Per-session; `segmented_wav_path`
    /// bumps on file collision for cross-session (transfer) segments.
    recording_seq: u32,
}
#[derive(Clone)]
pub struct SipSessionHandle {
    session_id: SessionId,
    cmd_tx: mpsc::Sender<CallCommand>,
    snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>>,
    app_event_bridge: crate::proxy::proxy_call::state::AppEventBridge,
}
impl SipSessionHandle {
    pub fn send_command(&self, cmd: CallCommand) -> anyhow::Result<()> {
        match self.cmd_tx.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(session_id = %self.session_id.0, "SipSession command channel full, command dropped");
                Err(anyhow::anyhow!("command channel full"))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!(session_id = %self.session_id.0, "SipSession command channel closed, command dropped");
                Err(anyhow::anyhow!("command channel closed"))
            }
        }
    }

    pub async fn send_command_async(&self, cmd: CallCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|e| anyhow::anyhow!("channel closed: {}", e))
    }

    pub(crate) async fn query_recorder_status(
        &self,
    ) -> anyhow::Result<crate::media::media_recorder::RecorderStatus> {
        let (reply, response) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::QueryRecorderStatus { reply })
            .await?;
        tokio::time::timeout(Duration::from_secs(5), response)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for recorder query"))?
            .map_err(|_| anyhow::anyhow!("session closed before answering recorder query"))?
    }

    pub fn session_id(&self) -> &str {
        &self.session_id.0
    }

    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        self.snapshot_cache.read().clone()
    }

    pub fn send_app_event(&self, event: crate::call::app::ControllerEvent) -> bool {
        self.app_event_bridge.send_app_event(event)
    }

    pub fn set_app_event_sender(
        &self,
        sender: Option<mpsc::UnboundedSender<crate::call::app::ControllerEvent>>,
    ) {
        self.app_event_bridge.set_app_event_sender(sender);
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
            app_event_bridge: crate::proxy::proxy_call::state::AppEventBridge::new(),
        }
    }
}
pub(super) enum ConstructMode<'a> {
    Uas { server_dialog: &'a InviteDialog },
    Uac,
}

impl SipSession {
    pub const CALLER_TRACK_ID: &'static str = "caller-track";
    pub const CALLEE_TRACK_ID: &'static str = "callee-track";
    const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
    const CALLER_REJECTION_ACK_TIMEOUT: Duration = Duration::from_secs(3);
    const MID_DIALOG_TIMEOUT: Duration = Duration::from_secs(30);
    /// Minimum length (ms) a `tone://` cue is rendered for. Shorter specs are
    /// padded up to this so a failure tone is always audible before the reject.
    const MIN_TONE_DURATION_MS: u64 = 1000;
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

    /// Whether app/transfer/RWI-originated calls on this session should be
    /// routed through the route table: session-level dialplan override falling
    /// back to the global `ProxyConfig.route_originated_calls`.
    pub(super) fn route_originated_enabled(&self) -> bool {
        self.context
            .dialplan
            .route_originated_calls
            .unwrap_or(self.server.proxy_config.load().route_originated_calls)
    }

    /// Route a not-registered (external) leg target through the route table
    /// when `route_originated_calls` is enabled. Returns the possibly-rewritten
    /// `Location` plus any routing hints whose concurrency resources the caller
    /// must release on teardown (via [`SipSession::track_routed_leg_hints`]).
    ///
    /// - `Forward(option, hints)`: callee/destination/credential/headers are
    ///   applied to the location; returns `(location, hints)`.
    /// - `NotHandled`: location returned unchanged, no hints.
    /// - `Abort(code, reason)`: returns `Err` with the SIP status code.
    /// - `Queue`/`Application`: not supported for a dialed leg — treated as
    ///   `NotHandled` (the leg is dialed directly to the original target).
    pub(super) async fn route_originated_leg(
        &self,
        location: &crate::call::Location,
    ) -> Result<(crate::call::Location, Option<crate::config::DialplanHints>)> {
        if !self.route_originated_enabled() {
            return Ok((location.clone(), None));
        }
        let caller = match self.context.dialplan.caller.clone() {
            Some(c) => c,
            None => return Ok((location.clone(), None)),
        };
        let contact = self
            .context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .or_else(|| self.server.contact_uri_for_location_with_sip_contact(
                location, self.context.dialplan.media.sip_contact.as_ref(),
            ))
            .unwrap_or_else(|| caller.clone());
        // Carry original caller headers (X-CRM-*, X-CC-*, etc.) so header-based
        // match/rewrite rules behave like the inbound path.
        let carry_headers: Vec<rsipstack::sip::Header> = self
            .caller_dialog
            .as_ref()
            .map(|d| d.initial_request().headers.iter().cloned().collect())
            .unwrap_or_default();

        match route_outbound_leg(
            &self.server,
            &location.aor,
            &caller,
            &contact,
            if carry_headers.is_empty() {
                None
            } else {
                Some(carry_headers)
            },
            self.context.cookie.clone(),
        )
        .await?
        {
            Some(crate::config::RouteResult::Forward(option, hints)) => {
                let mut routed = location.clone();
                routed.aor = option.callee.clone();
                routed.destination = option.destination.clone();
                routed.credential = option.credential.clone();
                routed.headers = option.headers.clone();
                routed.contact_raw = Some(option.callee.to_string());
                Ok((routed, hints))
            }
            Some(crate::config::RouteResult::NotHandled(_, _))
            | Some(crate::config::RouteResult::Queue { .. })
            | Some(crate::config::RouteResult::Application { .. })
            | None => Ok((location.clone(), None)),
            Some(crate::config::RouteResult::Abort(code, reason)) => Err(anyhow!(
                "route aborted for originated leg: {} {}",
                code.code(),
                reason.unwrap_or_default()
            )),
        }
    }

    /// Track routing hints (concurrency holds + concurrent-call lease) acquired
    /// by `route_originated_leg` so they are released with the session on
    /// teardown (cleanup + Drop).
    pub(super) fn track_routed_leg_hints(&mut self, hints: Option<crate::config::DialplanHints>) {
        if let Some(hints) = hints {
            if !hints.concurrency_holds.is_empty() {
                self.context
                    .dialplan
                    .concurrency_holds
                    .lock()
                    .extend(hints.concurrency_holds);
            }
            if !hints.concurrent_call_lease.is_empty() {
                self.transient_leases.push(hints.concurrent_call_lease);
            }
        }
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
    pub(super) fn bridge(&self) -> Option<&MediaBridge> {
        self.media.bridge.as_ref()
    }
    pub(super) fn bridge_mut(&mut self) -> Option<&mut MediaBridge> {
        self.media.bridge.as_mut()
    }

    /// Build a `LegConfig` for a MediaBridge leg from the dialplan media
    /// settings. Shared by all leg-creation paths (`ensure_media_leg`,
    /// `ensure_caller_leg`, `create_callee_track`) so transport/codec/port
    /// handling stays consistent.
    /// Whether the media path should carry video for this call, per the
    /// dialplan video policy. `Strip` disables video (audio-only); `PassThrough`
    /// and `Transcode` (and the default `None`) enable it — video is relayed at
    /// the transport level, so `PassThrough` and `Transcode` behave the same
    /// for now (video transcoding is not implemented).
    fn video_relay_enabled(&self) -> bool {
        !matches!(
            self.context.dialplan.media.video_policy,
            Some(crate::proxy::routing::VideoPolicy::Strip)
        )
    }

    fn video_caps_from_sdp(&self, sdp: &str) -> Vec<rustrtc::VideoCapability> {
        let proxy_config = self.server.proxy_config.load();
        crate::media::negotiate::MediaNegotiator::video_caps_for_config(
            &crate::media::negotiate::MediaNegotiator::extract_video_codecs(sdp),
            &proxy_config.video_codecs,
        )
    }

    fn build_leg_config(
        &self,
        transport: rustrtc::TransportMode,
        codecs: Vec<crate::media::negotiate::CodecInfo>,
        video_codecs: Vec<rustrtc::config::VideoCapability>,
    ) -> crate::media::leg::LegConfig {
        let is_webrtc = transport == rustrtc::TransportMode::WebRtc;
        let rtp_port_range = if is_webrtc {
            self.context
                .dialplan
                .media
                .webrtc_port_start
                .zip(self.context.dialplan.media.webrtc_port_end)
        } else {
            self.context
                .dialplan
                .media
                .rtp_start_port
                .zip(self.context.dialplan.media.rtp_end_port)
        };
        crate::media::leg::LegConfig {
            // WebRTC legs gather ICE candidates; relay-only (per dialplan)
            // strips host/srflx so the browser must use the TURN path.
            ice_servers: self
                .context
                .dialplan
                .media
                .ice_servers
                .clone()
                .unwrap_or_default(),
            relay_only: self.context.dialplan.media.relay_only,
            transport,
            codecs,
            video_codecs,
            rtp_port_range,
            external_ip: self.context.dialplan.media.external_ip.clone(),
            // WebRTC must gather host candidates across interfaces. A fixed
            // SIP/RTP bind address is only appropriate for RTP/SDES legs.
            bind_ip: if is_webrtc {
                None
            } else {
                self.context.dialplan.media.bind_ip.clone()
            },
            cname: Some(self.server.rtc_cname.clone()),
            comfort_noise: self.context.dialplan.media.comfort_noise,
            comfort_noise_level_db: self.context.dialplan.media.comfort_noise_level_db,
            // ICE-lite only ever applies to plain-RTP legs (`LegConfig` forces
            // it off for WebRTC/Srtp), so pass the dialplan flag through
            // unconditionally — WebRTC callers are unaffected.
            enable_ice_lite: self.context.dialplan.media.ice_lite,
            // Symmetric-RTP latching: on plain RTP/SRTP legs, follow the
            // peer's observed source address so NAT'd softphones whose SDP
            // advertises a private `c=` still receive audio. WebRTC legs
            // resolve their address via ICE and ignore this.
            enable_latching: self.context.dialplan.media.enable_latching,
            probation_max_packets: self.context.dialplan.media.probation_max_packets,
            relay_ready_timeout: self
                .context
                .dialplan
                .media
                .relay_ready_timeout_secs
                .map(std::time::Duration::from_secs),
        }
    }

    /// Media endpoints outlive bridge membership and are owned by the session.
    pub(super) fn media_leg(&self, id: &LegId) -> Option<crate::media::leg::Leg> {
        self.legs.media_leg(id)
    }

    pub(crate) fn media_side_for_leg(&self, id: &LegId) -> Option<LegSide> {
        self.bridge().and_then(|mb| mb.side_for_leg(id))
    }

    pub fn with_handle(id: SessionId) -> (SipSessionHandle, mpsc::Receiver<CallCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_CAPACITY);
        let snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>> = Arc::new(RwLock::new(None));

        let handle = SipSessionHandle {
            session_id: id,
            cmd_tx,
            snapshot_cache,
            app_event_bridge: crate::proxy::proxy_call::state::AppEventBridge::new(),
        };

        (handle, cmd_rx)
    }

    /// Create the capture queue and task before constructing caller leg A.
    /// The sender becomes immutable state on the leg's plaintext RTP tap.
    /// Recording capture is attached to the caller peer so a call can
    /// start recording on demand at any later stage (IVR node, agent
    /// connect, API command) even when no recording policy enabled it.
    /// Without an installed recorder the task simply discards the tapped
    /// packets, so the idle cost is a single lightweight task per session.
    fn setup_recording_capture(
        &mut self,
    ) -> Result<Option<crate::media::media_recorder::RecorderSender>> {
        let _media_runtime_guard = crate::utils::media_enter();
        let sender = self.media.recording.setup_recorder_task()?;
        Ok(Some(sender))
    }

    /// Auto-start live transcription when `[proxy.transcript.remote]
    /// auto_start = true` **or** the dialplan carries a per-call
    /// [`TranscriptionPlan`] (attached by a dialplan inspector). Best-effort:
    /// failures (bypass mode, provider errors) are logged and never fatal to
    /// the call. Holds one transcription reference for the rest of the call.
    ///
    /// Re-entry safe: `accept_call` / `attach_caller_dialog` can run more
    /// than once per session (callee re-attach, API `Answer`, queue
    /// playback). Unlike the `StartTranscription` command (whose executor
    /// ref-counts), a direct re-run here would silently REPLACE the running
    /// provider and reset the reference count — so a running transcription
    /// is never touched.
    async fn maybe_autostart_live_transcription(&mut self, at: &'static str) {
        if self.live_transcription.is_some() {
            return;
        }
        let globally_enabled = self
            .server
            .proxy_config
            .load()
            .transcript
            .as_ref()
            .and_then(|t| t.remote.as_ref())
            .and_then(|r| r.auto_start)
            .unwrap_or(false);
        let per_call_plan = self
            .context
            .dialplan
            .extensions
            .get::<crate::call::transcription::TranscriptionPlan>()
            .is_some();
        if !globally_enabled && !per_call_plan {
            return;
        }
        if let Err(error) = self.start_live_transcription(None).await {
            warn!(
                session_id = %self.id,
                at,
                %error,
                "Auto start live transcription failed"
            );
        }
    }

    /// Fill missing file options from per-call policy, then server policy.
    /// Explicit RecorderOption values win, including stereo_swap = false.
    fn recorder_option(&self, path: String) -> crate::media::recorder::RecorderOption {
        let global = self.server.recording_policy.load();
        let global = global.as_ref().as_ref();
        let local = self.context.dialplan.recording_policy.as_ref();
        let mut option = self.context.dialplan.recording.option.clone().unwrap_or_default();
        option.recorder_file = path;
        option.samplerate = option.samplerate
            .or_else(|| local.and_then(|p| p.samplerate))
            .or_else(|| global.and_then(|p| p.samplerate));
        option.ptime = option.ptime
            .or_else(|| local.and_then(|p| p.ptime))
            .or_else(|| global.and_then(|p| p.ptime));
        option.stereo_swap = option.stereo_swap
            .or_else(|| local.and_then(|p| p.stereo_swap))
            .or_else(|| self.context.dialplan.recording.stereo_swap.then_some(true))
            .or_else(|| global.and_then(|p| p.stereo_swap));
        option
    }

    /// Clear the `discard` marker on the active recording segment. The
    /// originate failure path uses this to KEEP the pre-answer ringback slice
    /// (an unanswered call's artifact) when it stops the recorder — answered
    /// calls instead stop while the marker is armed, deleting the file.
    pub(crate) fn clear_active_recording_discard(&mut self) {
        if let Some(active) = self.active_recording.as_mut() {
            active.discard = false;
        }
    }

    /// Close the active recording segment when the session hands control to a
    /// successor application (transfer / queue / IVR jump). App-scoped sources
    /// ([`crate::callrecord::RecordingSource::Ivr`]) end with their owning
    /// app — the IVR stage's file is finalized at the hand-off ("截断") so the
    /// agent stage records as its own segment. Policy full-call recordings
    /// (`full`) and external segments keep rolling across the hand-off.
    pub(crate) async fn close_app_scoped_recording(&mut self) {
        let is_app_scoped = self
            .active_recording
            .as_ref()
            .is_some_and(|a| a.source == crate::callrecord::RecordingSource::Ivr);
        if !is_app_scoped {
            return;
        }
        let outcome = self.media.recording.stop_recording().await;
        match outcome {
            Ok(Some(result)) => {
                info!(
                    session_id = %self.id,
                    path = %result.path,
                    "IVR recording segment closed at app hand-off"
                );
                self.publish_recording_complete(result);
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    session_id = %self.id,
                    %error,
                    "Failed to close IVR recording segment at app hand-off"
                );
            }
        }
    }

    /// Install the recorder implementation selected for this call. Signaling
    /// call sites decide when automatic installation is allowed.
    pub(crate) async fn set_auto_recorder(&mut self) -> Result<()> {        let recording = self.context.dialplan.recording.clone();
        if self.media.recording.has_recorder().await
        {
            return Ok(());
        }

        // Source-scoped auto start ([recording].auto_start_except): a call
        // routed to an exempt application (e.g. IVR) installs no auto
        // recorder — the IVR flow records only via its smart-node
        // `record start` actions (segmented IVR/agent recordings).
        let routed_app = match &self.context.dialplan.flow {
            crate::call::DialplanFlow::Application { app_name, .. } => Some(app_name.as_str()),
            _ => None,
        };
        {
            let policy_guard = self.server.recording_policy.load();
            let policy = policy_guard
                .as_ref()
                .as_ref()
                .or(self.context.dialplan.recording_policy.as_ref());
            if let Some(policy) = policy && !policy.auto_start_covers_app(routed_app) {
                debug!(
                    session_id = %self.id,
                    routed_app = routed_app.unwrap_or("(none)"),
                    "auto recorder skipped: routed app exempted by [recording].auto_start_except"
                );
                return Ok(());
            }
        }

        if recording.uses_file_media() || recording.option.is_some() {
            let path = recording
                .option
                .as_ref()
                .map(|option| option.recorder_file.clone())
                .ok_or_else(|| anyhow!("file recording strategy has no output path"))?;
            if path.trim().is_empty() {
                return Err(anyhow!("file recording strategy has no output path"));
            }
            let profile = self.media_leg(&LegId::from("caller")).and_then(|peer| peer.negotiated())
                .ok_or_else(|| anyhow!("No caller media profile for recording"))?;
            let option = self.recorder_option(path.clone());
            self.media.recording
                .start_recording(profile, option, 2, false, None)
                .await?;
            self.active_recording = Some(crate::callrecord::ActiveRecording {
                path,
                segment_type: "full".to_string(),
                source: crate::callrecord::RecordingSource::Full,
                segment_id: "full".to_string(),
                seq: 1,
                label: "full".to_string(),
                started_at: chrono::Utc::now(),
                notify_app: false,
                unique_id: uuid::Uuid::new_v4().to_string(),
                discard: false,
            });
            debug!(session_id = %self.id, backend = "file", "auto recorder installed");
            return Ok(());
        }

        let sipflow_backend = self
            .server
            .sip_flow
            .as_ref()
            .and_then(|flow| flow.backend())
            .ok_or_else(|| anyhow!("SipFlow recording strategy has no backend"))?;
        let recorder = crate::media::media_recorder::SipflowRecorder::new(
            sipflow_backend,
            self.context.session_id.clone(),
        );
        self.media.recording.set_recorder(Box::new(recorder), None)
            .await?;
        debug!(session_id = %self.id, backend = "sipflow", "auto recorder installed");
        Ok(())
    }

    fn root_session_id_str(&self) -> String {
        self.meta
            .root_session_id
            .clone()
            .unwrap_or_else(|| self.context.session_id.clone())
    }

    fn recording_root_dir(&self) -> String {
        self.server
            .recording_policy
            .load()
            .as_ref()
            .as_ref()
            .map(|p| p.recorder_path())
            .unwrap_or_else(|| "recordings".to_string())
    }

    /// Resolve the file-name label for a recording segment: explicit
    /// `config.label` → resolved/known agent id → ivr name (session
    /// extensions) → `segment_type`. Extensions are populated by the routing
    /// layer (`ivr`) and the CC addon (`resolved_agent_id` / `agent_id`) as
    /// the call progresses, so the same session yields `main-ivr` during the
    /// IVR stage and `1001` once an agent answers.
    fn recording_label(&self, config_label: Option<&str>, segment_type: &str) -> String {
        config_label
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.session_ext_get("resolved_agent_id"))
            .or_else(|| self.session_ext_get("agent_id"))
            .or_else(|| self.session_ext_get("ivr"))
            .unwrap_or_else(|| segment_type.to_string())
    }

    /// Close out the active recording segment bookkeeping: move
    /// [`crate::callrecord::ActiveRecording`] state into a completed
    /// [`crate::callrecord::RecordingSegment`]. Returns `None` when the
    /// segment was flagged `discard` (e.g. an outbound pre-answer ringback
    /// slice on an answered call): the file is deleted, nothing is
    /// registered in the CDR, and the caller must suppress all recording
    /// events. Otherwise returns `(notify_app, unique_id)` — the
    /// recording-level identifier to carry on RWI `record_stopped` (and to
    /// reconcile with the later `recording_metadata_available`), plus
    /// whether the running CallApp should be notified.
    fn finalize_active_recording_segment(
        &mut self,
        result: &crate::media::media_recorder::RecordingResult,
    ) -> Option<(bool, Option<String>)> {
        let ended_at = chrono::Utc::now();
        let active = self.active_recording.take();
        if active.as_ref().is_some_and(|a| a.discard) {
            let path = std::path::Path::new(&result.path);
            if let Err(error) = std::fs::remove_file(path) {
                warn!(
                    session_id = %self.id,
                    path = %result.path,
                    %error,
                    "Failed to discard recording segment file"
                );
            }
            let _ = std::fs::remove_file(crate::callrecord::upload_failed_marker_path(path));
            return None;
        }
        let (segment_type, segment_id, seq, label, started_at, unique_id, notify_app) =
            if let Some(active) = active {
                (
                    active.segment_type,
                    active.segment_id,
                    active.seq,
                    active.label,
                    Some(active.started_at.to_rfc3339()),
                    Some(active.unique_id),
                    active.notify_app,
                )
            } else {
                (
                    "full".to_string(),
                    "full".to_string(),
                    0,
                    "full".to_string(),
                    None,
                    Some(uuid::Uuid::new_v4().to_string()),
                    false,
                )
            };
        self.completed_recording_segments
            .push(crate::callrecord::RecordingSegment {
                path: result.path.clone(),
                size: result.file_size,
                segment_type,
                segment_id,
                seq,
                label,
                started_at,
                ended_at: Some(ended_at.to_rfc3339()),
                duration_secs: result.duration_secs,
                unique_id: unique_id.clone(),
            });
        Some((notify_app, unique_id))
    }

    /// Put a leg on hold playing a file as hold music (looping).
    /// Ensure a conference exists — create it if missing.

    pub(super) async fn ensure_conference(&self, conf_id: &str, max: Option<usize>) -> Result<()> {
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id);
        if self
            .server
            .conference_server
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            info!(session_id = %self.id, conf_id = %conf_id, "Creating conference");
            self.server
                .conference_server
                .create_conference(conf_id_obj, max)
                .await
                .map_err(|e| anyhow!("Failed to create conference '{}': {}", conf_id, e))?;
        }
        Ok(())
    }

    /// Attach one participant to a mixer and retain its bridge on the local
    /// leg. Other participants in this session keep their own bridge handles.
    pub(super) async fn try_start_and_store_bridge(
        &mut self,
        conf_id: &str,
        leg: &LegId,
        label: &str,
    ) -> Result<()> {
        let prefix = format!("{}-", self.id);
        let local_leg = LegId::from(leg.as_str().strip_prefix(&prefix).unwrap_or(leg.as_str()));
        self.require_leg(&local_leg)?;
        let participant_leg = self.participant_leg(&local_leg);
        let current_room = self
            .server
            .conference_server
            .get_conference_id_for_leg(&participant_leg)
            .await;
        if let Some(room) = current_room.as_ref() {
            if room.0 == conf_id
                && self
                    .legs
                    .conference_bridge_handle(&local_leg)
                    .is_some_and(|handle| !handle.cancel_token.is_cancelled())
            {
                return Ok(());
            }
        }
        // Moving to another mixer must stop the old bridge and remove its
        // membership before registering this participant in the new room.
        drop(self.legs.remove_conference_bridge_handle(&local_leg));
        if let Some(room) = current_room {
            self.server
                .conference_server
                .remove_participant(&room, &participant_leg)
                .await?;
        }
        if self.conference_bridge.conf_id.is_none() {
            if let Some(mb) = self.bridge_mut() {
                mb.unbridge().await?;
            }
        }
        let handle = self
            .start_conference_media_bridge(conf_id, &participant_leg)
            .await
            .map_err(|error| {
                warn!(session_id = %self.id, %participant_leg, %error, "Failed to start {}", label);
                error
            })?;
        self.legs.set_conference_bridge_handle(local_leg, handle);
        self.conference_bridge.conf_id = Some(conf_id.to_string());
        info!(session_id = %self.id, leg_id = %participant_leg, "{} started", label);
        Ok(())
    }

    /// Start (or restart if already running) an application.
    pub(crate) async fn ensure_app_running(
        &self,
        kind: &str,
        params: Option<serde_json::Value>,
        label: &str,
    ) -> Result<()> {
        self.ensure_app_running_with(kind, params, true, label, None)
            .await
    }

    pub(crate) async fn ensure_app_running_with_route_context(
        &self,
        kind: &str,
        params: Option<serde_json::Value>,
        auto_answer: bool,
        label: &str,
        route_context: crate::call::app::AppRouteContext,
    ) -> Result<()> {
        self.ensure_app_running_with(kind, params, auto_answer, label, Some(route_context))
            .await
    }

    /// Like [`Self::ensure_app_running`] but with an explicit `auto_answer`.
    ///
    /// Shared by `ensure_app_running` and `start_queue_app`: the app runtime
    /// keeps the previously-started app registered (e.g. an IVR that handed
    /// control over via `AppAction::Transfer`) until an explicit `stop_app`,
    /// so `start_app` returns [`AppRuntimeError::AlreadyRunning`]. All app
    /// transitions recover by stopping the stale app and restarting instead of
    /// failing into dead air.
    async fn ensure_app_running_with(
        &self,
        kind: &str,
        params: Option<serde_json::Value>,
        auto_answer: bool,
        label: &str,
        route_context: Option<crate::call::app::AppRouteContext>,
    ) -> Result<()> {
        use crate::call::runtime::AppRuntimeError;
        let result = if let Some(context) = route_context.clone() {
            self.app_runtime
                .start_app_with_route_context(kind, params.clone(), auto_answer, context)
                .await
        } else {
            self.app_runtime
                .start_app(kind, params.clone(), auto_answer)
                .await
        };
        match result {
            Ok(()) => {
                // App now drives the session — suppress the RTP watchdog unless
                // a real callee is already bridged.
                self.sync_rtp_timeout_pause();
                Ok(())
            }
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
                let restarted = if let Some(context) = route_context {
                    self.app_runtime
                        .start_app_with_route_context(kind, params, auto_answer, context)
                        .await
                } else {
                    self.app_runtime.start_app(kind, params, auto_answer).await
                };
                restarted
                    .map(|()| self.sync_rtp_timeout_pause())
                    .map_err(|e| anyhow!("Failed to restart {}: {:?}", label, e))
            }
            Err(e) => Err(anyhow!("Failed to start {}: {:?}", label, e)),
        }
    }

    /// Map a REFER status code to a human-readable reason string.
    pub(super) fn refer_reason_for_status(status: u16) -> Option<&'static str> {
        match status {
            202 | 100..=199 => None,
            405 | 420 | 501 => Some("refer_not_supported"),
            _ if status >= 400 => Some("refer_rejected"),
            _ => Some("unexpected_response"),
        }
    }

    /// Unified constructor shared by [`new`] (UAS) and [`new_uac`] (UAC).
    fn new_inner(
        server: SipServerRef,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
        context: CallContext,
        mode: ConstructMode<'_>,
        use_media_proxy: bool,
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

        let cmd_capacity = server.proxy_config.load().session_cmd_channel_capacity;
        let (cmd_tx, cmd_rx) = mpsc::channel(cmd_capacity);
        let snapshot_cache: Arc<RwLock<Option<SessionSnapshot>>> = Arc::new(RwLock::new(None));
        let app_event_bridge = crate::proxy::proxy_call::state::AppEventBridge::new();

        let sip_handle = SipSessionHandle {
            session_id: session_id.clone(),
            cmd_tx: cmd_tx.clone(),
            snapshot_cache: snapshot_cache.clone(),
            app_event_bridge: app_event_bridge.clone(),
        };

        // Build ApplicationContext for call apps (IVR, voicemail, etc.).
        // UAS mode extracts SIP headers from the inbound INVITE; UAC mode has
        // no inbound request.
        let sip_headers = match mode {
            ConstructMode::Uas { server_dialog } => {
                let hdrs = crate::call::app::extract_sip_headers(&server_dialog.initial_request());
                if let Some(ref routed) = context.dialplan.routed_headers {
                    crate::call::app::merge_sip_headers(&hdrs, routed)
                } else {
                    hdrs
                }
            }
            ConstructMode::Uac => Default::default(),
        };
        // Resolve the root session id for the whole logical call.
        // UAS: an inbound INVITE re-attaches this leg to an existing root
        // session when it carries, in priority order:
        //   1. a SIP `Session-ID` header (RFC 7989) whose local-uuid is the
        //      upstream PBX/cluster global session id (B2BUA legs stamped by
        //      `session_id_for_outgoing_leg`);
        //   2. a CC User-to-User header (RFC 7433, purpose=call-center) —
        //      legacy channel, kept for mixed-version clusters.
        // Otherwise this session IS the root.
        // UAC (originate): this session is the root by definition.
        let root_session_id = match mode {
            ConstructMode::Uas { server_dialog } => {
                let request = server_dialog.initial_request();
                crate::call::session_id::extract_inherited(&request).or_else(|| {
                    crate::call::uui::extract_cc_uui(&request.headers).map(|uui| uui.session_id)
                })
            }
            ConstructMode::Uac => None,
        };

        let call_info = CallInfo {
            session_id: session_id_str.clone(),
            caller: original_caller.clone(),
            callee: original_callee.clone(),
            direction: context.dialplan.direction.to_string(),
            started_at: chrono::Utc::now(),
            sip_headers,
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
                proxy: (*server.proxy_config.load_full()).clone(),
                ..Default::default()
            }),
            server.http_client.clone(),
        );
        app_ctx.rwi_gateway = server.rwi_gateway.clone();
        app_ctx.ivr_trace = server.ivr_trace.clone();
        app_ctx.session_extensions = session_extensions.clone();

        let app_factory = Arc::new(BuiltinAppFactory::new(
            server.addon_registry.clone(),
            server.agent_registry.clone(),
        ));
        app_ctx.app_factory = Some(app_factory.clone());

        // Populate RWI CallMetaStore so events emitted from this session
        // (call_hangup, call_no_answer, etc.) are enriched with call context.
        if let Some(ref gw) = server.rwi_gateway {
            let meta = crate::rwi::proto::CallMeta {
                session_id: Some(
                    root_session_id
                        .clone()
                        .unwrap_or_else(|| session_id_str.clone()),
                ),
                caller: Some(original_caller.clone()),
                callee: Some(original_callee.clone()),
                caller_name: extract_sip_username(&original_caller),
                callee_name: extract_sip_username(&original_callee),
                direction: Some(context.dialplan.direction.to_string()),
                trunk: context
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("trunk").cloned()),
                // UAS: root = this session's own call context (root=self).
                // UAC: no cross-session propagation for originated legs.
                root: match mode {
                    ConstructMode::Uas { .. } => Some(crate::rwi::proto::RootCallInfo {
                        caller: Some(original_caller.clone()),
                        caller_name: extract_sip_username(&original_caller),
                        callee: Some(original_callee.clone()),
                        callee_name: extract_sip_username(&original_callee),
                        call_id: Some(session_id_str.clone()),
                        start_time: Some(context.created_at.clone()),
                    }),
                    ConstructMode::Uac => None,
                },
                ..Default::default()
            };
            gw.read().meta_store.insert(session_id_str.clone(), meta);
        }

        let app_runtime: Arc<dyn AppRuntime> = Arc::new(
            DefaultAppRuntime::new(AppRuntimeConfig {
                session_id: session_id_str.clone(),
                handle: sip_handle.clone(),
                context: Arc::new(app_ctx),
            })
            .with_factory(app_factory),
        );

        let mut meta = crate::proxy::proxy_call::call_meta::CallMeta::default();
        meta.root_session_id = root_session_id;
        meta.routed_caller = context.dialplan.caller.as_ref().map(|uri| uri.to_string());
        meta.routed_callee = context
            .dialplan
            .first_target()
            .map(|target| target.aor.to_string());

        // Caller offer SDP: UAS extracts from the inbound INVITE; UAC has none
        // yet (populated later when media is set up).
        let caller_offer = match mode {
            ConstructMode::Uas { server_dialog } => {
                Self::extract_sdp(server_dialog.initial_request().body())
            }
            ConstructMode::Uac => None,
        };

        // Caller dialog + leg wiring differs by mode.
        let (caller_dialog_field, caller_leg_dialog) = match mode {
            ConstructMode::Uas { server_dialog } => (
                Some(server_dialog.clone()),
                Some(rsipstack::dialog::dialog::Dialog::Invite(
                    server_dialog.clone(),
                )),
            ),
            ConstructMode::Uac => (None, None),
        };

        let session = Self {
            id: session_id.clone(),
            state: SessionState::Initializing,
            bridge: BridgeConfig::new(),
            media_profile: media_profile.clone(),
            app_runtime,
            snapshot_cache: snapshot_cache.clone(),
            server,
            caller_dialog: caller_dialog_field,
            callee_dialogs: Arc::new(DashMap::new()),
            legs: {
                use crate::proxy::proxy_call::leg_registry::LegRegistry;
                let mut lr = LegRegistry::new();
                let caller_id = LegId::from("caller");
                lr.add_leg(
                    caller_id.clone(),
                    Leg::new(caller_id),
                    caller_leg_dialog,
                );
                lr.insert(LegId::from("callee"), Leg::new(LegId::new("")));
                lr
            },
            pending_hangup: HashSet::new(),
            context,
            concurrent_call_lease,
            transient_leases: Vec::new(),
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
            answered_event_emitted: false,
            app_event_bridge: app_event_bridge.clone(),
            extensions: session_extensions,
            conference_bridge: crate::call::runtime::SessionConferenceBridge::new(),
            bridge_dtmf_tx: Arc::new(parking_lot::RwLock::new(None)),
            bridge_trace_context: Arc::new(parking_lot::Mutex::new(None)),
            bridge_dtmf_digits: Arc::new(parking_lot::Mutex::new(Vec::new())),
            realtime_bridge: None,
            voip_bridge: None,
            cmd_tx: Some(cmd_tx.clone()),
            handle: sip_handle.clone(),
            session_registry_guard: None,
            dtmf_digits: Vec::new(),
            active_plays: std::collections::HashMap::new(),
            live_transcription: None,
            active_recording: None,
            completed_recording_segments: Vec::new(),
            recording_seq: 0,
        };

        (session, sip_handle, cmd_rx)
    }

    pub fn new(
        server: SipServerRef,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
        context: CallContext,
        server_dialog: InviteDialog,
        use_media_proxy: bool,
    ) -> (Self, SipSessionHandle, mpsc::Receiver<CallCommand>) {
        Self::new_inner(
            server,
            cancel_token,
            call_record_sender,
            context,
            ConstructMode::Uas {
                server_dialog: &server_dialog,
            },
            use_media_proxy,
        )
    }

    /// Construct a SipSession in **UAC / outbound mode** (no inbound caller
    /// dialog). RWI originate prepares MediaBridge A after construction, uses
    /// its SDP for the first outbound INVITE, and attaches the answered dialog
    /// to that same leg. Later call.leg_add dialogs use the callee state channel.
    #[allow(clippy::too_many_arguments)]
    pub fn new_uac(
        server: SipServerRef,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
        context: CallContext,
        use_media_proxy: bool,
    ) -> (Self, SipSessionHandle, mpsc::Receiver<CallCommand>) {
        Self::new_inner(
            server,
            cancel_token,
            call_record_sender,
            context,
            ConstructMode::Uac,
            use_media_proxy,
        )
    }

    /// Resolve the effective ring/setup timeout for a call.
    ///
    /// Precedence: per-call/route/trunk `dialplan.max_ring_time`, then the
    /// live (hot-reloadable) global `ProxyConfig.max_ring_time`. `None` means
    /// the ring timeout is disabled (ring until answered or caller cancels).
    fn effective_ring_timeout(
        dialplan: &crate::call::Dialplan,
        server: &SipServerRef,
    ) -> Option<std::time::Duration> {
        dialplan.max_ring_time.or_else(|| {
            server
                .proxy_config
                .load()
                .max_ring_time
                .filter(|&secs| secs > 0)
                .map(std::time::Duration::from_secs)
        })
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
        let max_ring_time = Self::effective_ring_timeout(&context.dialplan, &server);

        let local_contact = server
            .contact_uri_for_transaction(tx)
            .or_else(|| server.default_contact_uri());

        let (state_tx, state_rx) = mpsc::unbounded_channel();

        let server_dialog = server
            .dialog_layer
            .get_or_create_server_invite(tx, state_tx, None, local_contact.clone())
            .map_err(|e| anyhow!("Failed to create server dialog: {}", e))?;

        let use_media_proxy = Self::check_media_proxy(&context, &context.dialplan.media.proxy_mode);

        let (mut session, handle, cmd_rx) = SipSession::new(
            server.clone(),
            cancel_token.clone(),
            call_record_sender,
            context,
            server_dialog.clone(),
            use_media_proxy,
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
            // Preserve the caller's raw original offer for hold/unhold
            // re-INVITE SDP (Chrome rejects PBX-rewritten re-offers).
            session.media.raw_caller_offer = Some(offer_sdp);
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

        // Publish this session's owning node in the cluster session registry
        // (no-op backend in single-node mode).
        session.register_in_session_registry().await;

        // Emit CallCreated event via RWI gateway if configured. The `direction`
        // field is injected by CallMetaStore enrichment (meta was inserted
        // above, before this event is dispatched).
        let incoming_sip_headers = {
            let hdrs = crate::call::app::extract_sip_headers(&server_dialog.initial_request());
            if let Some(ref routed) = session.context.dialplan.routed_headers {
                crate::call::app::merge_sip_headers(&hdrs, routed)
            } else {
                hdrs
            }
        };
        // The RWI app announces the call when it is ready, in its route context.
        if let Some(ref gw) = server.rwi_gateway
            && !matches!(&session.context.dialplan.flow,
                crate::call::DialplanFlow::Application { app_name, .. } if app_name == "rwi")
        {
            let ev = crate::rwi::CallCreated {
                call_id: session_id.clone(),
                context: "default".into(),
                caller: original_caller,
                callee: original_callee,
                trunk: None,
                sip_headers: incoming_sip_headers,
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
                // Incoming calls have no RWI owner yet. Notify subscribers so
                // a client can discover the call and explicitly attach to it.
                g.fan_out(&ev.context, &ev);
            });
        }

        let mut server_dialog_clone = server_dialog.clone();

        crate::utils::spawn(async move {
            session
                .process(state_rx, callee_state_rx, cmd_rx, dialog_guard)
                .await
        });

        // Backstop for the whole setup phase: ring timeout + 15s slack so the
        // session's own ring-timeout rejection (with no-answer tone) can fire
        // first. When no ring timeout is configured this backstop is disabled
        // (the call rings until answered or cancelled). No clamp is applied —
        // the configured value is honored verbatim.
        let max_setup_duration: Option<std::time::Duration> =
            max_ring_time.map(|d| d + Duration::from_secs(15));
        let mut timeout: futures::future::BoxFuture<'static, ()> = match max_setup_duration {
            Some(d) => tokio::time::sleep(d).boxed(),
            None => futures::future::pending().boxed(),
        };
        let mut session_cancelled = false;

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
                _ = cancel_token.cancelled(), if !session_cancelled => {
                    debug!(session_id = %session_id, "Call cancelled via token");
                    if matches!(
                        server_dialog_clone.state(),
                        DialogState::Terminated(_, TerminatedReason::UasDecline)
                    ) {
                        session_cancelled = true;
                        timeout = tokio::time::sleep(Self::CALLER_REJECTION_ACK_TIMEOUT).boxed();
                        continue;
                    }
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

    /// This node's identity for home-proxy self detection: the
    /// cluster-internal address resolved from `[cluster].peers` when
    /// available, otherwise the default contact URI — the same address the
    /// registrar stamps as `home_proxy` when cluster self resolution fails,
    /// so fallback-stamped registrations compare exactly.
    fn self_ident_addr(&self) -> Option<SipAddr> {
        self.server.cluster_self_addr.clone().or_else(|| {
            self.server
                .default_contact_uri()
                .and_then(|uri| SipAddr::try_from(uri).ok())
        })
    }

    /// Decide whether the callee leg must be routed to the registration's home
    /// node (`true`) or delivered locally (`false`).
    ///
    /// The self check compares the registration's home address against this
    /// node's deterministic identity: the cluster-internal address resolved
    /// from `[cluster].peers` when available, otherwise the default contact
    /// URI — the very address the registrar stamps as `home_proxy` when
    /// cluster self resolution fails. Live endpoint addresses must NOT be
    /// consulted here: `endpoint.get_addrs()` mixes in the remote addresses
    /// of active connections, so a peer node's home address — which this node
    /// may have connected to before — can wrongly look "local" and the
    /// INVITE gets pushed into a connection owned by another node and
    /// silently black-holed.
    fn route_via_home_proxy(
        target: &Location,
        cluster_enabled: bool,
        self_ident: Option<&SipAddr>,
    ) -> bool {
        if !cluster_enabled {
            return false;
        }
        let Some(home_proxy) = target.home_proxy.as_ref() else {
            return false;
        };
        match self_ident {
            Some(self_addr) => self_addr.addr.to_string() != home_proxy.addr.to_string(),
            // Unknown self identity: deliver through the home node, which is
            // authoritative for its own registrations.
            None => true,
        }
    }

    fn resolve_outbound_callee_uri(
        target: &Location,
        route_via_home_proxy: bool,
    ) -> rsipstack::sip::Uri {
        if route_via_home_proxy && let Some(registered_aor) = target.registered_aor.as_ref() {
            let mut uri = registered_aor.clone();
            if let Some(home_proxy) = target.home_proxy.as_ref() {
                uri.host_with_port = home_proxy.addr.clone();
                // The cluster hop uses the home proxy's transport, not the
                // endpoint transport carried by the registered AoR.
                uri.params
                    .retain(|param| !matches!(param, rsipstack::sip::Param::Transport(_)));
                if let Some(transport) = home_proxy.r#type {
                    uri.params.push(rsipstack::sip::Param::Transport(transport));
                }
            }
            return uri;
        }

        target.aor.clone()
    }

    /// True when media bypasses the local bridge entirely (pure B2BUA
    /// passthrough at the signaling layer).
    pub(super) fn bypasses_local_media(&self) -> bool {
        self.media_profile.path == MediaPathMode::Bypass && self.media_leg(&LegId::from("caller")).is_none()
    }

    /// Resolve the RTP inactivity timeout (dialplan override → proxy config).
    /// A proxy-level value of `0` explicitly disables the timeout.
    fn rtp_timeout_config(&self) -> Option<Duration> {
        self.context.dialplan.rtp_timeout.or_else(|| {
            self.server
                .proxy_config
                .load()
                .rtp_timeout
                .filter(|secs| *secs > 0)
                .map(Duration::from_secs)
        })
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
            let rtp_side = match side {
                crate::media::media_bridge::LegSide::A => {
                    crate::call::domain::RtpTimeoutSide::Caller
                }
                crate::media::media_bridge::LegSide::B => {
                    crate::call::domain::RtpTimeoutSide::Callee
                }
            };
            crate::utils::spawn(async move {
                if rx.await.is_ok() {
                    warn!(session_id = %sid, leg_side = ?side, "RTP inactivity timeout fired");
                    if let Some(tx) = cmd_tx {
                        let _ = tx.try_send(CallCommand::Hangup(
                            crate::call::domain::HangupCommand::all(None, None)
                                .with_rtp_timeout_side(rtp_side),
                        ));
                    }
                }
            });
        }
    }

    /// Spawn a monitor for fast-path relay arming failure. When the relay
    /// cannot be armed (e.g. a WebRTC leg's DTLS/SRTP transport never becomes
    /// ready), the bridge latch flips and the monitor sends
    /// `CallCommand::RelayArmFailure` so the session re-bridges in transcode
    /// mode instead of going silently dead.
    fn arm_relay_arm_failure_monitor(
        mb: &crate::media::media_bridge::MediaBridge,
        cmd_tx: Option<mpsc::Sender<CallCommand>>,
        session_id: &str,
    ) {
        let mut rx = mb.relay_arm_failed_rx();
        let cmd_tx = cmd_tx.clone();
        let sid = session_id.to_string();
        crate::utils::spawn(async move {
            loop {
                if *rx.borrow_and_update() {
                    break;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
            warn!(session_id = %sid, "fast-path relay arming failed");
            if let Some(tx) = cmd_tx {
                let _ = tx.try_send(CallCommand::RelayArmFailure);
            }
        });
    }

    /// Monitor the bridge's media-health snapshots: append a bounded number
    /// of `media_health` trace entries (periodic + at first stall) and fire
    /// `proxy.media_stalled` (CDR error chip + RWI `call_error`) once per
    /// leg that keeps receiving zero inbound RTP while the route is active —
    /// the mid-call signal for firewall / mis-advertised-address black
    /// holes that previously was only discoverable at hangup
    /// (`leg_media_incomplete`) or by reading logs.
    pub(crate) fn arm_media_health_monitor(
        mb: &crate::media::media_bridge::MediaBridge,
        cmd_tx: Option<mpsc::Sender<CallCommand>>,
        session_id: &str,
        stall_detect_secs: Option<u64>,
        trace_interval_secs: Option<u64>,
    ) {
        match stall_detect_secs {
            // `0` disables stall detection: route age can never reach
            // `Duration::MAX`, so no leg is ever flagged stalled.
            Some(0) => mb.set_stall_detect(std::time::Duration::MAX),
            Some(secs) => {
                mb.set_stall_detect(std::time::Duration::from_secs(secs));
            }
            None => {} // bridge default (15s)
        }
        let mut rx = mb.health_rx();
        let sid = session_id.to_string();
        let reported: Arc<parking_lot::Mutex<std::collections::HashSet<String>>> =
            Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new()));
        // Periodic snapshots: 5s sampler tick, publish every
        // `trace_interval_secs`, bounded to 20 entries per call so long
        // calls cannot bloat the CDR metadata.
        let every_ticks = match trace_interval_secs.unwrap_or(30) {
            0 => 0,
            secs => (secs as usize / 5).max(1),
        };
        let tick: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let periodic_emitted: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        const MAX_PERIODIC_TRACE_EVENTS: usize = 20;
        crate::utils::spawn(async move {
            loop {
                match rx.changed().await {
                    Ok(()) => {}
                    Err(_) => return, // bridge dropped — session is over
                }
                let Some(snapshot) = rx.borrow_and_update().clone() else {
                    continue;
                };
                let n = tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if every_ticks > 0
                    && n % every_ticks == 0
                    && periodic_emitted.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        < MAX_PERIODIC_TRACE_EVENTS
                {
                    if let Ok(detail) = serde_json::to_value(snapshot.as_ref()) {
                        if let Some(tx) = &cmd_tx {
                            let _ = tx.try_send(CallCommand::Trace {
                                event: crate::call_errors::TraceEvent::new(
                                    crate::call_errors::TraceKind::MediaHealth,
                                    format!("media health @ {:.0}s", snapshot.route_age_secs),
                                )
                                .detail(detail),
                            });
                        }
                    }
                }
                for side in snapshot.stalled_sides() {
                    if !reported.lock().insert(side.to_string()) {
                        continue; // once per leg per call
                    }
                    let detail = serde_json::to_value(snapshot.as_ref()).ok();
                    warn!(
                        session_id = %sid,
                        leg = side,
                        route_age_s = format!("{:.0}", snapshot.route_age_secs),
                        "media stalled: no inbound RTP on connected leg"
                    );
                    if let Some(tx) = &cmd_tx {
                        let _ = tx.try_send(CallCommand::ReportCallError {
                            stage: "proxy".to_string(),
                            app: "proxy".to_string(),
                            code: "proxy.media_stalled".to_string(),
                            severity: crate::call_errors::ErrSeverity::Warn,
                            message: format!("no inbound media on {side} leg"),
                            sip_status: None,
                            detail,
                        });
                    }
                }
            }
        });
    }

    /// Reconcile the RTP-inactivity watchdog suppression against the current
    /// session state. Apps (IVR / voicemail / queue / conference) keep the
    /// watchdog active so a caller that drops media without a BYE is still
    /// detected. The watchdog is suppressed (never fires) only while a blind
    /// transfer is in progress (new B-leg ringing / REFER pending); it is
    /// re-armed when the new leg answers or the bridge is established.
    ///
    /// Hold is handled separately via `pause_rtp_timeout` on the held leg, so
    /// it never clashes with this `set_app_paused` flag. A composed bridge also
    /// pauses its watchdog while its newly dialed peer is still ringing.
    pub(crate) fn sync_rtp_timeout_pause(&self) {
        let waiting_for_peer = self.bridge.active && self.bridge.legs.iter().any(|id|
            self.legs.get(id).is_some_and(|leg| leg.source_leg.is_some()
                && matches!(leg.state, LegState::Initializing | LegState::Ringing)));
        for id in self.legs.keys() {
            if let Some(peer) = self.media_leg(id) { peer.set_app_paused(self.meta.transfer_in_progress || waiting_for_peer); }
        }
    }

    /// Fast-path relay arming failed (e.g. a WebRTC leg's DTLS/SRTP transport
    /// never became ready). Fall back to transcoding so the call keeps media.
    async fn handle_relay_arm_failure(&mut self) -> Result<()> {
        let Some(mb) = self.media.bridge.as_mut() else {
            return Ok(());
        };
        mb.force_transcode().await
    }

    /// Human-readable label for the leg that stopped sending RTP: the session
    /// leg's display name + endpoint when available, falling back to the
    /// routing metadata (routed / connected URI), and finally to the running
    /// app name (IVR / voicemail / queue) for app-driven calls.
    fn rtp_timeout_leg_label(&self, side: crate::call::domain::RtpTimeoutSide) -> String {
        let leg_key = match side {
            crate::call::domain::RtpTimeoutSide::Caller => "caller",
            crate::call::domain::RtpTimeoutSide::Callee => "callee",
        };
        let leg = self.legs.get(&LegId::from(leg_key));
        let endpoint = leg.and_then(|l| l.endpoint.clone());
        let (name, endpoint) = match side {
            crate::call::domain::RtpTimeoutSide::Caller => {
                let name = self.meta.routed_caller.clone();
                let endpoint = endpoint
                    .or_else(|| self.meta.routed_caller.clone())
                    .or_else(|| Some(self.context.original_caller.clone()));
                (name, endpoint)
            }
            crate::call::domain::RtpTimeoutSide::Callee => {
                let name = self
                    .meta
                    .routed_callee
                    .clone()
                    .or_else(|| self.meta.connected_callee.clone());
                let endpoint = endpoint
                    .or_else(|| self.meta.connected_callee.clone())
                    .or_else(|| self.meta.routed_callee.clone());
                (name, endpoint)
            }
        };
        match (name, endpoint) {
            (Some(n), Some(e)) => format!("{} <{}>", n, e),
            (Some(n), None) => n,
            (None, Some(e)) => e,
            (None, None) => self
                .meta
                .app_name
                .clone()
                .map(|a| format!("app:{}", a))
                .unwrap_or_else(|| "unknown".to_string()),
        }
    }
    /// Adapter that makes an optional caller-dialog receiver behave like a
    /// real one in `run_main_loop`. When `None` (UAC mode) it parks forever so
    /// the corresponding `select!` arm never fires.
    async fn recv_opt_state(
        rx: &mut Option<mpsc::UnboundedReceiver<DialogState>>,
    ) -> Option<DialogState> {
        match rx {
            Some(r) => r.recv().await,
            None => std::future::pending::<Option<DialogState>>().await,
        }
    }

    pub(crate) async fn recv_recorder_finished(
        recording: &mut crate::media::media_recorder::RecordingSession,
    ) -> Option<crate::media::media_recorder::RecordingCompletion> {
        let result = recording.recv_recorder_finished().await;
        match result {
            Some(result) => Some(result),
            None => std::future::pending().await,
        }
    }

    /// Unified event loop shared by [`process`] (UAS) and [`process_uac`] (UAC).
    ///
    /// Drives hangup drain, cancellation, caller/callee dialog-state events,
    /// command dispatch, session-timer refresh, and max-call-duration. Exits
    /// when cancelled, all hangups drained, and all dialogs terminated.
    async fn run_main_loop(
        &mut self,
        mut state_rx: Option<mpsc::UnboundedReceiver<DialogState>>,
        mut callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut cmd_rx: mpsc::Receiver<CallCommand>,
    ) -> Result<()> {
        let hangup_futures = FuturesUnordered::new();
        let timeout = futures::future::pending::<()>().boxed();
        let mut cancelled = false;
        tokio::pin!(hangup_futures);
        tokio::pin!(timeout);

        let max_duration_sleep = {
            // Fallback of last resort: a dialplan without `max_call_duration`
            // must still have a hard ceiling, otherwise a call that loses both
            // its RTP watchdog (un-bridged media) and its peers can hang
            // forever. Dialplans normally carry 1h from `Dialplan::default`.
            const FALLBACK_MAX_CALL_DURATION: std::time::Duration =
                std::time::Duration::from_secs(3600);
            let max_dur = self
                .context
                .dialplan
                .max_call_duration
                .unwrap_or(FALLBACK_MAX_CALL_DURATION);
            debug!(session_id = %self.context.session_id, ?max_dur, "Max call duration timer armed");
            tokio::time::sleep(max_dur).boxed()
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
                && self
                    .caller_dialog
                    .as_ref()
                    .is_none_or(|d| d.state().is_terminated())
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
                    *timeout = tokio::time::sleep(Self::SHUTDOWN_DRAIN_TIMEOUT).boxed();
                    cancelled = true;
                }

                Some(state) = Self::recv_opt_state(&mut state_rx) => {
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

                Some(result) = Self::recv_recorder_finished(&mut self.media.recording) => {
                    match result {
                        Ok(Some(result)) => {
                            self.publish_recording_complete(result);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            warn!(session_id = %self.id, %error, "recording task failed");
                        }
                    }
                }

                _ = &mut timeout, if cancelled => {
                    break;
                }

                Some(expired) = self.timer_queue.next(), if !cancelled && !self.timer_queue.is_empty() => {
                    let scheduled = expired.into_inner();

                    match self.next_timer_action(&scheduled) {
                        Some(TimerAction::Refresh) => {
                            let refresh_ok = match if self.caller_dialog.is_some()
                                && scheduled == self.caller_dialog_id()
                            {
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

        Ok(())
    }

    pub async fn process(
        &mut self,
        state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        cmd_rx: mpsc::Receiver<CallCommand>,
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
        // `None` (max_ring_time disabled) rings indefinitely.
        let ring_timeout = Self::effective_ring_timeout(&self.context.dialplan, &self.server);
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
                _ = tokio::time::sleep(
                    ring_timeout.unwrap_or(std::time::Duration::from_secs(24 * 60 * 60))
                ), if ring_timeout.is_some() => Err(into_callee_err(
                    &StatusCode::RequestTimeout,
                    Some("Ring timeout".to_string()),
                )),
                _ = setup_cancel_token.cancelled() => Err(into_callee_err(
                    &StatusCode::RequestTerminated,
                    Some("Call cancelled during setup".to_string()),
                )),
            }
        };

        let mut setup_error = setup_result.err();

        if let Some((status_code, text, reason)) = setup_error.as_ref() {
            warn!(session_id = %self.context.session_id, ?status_code, ?text, ?reason, "Dialplan execution failed");

            let caller_cancelled = self.caller_dialog.as_ref().is_some_and(|dialog| {
                matches!(
                    dialog.state(),
                    DialogState::Terminated(_, TerminatedReason::UacCancel)
                )
            });

            if caller_cancelled {
                info!(
                    session_id = %self.context.session_id,
                    "Caller cancelled during setup; skipping rejection and failure tone"
                );
                self.meta.error_code =
                    Some(&crate::proxy::proxy_call::error_catalog::DIAL_CALLER_CANCELLED);
                self.meta.last_error = Some((
                    StatusCode::RequestTerminated,
                    Some("Caller cancelled".to_string()),
                ));
                self.meta.invite_final_status.get_or_insert(487);
                self.meta.hangup_reason = Some(CallRecordHangupReason::Canceled);
                self.cleanup().await;
                return Ok(());
            }
        }

        // Busy-wait (camp-on): on a 486 Busy from the callee, park the caller
        // with looping hold audio and re-dial the targets until one becomes
        // free or the wait budget expires.
        if setup_error
            .as_ref()
            .is_some_and(|(code, _, _)| *code == StatusCode::BusyHere.code())
            && let Some(plan) = self.context.dialplan.busy_wait.clone()
        {
            match self.busy_wait_and_redial(plan, &mut callee_state_rx).await {
                Ok(()) => {
                    info!(
                        session_id = %self.id,
                        session_id = %self.context.session_id,
                        "busy_wait succeeded; callee became free, connecting caller"
                    );
                    self.meta.error_code = None;
                    setup_error = None;
                }
                Err(err) => {
                    // The caller may have hung up while camping on the busy
                    // target — mirror the caller-cancelled path instead of
                    // rejecting a dead dialog.
                    let caller_gone = self
                        .caller_dialog
                        .as_ref()
                        .is_none_or(|d| d.state().is_terminated());
                    if caller_gone {
                        info!(
                            session_id = %self.context.session_id,
                            "Caller gone during busy_wait; skipping rejection"
                        );
                        self.meta.error_code =
                            Some(&crate::proxy::proxy_call::error_catalog::DIAL_CALLER_CANCELLED);
                        self.meta.last_error = Some((
                            StatusCode::RequestTerminated,
                            Some("Caller cancelled".to_string()),
                        ));
                        self.meta
                            .invite_final_status
                            .get_or_insert(StatusCode::RequestTerminated.code());
                        self.meta.hangup_reason = Some(CallRecordHangupReason::Canceled);
                        self.cleanup().await;
                        return Ok(());
                    }
                    setup_error = Some(err);
                }
            }
        }

        if let Some((status_code, text, reason)) = setup_error {
            if let Err(e) = self
                .reject_with_tone(status_code, text.clone(), reason.clone())
                .await
            {
                warn!(session_id = %self.context.session_id, error = %e, "Failed to send rejection with tone");
            }
            // Store error so cleanup/CDR can report the failure reason
            self.meta.last_error =
                Some((StatusCode::Other(status_code, text.clone()), reason.clone()));
            self.meta.invite_final_status.get_or_insert(status_code);
            self.meta.hangup_reason = Some(sip_status_to_hangup_reason(status_code));
            // Ensure cleanup runs (generates CDR) even on early failure
            self.cleanup().await;
            return Err(anyhow!("Dialplan failed: {} {:?}", status_code, reason));
        }

        self.run_main_loop(Some(state_rx), callee_state_rx, cmd_rx)
            .await?;

        self.cleanup().await;

        let _ = _cancel_guard;

        Ok(())
    }

    /// Run loop for a **UAC / outbound** session (RWI originate).
    ///
    /// Unlike `process`, this skips the inbound setup phase (no ringback,
    /// no early-media 183, no dialplan execution). The first outbound
    /// `ClientInviteDialog` is attached via [`attach_caller_dialog`]. This
    /// loop drives:
    ///   * first INVITE / caller dialog state events (`caller_state_rx`)
    ///   * call.leg_add dialog state events (`callee_state_rx`)
    ///   * command processing (`cmd_rx`)
    ///   * session-timer refresh / max-duration / hangup drain
    pub async fn process_uac(
        &mut self,
        caller_state_rx: mpsc::UnboundedReceiver<DialogState>,
        callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        cmd_rx: mpsc::Receiver<CallCommand>,
        _dialog_guard: ClientDialogGuard,
    ) -> Result<()> {
        let _cancel_guard = self.cancel_token.clone().drop_guard();

        self.run_main_loop(Some(caller_state_rx), callee_state_rx, cmd_rx)
            .await?;

        self.cleanup().await;
        let _ = _cancel_guard;
        Ok(())
    }

    /// Attach a callee `ClientInviteDialog` (B leg) to this UAC session after
    /// the outbound INVITE is answered. Registers the dialog in `callee_dialogs`
    /// and the leg registry so the command loop and media bridge can drive it.
    pub async fn attach_callee_dialog(&mut self, dialog: InviteDialog, callee_sdp: Option<String>) {
        let dialog_id = dialog.id();
        info!(session_id = %self.id, %dialog_id, "Attaching callee dialog to UAC session");

        self.callee_dialogs.insert(dialog_id.clone(), ());

        // Register the callee leg with a real dialog.
        let callee_id = LegId::from("callee");
        let dialog_enum = rsipstack::dialog::dialog::Dialog::Invite(dialog);
        self.legs.set_dialog(callee_id.clone(), dialog_enum);

        // Ensure the B leg exists and apply the callee's answer SDP.
        if let Some(sdp) = callee_sdp {
            // Detect the transport from the callee's SDP (DTLS fingerprint /
            // a=setup implies WebRTC, RTP/SAVP or a=crypto implies SDES-SRTP,
            // otherwise plain RTP) instead of assuming RTP — a WebRTC callee
            // (DTLS-SRTP) must build its MediaBridge leg with the matching
            // transport or SRTP negotiation will fail.
            let mut transport = crate::media::negotiate::detect_transport(&sdp);
            if transport == rustrtc::TransportMode::Srtp {
                // The outbound INVITE offer on originated legs is generated by
                // RtpTrackBuilder without `a=crypto`, so a SAVP+crypto answer
                // cannot be honored: the remote expects SRTP but never
                // received our SDES key. Keep the leg plain and surface the
                // config gap instead of producing one-way-protected media.
                warn!(session_id = %self.id,
                    "Callee answered with SDES-SRTP but the outbound offer was plain RTP; keeping plain RTP leg (enable SRTP for originated legs to fix)");
                transport = rustrtc::TransportMode::Rtp;
            }
            if let Err(e) = self
                .ensure_media_leg(LegSide::B, &sdp, transport)
                .await
            {
                warn!(session_id = %self.id, error = %e, "Failed to create callee MediaBridge leg");
                return;
            }
            {
                if let Some(leg) = self.media_leg(&callee_id) {
                    // UAC mode: the leg has no local offer yet (the INVITE SDP
                    // was generated before this attach). Generate an offer with
                    // the answer's codecs so set_remote_description(answer) has
                    // a matching local offer and the negotiated profile lands.
                    if leg.negotiated().is_none() {
                        if let Ok(offer) = leg.create_offer().await {
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

    /// Observe caller input independently of bridge membership. The application
    /// decides whether digits are currently accepted; RTP is never consumed here.
    /// Started once per caller peer; exits when its DTMF sender is dropped.
    fn spawn_dtmf_forwarder(&self) {
        let Some(peer) = self.media_leg(&LegId::from("caller")) else { return; };
        let mut rx = peer.subscribe_dtmf();
        let leg_id = peer.id().to_string();
        let app_runtime = self.app_runtime.clone();
        let rwi_gateway = self.server.rwi_gateway.clone();
        let bridge_dtmf_tx = self.bridge_dtmf_tx.clone();
        let bridge_trace_context = self.bridge_trace_context.clone();
        let bridge_dtmf_digits = self.bridge_dtmf_digits.clone();
        let original_caller = self.context.original_caller.clone();
        let original_callee = self.context.original_callee.clone();
        let session_id = self.context.session_id.clone();
        // App flows start asynchronously AFTER the answer (the IVR/queue step
        // provider may involve external HTTP roundtrips). Digits pressed in
        // that window used to be silently discarded (`app_runtime.is_running()`
        // false) — the classic "first DTMF lost". Buffer them (bounded, with
        // a TTL) and replay in order once the app is running. Sessions that
        // never run an app keep the previous drop-on-no-app behaviour.
        let app_expected = matches!(
            self.context.dialplan.flow,
            crate::call::DialplanFlow::Application { .. } | crate::call::DialplanFlow::Queue { .. }
        );
        crate::utils::spawn(async move {
            const MAX_PENDING: usize = 16;
            const PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(5);
            let mut pending: std::collections::VecDeque<(char, String, std::time::Instant)> =
                Default::default();
            let mut retry = tokio::time::interval(std::time::Duration::from_millis(50));
            retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut app_seen = app_runtime.is_running();
            loop {
                tokio::select! {
                    biased;
                    ev = rx.recv() => match ev {
                        Ok(ev) => {
                            if ev.direction != crate::media::ingress_tap::PacketDirection::Ingress { continue; }
                            app_seen |= app_runtime.is_running();
                            let leg_id = leg_id.clone();
                            // Same SIP-header source as the executor's own
                            // trace entries (invocation → call-info fallback);
                            // routing metadata is NOT the INVITE header map.
                            let sip_headers =
                                super::util::trace_sip_headers(&app_runtime).await;
                            let injected = forward_dtmf_event(
                                ev.digit,
                                &leg_id,
                                &session_id,
                                &app_runtime,
                                &rwi_gateway,
                                &bridge_dtmf_tx,
                                &bridge_trace_context,
                                &bridge_dtmf_digits,
                                &original_caller,
                                &original_callee,
                                sip_headers,
                            );
                            if !injected && app_expected && !app_seen {
                                pending.push_back((ev.digit, leg_id, std::time::Instant::now()));
                                while pending.len() > MAX_PENDING {
                                    pending.pop_front();
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = retry.tick(), if !pending.is_empty() => {
                        // Drop entries that waited past the TTL (app never came
                        // up, e.g. a failed start) so the ticker stops.
                        let now = std::time::Instant::now();
                        while pending
                            .front()
                            .is_some_and(|(_, _, at)| now.duration_since(*at) > PENDING_TTL)
                        {
                            pending.pop_front();
                        }
                        app_seen |= app_runtime.is_running();
                        while app_runtime.is_running()
                            && let Some((digit, leg_id, _)) = pending.pop_front()
                        {
                            inject_dtmf_into_app(
                                digit,
                                &leg_id,
                                &session_id,
                                &app_runtime,
                                &rwi_gateway,
                            );
                        }
                    }
                }
            }
        });
    }

    /// Ensure the proxy role has an independent media peer, using the codecs
    /// negotiated in `sdp` if needed. Idempotent.
    async fn ensure_media_leg(
        &mut self,
        side: LegSide,
        sdp: &str,
        transport: rustrtc::TransportMode,
    ) -> Result<()> {
        let codecs = crate::media::negotiate::MediaNegotiator::extract_codec_params(sdp).audio;
        // Answerer legs need a video transceiver whenever the remote offer
        // carries video (rustrtc's answer builder requires a transceiver per
        // remote section), so the caps are always derived from `sdp`. The video
        // policy (strip) is enforced when the answer SDP is assembled.
        let video_codecs = self.video_caps_from_sdp(sdp);
        let cfg = self.build_leg_config(transport, codecs, video_codecs);
        let leg_name = match side { LegSide::A => "caller", LegSide::B => "callee" };
        if self.media_leg(&LegId::from(leg_name)).is_some() {
            return Ok(());
        }
        let recorder_sender = if side == LegSide::A {
            self.setup_recording_capture()?
        } else {
            None
        };
        let leg = crate::media::leg::LegInner::new(leg_name, &cfg, recorder_sender)?;
        self.legs.set_media_leg(&LegId::from(leg_name), leg);
        if side == LegSide::A { self.spawn_dtmf_forwarder(); }
        Ok(())
    }

    /// Create the one real media connection used by an RWI-originated call.
    /// Its offer is sent in the first outbound INVITE and the resulting answer
    /// must be applied back to this same A leg.
    pub async fn prepare_originate_caller_leg(
        &mut self,
        codecs: Vec<crate::media::CodecInfo>,
    ) -> Result<String> {
        if codecs.is_empty() {
            return Err(anyhow!("No codecs configured for originate caller leg"));
        }
        let has_a = self.media_leg(&LegId::from("caller"))
            .is_some();
        if has_a {
            return Err(anyhow!("Originate caller MediaBridge A leg already exists"));
        }

        let cfg = self.build_leg_config(rustrtc::TransportMode::Rtp, codecs, Vec::new());
        let recorder_sender = self.setup_recording_capture()?;
        let leg_label = "caller";
        let leg = crate::media::leg::LegInner::new(leg_label, &cfg, recorder_sender)?;
        let offer = leg.create_offer().await?;
        if offer.is_empty() {
            return Err(anyhow!("Originate caller SDP offer is empty"));
        }
        self.legs.set_media_leg(&LegId::from("caller"), leg);
        self.spawn_dtmf_forwarder();
        self.media.caller_offer = Some(offer.clone());
        Ok(offer)
    }

    /// Attach the primary caller (A leg) dialog to the session. Used in UAC
    /// mode (RWI originate) after the first outbound INVITE is answered.
    /// MediaBridge A must already exist and must be the source of that INVITE's
    /// offer; this method never creates a replacement or fallback media leg.
    pub async fn attach_caller_dialog(
        &mut self,
        dialog: InviteDialog,
        caller_sdp: Option<String>,
    ) -> Result<()> {
        let dialog_id = dialog.id();
        info!(session_id = %self.id, %dialog_id, "Attaching caller dialog to session");

        let Some(answer) = caller_sdp.as_deref() else {
            let _ = dialog.hangup().await;
            return Err(anyhow!("Answered originate INVITE has no SDP answer"));
        };
        let Some(leg) = self.media_leg(&LegId::from("caller"))
        else {
            let _ = dialog.hangup().await;
            return Err(anyhow!("Originate caller MediaBridge A leg is missing"));
        };
        // A provisional response may already have applied a `Pranswer` to
        // this leg. The final response must still apply its SDP
        // as `Answer` to promote the same peer connection; do not replace or
        // reject the leg merely because early media negotiated it first.
        if let Err(e) = leg.apply_sdp(answer, rustrtc::SdpType::Answer).await {
            // The SIP dialog is already confirmed. If media negotiation cannot
            // complete, terminate it here rather than relying on Drop (which
            // cannot await a BYE transaction).
            let _ = dialog.hangup().await;
            return Err(e);
        }
        leg.accept();

        self.caller_dialog = Some(dialog.clone());
        let caller_id = LegId::from("caller");
        let dialog_enum = rsipstack::dialog::dialog::Dialog::Invite(dialog);
        self.legs.set_dialog(caller_id.clone(), dialog_enum);

        let auto_start_on_answer = {
            let recording = &self.context.dialplan.recording;
            recording.enabled && recording.auto_start
        };
        if auto_start_on_answer && let Err(error) = self.set_auto_recorder().await {
            warn!(session_id = %self.id, %error, "Auto recorder installation at final answer failed");
        }
        self.maybe_autostart_live_transcription("originate_answer")
            .await;
        // The caller dialog is already answered when it is attached. Its
        // Confirmed state may have been consumed by the originate setup loop
        // or may still be queued for process_uac, so mark it Connected here;
        // handling a queued Confirmed state again is idempotent. Subsequent
        // re-INVITE/BYE states are handled by the normal caller-state branch.
        self.update_leg_state(&LegId::from("caller"), LegState::Connected);
        info!(session_id = %self.id, "UAC caller leg marked Connected after attaching caller dialog");
        Ok(())
    }

    fn next_timer_action(&mut self, scheduled: &DialogId) -> Option<TimerAction> {
        self.timer_keys.remove(scheduled);
        let timer = self.timers.get_mut(scheduled)?;

        if timer.is_expired() {
            return Some(TimerAction::Expired);
        }

        if timer.should_we_refresh() && timer.should_refresh() && timer.start_refresh() {
            return Some(TimerAction::Refresh);
        }

        None
    }

    /// Read a string from the session-extensions `HashMap` bag.
    pub(crate) fn session_ext_get(&self, key: &str) -> Option<String> {
        self.extensions
            .read()
            .get::<std::collections::HashMap<String, String>>()
            .and_then(|m| m.get(key).cloned())
            .filter(|s| !s.is_empty())
    }

    /// Write a string into the session-extensions `HashMap` bag (creates the
    /// map if missing). Empty values are ignored.
    pub(crate) fn session_ext_set(&self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if value.is_empty() {
            return;
        }
        let mut ext = self.extensions.write();
        if let Some(existing) = ext.get_mut::<std::collections::HashMap<String, String>>() {
            existing.insert(key.to_string(), value);
        } else {
            let mut m = std::collections::HashMap::new();
            m.insert(key.to_string(), value);
            ext.insert(m);
        }
    }

    /// Resolve the leg that owns the given dialog (Display form). Checks the
    /// caller dialog first, then the leg registry, then the registered callee
    /// dialogs. Used by the inbound-REFER blind-transfer path to identify the
    /// transferor leg; the literal `callee` answer resolves to the connected
    /// agent leg via `resolve_transfer_leg` at transfer time.
    pub(crate) fn leg_id_for_dialog(&self, dialog_id: &str) -> Option<LegId> {
        if self
            .caller_dialog
            .as_ref()
            .is_some_and(|d| d.id().to_string() == dialog_id)
        {
            return Some(LegId::from("caller"));
        }
        for (leg_id, _) in self.legs.iter() {
            if self
                .legs
                .get_dialog(leg_id)
                .is_some_and(|d| d.id().to_string() == dialog_id)
            {
                return Some(leg_id.clone());
            }
        }
        if self
            .callee_dialogs
            .iter()
            .any(|entry| entry.key().to_string() == dialog_id)
        {
            return Some(LegId::from("callee"));
        }
        None
    }

    /// Publish CC agent attribution into the RWI `CallMetaStore` so every
    /// `call_*` event dispatched from this session is enriched with
    /// `agent_id` / `agent_name` / `queue_id` (same flat-merge mechanism as
    /// `direction`). This replaces the addon-emitted `cc_ringing` /
    /// `cc_answered` / `cc_hangup` / `cc_held` / `cc_unheld` events.
    ///
    /// Sources:
    /// - `agent_id` / `agent_name` — session extensions, written by the CC
    ///   session hook (`publish_agent_context`) during the ringing /
    ///   connected / ended lifecycle callbacks. Call this only AFTER the
    ///   hooks have fired.
    /// - `queue_id` — the effective queue name from the session call meta.
    ///
    /// Existing meta fields are never cleared: a call that stops resolving an
    /// agent (e.g. sequential fallback to a non-agent) keeps its earlier
    /// attribution.
    ///
    /// When the meta entry has already been REMOVED (a queue→agent dispatch
    /// transfers the caller away and the transfer completion releases the
    /// original session's gateway state — including its CallMetaStore entry —
    /// before this session's final `call_hangup` is emitted), it is REBUILT
    /// from the live session context so the last lifecycle events keep their
    /// flat enrichment (caller/callee/direction + agent attribution).
    fn sync_agent_context_to_rwi_meta(&self) {
        let Some(ref gw) = self.server.rwi_gateway else {
            return;
        };
        let agent_id = self.session_ext_get("agent_id");
        let agent_name = self.session_ext_get("agent_name");
        let queue_id = crate::proxy::proxy_call::call_meta::effective_queue_name(&self.meta);
        if agent_id.is_none() && agent_name.is_none() && queue_id.is_none() {
            return;
        }
        let session_id = self.context.session_id.clone();
        let gw = gw.read();
        let mut meta = gw.meta_store.get_sync(&session_id).unwrap_or_else(|| {
            // Rebuild from the live session (mirror of the constructor's
            // initial CallMeta population).
            crate::rwi::proto::CallMeta {
                session_id: Some(
                    self.meta
                        .root_session_id
                        .clone()
                        .unwrap_or_else(|| session_id.clone()),
                ),
                caller: Some(self.context.original_caller.clone()),
                callee: Some(self.context.original_callee.clone()),
                caller_name: extract_sip_username(&self.context.original_caller),
                callee_name: extract_sip_username(&self.context.original_callee),
                direction: Some(self.context.dialplan.direction.to_string()),
                trunk: self
                    .context
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("trunk").cloned()),
                agent_id: agent_id.clone(),
                agent_name: agent_name.clone(),
                queue_id: queue_id.clone(),
                root: Some(crate::rwi::proto::RootCallInfo {
                    caller: Some(self.context.original_caller.clone()),
                    caller_name: extract_sip_username(&self.context.original_caller),
                    callee: Some(self.context.original_callee.clone()),
                    callee_name: extract_sip_username(&self.context.original_callee),
                    call_id: Some(
                        self.meta
                            .root_session_id
                            .clone()
                            .unwrap_or_else(|| session_id.clone()),
                    ),
                    start_time: Some(self.context.created_at.clone()),
                }),
                ..Default::default()
            }
        });
        if meta.agent_id.is_none() {
            meta.agent_id = agent_id;
        }
        if meta.agent_name.is_none() {
            meta.agent_name = agent_name;
        }
        if meta.queue_id.is_none() {
            meta.queue_id = queue_id;
        }
        gw.meta_store.insert(session_id, meta);
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
            root_session_id: self.meta.root_session_id.clone(),
            caller: self.context.original_caller.clone(),
            callee: self.context.original_callee.clone(),
            connected_callee: self.meta.connected_callee.clone(),
            queue_name: crate::proxy::proxy_call::call_meta::effective_queue_name(&self.meta),
            skill_group_id: crate::proxy::proxy_call::call_meta::effective_skill_group_id(
                &self.meta,
            ),
            transferred: self.meta.transferred,
            direction: self.context.dialplan.direction.to_string(),
            started_at: Some(self.context.created_at.clone()),
            extensions: self.extensions.clone(),
        }
    }

    /// Fire the `on_call_ringing` session hooks for this session.
    ///
    /// Used by paths that dial a leg outside the regular `dial_sequential`
    /// flow (notably the RWI originate setup loop in `rwi/processor.rs`),
    /// so a ringing provisional (180, or 183/180-with-SDP early media when
    /// `early_media` is set) reaches addons — the CC addon resolves and
    /// publishes the agent attribution and transitions the agent Idle →
    /// Ringing.
    pub(crate) async fn fire_on_call_ringing_hooks(&self, early_media: bool) {
        if self.server.session_hooks.is_empty() {
            return;
        }
        let ctx = self.session_hook_ctx();
        for hook in self.server.session_hooks.iter() {
            hook.on_call_ringing(&ctx, early_media).await;
        }
        // Hooks may have resolved + published the agent attribution — make it
        // visible to event enrichment before the caller emits `call_ringing`.
        self.sync_agent_context_to_rwi_meta();
    }

    /// Fire the `on_call_connected` session hooks for this session.
    ///
    /// Used by the RWI originate setup loop after the first outbound INVITE
    /// is confirmed (200 OK) and attached as the caller dialog, so addons
    /// learn the call connected — the CC addon transitions the agent
    /// Ringing/Idle → Busy.
    pub(crate) async fn fire_on_call_connected_hooks(&self) {
        if self.server.session_hooks.is_empty() {
            return;
        }
        let ctx = self.session_hook_ctx();
        for hook in self.server.session_hooks.iter() {
            hook.on_call_connected(&ctx).await;
        }
        self.sync_agent_context_to_rwi_meta();
    }

    /// Fire the `on_call_ended` session hooks for this session.
    ///
    /// Used by the RWI originate setup loop when the first outbound INVITE
    /// FAILS (rejected / no-answer / timeout / media setup failure) — the
    /// UAC session loop never starts on those paths, so without this the
    /// CC addon would never learn the call ended (agent stuck in Ringing).
    pub(crate) async fn fire_on_call_ended_hooks(
        &self,
        reason: Option<&crate::callrecord::CallRecordHangupReason>,
        duration_secs: u64,
    ) {
        if self.server.session_hooks.is_empty() {
            return;
        }
        let ctx = self.session_hook_ctx();
        for hook in self.server.session_hooks.iter() {
            hook.on_call_ended(&ctx, reason, duration_secs).await;
        }
        self.sync_agent_context_to_rwi_meta();
    }

    fn ok_or_failure<T>(result: anyhow::Result<T>) -> CommandResult {
        match result {
            Ok(_) => CommandResult::success(),
            Err(e) => CommandResult::failure(e.to_string()),
        }
    }

    /// Extract the user-part of a SIP URI string (`sip:bob@host` → `bob`).
    /// Returns `None` when the URI has no usable user part.
    fn uri_user_part(uri: &str) -> Option<String> {
        uri.strip_prefix("sip:")
            .or_else(|| uri.strip_prefix("sips:"))
            .and_then(|s| s.split('@').next())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// True while a queue app is driving this session — used to gate
    /// queue-specific extension bookkeeping.
    fn in_queue_context(&self) -> bool {
        self.app_runtime
            .current_app()
            .as_deref()
            .is_some_and(|app| app == "queue")
            || crate::proxy::proxy_call::call_meta::has_queue_name(&self.meta)
    }

    /// Resolve the agent id for a queue agent-leg event.
    ///
    /// Prefers the user-part of the leg's endpoint URI **when the agent
    /// registry confirms it is a registered agent** — sequential fallback
    /// dials a different agent than the session-level `resolved_agent_id`,
    /// which stays pinned to the first resolved agent. Legs dialed through
    /// WebRTC contacts carry temp user parts that are NOT agent ids, so an
    /// unvalidated user-part (or a missing registry) falls back to the
    /// session-level value.
    async fn leg_agent_id(&self, agent_uri: Option<&str>) -> Option<String> {
        let session_level = self.session_ext_get("resolved_agent_id");
        let user = agent_uri
            .and_then(Self::uri_user_part)
            .filter(|u| Some(u.as_str()) != session_level.as_deref());
        match (user, &self.server.agent_registry) {
            (Some(user), Some(registry)) => {
                if registry.get_agent(&user).await.is_some() {
                    Some(user)
                } else {
                    session_level
                }
            }
            _ => session_level,
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

        // Bypass relay assumes both legs negotiated the same transport style.
        // That does not hold when the legs were negotiated differently (e.g.
        // the initial INVITE was regenerated as WebRTC for a WS callee while
        // the caller's session is RTP): forwarding the offer verbatim sends
        // the peer a style it must reject with 488. Regenerate the offer in
        // the target leg's own style, translating the requested direction.
        let target_leg = match target_side {
            DialogSide::Callee => LegId::from("callee"),
            DialogSide::Caller => LegId::from("caller"),
        };
        let offer_body = if method == rsipstack::sip::Method::Invite
            && Self::offer_is_webrtc(offer_sdp)
                != self
                    .sdp_for_side_with_direction(&target_leg, "sendrecv")
                    .map(|ref sdp| Self::offer_is_webrtc(sdp))
                    .unwrap_or(Self::offer_is_webrtc(offer_sdp))
        {
            let direction = Self::cross_leg_direction(offer_sdp);
            info!(
                session_id = %self.id,
                offer_webrtc = Self::offer_is_webrtc(offer_sdp),
                %direction,
                "re-INVITE offer style mismatch across legs; regenerating in target leg style"
            );
            self.sdp_for_side_with_direction(&target_leg, direction)?
        } else {
            offer_sdp.to_string()
        };

        let response = self
            .send_mid_dialog_request_to_side(
                target_side,
                method,
                headers,
                Some(offer_body.as_bytes().to_vec()),
            )
            .await?
            .ok_or_else(|| anyhow!("{} timed out", method))?;

        let status = response.status_code.clone();
        let answer_sdp = Self::extract_sdp(response.body());

        // Persist the relayed negotiation on success, mirroring what the
        // initial INVITE flow stores. Without this, session-timer refreshes
        // re-INVITE each leg with the ORIGINAL offer/answer — silently
        // un-holding a leg whose direction was rewritten by a relayed hold.
        if status.kind() == rsipstack::sip::status_code::StatusCodeKind::Successful {
            match target_side {
                DialogSide::Callee => {
                    self.media.callee_offer = Some(offer_body);
                    if let Some(ref answer) = answer_sdp {
                        self.media.callee_answer_sdp = Some(answer.clone());
                    }
                }
                DialogSide::Caller => {
                    self.media.caller_offer = Some(offer_body);
                    if let Some(ref answer) = answer_sdp {
                        self.media.answer = Some(answer.clone());
                    }
                }
            }
        }

        Ok((status, answer_sdp))
    }

    /// Negotiate a video m-line with the opposite anchored leg before
    /// answering a re-INVITE that adds video. The outgoing SDP is generated by
    /// that leg's own PeerConnection, so it advertises RustPBX's transport
    /// addresses rather than leaking the source endpoint's addresses through
    /// the B2BUA.
    async fn negotiate_added_video_with_peer(
        &mut self,
        source_side: DialogSide,
        method: rsipstack::sip::Method,
        offered_caps: &[rustrtc::VideoCapability],
    ) -> Result<Vec<rustrtc::VideoCapability>> {
        if offered_caps.is_empty() {
            return Ok(Vec::new());
        }
        let target_side = match source_side {
            DialogSide::Caller => DialogSide::Callee,
            DialogSide::Callee => DialogSide::Caller,
        };
        let target_bridge_side = match target_side {
            DialogSide::Caller => LegSide::A,
            DialogSide::Callee => LegSide::B,
        };
        let target_leg = self
            .media
            .bridge
            .as_ref()
            .and_then(|bridge| bridge.leg(target_bridge_side))
            .ok_or_else(|| anyhow!("opposite media leg is unavailable for video re-INVITE"))?;

        // Only a BUNDLE destination needs globally unique audio/video payload
        // types. Plain RTP has separate audio and video sockets, so preserving
        // an offered PT such as 96 on both m-lines is valid and avoids a
        // needless PT change during a SIP re-INVITE.
        let mut target_video_caps = offered_caps.to_vec();
        if target_leg.pc().config().transport_mode == rustrtc::TransportMode::WebRtc {
            let occupied_audio_payload_types = target_leg
                .pc()
                .config()
                .media_capabilities
                .as_ref()
                .into_iter()
                .flat_map(|capabilities| capabilities.audio.iter())
                .map(|capability| capability.payload_type);
            MediaNegotiator::remap_bundle_video_payload_types(
                &mut target_video_caps,
                occupied_audio_payload_types,
            )?;
        }
        let source_target_video_caps: Vec<_> = offered_caps
            .iter()
            .cloned()
            .zip(target_video_caps.iter().cloned())
            .collect();

        crate::media::leg::ensure_video_sender_for_pc(target_leg.pc(), &target_video_caps[0])?;
        if let Some(transceiver) = target_leg
            .pc()
            .get_transceivers()
            .into_iter()
            .find(|transceiver| transceiver.kind() == rustrtc::MediaKind::Video)
        {
            transceiver.set_direction(rustrtc::TransceiverDirection::SendRecv);
        }
        let generated_offer = target_leg.prepare_offer().await?;
        let mut target_offer_sdp = MediaNegotiator::rewrite_video_capabilities(
            rustrtc::SdpType::Offer,
            &generated_offer.to_sdp_string(),
            &target_video_caps,
        )
        .map_err(|error| anyhow!("failed to build opposite-leg video offer: {error}"))?;

        // A video-only change must not reorder or expand the already selected
        // audio codec. Keep the target leg's current audio and DTMF entries.
        if let Some(profile) = target_leg.negotiated() {
            let mut selected_audio = Vec::new();
            if let Some(audio) = profile.audio {
                selected_audio.push(audio.to_codec_info());
            }
            if let Some(dtmf) = profile.dtmf {
                selected_audio.push(dtmf.to_codec_info());
            }
            if !selected_audio.is_empty()
                && let Some(rewritten) =
                    crate::media::negotiate::MediaNegotiator::rewrite_sdp_codec_list(
                        &target_offer_sdp,
                        &selected_audio,
                    )
            {
                target_offer_sdp = rewritten;
            }
        }

        let target_offer = Self::parse_sdp(
            rustrtc::SdpType::Offer,
            &target_offer_sdp,
            "opposite-leg video re-INVITE offer",
        )?;
        let offered_codec_names: Vec<_> = offered_caps
            .iter()
            .map(|cap| cap.codec_name.as_str())
            .collect();
        info!(
            session_id = %self.id,
            source_side = ?source_side,
            target_side = ?target_side,
            codecs = ?offered_codec_names,
            "propagating added video to opposite anchored leg"
        );
        let response = self
            .send_mid_dialog_request_to_side(
                target_side,
                method,
                Self::sdp_headers(),
                Some(target_offer_sdp.as_bytes().to_vec()),
            )
            .await?
            .ok_or_else(|| anyhow!("opposite-leg video re-INVITE timed out"))?;

        if response.status_code.kind() != rsipstack::sip::status_code::StatusCodeKind::Successful {
            warn!(
                session_id = %self.id,
                side = ?target_side,
                status = %response.status_code,
                "opposite leg rejected video re-INVITE"
            );
            return Ok(Vec::new());
        }
        let peer_answer_sdp = Self::extract_sdp(response.body())
            .ok_or_else(|| anyhow!("opposite-leg video re-INVITE returned no SDP"))?;
        // The peer answers the target-leg offer and therefore echoes the
        // target-leg PT. Return the paired source capability when building the
        // source-leg answer, so a non-BUNDLE PT such as 96 remains 96 even when
        // the WebRTC target had to advertise it as 97.
        let peer_accepted_video = MediaNegotiator::extract_video_codecs(&peer_answer_sdp);
        let accepted_caps: Vec<_> = source_target_video_caps
            .iter()
            .filter(|(_, target_cap)| {
                peer_accepted_video.iter().any(|accepted_cap| {
                    accepted_cap.payload_type == target_cap.payload_type
                        && accepted_cap
                            .name
                            .eq_ignore_ascii_case(&target_cap.codec_name)
                        && accepted_cap.clock_rate == target_cap.clock_rate
                })
            })
            .map(|(source_cap, _)| source_cap.clone())
            .collect();
        let accepted_codec_names: Vec<_> = accepted_caps
            .iter()
            .map(|cap| cap.codec_name.as_str())
            .collect();
        info!(
            session_id = %self.id,
            target_side = ?target_side,
            codecs = ?accepted_codec_names,
            "opposite anchored leg answered added-video negotiation"
        );

        // Commit the prepared local offer only after the peer accepted it. A
        // rejection therefore leaves rustrtc in Stable instead of requiring a
        // rollback (which it does not support).
        target_leg
            .pc()
            .set_local_description(target_offer)
            .map_err(|error| anyhow!("failed to commit opposite-leg video offer: {error}"))?;
        target_leg
            .apply_sdp(&peer_answer_sdp, rustrtc::SdpType::Answer)
            .await?;
        target_leg.refresh_observer();

        let target_leg_id = match target_side {
            DialogSide::Caller => LegId::from("caller"),
            DialogSide::Callee => LegId::from("callee"),
        };
        self.legs
            .set_video_state(&target_leg_id, !accepted_caps.is_empty());
        match target_side {
            DialogSide::Caller => {
                self.media.caller_offer = Some(peer_answer_sdp);
                self.media.answer = Some(target_offer_sdp);
            }
            DialogSide::Callee => {
                self.media.callee_offer = Some(target_offer_sdp);
                self.media.callee_answer_sdp = Some(peer_answer_sdp);
            }
        }

        Ok(accepted_caps)
    }

    fn parse_sdp(
        sdp_type: rustrtc::SdpType,
        sdp: &str,
        context: &str,
    ) -> anyhow::Result<rustrtc::SessionDescription> {
        rustrtc::SessionDescription::parse(sdp_type, sdp)
            .map_err(|e| anyhow::anyhow!("Failed to parse {} SDP: {}", context, e))
    }

    /// RFC 7989 Session-ID local-uuid to stamp on outgoing legs.
    ///
    /// Prefers the inherited root session id (one stable id across the whole
    /// logical call — cluster peers and downstream legs re-attach to it),
    /// falling back to this session's own id when it is the root. `None` for
    /// P2P flows (`session_id_enabled == false`) or non-UUID-shaped legacy
    /// ids, which keep relying on the UUI (RFC 7433) correlation channel.
    fn session_id_for_outgoing_leg(&self) -> Option<String> {
        if !self.context.dialplan.session_id_enabled {
            return None;
        }
        let candidate = self
            .meta
            .root_session_id
            .as_deref()
            .or(self.context.dialplan.session_id.as_deref())
            .unwrap_or(&self.context.session_id);
        crate::call::session_id::normalize(candidate)
    }

    async fn build_target_invite_option(
        &mut self,
        target: &crate::call::Location,
        leg_id_override: Option<&str>,
        transfer_headers: Option<&HashMap<String, String>>,
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
        let validated_transfer_headers = transfer_headers
            .map(Self::validate_transfer_headers)
            .transpose()
            .map_err(|reason| {
                warn!(
                    session_id = %self.context.session_id,
                    header_count = transfer_headers.map_or(0, HashMap::len),
                    reason = %reason,
                    "Rejected transfer headers"
                );
                into_callee_err(&rsipstack::sip::StatusCode::BadRequest, Some(reason))
            })?
            .unwrap_or_default();

        let cluster_enabled = !self.server.cluster_peer_ips.is_empty();
        let self_ident = self.self_ident_addr();
        let route_via_home_proxy =
            Self::route_via_home_proxy(target, cluster_enabled, self_ident.as_ref());
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
            .load()
            .session_expires
            .unwrap_or(crate::proxy::proxy_call::session_timer::DEFAULT_SESSION_EXPIRES);
        if self
            .server
            .proxy_config
            .load()
            .session_timer_mode()
            .is_enabled()
        {
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

        let passthrough_rule = match self.context.dialplan.header_passthrough.clone() {
            Some(rule) => Some(rule),
            None => {
                self.server
                    .header_passthrough_for(target, false, &callee_uri)
                    .await
            }
        };
        if let Some(rule) = &passthrough_rule {
            let selected = crate::call::Dialplan::select_passthrough_headers(
                &self.context.dialplan.original.headers.0,
                &headers,
                rule,
            );
            headers.extend(selected);
        }
        if !validated_transfer_headers.is_empty() {
            info!(
                session_id = %self.context.session_id,
                header_count = validated_transfer_headers.len(),
                "Applying transfer headers to outbound INVITE"
            );
            for (name, value) in validated_transfer_headers {
                headers.retain(|header| !header.name().eq_ignore_ascii_case(&name));
                headers.push(rsipstack::sip::Header::Other(name, value));
            }
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
                Some(r#"SIP;cause=500;text="Media resource allocation failed""#.to_string()),
            )
        })?;
        let content_type = offer.as_ref().map(|_| "application/sdp".to_string());

        let contact_uri = self
            .context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .or_else(|| {
                self.server.contact_uri_for_location_with_sip_contact(
                    target,
                    self.context.dialplan.media.sip_contact.as_ref(),
                )
            })
            .unwrap_or_else(|| caller.clone());

        let callee_call_id = self.context.dialplan.call_id.clone().unwrap_or_else(|| {
            rsipstack::transaction::make_call_id(
                self.server.endpoint.inner.option.callid_suffix.as_deref(),
                self.server.endpoint.inner.option.callid_format,
            )
            .value()
            .to_string()
        });
        self.meta.callee_call_ids.insert(callee_call_id.clone());

        // CTI uses the agent leg's SIP Call-ID, including while it is ringing.
        // Register it before either sequential or parallel dialing sends INVITE.
        let registry = &self.server.active_call_registry;
        if let Some(handle) = registry.get_handle(&self.context.session_id) {
            registry.register_dialog(callee_call_id.clone(), handle);
        }

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
            // RFC 7989: carry the global session id as the local-uuid so the
            // downstream endpoint/cluster peer can correlate the logical call.
            // P2P flows (session_id_enabled == false) stay header-free.
            session_id: self.session_id_for_outgoing_leg(),
            contact: contact_uri,
            ..Default::default()
        };

        // B-leg SIP destination for the CDR `calleePeer` field. Stashed here
        // at dial time because cleanup() clears the leg dialogs before
        // reporting. Last dial wins. Bare `ip:port` (SipAddr's Display would
        // add a transport prefix). When the target carries no explicit
        // destination (URI-routed leg), the INVITE goes to the request-URI
        // host — use it (exact for IP-literal hosts like trunks).
        self.meta.callee_peer = option
            .destination
            .as_ref()
            .map(|d| d.addr.to_string())
            .or_else(|| Some(callee_uri.host_with_port.to_string()));

        Ok((option, callee_uri, callee_call_id))
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
                self.build_local_dialog_answer(side, request.method.clone(), &offer_sdp)
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
                        // Negotiation updates the peers; only the session decides
                        // whether they should be connected. Refresh codec/video
                        // routing after hold state has been applied, so a held
                        // pair cannot be reconnected by an SDP update.
                        if !self.bypasses_local_media()
                            && self.bridge().is_some_and(|bridge| bridge.is_bridged())
                        {
                            self.update_media_path().await;
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
        match state {
            DialogState::Confirmed(_, _) => {
                self.update_leg_state(&LegId::from("caller"), LegState::Connected);
            }
            DialogState::Updated(dialog_id, request, tx_handle) => {
                self.handle_updated_dialog(DialogSide::Caller, dialog_id, request, tx_handle)
                    .await?;
            }
            DialogState::Options(_, _, tx_handle) => {
                // During drain every OPTIONS reports 500 to signal the
                // draining state (a 500 to an in-dialog OPTIONS does not
                // terminate the dialog, so active calls are unaffected).
                let code = if crate::shutdown::is_draining() {
                    rsipstack::sip::StatusCode::ServerInternalError
                } else {
                    rsipstack::sip::StatusCode::OK
                };
                tx_handle.respond(code, None, None).await.ok();
            }
            DialogState::Info(_, request, tx_handle) => {
                self.handle_dialog_info(DialogSide::Caller, request, tx_handle)
                    .await?;
            }
            DialogState::Notify(_, request, tx_handle) => {
                self.handle_dialog_notify(request, tx_handle).await?;
            }
            DialogState::Terminated(_, reason) => {
                self.update_leg_state(&LegId::from("caller"), LegState::Ended);
                self.meta.pending_transfer_outcome = None;

                // Our own teardown BYE also emits a Terminated event. Keep an
                // earlier root cause (for example RTP timeout or autohangup).
                match reason {
                    TerminatedReason::UacBye => {
                        if self.meta.hangup_reason.is_none() {
                            self.meta.hangup_reason = Some(CallRecordHangupReason::ByCaller);
                        }
                        info!(session_id = %self.id, "Caller initiated hangup (UacBye)");
                    }
                    TerminatedReason::UasBye => {
                        if self.meta.hangup_reason.is_none() {
                            self.meta.hangup_reason = Some(CallRecordHangupReason::ByCallee);
                        }
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
                // If an app (voicemail / IVR record / csat) is recording when the
                // caller hangs up, finalize the recording now so the app receives
                // RecordingComplete and can persist (e.g. save the voicemail)
                // before we cancel the event loop. persist is spawned, so a brief
                // bounded grace is enough for on_record_complete to kick it off.
                self.finalize_recording_for_app_shutdown().await;
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
        side: DialogSide,
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

        let leg_label = match side {
            DialogSide::Caller => "caller",
            DialogSide::Callee => "callee",
        };

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
            if let Some(digit_char) = parse_dtmf_digit(&body_text) {
                forward_dtmf_event(
                    digit_char,
                    leg_label,
                    &self.context.session_id,
                    &self.app_runtime,
                    &self.server.rwi_gateway,
                    &self.bridge_dtmf_tx,
                    &self.bridge_trace_context,
                    &self.bridge_dtmf_digits,
                    &self.context.original_caller,
                    &self.context.original_callee,
                    super::util::trace_sip_headers(&self.app_runtime).await,
                );
            }
            // Forward DTMF INFO to the peer dialog
            let peer_side = match side {
                DialogSide::Caller => "callee",
                DialogSide::Callee => "caller",
            };
            let forward_result = match side {
                DialogSide::Caller => {
                    // Forward to the connected callee dialog
                    match self.meta.connected_callee_dialog_id.clone() {
                        Some(callee_id) => match self.server.dialog_layer.get_dialog(&callee_id) {
                            Some(dlg) => {
                                let fwd_headers = vec![rsipstack::sip::Header::ContentType(
                                    rsipstack::sip::headers::ContentType::from(
                                        "application/dtmf-relay",
                                    ),
                                )];
                                Self::send_info_to_dialog(
                                    &dlg,
                                    fwd_headers,
                                    request.body().to_vec(),
                                )
                                .await
                            }
                            None => Ok(()),
                        },
                        None => Ok(()),
                    }
                }
                DialogSide::Callee => {
                    // Forward to the caller (server) dialog
                    match self.caller_dialog.as_ref() {
                        Some(server_dialog) => {
                            let fwd_headers = vec![rsipstack::sip::Header::ContentType(
                                rsipstack::sip::headers::ContentType::from(
                                    "application/dtmf-relay",
                                ),
                            )];
                            server_dialog
                                .info(Some(fwd_headers), Some(request.body().to_vec()))
                                .await
                                .map(|_| ())
                                .map_err(|e| anyhow::anyhow!(e))
                        }
                        None => Ok(()),
                    }
                }
            };
            if let Err(e) = forward_result {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    error = %e,
                    "Failed to forward SIP INFO DTMF to {}", peer_side
                );
            } else {
                debug!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    "Forwarded SIP INFO DTMF to {}", peer_side
                );
            }
        } else if is_picture_fast_update {
            self.handle_picture_fast_update(side).await;
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
                        side_only: false,
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
                    unique_id: None,
                    discard: None,
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
                    channels: params
                        .and_then(|p| p.get("channels"))
                        .and_then(|v| v.as_u64().map(|c| c as u16)),
                    mono_caller_only: params
                        .and_then(|p| p.get("mono_caller_only"))
                        .and_then(|v| v.as_bool()),
                    segment_type: params
                        .and_then(|p| p.get("type").or_else(|| p.get("segment_type")))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    segment_id: params
                        .and_then(|p| p.get("id").or_else(|| p.get("segment_id")))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    label: params
                        .and_then(|p| p.get("label"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    notify_app: params
                        .and_then(|p| p.get("notify_app"))
                        .and_then(|v| v.as_bool()),
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
        // Accept route_point at the top level (cc-phone `insertIvr`) or inside
        // ivr_params (widget panel), and resolve it the same way
        // `start_ivr_app` does (`resolve_ivr_file`), so both filesystem and
        // DB-backed (ivr_editor) IVRs work by name.
        let route_point = p
            .and_then(|p| p.get("route_point"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                ivr_params
                    .as_ref()
                    .and_then(|v| v.get("route_point"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
        let mut ivr_params = ivr_params.unwrap_or(serde_json::json!({}));
        let has_file_or_mode = ivr_params.get("file").is_some() || ivr_params.get("mode").is_some();
        if let (Some(route_point), false) = (&route_point, has_file_or_mode) {
            let route_point = route_point.trim();
            if !route_point.is_empty() {
                let file = self.server.data_context.resolve_ivr_file(route_point).await;
                if let Some(obj) = ivr_params.as_object_mut() {
                    obj.insert("file".to_string(), serde_json::json!(file));
                } else {
                    ivr_params = serde_json::json!({"file": file});
                }
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
            self.propagate_hold_to_side(LegSide::B,
                &[],
                override_music,
            )
            .await?;
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

                // A BYE cascaded from the caller leg is not a new root cause.
                // Only populate the reason when nothing recorded it earlier.
                match &reason {
                    TerminatedReason::UasBye => {
                        if self.meta.hangup_reason.is_none() {
                            self.meta.hangup_reason = Some(CallRecordHangupReason::ByCallee);
                        }
                        info!(session_id = %self.id, "Callee initiated hangup (UasBye)");
                    }
                    TerminatedReason::UacBye => {
                        if self.meta.hangup_reason.is_none() {
                            self.meta.hangup_reason = Some(CallRecordHangupReason::ByCaller);
                        }
                        info!(session_id = %self.id, "Caller initiated hangup (UacBye) on callee dialog");
                    }
                    _ => {
                        debug!(session_id = %self.id, ?reason, "Callee dialog terminated with reason");
                    }
                }

                if connected_callee_terminated {
                    self.meta.connected_callee = None;
                    self.meta.connected_callee_dialog_id = None;

                    // Run the unified post-disconnect handler directly (CSAT
                    // hooks first, then return_app, then hangup).
                    if !self
                        .caller_dialog
                        .as_ref()
                        .is_none_or(|d| d.state().is_terminated())
                    {
                        self.handle_start_return_app().await;
                    }
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
                        self.meta.invite_final_status.get_or_insert(code.code());
                        if self.meta.hangup_reason.is_none() {
                            self.meta.hangup_reason =
                                Some(sip_status_to_hangup_reason(code.code()));
                        }

                        // If the callee extension has voicemail enabled, chain to
                        // voicemail instead of passing the rejection to the caller.
                        // This covers busy, no-answer, offline, decline, etc.
                        if self.context.dialplan.voicemail_enabled
                            && !self
                                .caller_dialog
                                .as_ref()
                                .is_none_or(|d| d.state().is_terminated())
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
                // During drain every OPTIONS reports 500 to signal the
                // draining state (a 500 to an in-dialog OPTIONS does not
                // terminate the dialog, so active calls are unaffected).
                let code = if crate::shutdown::is_draining() {
                    rsipstack::sip::StatusCode::ServerInternalError
                } else {
                    rsipstack::sip::StatusCode::OK
                };
                tx_handle.respond(code, None, None).await.ok();
            }
            DialogState::Info(_, request, tx_handle) => {
                self.handle_dialog_info(DialogSide::Callee, request, tx_handle)
                    .await?;
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

    /// Prepare caller media for queue/app audio playback.
    ///
    /// If the caller dialog is not yet confirmed (still in early/183 state),
    /// this answers the call (200 OK) so that audio can be played on the
    /// confirmed media bridge.  Used by the `CallCommand::Play` handler when
    /// the queue app plays hold music, busy prompts, etc.
    async fn prepare_queue_playback_media(&mut self) {
        if self
            .caller_dialog
            .as_ref()
            .is_some_and(|d| d.state().is_confirmed())
        {
            if self.media_leg(&LegId::from("caller")).is_none() {
                warn!(session_id = %self.context.session_id, "Queue playback: answered caller has no media peer");
            }
            return;
        }

        if let Err(error) = self.accept_call(None, None).await {
            warn!(session_id = %self.context.session_id,
                error = %error,
                "Queue playback: failed to prepare caller media before audio"
            );
        }
    }

    /// Start the queue app from a resolved [`QueuePlan`].
    ///
    /// Shared by both the route→queue dispatch (`execute_flow`) and the
    /// queue-transfer path (`handle_queue_transfer`).  Resolves custom targets
    /// (skill-groups), applies the optional `queue_location_enricher`, stores
    /// the resolved plan in `pending_queue`, then starts the "queue" app.
    pub(crate) async fn start_queue_app(
        &mut self,
        mut plan: crate::call::QueuePlan,
        overflow_overrides: Option<crate::call::app::QueueOverflowOverrides>,
    ) -> Result<()> {
        use crate::call::DialStrategy;

        let agents = match &plan.dial_strategy {
            Some(DialStrategy::Sequential(l)) => l.clone(),
            Some(DialStrategy::Parallel(l)) => l.clone(),
            None => Vec::new(),
        };

        // Broadcast `queue_joined` BEFORE agent resolution so it is strictly
        // the FIRST queue event of this entry — skill-group candidate /
        // assignment events emitted during `resolve_custom_targets` below must
        // never precede it. Uses the same display id the queue app factory
        // will put into `QueueConfig::name`, so every later queue event
        // (`queue_agent_offered`, `queue_left`, ...) matches.
        let joined_emitted = self.server.rwi_gateway.is_some();
        if joined_emitted {
            let gw = self.server.rwi_gateway.as_ref().unwrap().read();
            gw.broadcast(&crate::rwi::QueueJoined {
                call_id: self.context.session_id.clone(),
                queue_id: plan.display_queue_id(),
            });
        }

        // Resolve custom targets (skill-groups → specific agents). Capture the
        // primary skill-group id BEFORE resolution rewrites the dial strategy —
        // the queue app factory needs it to resolve the escalation plan.
        let primary_skill_group = agents.iter().find_map(|l| {
            let uri_str = l.aor.to_string();
            uri_str
                .strip_prefix("skill-group:")
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
        });
        let resolved_agents = self.resolve_custom_targets(agents).await;

        // Enrich via queue_location_enricher if configured
        let resolved_agents = if let Some(enricher) = &self.server.queue_location_enricher {
            let caller_headers: Vec<rsipstack::sip::Header> = self
                .caller_dialog
                .as_ref()
                .map(|d| d.initial_request().headers.into())
                .unwrap_or_default();
            let direction_str = self.context.dialplan.direction.to_string();
            let caller =
                crate::models::call_record::extract_sip_username(&self.context.original_caller)
                    .unwrap_or_else(|| self.context.original_caller.clone());
            let callee =
                crate::models::call_record::extract_sip_username(&self.context.original_callee)
                    .unwrap_or_else(|| self.context.original_callee.clone());
            let queue_id_owned = plan.queue_name.clone();
            let queue_label_owned = plan
                .label
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| queue_id_owned.clone());
            // UUI `sg=` / screen-pop context: prefer the primary skill group
            // the queue actually dials (`skill-group:{id}` target); fall back
            // to the queue id for plain queues.
            let skill_owned = primary_skill_group
                .clone()
                .unwrap_or_else(|| queue_id_owned.clone());
            // UUI `ivr=` — set by `start_ivr_app` / IVR application flow so a
            // later queue dispatch carries the originating IVR short code.
            let ivr_owned = self.session_ext_get("ivr");
            let ticket_id = other_header_ci(
                &caller_headers,
                &["X-CRM-Ticket-Id", "X-Ticket-Id", "X-CRM-Ticket"],
            );
            let customer_id =
                other_header_ci(&caller_headers, &["X-CRM-Customer-Id", "X-Customer-Id"]);
            let session_id_owned = self.context.session_id.to_string();
            self.server.active_call_registry.set_context_meta(
                session_id_owned.clone(),
                crate::proxy::active_call_registry::ActiveCallContextMeta {
                    queue_id: Some(queue_id_owned.clone()).filter(|s| !s.is_empty()),
                    queue_name: Some(queue_label_owned.clone()).filter(|s| !s.is_empty()),
                    skill_group_id: Some(skill_owned.clone()).filter(|s| !s.is_empty()),
                    ivr_node_id: ivr_owned.clone(),
                    ticket_id,
                    customer_id,
                },
            );
            enricher
                .enrich(
                    resolved_agents,
                    &crate::proxy::call::QueueEnrichContext {
                        session_id: &session_id_owned,
                        queue_name: &queue_label_owned,
                        queue_id: &queue_id_owned,
                        caller: &caller,
                        callee: &callee,
                        direction: &direction_str,
                        skill_group_id: Some(&skill_owned),
                        ivr_node_id: ivr_owned.as_deref(),
                        caller_headers: &caller_headers,
                    },
                )
                .await
        } else {
            resolved_agents
        };

        let is_parallel = matches!(plan.dial_strategy, Some(DialStrategy::Parallel(_)));
        let agent_uris: Vec<String> = resolved_agents.iter().map(|l| l.aor.to_string()).collect();

        // The queue app dials from `plan.dial_strategy`, so write the resolved
        // locations back into the plan. Custom targets (skill-groups) that
        // resolved to zero reachable agents become an empty strategy — the
        // queue app then plays the busy prompt and executes its fallback
        // instead of dialing the raw `skill-group:` URI as if it were a SIP
        // contact (which would leave the caller on hold music forever).
        plan.dial_strategy = Some(if is_parallel {
            DialStrategy::Parallel(resolved_agents)
        } else {
            DialStrategy::Sequential(resolved_agents)
        });

        info!(session_id = %self.id,
            queue = %plan.queue_name,
            agents = agent_uris.len(),
            parallel = is_parallel,
            "Starting queue app"
        );

        // Authoritative queue metadata for hooks, CDR, abandon detection and
        // cc_* / RWI webhook correlation (`session_hook_ctx` reads CallMeta).
        let queue_id = if !plan.queue_name.is_empty() {
            plan.queue_name.clone()
        } else if let Some(label) = plan.label.as_ref().filter(|s| !s.is_empty()) {
            label.clone()
        } else {
            String::new()
        };
        if !queue_id.is_empty() {
            self.meta.queue_name = Some(queue_id);
        }
        if let Some(label) = plan.label.as_ref().filter(|s| !s.is_empty()) {
            self.meta.queue_label = Some(label.clone());
        }
        if let Some(sg) = primary_skill_group.as_ref().filter(|s| !s.is_empty()) {
            self.meta.skill_group_id = Some(sg.clone());
        }

        let has_resolved_agents = !agent_uris.is_empty();

        // Store resolved plan in context for the queue app factory
        if let Some(ctx) = self.app_runtime.app_context() {
            *ctx.pending_queue.lock() = Some(PendingQueuePlan {
                plan: plan.clone(),
                agent_uris,
                parallel: is_parallel,
                skill_group_id: primary_skill_group,
                overflow_overrides,
                joined_emitted,
            });
        }

        if let Err(e) = self
            .ensure_app_running_with(
                "queue",
                None,
                plan.accept_immediately,
                &format!("queue '{}'", plan.queue_name),
                None,
            )
            .await
        {
            let info = &crate::proxy::proxy_call::error_catalog::QUEUE_START_FAILED;
            self.meta.error_code = Some(info);
            let detail = serde_json::json!({
                "queue": plan.queue_name,
                "error": format!("{e:?}"),
            });
            self.record_trace(crate::call_errors::call_error_trace(info, Some(detail.clone())));
            self.emit_typed_rwi_event(&crate::rwi::CallError {
                call_id: self.context.session_id.clone(),
                session_id: Some(self.context.session_id.clone()),
                stage: "queue".to_string(),
                app: info.app.to_string(),
                code: info.code.to_string(),
                severity: info.severity.as_str().to_string(),
                message: info.message.to_string(),
                sip_status: info.sip_status,
                detail: Some(detail),
            });
            // Avoid an orphan `queue_joined`: with the queue app never
            // started, no `queue_left` would ever close the lifecycle.
            if joined_emitted
                && let Some(gw) = self.server.rwi_gateway.as_ref()
            {
                gw.read().broadcast(&crate::rwi::QueueLeft {
                    call_id: self.context.session_id.clone(),
                    queue_id: plan.display_queue_id(),
                    reason: Some("start_failed".to_string()),
                    skill_groups: None,
                });
            }
            return Err(anyhow!("Failed to start queue app: {:?}", e));
        }

        // The queue app now drives the session. Attribute the terminal phase to
        // the queue and record the queue entry so the call trace shows the full
        // timeline (IVR → … → Entered queue → caller abandoned → end) instead
        // of jumping straight from "answered" to "queue.abandoned".
        self.meta.app_name = Some("queue".to_string());
        let queue_name = plan.queue_name.clone();
        self.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Queue,
                if queue_name.is_empty() {
                    "Entered queue".to_string()
                } else {
                    format!("Entered queue '{}'", queue_name)
                },
            )
            .severity(crate::call_errors::ErrSeverity::Info)
            .detail(serde_json::json!({ "queue_name": queue_name })),
        );

        // Inject dial_next_agent to kick off sequential agent dialing
        // (parallel mode auto-dials in on_enter). Skip when no agents were
        // resolved — on_enter already fell through to the busy-prompt/fallback
        // path, so a second dial attempt would replay the busy prompt.
        if !is_parallel && has_resolved_agents {
            let _ = self.app_runtime.inject_event(serde_json::json!({
                "type": "custom",
                "name": "dial_next_agent",
                "data": {},
            }));
        }

        Ok(())
    }

    /// Start the queue app from `CallCommand::StartApp("queue", params)` app
    /// params — see [`crate::call::QueuePlan::from_app_params`] for the
    /// accepted target forms. The optional ACD `priority` is stored as a
    /// session extension (`queue_priority`) for ACD/pacing consumers; the
    /// plan itself carries the dial target only.
    async fn start_queue_app_from_params(
        &mut self,
        params: Option<&serde_json::Value>,
    ) -> Result<()> {
        let Some(params) = params else {
            anyhow::bail!("queue app requires params with a `queue` target");
        };
        let (plan, priority) = crate::call::QueuePlan::from_app_params(params)?;
        if let Some(priority) = priority {
            self.session_ext_set("queue_priority", priority.to_string());
        }
        self.start_queue_app(plan, None).await
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

                    match self.start_queue_app(plan.clone(), None).await {
                        Ok(()) => Ok(()),
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
                    info!(app_name = %app_name, "Executing application flow");
                    self.meta.app_name = Some(app_name.clone());
                    // Stamp the app-routed contract: the CALLER of an
                    // application session (IVR / queue entry / conference…
                    // ) is a self-service customer, NOT an agent on a call —
                    // CcCallSessionHook must not mark a registered agent
                    // caller Busy just because the caller identity resolves
                    // to one (that used to lock the agent out of ACD
                    // dispatch for the whole call).
                    self.session_ext_set("cc_app_routed", app_name.clone());
                    if app_name == "ivr" {
                        let ivr_short = app_params
                            .as_ref()
                            .and_then(|p| {
                                p.get("name")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string)
                                    .or_else(|| {
                                        p.get("file").and_then(|v| v.as_str()).and_then(|f| {
                                            std::path::Path::new(f)
                                                .file_stem()
                                                .and_then(|s| s.to_str())
                                                .map(str::to_string)
                                        })
                                    })
                            })
                            .unwrap_or_else(|| "ivr".into());
                        self.session_ext_set("ivr", ivr_short);
                    }
                                if let Err(e) = self
                        .app_runtime
                        .start_app(app_name, app_params.clone(), *auto_answer)
                        .await
                    {
                        warn!(session_id = %self.id, app_name = %app_name, error = %e, "Failed to start application");
                        // Select a standardized error code by app name so the
                        // terminal End trace and CDR carry a meaningful reason
                        // (e.g. missing IVR config). Falling through to
                        // `reject_with_tone` plays the configured failure cue
                        // (see RingbackAudio::error) before the rejection.
                        let info = match app_name.as_str() {
                            "voicemail" => {
                                &crate::call::app::error_catalog::VOICEMAIL_START_FAILED
                            }
                            "conference" => {
                                &crate::call::app::error_catalog::CONFERENCE_START_FAILED
                            }
                            _ => &crate::call::app::error_catalog::IVR_START_FAILED,
                        };
                        self.meta.error_code = Some(info);
                        let kind = if app_name == "voicemail" {
                            crate::call_errors::TraceKind::Voicemail
                        } else {
                            crate::call_errors::TraceKind::Ivr
                        };
                        self.record_trace(
                            crate::call_errors::TraceEvent::new(
                                kind,
                                format!("Failed to start {} application: {}", app_name, e),
                            )
                            .severity(crate::call_errors::ErrSeverity::Error)
                            .code(info.code)
                            .detail(serde_json::json!({
                                "app": app_name,
                                "error": e.to_string(),
                            })),
                        );
                        return Err((
                            info.sip_status.unwrap_or(500),
                            format!("Failed to start {} application", app_name),
                            None,
                        ));
                    }
                    // Start succeeded — record the descriptive Info trace.
                    if app_name == "voicemail" {
                        let ext = app_params
                            .as_ref()
                            .and_then(|p| p.get("extension").and_then(|v| v.as_str()))
                            .unwrap_or_default();
                        self.record_trace(
                            crate::call_errors::TraceEvent::new(
                                crate::call_errors::TraceKind::Voicemail,
                                if ext.is_empty() {
                                    "Voicemail application started".to_string()
                                } else {
                                    format!("Voicemail: caller routed to mailbox '{}'", ext)
                                },
                            )
                            .severity(crate::call_errors::ErrSeverity::Info),
                        );
                    } else {
                        self.record_trace(
                            crate::call_errors::TraceEvent::new(
                                crate::call_errors::TraceKind::Ivr,
                                format!("Application '{}' started", app_name),
                            )
                            .severity(crate::call_errors::ErrSeverity::Info),
                        );
                    }
                    if app_name == "conference" {
                        // The conference app calls ctrl.answer() in on_enter,
                        // which only QUEUES the Answer command. Joining inline
                        // here would race the answer (no media tracks yet).
                        // Instead queue a JoinConference command behind it:
                        // command ordering guarantees the leg is Connected
                        // when the join runs.
                        let conf_id = app_params
                            .as_ref()
                            .and_then(|p| p.get("id").and_then(|v| v.as_str()))
                            .unwrap_or(&format!("conf-{}", self.id.0))
                            .to_string();
                        Self::send_or_log_cmd(
                            &self.cmd_tx,
                            CallCommand::JoinConference { conf_id },
                            "queue JoinConference",
                            &self.context.session_id.to_string(),
                        );
                                    } else {
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
        self.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Ring,
                format!("Dialing {} target(s) sequentially", targets.len()),
            )
            .severity(crate::call_errors::ErrSeverity::Info),
        );
        if targets.is_empty() {
            self.meta.error_code = Some(&crate::proxy::proxy_call::error_catalog::DIAL_NO_TARGETS);
            return Err(into_callee_err(
                &StatusCode::TemporarilyUnavailable,
                Some("No targets to dial".to_string()),
            ));
        }

        let mut last_error = into_callee_err(
            &StatusCode::TemporarilyUnavailable,
            Some("All targets failed".to_string()),
        );

        for (idx, target) in targets.iter().enumerate() {
            info!(index = idx, target = %target.aor, "Trying sequential target");

            match self
                .try_single_target(target, callee_state_rx, None, None, None)
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

        self.meta.error_code =
            Some(&crate::proxy::proxy_call::error_catalog::DIAL_ALL_TARGETS_FAILED);
        Err(last_error)
    }

    async fn dial_parallel(
        &mut self,
        targets: &[crate::call::Location],
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        if targets.is_empty() {
            self.meta.error_code = Some(&crate::proxy::proxy_call::error_catalog::DIAL_NO_TARGETS);
            return Err(into_callee_err(
                &StatusCode::TemporarilyUnavailable,
                Some("No targets to dial".to_string()),
            ));
        }

        for target in targets {
            info!(target = %target.aor, "dial_parallel: target");
        }

        self.fork_targets_parallel(targets, callee_state_rx)
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
        _callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        use futures::StreamExt;
        use futures::stream::FuturesUnordered;
        use rsipstack::dialog::invitation::InviteOption;
        use rsipstack::sip::StatusCodeKind;

        self.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Ring,
                format!("Forking INVITEs to {} target(s) in parallel", targets.len()),
            )
            .severity(crate::call_errors::ErrSeverity::Info),
        );

        if targets.is_empty() {
            self.meta.error_code = Some(&crate::proxy::proxy_call::error_catalog::DIAL_NO_TARGETS);
            return Err(into_callee_err(
                &StatusCode::TemporarilyUnavailable,
                Some("No targets to dial".to_string()),
            ));
        }

        if self.context.dialplan.caller.is_none() {
            self.meta.error_code = Some(&crate::proxy::proxy_call::error_catalog::DIAL_NO_CALLER);
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
            .load()
            .session_expires
            .unwrap_or(DEFAULT_SESSION_EXPIRES);
        let _session_timer_enabled = self
            .server
            .proxy_config
            .load()
            .session_timer_mode()
            .is_enabled();

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
                .build_target_invite_option(target, Some(&leg_id_str), None)
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
                    biased;
                    result = dlg.do_invite(invite_option, fork_tx) => {
                        Some((idx, result, callee_uri))
                    }
                    _ = ct.cancelled() => {
                        None
                    }
                }
            });

            fork_set.push(join);
        }

        // If the caller hasn't confirmed yet, send 180 Ringing before forking
        if !self
            .caller_dialog
            .as_ref()
            .is_some_and(|d| d.state().is_confirmed())
        {
            if let Some(dialog) = self.caller_dialog.as_ref() {
                let _ = dialog.ringing(None, None);
            }
        }

        // Race the forked INVITEs – first 200 OK wins
        // Fire on_call_ringing hooks now that callees are ringing.
        // Use the first target's AOR for agent identity.
        // (No provisional response yet — early_media is always false here.)
        if !self.server.session_hooks.is_empty() && !targets.is_empty() {
            self.meta.routed_callee = Some(targets[0].aor.to_string());
            let ctx = self.session_hook_ctx();
            for hook in self.server.session_hooks.iter() {
                hook.on_call_ringing(&ctx, false).await;
            }
            // The CC hook resolves + publishes the agent attribution here
            // (parallel-ring targets) — sync it so the per-provisional
            // `call_ringing` emitted below carries it.
            self.sync_agent_context_to_rwi_meta();
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
                        self.cleanup_loser_fork_dialogs(&mut fork_set).await;
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
                    self.cleanup_loser_fork_dialogs(&mut fork_set).await;
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

                            // Drain the loser forks: any that already got a 2xx
                            // have their confirmed dialog registered in
                            // dialog_layer by `do_invite`, but no
                            // ClientDialogGuard is created for them — the old
                            // cleanup only removed the leg from LegRegistry,
                            // leaking the dialog entry. Remove + hang up each
                            // confirmed loser.
                            self.cleanup_loser_fork_dialogs(&mut fork_set).await;

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

    /// Drain the remaining parallel-fork results after a fork race has been
    /// decided (winner chosen or all cancelled).
    ///
    /// A losing fork that already received a 2xx has its confirmed dialog
    /// registered in `dialog_layer` by rsipstack's `do_invite` (under the
    /// confirmed dialog id) and no `ClientDialogGuard` is created for it — the
    /// old cleanup only removed the leg from `LegRegistry`, which leaks the
    /// dialog entry. This removes and hangs up each confirmed loser so the
    /// dialog layer returns to empty once the call drains.
    async fn cleanup_loser_fork_dialogs(
        &self,
        fork_set: &mut futures::stream::FuturesUnordered<
            tokio::task::JoinHandle<
                Option<(
                    usize,
                    rsipstack::Result<(InviteDialog, Option<rsipstack::sip::Response>)>,
                    rsipstack::sip::Uri,
                )>,
            >,
        >,
    ) {
        use futures::StreamExt;
        use rsipstack::sip::StatusCodeKind;
        while let Some(join_result) = fork_set.next().await {
            if let Ok(Some((_loser_idx, Ok((loser_dialog, loser_resp)), _loser_uri))) = join_result
            {
                let confirmed = loser_resp
                    .as_ref()
                    .is_some_and(|r| r.status_code.kind() == StatusCodeKind::Successful);
                if confirmed {
                    let loser_id = loser_dialog.id();
                    self.server.dialog_layer.remove_dialog(&loser_id);
                    let dlg = loser_dialog.clone();
                    crate::utils::spawn(async move {
                        if let Err(e) = dlg.hangup().await {
                            warn!(id = %dlg.id(), error = %e, "fork loser hangup failed");
                        }
                    });
                }
            }
        }
    }

    /// Send 183 Session Progress with early media audio played to the caller.
    /// Supports file paths and `tone://frequency,duration_ms` format. The ring
    /// tone loops until the callee answers; the caller-side handle is dropped.
    async fn send_early_media_tone(&mut self, audio_path: &str) -> Result<()> {
        let loop_playback = !audio_path.starts_with("tone://");
        self.send_early_media(audio_path, loop_playback)
            .await
            .map(|_| ())
    }

    /// Play a one-shot early-media cue (e.g. a failure/beep tone) through the
    /// caller media peer.
    async fn send_early_media_cue(
        &mut self,
        audio_path: &str,
    ) -> Result<Option<crate::media::media_bridge::PlaybackHandle>> {
        let loop_playback = !audio_path.starts_with("tone://");
        self.send_early_media(audio_path, loop_playback).await
    }

    /// Prepare the caller media peer, send 183 Session Progress,
    /// and play `audio_path` as early media (ringback tone or short cue).
    /// Returns the playback handle when audio actually started.
    async fn send_early_media(
        &mut self,
        audio_path: &str,
        loop_playback: bool,
    ) -> Result<Option<crate::media::media_bridge::PlaybackHandle>> {
        // Ensure caller media exists before sending early audio.
        if let Err(e) = self.ensure_caller_leg().await {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                error = %e,
                "Failed to ensure caller leg for 183 early media"
            );
            return Ok(None);
        }

        if !self.media.early_media_sent {
            // Get caller-facing answer SDP from the caller leg's PC
            let answer_sdp = self.media_leg(&LegId::from("caller"))
                .and_then(|leg| leg.pc().local_description())
                .map(|desc| desc.to_sdp_string())
                .or_else(|| self.media.answer.clone())
                .unwrap_or_default();

            if answer_sdp.is_empty() {
                warn!(session_id = %self.id, "Cannot send 183: no local SDP available on caller leg");
                return Ok(None);
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
        }

        let peer = self.media_leg(&LegId::from("caller")).ok_or_else(|| anyhow!("No caller media peer"))?;
        let audio: Box<dyn crate::media::audio_source::AudioSource> = if let Some(spec) = audio_path.strip_prefix("tone://") {
            let (frequency, duration) = spec.split_once(',').ok_or_else(|| anyhow!("Invalid tone spec: {}", spec))?;
            Box::new(crate::media::audio_source::ToneAudioSource::new(
                frequency.trim().parse()?,
                Duration::from_millis(duration.trim().parse::<u64>()?.max(Self::MIN_TONE_DURATION_MS)),
                8000,
            )?)
        } else {
            Box::new(crate::media::audio_source::FileAudioSource::new(Self::resolve_audio_file_path(audio_path), loop_playback).await?)
        };
        if let Some(mb) = self.bridge_mut() { mb.unbridge().await?; }
        let handle = peer.play_media(audio, loop_playback).await?;
        self.record_play_start("progress-media", "ringback");
        Ok(Some(handle))
    }

    /// Natural playback duration of a failure-tone spec: the tone:// duration
    /// (with the minimum floor), or the audio file's estimated length. Used to
    /// wait for the cue to play once before the rejection.
    fn failure_tone_duration(spec: &str) -> std::time::Duration {
        if let Some(tone_spec) = spec.strip_prefix("tone://")
            && let Some((_, duration_ms)) = tone_spec.split_once(',')
            && let Ok(duration_ms) = duration_ms.trim().parse::<u64>()
        {
            return Duration::from_millis(duration_ms.max(Self::MIN_TONE_DURATION_MS));
        }
        let resolved = Self::resolve_audio_file_path(spec);
        crate::media::audio_source::estimate_audio_duration(&resolved)
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
            // `tone://` plays for its spec duration (with a minimum floor);
            // file tones play to natural completion. Either way the caller
            // hears the full cue before the rejection — never cut short.
            let tone_ms = path
                .strip_prefix("tone://")
                .map(|_| Self::failure_tone_duration(path).as_millis());
            info!(session_id = %self.id,
                session_id = %self.context.session_id,
                status = %status,
                audio = %path,
                tone_duration_ms = tone_ms,
                "Playing failure tone before rejection",
            );
            // Play the cue (looping) and wait its natural duration — the
            // caller hears the full prompt (file length or tone spec duration)
            // before the rejection; never cut short.
            match self.send_early_media_cue(path).await {
                Ok(Some(_handle)) => {
                    tokio::time::sleep(Self::failure_tone_duration(path)).await;
                    self.record_play_end("progress-media", false);
                }
                Ok(None) => {
                    // 183 already sent or no caller media — nothing to await.
                }
                Err(e) => {
                    warn!(session_id = %self.context.session_id, error = %e, "Failed to play failure tone");
                }
            }
        }
        if let Some(dialog) = self.caller_dialog.as_ref() {
            dialog.reject(Some(status), reason.clone())?;
        }
        Ok(())
    }

    /// Busy-wait (camp-on): the callee rejected with 486 Busy and the route
    /// carries a `[busy_wait]` policy. Park the caller with looping hold audio
    /// (183 early media) and re-dial the dialplan targets every
    /// `retry_interval` until one answers, the caller hangs up, or the wait
    /// budget expires.
    ///
    /// Returns `Ok(())` once a target connected — the bridge/answer is already
    /// handled by `finalize_callee_connection`, so the caller resumes into the
    /// main call loop. Returns the final `CalleeError` otherwise (the caller
    /// is rejected with that status, preceded by the configured failure tone).
    async fn busy_wait_and_redial(
        &mut self,
        plan: crate::call::BusyWaitPlan,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), CalleeError> {
        let targets: Vec<crate::call::Location> = self
            .context
            .dialplan
            .flow
            .find_targets()
            .cloned()
            .unwrap_or_default();
        if targets.is_empty() {
            warn!(
                session_id = %self.context.session_id,
                "busy_wait configured but the dialplan has no targets; rejecting"
            );
            return Err(into_callee_err(
                &StatusCode::BusyHere,
                Some("Busy Here".to_string()),
            ));
        }

        info!(
            session_id = %self.id,
            session_id = %self.context.session_id,
            targets = targets.len(),
            retry_interval = ?plan.retry_interval,
            max_wait = ?plan.max_wait,
            hold_audio = %plan.hold_audio,
            "Callee busy; parking caller with hold audio and re-dialing"
        );

        if let Err(e) = self.send_early_media_tone(&plan.hold_audio).await {
            warn!(
                session_id = %self.context.session_id,
                error = %e,
                "Failed to play busy-wait hold audio"
            );
        }

        let started = Instant::now();
        let deadline = plan.max_wait.map(|d| started + d);
        let mut last_busy = into_callee_err(&StatusCode::BusyHere, Some("Busy Here".to_string()));

        loop {
            tokio::select! {
                _ = tokio::time::sleep(plan.retry_interval) => {}
                _ = self.cancel_token.cancelled() => {
                    return Err(into_callee_err(
                        &StatusCode::RequestTerminated,
                        Some("Caller cancelled".to_string()),
                    ));
                }
            }

            if self
                .caller_dialog
                .as_ref()
                .is_none_or(|d| d.state().is_terminated())
            {
                info!(
                    session_id = %self.context.session_id,
                    "Caller hung up while waiting for busy target"
                );
                return Err(into_callee_err(
                    &StatusCode::RequestTerminated,
                    Some("Caller cancelled".to_string()),
                ));
            }

            if let Some(deadline) = deadline
                && Instant::now() >= deadline
            {
                info!(
                    session_id = %self.context.session_id,
                    "busy_wait budget expired; rejecting with the last busy status"
                );
                if let Some(peer) = self.media_leg(&LegId::from("caller")) {
                    peer.stop_playback().await.ok();
                }
                return Err(last_busy);
            }

            let mut saw_busy = false;
            for target in &targets {
                match self
                    .try_single_target(target, callee_state_rx, None, None, None)
                    .await
                {
                    Ok(()) => return Ok(()),
                    Err(err @ (code, _, _)) if code == StatusCode::BusyHere.code() => {
                        saw_busy = true;
                        last_busy = err;
                    }
                    Err(other) => {
                        // Non-busy failure (offline / no-answer / error …) —
                        // stop camping and let the caller be rejected.
                        warn!(
                            session_id = %self.context.session_id,
                            target = %target.aor,
                            error = ?other,
                            "Non-busy failure during busy_wait; giving up"
                        );
                        if let Some(peer) = self.media_leg(&LegId::from("caller")) {
                            peer.stop_playback().await.ok();
                        }
                        return Err(other);
                    }
                }
            }
            debug_assert!(saw_busy, "round without busy or terminal failure");
        }
    }

    /// Ensure the caller media peer exists independently of a bridge.
    /// Idempotent — safe to call multiple times.
    async fn ensure_caller_leg(&mut self) -> Result<()> {
        let caller_offer = self
            .media
            .caller_offer
            .clone()
            .ok_or_else(|| anyhow!("No caller offer available"))?;
        // Classify from the offer itself (ICE+DTLS → WebRtc, RTP/SAVP or
        // a=crypto → Srtp, otherwise plain Rtp) so an SDES-SRTP offer gets an
        // Srtp caller leg and an `RTP/SAVP`+`a=crypto` answer — answering a
        // SAVP offer with plain `RTP/AVP` is an RFC 3264 violation and strict
        // clients (e.g. Groundwire over sips) refuse to flow media (issue
        // #281).
        let transport = Self::sdp_transport_mode(&caller_offer);
        let codecs = crate::media::negotiate::MediaNegotiator::build_codec_list_from_offer(
            &caller_offer,
            &[],
        );
        // The caller leg is the ANSWERER: it needs a video transceiver whenever
        // the caller's offer carries a video m-line (rustrtc's answer builder
        // requires a transceiver per remote section). The transceiver is added
        // from the offered caps regardless of the video policy; when the policy
        // strips video, the video m-line is forced inactive in the answer below.
        let video_codecs = self.video_caps_from_sdp(&caller_offer);

        let cfg = self.build_leg_config(transport.clone(), codecs, video_codecs);
        let caller_label = "caller";

        if self.media_leg(&LegId::from("caller"))
            .is_some()
        {
            return Ok(());
        }
        let recorder_sender = self.setup_recording_capture()?;
        let leg = crate::media::leg::LegInner::new(caller_label, &cfg, recorder_sender)?;
        let answer = leg
            .apply_sdp(&caller_offer, rustrtc::SdpType::Offer)
            .await?;
        self.legs.set_media_leg(&LegId::from("caller"), leg);
        self.spawn_dtmf_forwarder();

        // Video policy = strip: disable video on the media path — the answer's
        // video m-line is forced inactive (port 0) so the caller can't send
        // video the proxy won't relay.
        let answer = if self.video_relay_enabled() {
            answer
        } else {
            crate::media::negotiate::MediaNegotiator::strip_video_from_sdp(&answer)
                .unwrap_or(answer)
        };
        self.media.answer = Some(answer);

        // Record the caller transport hint. The callee hint is intentionally
        // NOT set here: every callee-creation path (`create_callee_track`,
        // `initiate_sip_leg`, `build_target_invite_option`) derives and stores
        // its own mode via `callee_transport_mode()`. Overwriting it here
        // clobbered the proxy path's Srtp hint with an "opposite of caller"
        // guess (issue #281).
        self.legs.set_transport(LegId::from("caller"), transport);

        let auto_start_on_media_setup = {
            let recording = &self.context.dialplan.recording;
            recording.enabled
                && recording.auto_start
                && recording.auto_start_at == crate::config::RecordingAutoStartAt::Media
        };
        if auto_start_on_media_setup && let Err(error) = self.set_auto_recorder().await {
            warn!(session_id = %self.id, %error, "Auto recorder installation after caller media setup failed");
        }

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
    fn rewrite_answer_to_selected_audio_codecs(
        &self,
        answer_sdp: &str,
        offer_sdp: &str,
        preferred_peer_sdp: Option<&str>,
        previous_negotiated_codec: Option<CodecType>,
        context: &str,
    ) -> String {
        let allow_codecs = self.resolve_effective_codecs();
        let preferred_audio_codecs: Vec<CodecType> = preferred_peer_sdp
            .map(|sdp| {
                MediaNegotiator::extract_codec_params(sdp)
                    .audio
                    .into_iter()
                    .map(|codec| codec.codec)
                    .collect()
            })
            .filter(|codecs: &Vec<CodecType>| !codecs.is_empty())
            .unwrap_or(allow_codecs);
        let offered_audio_codecs = MediaNegotiator::extract_codec_params(offer_sdp);
        let previous_offered = previous_negotiated_codec.filter(|previous_codec| {
            offered_audio_codecs
                .audio
                .iter()
                .any(|codec| codec.codec == *previous_codec)
        });
        let selected_audio_codecs = if let Some(previous_codec) = previous_offered {
            MediaNegotiator::build_codec_list_from_offer(offer_sdp, &[previous_codec])
        } else {
            MediaNegotiator::build_codec_list_from_offer(offer_sdp, &preferred_audio_codecs)
        };
        if selected_audio_codecs.is_empty() {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                context,
                "No compatible audio codec selected for SDP answer"
            );
            return answer_sdp.to_string();
        }
        debug!(
            session_id = %self.context.session_id,
            context,
            previous_negotiated_codec = ?previous_negotiated_codec,
            selected_audio_codecs = ?selected_audio_codecs.iter().map(|c| (c.payload_type, &c.codec, c.clock_rate)).collect::<Vec<_>>(),
            "SDP answer codec selection before rewrite"
        );

        MediaNegotiator::rewrite_sdp_codec_list(answer_sdp, &selected_audio_codecs).unwrap_or_else(
            || {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    context,
                    "Failed to rewrite SDP answer to selected audio codec"
                );
                answer_sdp.to_string()
            },
        )
    }

    async fn resolve_custom_targets(
        &mut self,
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
                                        let mut agent_location = crate::call::Location {
                                            aor: uri,
                                            contact_raw: Some(agent_uri.clone()),
                                            ..Default::default()
                                        };
                                        // External/unregistered agent — route through
                                        // the route table if enabled (trunk + rewrite).
                                        match self.route_originated_leg(&agent_location).await {
                                            Ok((routed, hints)) => {
                                                agent_location = routed;
                                                self.track_routed_leg_hints(hints);
                                            }
                                            Err(e) => {
                                                warn!(session_id = %self.id, agent = %agent_uri, error = %e, "Route lookup failed for external agent; dialing directly");
                                            }
                                        }
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
            if location.destination.is_some() || location.home_proxy.is_some() {
                resolved.push(location);
                continue;
            }

            // Include the port when deciding whether this target belongs to the
            // PBX. An explicit URI on another port is a directly dialable SIP
            // service, not an unregistered local extension.
            let target_realm = location.aor.host_with_port.to_string();
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
                Ok(_) => {
                    warn!(session_id = %self.id,
                        target = %location.aor,
                        "Queue target is an unregistered local extension, skipping"
                    );
                }
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

    pub(crate) async fn try_single_target(
        &mut self,
        target: &crate::call::Location,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
        no_trying_timeout: Option<std::time::Duration>,
        caller: Option<rsipstack::sip::Uri>,
        transfer_headers: Option<&HashMap<String, String>>,
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
            .load()
            .session_expires
            .unwrap_or(crate::proxy::proxy_call::session_timer::DEFAULT_SESSION_EXPIRES);
        let caller_is_webrtc = self.is_caller_webrtc();
        let _ = caller_is_webrtc;
        self.legs.set_transport(
            crate::call::domain::LegId::from("caller"),
            self.caller_transport_mode(),
        );

        let (mut invite_option, callee_uri, callee_call_id) = self
            .build_target_invite_option(target, None, transfer_headers)
            .await?;
        if let Some(caller) = caller {
            invite_option.caller = caller;
        }

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
                                if self.server.proxy_config.load().session_timer_mode().is_enabled()
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
                    if let Some(DialogState::Trying(_)) = state {
                        // rsipstack reports 100 Trying separately from Early.
                        no_trying_dismissed = true;
                    } else if let Some(DialogState::Early(_, ref response)) = state {
                        // Any non-100 provisional response also proves the downstream
                        // trunk is alive; dismiss the no-trying timer from now on.
                        no_trying_dismissed = true;

                        if self.meta.ring_time.is_none() {
                            self.meta.ring_time = Some(Instant::now());
                        }

                        let callee_sdp = String::from_utf8_lossy(response.body()).to_string();
                        if !callee_sdp.is_empty() && callee_sdp.contains("v=0") {
                            if !self.media.early_media_sent {
                                self.media.early_media_sent = true;
                                self.update_leg_state(&LegId::from("callee"), LegState::EarlyMedia);

                                // Provisional response carried SDP — fire the
                                // ringing session hooks first (the CC addon
                                // resolves + publishes the agent attribution)
                                // and then emit the core ringing event with
                                // early_media = true, enriched with the agent
                                // context.
                                self.fire_on_call_ringing_hooks(true).await;
                                self.emit_typed_rwi_event(&crate::rwi::CallRinging {
                                    leg_id: None,
                                    call_id: self.context.session_id.clone(),
                                    early_media: true,
                                });

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

                            // Fire on_call_ringing hooks first (plain 180, no
                            // SDP) so agent attribution is published, then
                            // emit the enriched core ringing event.
                            self.fire_on_call_ringing_hooks(false).await;
                            self.emit_typed_rwi_event(&crate::rwi::CallRinging {
                                leg_id: None,
                                call_id: self.context.session_id.clone(),
                                early_media: false,
                            });
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
        invite_option: &rsipstack::dialog::invitation::InviteOption,
        default_expires: u64,
    ) -> Result<(), CalleeError> {
        let callee_sdp = response.as_ref().and_then(|r: &rsipstack::sip::Response| {
            let body = r.body();
            Self::extract_sdp(body)
        });
        // Connecting the media peers replaces early-media playback directly;
        // do not switch the caller to silence before selecting the new source.

        let callee_guard =
            ClientDialogGuard::new(self.server.dialog_layer.clone(), dialog_id.clone());

        // Ensure callee leg exists in the MediaBridge.
        // For originate (UAC) path, the INVITE SDP was generated by RtpTrackBuilder,
        // so create_callee_track was never called and the bridge has no B leg.
        // Create one here so media_play / hold / comfort-noise can target it.
        if self.media.bridge.is_some() {
            let has_callee_leg = self.media_leg(&LegId::from("callee"))
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
                                offer,
                                &allow_codecs,
                            )
                        })
                        .unwrap_or_default();
                    let video_codecs = if self.video_relay_enabled() {
                        self.media
                            .caller_offer
                            .as_ref()
                            .map(|offer| self.video_caps_from_sdp(offer))
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let cfg = self.build_leg_config(callee_mode, codecs, video_codecs);
                    match crate::media::leg::LegInner::new(
                        "callee",
                        &cfg,
                        None,
                    ) {
                        Ok(leg) => {
                            if let Ok(offer) = leg.create_offer().await {
                                if self.bridge().is_some() {
                                    self.legs.set_media_leg(&LegId::from("callee"), leg);
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
            .prepare_caller_answer_from_callee_sdp(callee_sdp, false, rustrtc::SdpType::Answer)
            .await
            .map_err(|e| {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    error = %e,
                    "Failed to prepare caller answer"
                );
                into_callee_err(
                    &StatusCode::ServerInternalError,
                    Some(r#"SIP;cause=500;text="Media resource allocation failed""#.to_string()),
                )
            })?;

        self.meta.connected_callee_dialog_id = Some(dialog_id.clone());
        self.callee_dialogs.insert(dialog_id.clone(), ());
        self.callee_guards.push(callee_guard);

        self.accept_call(Some(callee_uri.to_string()), caller_answer)
            .await
            .map_err(|e| into_callee_err(&StatusCode::ServerInternalError, Some(e.to_string())))?;

        // Register callee dialog in unified map
        if let Some(dlg) = self.server.dialog_layer.get_dialog(&dialog_id) {
            self.legs.set_dialog(LegId::from("callee"), dlg);
        }
        if self
            .server
            .proxy_config
            .load()
            .session_timer_mode()
            .is_enabled()
        {
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
                    .and_then(SessionExpires::parse)
                    .map(|session_expires| session_expires.interval)
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
            let has_callee_leg = self.media_leg(&LegId::from("callee"))
                .is_some();

            let has_bridge = self.media_profile.path == MediaPathMode::Anchored || self.media.bridge.is_some();
            tracing::debug!(
                session_id = %self.id,
                has_bridge,
                has_callee_leg,
                "prepare_caller_answer_from_callee_sdp: checking MediaBridge path"
            );

            if has_bridge && has_callee_leg {
                let caller = self.media_leg(&LegId::from("caller"))
                    .ok_or_else(|| anyhow!("Missing caller media peer"))?;
                let callee = self.media_leg(&LegId::from("callee"))
                    .ok_or_else(|| anyhow!("Missing callee media peer"))?;
                if self.media.bridge.is_none() { self.media.bridge = Some(MediaBridge::new(self.id.to_string())); }
                self.bridge_mut().unwrap().select_pair(caller, callee).await?;
                // Direct dialing owns this selection; later media resume restores it.
                self.bridge = BridgeConfig::bridge(LegId::from("caller"), LegId::from("callee"));
                let cmd_tx = self.cmd_tx.clone();
                let session_id = self.context.session_id.clone();
                // Once the callee answers, normalize caller audio to the
                // selected audio codec and answer caller video with only the
                // video capabilities accepted by the callee.
                let caller_answer = match (
                    self.media.answer.as_deref(),
                    self.media.caller_offer.as_deref(),
                ) {
                    (Some(answer), Some(caller_offer)) => {
                        let answer = self.rewrite_answer_to_selected_audio_codecs(
                            answer,
                            caller_offer,
                            Some(callee_sdp_value),
                            None,
                            "MediaBridge caller answer",
                        );
                        let caller_video_caps = self.video_caps_from_sdp(caller_offer);
                        let accepted_video_caps = MediaNegotiator::accepted_video_capabilities(
                            &caller_video_caps,
                            callee_sdp_value,
                        );
                        let answer = MediaNegotiator::rewrite_video_capabilities(
                            rustrtc::SdpType::Answer,
                            &answer,
                            &accepted_video_caps,
                        )
                        .map_err(|error| {
                            anyhow!("failed to build MediaBridge caller video answer: {error}")
                        })?;
                        Some(answer)
                    }
                    _ => self.media.answer.clone(),
                };
                self.media.answer = caller_answer.clone();

                {
                    let mb = self.bridge_mut().ok_or_else(|| anyhow!("No MediaBridge"))?;
                    if let Some(callee_leg) = mb.leg(LegSide::B) {
                        let sdp_type = callee_sdp_type;
                        callee_leg.apply_sdp(callee_sdp_value, sdp_type).await?;
                    }

                    // Keep the actual sender, egress codec, bridge profile and
                    // recorder profile aligned with the SDP returned to caller.
                    // This must happen before accept() activates the route.
                    if let (Some(leg), Some(answer)) = (
                        mb.leg(LegSide::A),
                        caller_answer.as_deref(),
                    ) {
                        leg.apply_profile_from_sdp(answer).await?;
                    }
                }

                // One-shot: only the first answer path (app answer or callee
                // answer) arms the relay-arm-failure monitor — a second
                // spawn would double the warn and the fallback command.
                let arm_relay_monitor = !self.meta.relay_arm_monitor_spawned;
                if arm_relay_monitor {
                    self.meta.relay_arm_monitor_spawned = true;
                }
                // Same one-shot rule for the media-health monitor: the answer
                // path can run more than once per session, and a second
                // watcher on the same bridge would duplicate every periodic
                // media_health trace and every proxy.media_stalled error.
                let arm_health_monitor = !self.meta.media_health_monitor_spawned;
                if arm_health_monitor {
                    self.meta.media_health_monitor_spawned = true;
                }
                let stall_detect_secs = self.context.dialplan.media.stall_detect_secs;
                let trace_interval_secs = self.context.dialplan.media.media_trace_interval_secs;
                let mb = self.bridge_mut().ok_or_else(|| anyhow!("No MediaBridge"))?;
                mb.accept(LegSide::B).await;
                mb.accept(LegSide::A).await;
                if !is_early_media {
                    Self::arm_bridged_rtp_timeouts(mb, rtp_timeout, cmd_tx.clone(), &session_id);
                }
                if arm_health_monitor {
                    Self::arm_media_health_monitor(
                        mb,
                        cmd_tx.clone(),
                        &session_id,
                        stall_detect_secs,
                        trace_interval_secs,
                    );
                }
                if arm_relay_monitor {
                    Self::arm_relay_arm_failure_monitor(mb, cmd_tx, &session_id);
                }

                // A real callee answered — any in-progress transfer is over.
                self.meta.transfer_in_progress = false;
                self.sync_rtp_timeout_pause();

                self.media.callee_answer_sdp = Some(callee_sdp_value.clone());
                return Ok(caller_answer);
            }
        }

        let Some(callee_sdp_value) = callee_sdp else {
            if callee_sdp_type == rustrtc::SdpType::Answer && self.media.early_media_sent {
                debug!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    "Final 200 OK has no SDP; promoting the existing early-media descriptions to Answer"
                );

                let (caller_pc, callee_pc) = if self.media_profile.path == MediaPathMode::Anchored {
                    let caller_pc = self.media_leg(&LegId::from("caller")).map(|peer| peer.pc().clone());
                    let callee_pc = self.media_leg(&LegId::from("callee")).map(|peer| peer.pc().clone());
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

        if self
            .caller_dialog
            .as_ref()
            .is_some_and(|d| d.state().is_confirmed())
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

        let callee_sdp = Some(callee_sdp_value);
        let caller_answer = if self.media_profile.path == MediaPathMode::Anchored {
            if let Some(peer) = self.media_leg(&LegId::from("callee")) {
                peer.apply_sdp(callee_sdp.as_deref().unwrap(), callee_sdp_type).await?;
            }
            self.ensure_caller_leg().await?;
            self.media.answer.clone()
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

    pub(crate) async fn find_audio_receiver_track(
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

    fn resolve_effective_codecs(&self) -> Vec<CodecType> {
        if !self.context.dialplan.allow_codecs.is_empty() {
            return self.context.dialplan.allow_codecs.clone();
        }

        if let Some(codecs) = self.match_destination_trunk_codecs() {
            return codecs;
        }

        if let Some(ref codecs) = self.server.proxy_config.load().audio_codecs {
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
        let caller_mode = self.caller_transport_mode();
        let callee_mode = self.callee_transport_mode(callee_is_webrtc);
        self.legs
            .set_transport(LegId::from("caller"), caller_mode.clone());
        self.legs
            .set_transport(LegId::from("callee"), callee_mode.clone());

        if self.media_profile.path == MediaPathMode::Anchored {
            // Ensure caller leg exists
            self.ensure_caller_leg().await?;

            let (leg, mut sdp) = self.create_leg_peer(&LegId::from("callee"), callee_mode).await?;
            self.legs.set_media_leg(&LegId::from("callee"), leg);

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

            return Ok(sdp);
        }

        let (peer, sdp) = self.create_leg_peer(&LegId::from("callee"), callee_mode).await?;
        self.legs.set_media_leg(&LegId::from("callee"), peer);
        Ok(sdp)
    }

    async fn ensure_caller_answer_sdp(&mut self) -> Option<String> {
        if let Some(answer) = self.media.answer.clone() { return Some(answer); }
        if self.bypasses_local_media() {
            if let Some(answer) = self.media.callee_answer_sdp.clone() {
                self.media.answer = Some(answer.clone());
                return Some(answer);
            }
        }
        if let Err(error) = self.ensure_caller_leg().await {
            warn!(session_id = %self.id, %error, "Failed to prepare caller media");
            return None;
        }
        self.media.answer.clone()
    }

    pub async fn accept_call(&mut self, callee: Option<String>, sdp: Option<String>) -> Result<()> {
        self.meta.connected_callee = callee.clone();
        // A real callee/agent leg answered. Record this permanently so the
        // queue-abandon detector can tell "served then hung up" apart from
        // "hung up while still waiting" even after this leg terminates.
        if callee.is_some() {
            self.meta.ever_connected_callee = true;
        }

        let mut timer_headers = vec![];
        if self
            .server
            .proxy_config
            .load()
            .session_timer_mode()
            .is_enabled()
        {
            let default_expires = self
                .server
                .proxy_config
                .load()
                .session_expires
                .unwrap_or(DEFAULT_SESSION_EXPIRES);
            match self.init_server_timer(default_expires) {
                Ok(()) => {
                    let caller_dialog_id = self.caller_dialog_id();
                    if let Some(timer) = self.timers.get(&caller_dialog_id) {
                        if timer.enabled {
                            timer_headers.extend(build_session_timer_response_headers(timer));
                            debug!(session_id = %self.id,
                                session_expires = timer.session_interval.as_secs(),
                                refresher = ?timer.refresher,
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

        // Caller media setup is complete and the final response is ready.
        // The answer timing installs its recorder immediately before 200 OK.
        let auto_start_on_answer = {
            let recording = &self.context.dialplan.recording;
            recording.enabled
                && recording.auto_start
                && recording.auto_start_at == crate::config::RecordingAutoStartAt::Answer
        };
        if auto_start_on_answer && let Err(error) = self.set_auto_recorder().await {
            warn!(session_id = %self.id, %error, "Auto recorder installation at final answer failed");
        }
        self.maybe_autostart_live_transcription("call_answer").await;

        if let Some(answer_sdp) = answer_sdp {
            let mut headers = Self::sdp_headers();
            headers.extend(timer_headers);
            if let Some(dialog) = self.caller_dialog.as_ref() {
                if let Err(e) = dialog.accept(Some(headers), Some(answer_sdp.into_bytes())) {
                    if !self
                        .caller_dialog
                        .as_ref()
                        .is_some_and(|d| d.state().is_confirmed())
                    {
                        return Err(anyhow!("Failed to send answer: {}", e));
                    }
                }
            }
        }

        // A single-leg application does not arm the bridge RTP watchdog.
        if let Some(peer) = self.media_leg(&LegId::from("caller")) { peer.accept(); }

        // A call has been answered (real callee or app) — any in-progress
        // blind transfer is over. Reconcile the watchdog: it is suppressed
        // again below only if an app is still driving the session.
        self.meta.transfer_in_progress = false;
        self.sync_rtp_timeout_pause();

        // Record the "Call answered" trace exactly once per logical answer.
        // A repeated self/app answer (no callee) — e.g. `auto_answer` on app
        // start combined with the app's own `ctrl.answer()` in `on_enter` —
        // must not append a duplicate event. A callee/agent answer always
        // records and carries agent detail so the trace shows who picked up.
        let answer_is_new = self.meta.answer_time.is_none() || callee.is_some();
        self.meta.answer_time = Some(Instant::now());
        if answer_is_new {
            let mut ev = crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Answer,
                "Call answered",
            )
            .severity(crate::call_errors::ErrSeverity::Info);
            if callee.is_some() {
                // Agent identity: prefer the routing-layer `resolved_agent_id`
                // (set by resolve_custom_targets / CC routing), otherwise fall
                // back to the connected callee's user part.
                let resolved_agent_id = self
                    .session_ext_get("resolved_agent_id")
                    .unwrap_or_default();
                let connected_callee = self.meta.connected_callee.clone();
                let agent_id = if !resolved_agent_id.is_empty() {
                    resolved_agent_id
                } else {
                    connected_callee
                        .as_deref()
                        .and_then(extract_sip_username)
                        .unwrap_or_else(|| connected_callee.clone().unwrap_or_default())
                };
                let mut detail = serde_json::json!({
                    "callee": connected_callee.clone().unwrap_or_default(),
                    "agent_id": agent_id,
                    "queue_name": self.meta.queue_name.clone().unwrap_or_default(),
                });
                if detail["agent_id"] == serde_json::Value::String(String::new()) {
                    detail.as_object_mut().map(|m| m.remove("agent_id"));
                }
                ev = ev.detail(detail);
            }
            self.record_trace(ev);
        }
        // The INVITE final status for an answered call is the 2xx that
        // established it (200 in practice). Lock it so later signaling (BYE,
        // re-INVITE failures, transfer failures) cannot change the CDR status.
        self.meta.invite_final_status.get_or_insert(200);

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

        // A caller-only answer (queue/IVR hold media) is not a two-leg
        // connection. Dynamic agent legs fire this hook at LegConnected.
        // Check the connected callee rather than app runtime state, which
        // can race with app startup and says nothing about agent answer.
        if self.meta.connected_callee.is_some() && !self.server.session_hooks.is_empty() {
            let ctx = self.session_hook_ctx();
            for hook in self.server.session_hooks.iter() {
                hook.on_call_connected(&ctx).await;
            }
            self.sync_agent_context_to_rwi_meta();
        }
        if !self.app_runtime.is_running() && !self.answered_event_emitted {
            self.answered_event_emitted = true;
            self.emit_typed_rwi_event(&crate::rwi::CallAnswered {
                leg_id: None,
                call_id: self.context.session_id.clone(),
            });
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
        // direction is sendrecv (some endpoints use this convention). WebRTC
        // uses the same zero C-line with ICE for active media, so do not treat
        // an ICE media section as held.
        if let Some(offer) = offer {
            for section in offer
                .media_sections
                .iter()
                .filter(|section| section.kind == rustrtc::MediaKind::Audio)
            {
                if section.port == 0 {
                    return true;
                }
                if Self::section_has_zero_hold_connection(
                    section,
                    offer.session.connection.as_deref(),
                ) {
                    return true;
                }
            }
        }
        false
    }

    async fn get_local_reinvite_pc(&self, side: DialogSide) -> Option<rustrtc::PeerConnection> {
        let id = match side {
            DialogSide::Caller => LegId::from("caller"),
            DialogSide::Callee => LegId::from("callee"),
        };
        self.media_leg(&id).map(|peer| peer.pc().clone())
    }

    async fn build_local_answer_from_pc(
        pc: &rustrtc::PeerConnection,
        offer_sdp: &str,
        video_caps: Option<&[rustrtc::VideoCapability]>,
    ) -> Result<String> {
        let offer = Self::parse_sdp(rustrtc::SdpType::Offer, offer_sdp, "re-INVITE offer")?;
        let has_video = offer
            .media_sections
            .iter()
            .any(|section| section.kind == rustrtc::MediaKind::Video);
        pc.set_remote_description(offer)
            .await
            .map_err(|e| anyhow!("Failed to apply re-INVITE offer: {}", e))?;

        if has_video && let Some(first_cap) = video_caps.and_then(|caps| caps.first()) {
            crate::media::leg::ensure_video_sender_for_pc(pc, first_cap)?;
        }

        let answer = pc
            .create_answer()
            .await
            .map_err(|e| anyhow!("Failed to create re-INVITE answer: {}", e))?;

        let answer = if let Some(caps) = video_caps {
            let rewritten = MediaNegotiator::rewrite_video_capabilities(
                rustrtc::SdpType::Answer,
                &answer.to_sdp_string(),
                caps,
            )
            .map_err(|error| anyhow!("failed to build re-INVITE video answer: {error}"))?;
            Self::parse_sdp(
                rustrtc::SdpType::Answer,
                &rewritten,
                "filtered re-INVITE answer",
            )?
        } else {
            answer
        };

        pc.set_local_description(answer)
            .map_err(|e| anyhow!("Failed to set re-INVITE local answer: {}", e))?;

        pc.local_description()
            .map(|desc| desc.to_sdp_string())
            .ok_or_else(|| anyhow!("PeerConnection has no local description after re-INVITE"))
    }

    /// Returns `true` when the connection C-line value represents a "zero" address,
    /// commonly used to signal media hold per RFC 4317.
    fn is_zero_connection(c: &str) -> bool {
        let trimmed = c.trim();
        trimmed == "IN IP4 0.0.0.0" || trimmed == "IN IP6 ::" || trimmed == "IN IP6 0:0:0:0:0:0:0:0"
    }

    fn section_has_zero_hold_connection(
        section: &rustrtc::MediaSection,
        session_connection: Option<&str>,
    ) -> bool {
        let uses_ice = section.protocol.contains("UDP/TLS")
            || section
                .attributes
                .iter()
                .any(|attribute| matches!(attribute.key.as_str(), "ice-ufrag" | "candidate"));
        !uses_ice
            && section
                .connection
                .as_deref()
                .or(session_connection)
                .is_some_and(Self::is_zero_connection)
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
                if Self::section_has_zero_hold_connection(s, offer.session.connection.as_deref()) {
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
        method: rsipstack::sip::Method,
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
        let offer_video_active = offer_video_section.is_some_and(|section| {
            section.port != 0 && section.direction != rustrtc::Direction::Inactive
        });
        let mut offered_video_caps = offer_video_active.then(|| {
            if self.video_relay_enabled() {
                self.video_caps_from_sdp(offer_sdp)
            } else {
                Vec::new()
            }
        });

        let leg_key = match side {
            DialogSide::Caller => LegId::from("caller"),
            DialogSide::Callee => LegId::from("callee"),
        };
        let had_video = self.legs.leg_has_video(&leg_key);
        if offered_video_caps
            .as_ref()
            .is_some_and(|caps| !caps.is_empty())
        {
            let peer_key = match side {
                DialogSide::Caller => LegId::from("callee"),
                DialogSide::Callee => LegId::from("caller"),
            };
            if !self.legs.leg_has_video(&peer_key) {
                let accepted_by_peer = self
                    .negotiate_added_video_with_peer(
                        side,
                        method,
                        offered_video_caps.as_deref().unwrap_or_default(),
                    )
                    .await?;
                offered_video_caps = Some(accepted_by_peer);
            } else if let Some(peer_profile) = self
                .media
                .bridge
                .as_ref()
                .and_then(|_| match side {
                    DialogSide::Caller => self.media_leg(&LegId::from("callee")),
                    DialogSide::Callee => self.media_leg(&LegId::from("caller")),
                })
                .and_then(|leg| leg.negotiated())
            {
                let peer_video = peer_profile.video;
                if let Some(caps) = offered_video_caps.as_mut() {
                    caps.retain(|offered_cap| {
                        peer_video.iter().any(|peer_cap| {
                            offered_cap.codec_name.eq_ignore_ascii_case(&peer_cap.name)
                                && offered_cap.clock_rate == peer_cap.clock_rate
                        })
                    });
                }
            }
        }
        let accepted_video_active = offered_video_caps
            .as_ref()
            .is_some_and(|caps| !caps.is_empty());
        // Capture the active codec before applying the re-INVITE. If the new
        // offer still contains it, keep that codec instead of allowing a
        // reordered m-line to force an unnecessary transcode path.
        let previous_negotiated_codec = self
            .media
            .bridge
            .as_ref()
            .and_then(|_| match side {
                DialogSide::Caller => self.media_leg(&LegId::from("caller")),
                DialogSide::Callee => self.media_leg(&LegId::from("callee")),
            })
            .and_then(|leg| leg.negotiated())
            .and_then(|profile| profile.audio.map(|codec| codec.codec));

        // Track whether this leg accepted active video. A later re-INVITE may
        // transition the leg from audio-only to video and create the video
        // transceiver while building the local answer below.
        self.legs.set_video_state(&leg_key, accepted_video_active);
        if accepted_video_active && !had_video {
            if let Some(video_codec) = offered_video_caps.as_ref().and_then(|caps| caps.first()) {
                info!(session_id = %self.id,
                    "Dynamically adding video m-line (codec={}, PT={}, clock={}) for leg {:?}",
                    video_codec.codec_name, video_codec.payload_type, video_codec.clock_rate, side
                );
            }
        }

        let pc = self
            .get_local_reinvite_pc(side)
            .await
            .ok_or_else(|| anyhow!("No local PeerConnection available for {:?}", side))?;
        let mut answer_sdp =
            Self::build_local_answer_from_pc(&pc, offer_sdp, offered_video_caps.as_deref()).await?;
        if has_audio {
            let (preferred_peer_sdp, context) = match side {
                DialogSide::Caller => (
                    self.media.callee_answer_sdp.as_deref(),
                    "caller re-INVITE answer",
                ),
                DialogSide::Callee => (self.media.answer.as_deref(), "callee re-INVITE answer"),
            };
            answer_sdp = self.rewrite_answer_to_selected_audio_codecs(
                &answer_sdp,
                offer_sdp,
                preferred_peer_sdp,
                previous_negotiated_codec,
                context,
            );
        }

        // Align answer direction with offer per RFC 3264 §5.1
        answer_sdp = Self::align_answer_direction_with_offer(offer_sdp, &answer_sdp);

        // Refresh the peer's negotiated profile from the re-INVITE answer.
        // The session updates routing after applying the hold transition,
        // using the renegotiated codec instead of the call-setup profile.
        // Otherwise relay rules / RTCP relay generation can stay wrong (and
        // re-accumulate) after a mid-call codec change.
        if self.media.bridge.is_some() {
            let side_leg = match side {
                DialogSide::Caller => self.media_leg(&LegId::from("caller")),
                DialogSide::Callee => self.media_leg(&LegId::from("callee")),
            };
            if let Some(leg) = side_leg {
                leg.refresh_observer();
                if let Err(error) = leg.apply_profile_from_sdp(&answer_sdp).await {
                    warn!(
                        session_id = %self.id,
                        %error,
                        "Failed to synchronize MediaBridge leg codec after re-INVITE"
                    );
                }
            }
        }

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
        self.update_snapshot_cache();
        Ok(answer_sdp)
    }

    // ── Hold/Unhold propagation helpers ──

    /// Called when caller initiates hold (sendonly/inactive).
    /// Propagate a hold to a side: updates leg state, sends a hold re-INVITE
    /// (media bypass) or starts hold music on its peer. `override_music`,
    /// if `Some`, is used instead of the normal header/extension/config chain.
    async fn propagate_hold_to_side(
        &mut self,
        side: crate::media::media_bridge::LegSide,
        request_headers: &[rsipstack::sip::Header],
        override_music: Option<crate::call::domain::MediaSource>,
    ) -> Result<()> {
        let leg_key = if matches!(side, crate::media::media_bridge::LegSide::B) {
            "callee"
        } else {
            "caller"
        };
        info!(session_id = %self.id, %leg_key, "Propagating hold");

        self.update_leg_state(&LegId::from(leg_key), LegState::Hold);

        let music = override_music.or_else(|| self.resolve_hold_music(request_headers));
        let session_id = self.id.clone();

        if let Some(peer) = self.media_leg(&LegId::from(leg_key)) {
            if self.media_side_for_leg(peer.id()).is_some() {
                if let Some(mb) = self.bridge_mut() { mb.unbridge().await?; }
            }
            peer.pause_rtp_timeout();
            if let Some(music) = music {
                let path = match &music {
                    crate::call::domain::MediaSource::File { path } => path.clone(),
                    crate::call::domain::MediaSource::Url { url } => url.clone(),
                    _ => {
                        warn!(session_id = %session_id, "Unsupported hold music source type");
                        peer.set_egress_source(crate::media::egress::EgressSource::Silence).await?;
                        return Ok(());
                    }
                };
                let resolved = if path.starts_with("http://") || path.starts_with("https://") {
                    path
                } else {
                    Self::resolve_audio_file_path(&path)
                };
                match crate::media::audio_source::FileAudioSource::new(resolved.clone(), true).await {
                    Ok(audio) => {
                        peer.play_media(Box::new(audio), true).await?;
                        self.record_play_start(
                            format!("hold-music-{}", leg_key),
                            format!("hold music ({})", leg_key),
                        );
                    }
                    Err(e) => {
                        warn!(session_id = %session_id, %leg_key, path = %resolved, error = %e,
                            "Hold music failed to load, falling back to silence");
                        peer.set_egress_source(crate::media::egress::EgressSource::Silence).await?;
                    }
                }
            } else {
                peer.set_egress_source(crate::media::egress::EgressSource::Silence).await?;
            }
        } else {
            let hold_sdp = self.generate_sdp_for_side(&LegId::from(leg_key), true)?;
            if matches!(side, crate::media::media_bridge::LegSide::B) {
                if let Some(response_sdp) = self.send_reinvite_to_callee_dialogs(&hold_sdp).await? {
                    self.media.callee_answer_sdp = Some(response_sdp);
                }
            } else if let Err(e) = self
                .send_reinvite_to_leg(&LegId::from("caller"), hold_sdp)
                .await
            {
                warn!(session_id = %self.context.session_id, error = %e, "Failed to send hold re-INVITE to caller");
            }
        }
        Ok(())
    }

    async fn propagate_unhold_to_side(
        &mut self,
        side: crate::media::media_bridge::LegSide,
    ) -> Result<()> {
        let leg_key = if matches!(side, crate::media::media_bridge::LegSide::B) {
            "callee"
        } else {
            "caller"
        };
        info!(session_id = %self.id, %leg_key, "Propagating unhold");
        self.update_leg_state(&LegId::from(leg_key), LegState::Connected);
        if let Some(peer) = self.media_leg(&LegId::from(leg_key)) {
            // Restoring the route replaces hold playback with live media.
            // A separate stop could silence a relay that is already active.
            peer.resume_rtp_timeout();
            self.update_media_path().await;
        } else {
            let unhold_sdp = self.generate_sdp_for_side(&LegId::from(leg_key), false)?;
            if matches!(side, crate::media::media_bridge::LegSide::B) {
                if let Some(response_sdp) =
                    self.send_reinvite_to_callee_dialogs(&unhold_sdp).await?
                {
                    self.media.callee_answer_sdp = Some(response_sdp);
                }
            } else if let Err(e) = self
                .send_reinvite_to_leg(&LegId::from("caller"), unhold_sdp)
                .await
            {
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

        // Cross-leg hold propagation only applies when the proxy anchors media
        // (a MediaBridge is present). In bypass mode the peer already received
        // the relayed offer — this hold UPDATE was forwarded to the callee
        // as-is and answered there — so propagating hold with a separate
        // re-INVITE would push a redundant/conflicting offer to the peer and
        // wait for an answer that may never come, hanging the dialog.
        if self.bypasses_local_media() {
            return;
        }

        // Cross-leg propagation
        match side {
            DialogSide::Caller => {
                let callee_prev = self.leg_prev_state(&LegId::from("callee"));
                let callee_transition = match (callee_prev, new_state) {
                    (Some(LegState::Hold), LegState::Connected) => Some(false),
                    (Some(LegState::Connected), LegState::Hold) => Some(true),
                    _ => None,
                };
                if let Some(is_hold) = callee_transition {
                    if is_hold {
                        if let Err(e) = self
                            .propagate_hold_to_side(LegSide::B,
                                request_headers,
                                None,
                            )
                            .await
                        {
                            warn!(session_id = %self.id, error = %e, "Failed to propagate hold to callee");
                        }
                    } else if let Err(e) = self
                        .propagate_unhold_to_side(LegSide::B)
                        .await
                    {
                        warn!(session_id = %self.id, error = %e, "Failed to propagate unhold to callee");
                    }
                }
            }
            DialogSide::Callee => {
                let caller_prev = self.leg_prev_state(&LegId::from("caller"));
                let caller_transition = match (caller_prev, new_state) {
                    (Some(LegState::Hold), LegState::Connected) => Some(false),
                    (Some(LegState::Connected), LegState::Hold) => Some(true),
                    _ => None,
                };
                if let Some(is_hold) = caller_transition {
                    if is_hold {
                        if let Err(e) = self
                            .propagate_hold_to_side(LegSide::A,
                                request_headers,
                                None,
                            )
                            .await
                        {
                            warn!(session_id = %self.id, error = %e, "Failed to propagate hold to caller");
                        }
                    } else if let Err(e) = self
                        .propagate_unhold_to_side(LegSide::A)
                        .await
                    {
                        warn!(session_id = %self.id, error = %e, "Failed to propagate unhold to caller");
                    }
                }
            }
        }
    }

    /// Resolve hold music source by priority:
    /// 1. X-Hold-Music header in the re-INVITE request
    /// 2. X-Hold-Music in session extensions (from initial INVITE / CC addon,
    ///    e.g. skill-group `metadata.hold_music` injected by the CC hook)
    /// 3. PBX default from ProxyConfig (`[proxy].hold_music`)
    /// 4. Built-in default hold audio (sounds/phone-calling.wav)
    pub(crate) fn resolve_hold_music(
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
        if let Some(val) = self.session_ext_get("X-Hold-Music") {
            return Some(Self::parse_hold_music_value(&val));
        }
        // 3. PBX default
        if let Some(path) = &self.server.proxy_config.load().hold_music {
            return Some(crate::call::domain::MediaSource::File { path: path.clone() });
        }
        // 4. Built-in default so the held party always hears hold audio.
        Some(crate::call::domain::MediaSource::File {
            path: crate::call::DEFAULT_QUEUE_HOLD_AUDIO.to_string(),
        })
    }

    fn parse_hold_music_value(value: &str) -> crate::call::domain::MediaSource {
        let trimmed = value.trim().to_string();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            crate::call::domain::MediaSource::Url { url: trimmed }
        } else {
            crate::call::domain::MediaSource::File { path: trimmed }
        }
    }

    /// Generate hold (sendonly) / unhold (sendrecv) SDP for a side, reusing
    /// that side's last negotiated SDP (answer, else offer).
    fn generate_sdp_for_side(&self, side: &LegId, sendonly: bool) -> Result<String> {
        let direction = if sendonly { "sendonly" } else { "sendrecv" };
        self.sdp_for_side_with_direction(side, direction)
    }

    /// The side's last negotiated SDP (answer, else offer) rewritten to
    /// `direction`. Used when a relayed re-INVITE offer's transport style
    /// (RTP vs WebRTC) doesn't match what the target leg originally
    /// negotiated — the leg's own SDP is the only style it can process.
    fn sdp_for_side_with_direction(&self, side: &LegId, direction: &str) -> Result<String> {
        if let Some(peer) = self.legs.media_leg(side) {
            if let Some(sdp) = peer.pc().local_description() {
                return Ok(rustrtc::modify_sdp_direction(&sdp.to_sdp_string(), direction));
            }
        }
        let base_sdp = if side.0 == "callee" {
            self.media
                .callee_answer_sdp
                .as_deref()
                .or(self.media.callee_offer.as_deref())
        } else {
            self.media
                .answer
                .as_ref()
                .or(self.media.caller_offer.as_ref())
                .map(|s| s.as_str())
        }
        .ok_or_else(|| anyhow!("No SDP available for {} hold/unhold", side.0))?;
        Ok(rustrtc::modify_sdp_direction(base_sdp, direction))
    }

    /// RFC 3264 direction translation across the media anchor:
    /// the relaying leg's offer expresses its send/receive intent; the target
    /// leg's offer must express the complementary intent.
    fn cross_leg_direction(offer_sdp: &str) -> &'static str {
        let parsed = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, offer_sdp).ok();
        let dir = parsed
            .as_ref()
            .and_then(|d| {
                d.media_sections
                    .iter()
                    .find(|s| s.kind == rustrtc::MediaKind::Audio)
            })
            .map(|s| s.direction);
        match dir {
            Some(rustrtc::Direction::SendOnly) => "recvonly",
            Some(rustrtc::Direction::RecvOnly) => "sendonly",
            Some(rustrtc::Direction::Inactive) => "inactive",
            _ => "sendrecv",
        }
    }

    /// WebRTC-style SDP (DTLS fingerprint / TLS-SRTP profile / setup attr).
    /// Bypass-mode relay assumes both legs speak the same style; a mismatch
    /// means the peer cannot process the offer verbatim.
    fn offer_is_webrtc(sdp: &str) -> bool {
        sdp.contains("a=fingerprint")
            || sdp.contains("UDP/TLS/RTP/SAVP")
            || sdp.contains("a=setup:")
    }
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
                    // Keep the per-leg snapshots in sync with the forwarded
                    // re-INVITE so session refreshes reuse the latest
                    // negotiated SDP (same rationale as the bypass relay).
                    self.media.callee_offer = Some(offer_sdp.clone());
                    self.media.callee_answer_sdp = Some(answer_sdp.clone());
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

    /// Address advertised for a finished local recording in RWI events. The
    /// CDR pipeline archives pipeline-generated WAVs into
    /// `{root}/{YYYYMMDD}[/{HH}]/` right after session teardown (and before
    /// the CDR row is persisted), so events must carry the final archived
    /// location instead of the transient pre-archive path. Mirrors
    /// `RecordingUploadHook::archive_local_artifacts`: same root, same
    /// daily/hourly layout, date derived from the call start time (not the
    /// recording moment), and operator-supplied custom paths outside the
    /// root stay untouched. Falls back to the original path when no policy
    /// is active, the media type never archives (`http`/`sipflow`), or the
    /// call start time cannot be resolved — in every fallback the renamer
    /// also leaves the file in place, so the address stays valid.
    pub(crate) fn predicted_recording_event_path(&self, path: &str) -> String {
        let policy_guard = self.server.recording_policy.load();
        let Some(policy) = policy_guard.as_ref() else {
            return path.to_string();
        };
        match policy.effective_recording_type() {
            crate::config::RecordingType::Local | crate::config::RecordingType::S3 => {}
            crate::config::RecordingType::Http | crate::config::RecordingType::Sipflow => {
                return path.to_string();
            }
        }
        let Ok(start) = chrono::DateTime::parse_from_rfc3339(&self.context.created_at)
            .map(|t| t.with_timezone(&chrono::Utc))
        else {
            return path.to_string();
        };
        let subdir = crate::callrecord::RecordingSubdir::parse(policy.subdir.as_deref());
        crate::callrecord::predicted_archive_path(
            &policy.recorder_path(),
            Path::new(path),
            subdir,
            start,
        )
    }

    pub(crate) fn publish_recording_complete(
        &mut self,
        result: crate::media::media_recorder::RecordingResult,
    ) {
        let (notify_app, unique_id) = match self.finalize_active_recording_segment(&result) {
            Some(pair) => pair,
            None => {
                // Discarded segment (e.g. outbound ringback slice on an
                // answered call): the file is already gone and no CDR entry
                // was registered — suppress every recording event.
                info!(
                    session_id = %self.id,
                    path = %result.path,
                    "Recording segment discarded; no events emitted"
                );
                return;
            }
        };
        let path = result.path;
        let duration = Duration::from_secs_f64(result.duration_secs);
        let file_size = result.file_size;
        info!(session_id = %self.id, path = %path, unique_id = ?unique_id, duration = ?duration, file_size, "Recording stopped");
        let info = crate::call::app::RecordingInfo {
            path,
            duration,
            size_bytes: file_size,
        };
        if notify_app {
            let _ = self.app_event_bridge.send_app_event(
                crate::call::app::ControllerEvent::RecordingComplete(info.clone()),
            );
        }
        if let Some(gateway) = self.server.rwi_gateway.as_ref() {
            let call_id = self.context.session_id.clone();
            let meta = gateway.read().meta_store.get_sync(&call_id);
            let (caller_name, callee_name) = match meta {
                Some(ref meta) => (meta.caller_name.clone(), meta.callee_name.clone()),
                None => (None, None),
            };
            let reported_path = self.predicted_recording_event_path(&info.path);
            gateway.read().send_to_owner(&crate::rwi::RecordStopped {
                call_id: call_id.clone(),
                duration_secs: Some(info.duration.as_secs()),
                filename: Some(reported_path.clone()),
                unique_id,
                file_size: Some(info.size_bytes),
                download_url: Some(reported_path),
                caller_name,
                callee_name,
                called_phone: None,
                call_type: None,
                agent_id: None,
                agent_name: None,
                call_start_time: None,
                call_end_time: None,
                upload_time: None,
                switch_flag: None,
            });
        }
    }

    /// Called from the caller-hangup path: if an app is recording when the
    /// caller hangs up, finalize the recording and emit `RecordingComplete` so
    /// the running app's `on_record_complete` (e.g. voicemail message
    /// persistence) runs before the event loop is cancelled. Without this, a
    /// caller leaving a voicemail and hanging up would lose the message
    /// entirely — the primary voicemail use case.
    ///
    /// `on_record_complete` spawns its persistence work, so we only need to
    /// give the app event loop a brief, bounded window to execute it once.
    pub(crate) async fn finalize_recording_for_app_shutdown(&mut self) {
        if !self.media.recording.has_recorder_task() { return; }
        let outcome = self.media.recording.stop_recording().await;
        let completed = match outcome {
            Ok(Some(result)) => {
                self.publish_recording_complete(result);
                true
            }
            Ok(None) => false,
            Err(error) => {
                warn!(
                    session_id = %self.id,
                    %error,
                    "Failed to finalize recording on caller hangup"
                );
                return;
            }
        };
        // Only grant the event loop a grace when an app is actually running to
        // receive the RecordingComplete we just emitted. Call-level auto
        // recording has no app, so we must not delay session teardown (which
        // would also delay CDR generation) in that case.
        if completed && self.app_runtime.is_running() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn cleanup(&mut self) {
        trace!(session_id = %self.context.session_id, "Cleaning up session");

        // Cancel the session's token FIRST so every child token (leg forwarders,
        // dialog monitors, conference bridges, DTMF forwarders, bridge loops)
        // is signalled to stop immediately. Otherwise those tasks keep running
        // until the session object is dropped — a leak window between cleanup
        // and Drop.
        self.cancel_token.cancel();

        // Stop live transcription (closes ASR streams, emits transcript_ended).
        self.stop_live_transcription("call_ended").await;

        // Flush any in-flight media plays that finished naturally (without an
        // explicit stop) so the trace shows their full duration + completion.
        let leftover: Vec<String> = self.active_plays.keys().cloned().collect();
        for track_id in leftover {
            self.record_play_end(&track_id, false);
        }

        // The call has entered terminal cleanup. Release tenant, carrier, and
        // trunk concurrent-call permits before any potentially slow cleanup.
        self.concurrent_call_lease.release_all();
        // Release leases acquired by routed app/transfer/originate legs.
        for lease in self.transient_leases.drain(..) {
            lease.release_all();
        }

        // Disarm any RTP inactivity timeouts on both legs.
        if let Some(mb) = self.media.bridge.as_mut() {
            mb.disarm_rtp_timeout(LegSide::A);
            mb.disarm_rtp_timeout(LegSide::B);
        }

        // Ensure the running app (IVR/voicemail/queue) is notified of session end.
        if self.app_runtime.is_running() {
            let _ = self.app_runtime.stop_app(None).await;
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

        // Resolve the final hangup reason BEFORE the CDR snapshot is reported:
        // enrich with IVR end reason and queue abandon detection so the call
        // record (trace + hangup reason), session hooks and RWI webhook all
        // carry the same, accurate terminal outcome.
        self.resolve_final_hangup_reason().await;

        // Finalize any in-flight recording before the CDR snapshot: flush the
        // WAV file and emit RecordStopped. Explicit `record.stop` paths come
        // here as a harmless no-op (stop_recording on an already-stopped
        // bridge returns Ok(None)). This covers sessions whose hangup never
        // runs the dialog-terminated finalize path — notably RWI-originated
        // (UAC) calls, whose caller dialog state channel is not wired into the
        // main loop.
        if self.media.recording.has_recorder_task() {
            match self.media.recording.stop_recording().await {
                Ok(Some(result)) => self.publish_recording_complete(result),
                Ok(None) => {}
                Err(e) => {
                    warn!(session_id = %self.id, error = %e, "Failed to finalize recording on cleanup");
                }
            }
        }

        if let Some(reporter) = &self.reporter {
            let snapshot = self.record_snapshot();
            reporter.report(snapshot);
            self.cdr_sent
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        // Fire on_call_ended hooks.
        let ended_duration_secs = if !self.server.session_hooks.is_empty() {
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
            // Hooks may have published the agent attribution — make it
            // visible to event enrichment before `call_hangup` is emitted.
            self.sync_agent_context_to_rwi_meta();
            duration_secs
        } else {
            self.meta
                .answer_time
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0)
        };

        // Emit hangup webhook with Display (lowercase) reason and actual SIP status.
        let hangup_reason_str = self.meta.hangup_reason.clone().map(|r| r.to_string());
        // `initiator()` maps any callee hangup to "agent" (contact-center
        // centric). When no CC agent actually participated (no queue routing
        // and no resolved_agent_id), report "callee" so non-CC calls are not
        // mislabeled as agent-driven.
        let has_resolved_agent = self.session_ext_get("resolved_agent_id").is_some();
        let queue_name = self.meta.queue_name.clone();
        let hangup_by = self
            .meta
            .hangup_reason
            .as_ref()
            .map(|r| r.initiator().to_string())
            .map(|h| normalize_call_hangup_by(&h, queue_name.as_deref(), has_resolved_agent));
        let sip_status = self.meta.last_error.as_ref().map(|(sc, _)| sc.code());
        self.emit_typed_rwi_event(&crate::rwi::CallHangup {
            leg_id: None,
            call_id: self.context.session_id.clone(),
            reason: hangup_reason_str,
            hangup_by,
            sip_status,
            duration_secs: Some(ended_duration_secs),
        });

        // Tear down the transport-level RTP rewrite bridge (fast-path relay)
        // on both legs BEFORE destroying the engine session / closing the
        // PeerConnections. The fast-path relay runs inline on the media worker
        // (RtpTransport::receive → RewriteBridge → peer IceConn). If it is still
        // armed when the peer PeerConnection is torn down, the relay can race
        // with PC close and dereference cleared/freed state, causing a media-
        // worker segfault (observed "media-worker segfault at 3 ip 0x3" under
        // sustained ~900-concurrent load after ~35min). Clearing is idempotent.
        for id in self.legs.keys() {
            if let Some(peer) = self.media_leg(id) {
                peer.pc().clear_rtp_rewrite_bridge();
            }
        }

        // De-register any legs that joined a conference. The ConferenceManager
        // retains participant state (leg_to_conference, mixer participant,
        // channels) keyed by leg; without an explicit removal a leg that hangs
        // up without a `conference_remove` command leaks that state (and keeps
        // the audio-mixer task alive) until the conference is destroyed.
        let conference_leg_ids: Vec<LegId> = self.legs.keys().cloned().collect();
        for leg_id in conference_leg_ids {
            if let Err(error) = self
                .server
                .conference_manager
                .remove_leg_from_all(&leg_id)
                .await
            {
                debug!(
                    session_id = %self.context.session_id,
                    %leg_id,
                    error = %error,
                    "Conference participant cleanup failed"
                );
            }
        }

        // MediaBridge teardown is handled by the session Drop / cleanup path.
        // Close it eagerly so per-leg wire_leg monitor tasks and the DTMF
        // forwarder stop now, not only when the session object drops.
        if let Some(mut mb) = self.media.bridge.take() {
            mb.close();
        }

        // RWI ownership and CallMeta are released by the guard carried in the
        // CallRecord, after every asynchronous completion hook has finished.
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
    pub(crate) async fn resolve_final_hangup_reason(&mut self) {
        // ── 1. Queue abandon catch-all ──────────────────────────────
        // Covers the CallApp-based queue path (where execute_queue is not
        // called and meta.queue_name may not be set via the SIP layer).
        // EXCLUDED: sessions already dispatched to an agent via the
        // transfer/bridge path (`MarkTransferred` marks the surviving caller
        // session there). The agent leg lives in another session, so this
        // session's `ever_connected_callee` stays false — without the
        // exclusion the caller-side teardown after a served call (including
        // the post-call CSAT survey's system hangup) would be misclassified
        // as a queue abandon. `resolved_agent_id` cannot be used here: it is
        // planted at TARGET RESOLUTION time, before the agent ever connects,
        // so genuine abandons during agent ringing carry it too.
        let in_queue = crate::proxy::proxy_call::call_meta::has_queue_name(&self.meta);
        if in_queue
            && !self.meta.transferred
            && !self.meta.ever_connected_callee
            && self.meta.connected_callee.is_none()
            && matches!(
                self.meta.hangup_reason,
                Some(CallRecordHangupReason::ByCaller)
                    | Some(CallRecordHangupReason::Canceled)
                    | Some(CallRecordHangupReason::Abandoned)
                    | None
            )
        {
            self.meta.hangup_reason = Some(CallRecordHangupReason::Abandoned);
            self.meta.error_code = Some(&crate::proxy::proxy_call::error_catalog::QUEUE_ABANDONED);
            let queue_name = crate::proxy::proxy_call::call_meta::effective_queue_name(&self.meta)
                .unwrap_or_default();
            let msg = if queue_name.is_empty() {
                "Caller abandoned the queue".to_string()
            } else {
                format!("Caller abandoned queue '{}'", queue_name)
            };
            let mut ev =
                crate::call_errors::TraceEvent::new(crate::call_errors::TraceKind::Queue, msg)
                    .severity(crate::call_errors::ErrSeverity::Warn)
                    .code(crate::proxy::proxy_call::error_catalog::QUEUE_ABANDONED.code);
            let mut detail = serde_json::json!({});
            if !queue_name.is_empty() {
                detail["queue_name"] = serde_json::Value::String(queue_name);
            }
            let resolved_agent_id = self
                .session_ext_get("resolved_agent_id")
                .unwrap_or_default();
            if !resolved_agent_id.is_empty() {
                detail["agent"] = serde_json::Value::String(resolved_agent_id);
            }
            ev = ev.detail(detail);
            self.record_trace(ev);
        }

        // ── 2. IVR end reason bridge ────────────────────────────────
        // Read ivr_end_reason from the shared session variables (written by
        // StepIvrApp::on_exit). Only terminal IVR reasons override — transfer
        // reasons are skipped because the call continued past the IVR.
        // If the RTP-inactivity watchdog fired it is the authoritative teardown
        // cause: keep `RtpTimeout` (recorded by `handle_hangup`) and skip the
        // IVR override so the RTP timeout is never masked.
        if self.meta.rtp_timeout_fired {
            return;
        }
        // A queue abandon classified above must not be reclassified by a stale
        // IVR end reason left by the IVR flow that handed the call off to the
        // queue (e.g. `ivr_end_reason = "user_hangup"`).
        if self.meta.error_code.map(|info| info.code)
            == Some(crate::proxy::proxy_call::error_catalog::QUEUE_ABANDONED.code)
        {
            return;
        }
        if let Some(ctx) = self.app_runtime.app_context() {
            let ivr_end = ctx.get_var("ivr_end_reason");
            let ivr_error = ctx.get_var("ivr_last_error");

            let ivr_override = match ivr_end.as_deref() {
                Some("normal") => {
                    self.meta.error_code = Some(&crate::call::app::error_catalog::IVR_NORMAL);
                    Some(CallRecordHangupReason::BySystem)
                }
                Some("hangup") => {
                    self.meta.error_code = Some(&crate::call::app::error_catalog::IVR_HANGUP);
                    Some(CallRecordHangupReason::BySystem)
                }
                // NOTE: SessionEndTag::UserHangup serializes as "user_hangup";
                // "remote_hangup" is a legacy value kept for compatibility.
                Some("user_hangup") | Some("remote_hangup") => {
                    self.meta.error_code = Some(&crate::call::app::error_catalog::IVR_USER_HANGUP);
                    Some(CallRecordHangupReason::ByCaller)
                }
                Some("timeout") => {
                    self.meta.error_code = Some(&crate::call::app::error_catalog::IVR_TIMEOUT);
                    Some(CallRecordHangupReason::Autohangup)
                }
                Some("error") => {
                    let msg = ivr_error.unwrap_or_else(|| "unknown ivr error".to_string());
                    self.meta.error_code =
                        Some(&crate::call::app::error_catalog::IVR_EXECUTE_ERROR);
                    Some(CallRecordHangupReason::Other(format!("ivr_error: {}", msg)))
                }
                // transfer / transfer_to_queue / transfer_to_ivr / chained /
                // cancelled — call continued; keep the SIP-layer reason.
                _ => None,
            };

            // Only record "IVR ended" for a terminal IVR outcome. For
            // continuation values (transfer/chained/cancelled) the error code
            // (if any) is stale state from earlier in the call and must not be
            // attributed to the IVR's ending.
            if let Some(reason) = ivr_override {
                if let Some(info) = self.meta.error_code {
                    self.record_trace(
                        crate::call_errors::TraceEvent::new(
                            crate::call_errors::TraceKind::Ivr,
                            format!("IVR ended: {}", info.message),
                        )
                        .severity(info.severity)
                        .code(info.code),
                    );
                }
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
        let session_timer_mode = self.server.proxy_config.load().session_timer_mode();

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
            if let Some(session_expires) = SessionExpires::parse(&value) {
                if session_expires.interval < timer.min_se {
                    return Err(into_callee_err(
                        &StatusCode::SessionIntervalTooSmall,
                        Some(timer.min_se.as_secs().to_string()),
                    ));
                }

                timer.enabled = true;
                timer.session_interval = session_expires.interval;
                timer.active = true;
                timer.refresher =
                    select_server_timer_refresher(supported, true, session_expires.refresher);
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
        timer.mode = self.server.proxy_config.load().session_timer_mode();
        if let Some(session_expires) = session_expires_value
            .as_deref()
            .and_then(SessionExpires::parse)
        {
            timer.enabled = true;
            timer.active = true;
            timer.last_refresh = Instant::now();
            timer.session_interval = session_expires.interval;
            timer.refresher = select_client_timer_refresher(session_expires.refresher);
        } else if timer.mode.is_always() {
            timer.enabled = true;
            timer.active = true;
            timer.last_refresh = Instant::now();
            timer.session_interval = requested_session_interval;
            timer.refresher = SessionRefresher::Local;
        } else {
            timer.session_interval = requested_session_interval;
        }

        self.timers.insert(dialog_id.clone(), timer);
        self.schedule_timer(dialog_id);
    }

    pub(crate) fn caller_dialog_id(&self) -> DialogId {
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
            .and_then(SessionTimerState::next_timeout);
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
        if let Some(timer) = self.timers.get_mut(dialog_id) {
            apply_refresh_response(timer, &response.headers)?;
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

    /// Append an event to the session's diagnostic trace timeline. `ts` is
    /// computed as milliseconds since the session started.
    pub fn record_trace(&mut self, event: crate::call_errors::TraceEvent) {
        let mut ev = event;
        ev.ts = self.context.start_time.elapsed().as_millis() as i64;
        self.meta.trace.push(ev);
    }

    /// Record the start of a media playback for the trace. The `track_id` is
    /// later passed to [`Self::record_play_end`] to emit a `Play` event with
    /// duration and interruption status.
    pub fn record_play_start(&mut self, track_id: impl Into<String>, source: impl Into<String>) {
        self.active_plays.insert(
            track_id.into(),
            crate::proxy::proxy_call::state::ActivePlay {
                source: source.into(),
                started_at: std::time::Instant::now(),
            },
        );
    }

    /// Record the end of a media playback: emits a `Play` trace event carrying
    /// the played duration and whether it was interrupted. Idempotent — a
    /// second call for the same `track_id` is a no-op.
    pub fn record_play_end(&mut self, track_id: &str, interrupted: bool) {
        let Some(play) = self.active_plays.remove(track_id) else {
            return;
        };
        let duration_ms = play.started_at.elapsed().as_millis() as i64;
        let message = format!(
            "Played {} · {} · {}",
            play.source,
            format_duration_ms(duration_ms),
            if interrupted {
                "interrupted"
            } else {
                "completed"
            }
        );
        self.record_trace(
            crate::call_errors::TraceEvent::new(crate::call_errors::TraceKind::Play, message)
                .duration(duration_ms)
                .interrupted(interrupted)
                .detail(serde_json::json!({ "source": play.source })),
        );
    }

    /// Record a leg lifecycle event into the CDR leg timeline (bounded —
    /// see [`LegTimeline::MAX_EVENTS`]).
    pub(crate) fn record_leg_event(
        &mut self,
        leg_id: &str,
        event_type: crate::callrecord::LegTimelineEventType,
        peer_leg_id: Option<String>,
        details: Option<serde_json::Value>,
    ) {
        self.meta
            .leg_timeline
            .add_event(leg_id.to_string(), event_type, peer_leg_id, details);
    }

    /// Mark the call as transferred away and stamp the timeline. Sticky —
    /// repeats (e.g. blind transfer then consult merge) append events but
    /// never reset the flag.
    pub(crate) fn mark_transferred_with(&mut self, details: Option<serde_json::Value>) {
        self.meta.transferred = true;
        self.record_leg_event(
            "caller",
            crate::callrecord::LegTimelineEventType::Transferred,
            None,
            details,
        );
    }

    pub fn record_snapshot(&self) -> CallSessionRecordSnapshot {
        // Merge agent + routing data into a JSON map so the reporter picks it up.
        // Agent info was written into session extensions by CcCallSessionHook
        // (as a HashMap<String, String>). Routing metadata comes from the
        // dialplan extensions (also HashMap<String, String>). Values are JSON
        // so structured entries (e.g. the `trace` array) persist cleanly.
        let extensions = self.context.dialplan.extensions.clone();
        // Per-leg media counters, captured once: they feed both the CDR
        // `media_quality` metadata and the answered-but-silent-leg detection
        // below. Empty when no bridge legs exist.
        let legs: Vec<_> = [("A", LegId::from("caller")), ("B", self.resolve_transfer_leg(LegId::from("callee")))]
            .into_iter().filter_map(|(side, id)| {
                let peer = self.media_leg(&id)?;
                Some(peer.quality_report(side))
            }).collect();
        // Answered but a leg never delivered a single media packet — the
        // "silent leg" (browser ICE/DTLS never completed, one-way NAT/UDP
        // filtering, muted softphone, carrier answering without media).
        // Diagnostic only: never changes the hangup outcome; logged once and
        // annotated into the CDR (error chip + trace) so recordings without
        // audio are explainable. Skipped when a more specific error already
        // claimed the call (e.g. the RTP watchdog's `proxy.rtp_timeout`).
        // The callee side is only checked when a real remote callee leg
        // answered (`ever_connected_callee`): IVR/app playback legs never
        // receive remote RTP, so flagging them would be noise.
        let leg_silent = |side: &str| {
            legs.iter().any(|leg| {
                leg.side == side && leg.transport_rx_packets == 0 && leg.ingress_packets == 0
            })
        };
        let caller_silent = leg_silent("A");
        let callee_silent = self.meta.ever_connected_callee && leg_silent("B");
        let media_issue_legs = match (caller_silent, callee_silent) {
            (true, true) => Some("caller+callee"),
            (true, false) => Some("caller"),
            (false, true) => Some("callee"),
            (false, false) => None,
        };
        let media_issue = self.meta.answer_time.is_some()
            && self.meta.error_code.is_none()
            && media_issue_legs.is_some();
        if media_issue {
            let legs_str = media_issue_legs.unwrap_or_default();
            let callee_rx = legs
                .iter()
                .find(|leg| leg.side == "B")
                .map(|leg| leg.transport_rx_packets);
            warn!(
                session_id = %self.context.session_id,
                call_id = %self.context.session_id,
                legs = legs_str,
                caller_rx_packets = 0u64,
                callee_rx_packets = callee_rx.unwrap_or(0),
                "answered call ended with no media on leg(s); recording will have no audio from the affected side(s) (code=proxy.leg_media_incomplete)"
            );
        }
        // The effective error code: the detected leg-media issue applies only
        // when no more specific error already claimed the call.
        let effective_error_code = if media_issue {
            Some(&crate::proxy::proxy_call::error_catalog::LEG_MEDIA_INCOMPLETE)
        } else {
            self.meta.error_code
        };
        let metadata = {
            // Start with session extensions (CC agent info)
            let mut meta: std::collections::HashMap<String, serde_json::Value> = self
                .extensions
                .read()
                .get::<std::collections::HashMap<String, String>>()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            // Merge in dialplan extensions (routing metadata)
            if let Some(route_meta) = extensions
                .get::<std::collections::HashMap<String, String>>()
                .cloned()
            {
                for (k, v) in route_meta {
                    meta.entry(k)
                        .or_insert_with(|| serde_json::Value::String(v));
                }
            }
            if let Some(ref qn) = self.meta.queue_name {
                meta.insert(
                    "queue_name".to_string(),
                    serde_json::Value::String(qn.clone()),
                );
            }
            if let Some(info) = effective_error_code {
                meta.insert(
                    "error_code".to_string(),
                    serde_json::Value::String(info.code.to_string()),
                );
                if let Some(app) = info.code.split('.').next() {
                    meta.insert(
                        "error_app".to_string(),
                        serde_json::Value::String(app.to_string()),
                    );
                }
            }
            // Which leg(s) lacked media + a human-readable detail line. The
            // reporter's error enrichment prefers `error_detail` over the
            // catalog's generic message, so the UI chip names the side too;
            // `mediaIssueLegs` stays a flat string for queryability.
            if media_issue {
                if let Some(legs_str) = media_issue_legs {
                    meta.insert(
                        "mediaIssueLegs".to_string(),
                        serde_json::Value::String(legs_str.to_string()),
                    );
                }
                let detail = match media_issue_legs {
                    Some("caller+callee") => "no media from the caller and callee legs",
                    Some("callee") => "no media from the callee leg",
                    _ => "no media from the caller leg",
                };
                meta.insert(
                    "error_detail".to_string(),
                    serde_json::Value::String(detail.to_string()),
                );
            }
            if let Some(ref app) = self.meta.app_name {
                meta.insert(
                    "app_name".to_string(),
                    serde_json::Value::String(app.clone()),
                );
            }
            if let Some(ref ql) = self.meta.queue_label {
                meta.insert(
                    "queue_label".to_string(),
                    serde_json::Value::String(ql.clone()),
                );
            }
            if let Some(ref callee) = self.meta.connected_callee {
                meta.insert(
                    "connected_callee".to_string(),
                    serde_json::Value::String(callee.clone()),
                );
            }
            if let Some(side) = self.meta.rtp_timeout_side {
                let side_str = match side {
                    crate::call::domain::RtpTimeoutSide::Caller => "caller",
                    crate::call::domain::RtpTimeoutSide::Callee => "callee",
                };
                meta.insert(
                    "rtpTimeoutSide".to_string(),
                    serde_json::Value::String(side_str.to_string()),
                );
            }
            if let Some(leg) = self.meta.rtp_timeout_leg.clone() {
                meta.insert("rtpTimeoutLeg".to_string(), serde_json::Value::String(leg));
            }
            // Call trace: ordered timeline of transitions + media plays +
            // terminal outcome. Stored as a real JSON array under `trace`.
            let mut trace: Vec<crate::call_errors::TraceEvent> = self.meta.trace.clone();
            // One-shot media-issue event so the timeline explains the missing
            // audio next to the terminal outcome.
            if media_issue {
                let detail = serde_json::json!({ "legs": media_issue_legs });
                let mut ev = crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::MediaIssue,
                    "No media received on leg(s) of an answered call (0 RTP packets)",
                )
                .severity(crate::call_errors::ErrSeverity::Warn)
                .code(crate::proxy::proxy_call::error_catalog::LEG_MEDIA_INCOMPLETE.code)
                .detail(detail);
                ev.ts = self.context.start_time.elapsed().as_millis() as i64;
                trace.push(ev);
            }
            // Terminal End event — carries the hangup initiator and any
            // standardized error code so "why did the call end" is visible.
            {
                let queue_ctx =
                    crate::proxy::proxy_call::call_meta::effective_queue_name(&self.meta)
                        .map(|q| format!(" (queue '{}')", q));
                let (severity, code, msg) = if let Some(info) = effective_error_code {
                    let base = format!("Call ended: {}", info.message);
                    (
                        info.severity,
                        Some(info.code.to_string()),
                        match &queue_ctx {
                            Some(ctx) => format!("{base}{ctx}"),
                            None => base,
                        },
                    )
                } else {
                    let reason = self
                        .meta
                        .hangup_reason
                        .as_ref()
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let base = format!("Call ended: {}", reason);
                    (
                        crate::call_errors::ErrSeverity::Info,
                        None,
                        match &queue_ctx {
                            Some(ctx) => format!("{base}{ctx}"),
                            None => base,
                        },
                    )
                };
                let end =
                    crate::call_errors::TraceEvent::new(crate::call_errors::TraceKind::End, msg)
                        .severity(severity);
                let end = match code {
                    Some(c) => end.code(&c),
                    None => end,
                };
                // The End event is pushed directly (not via record_trace), so
                // stamp its timestamp explicitly or it would render as "+0ms".
                let mut end = end;
                end.ts = self.context.start_time.elapsed().as_millis() as i64;
                trace.push(end);
            }
            if !trace.is_empty() {
                if let Ok(arr) = serde_json::to_value(&trace) {
                    meta.insert("trace".to_string(), arr);
                }
            }
            // Transfer marker: lets consumers (CSAT suppression, reports)
            // spot transferred calls without parsing the leg timeline.
            if self.meta.transferred {
                meta.insert("transferred".to_string(), serde_json::Value::Bool(true));
            }
            // Who hung up + the normalized hangup reason, so the CC call-history
            // UI can display and filter by them.
            match self.meta.hangup_reason.as_ref() {
                Some(reason) => {
                    meta.insert(
                        "hangup_reason".to_string(),
                        serde_json::Value::String(reason.to_string()),
                    );
                    meta.insert(
                        "hangup_by".to_string(),
                        serde_json::Value::String(reason.initiator().to_string()),
                    );
                }
                None => {
                    meta.insert(
                        "hangup_reason".to_string(),
                        serde_json::Value::String("unknown".to_string()),
                    );
                    meta.insert(
                        "hangup_by".to_string(),
                        serde_json::Value::String("unknown".to_string()),
                    );
                }
            }
            // Session user data (set via REST `PUT .../userdata` or the RWI
            // `call.set_userdata` command) rides into the CDR under a
            // namespaced `user_data` key so it cannot collide with the flat
            // system keys above. Safe to read here: the RwiCallRecordGuard
            // keeps gateway state alive until the record has been built.
            if let Some(ref gw) = self.server.rwi_gateway {
                let data = gw.read().get_user_data(&self.context.session_id);
                if !data.is_empty() {
                    meta.insert("user_data".to_string(), serde_json::Value::Object(data));
                }
            }
            meta
        };

        let media_quality = if legs.is_empty() {
            None
        } else {
            serde_json::to_value(&legs).ok()
        };

        CallSessionRecordSnapshot {
            ring_time: self.meta.ring_time,
            answer_time: self.meta.answer_time,
            last_error: self.meta.last_error.clone(),
            root_session_id: self.meta.root_session_id.clone(),
            invite_final_status: self.meta.invite_final_status,
            hangup_reason: self.meta.hangup_reason.clone(),
            hangup_messages: self.recorded_hangup_messages(),
            original_caller: Some(self.context.original_caller.clone()),
            original_callee: Some(self.context.original_callee.clone()),
            routed_caller: self.meta.routed_caller.clone(),
            routed_callee: self.meta.routed_callee.clone(),
            connected_callee: self.meta.connected_callee.clone(),
            routed_contact: self.meta.routed_contact.clone(),
            routed_destination: self.meta.routed_destination.clone(),
            callee_peer: self.meta.callee_peer.clone(),
            last_queue_name: self.meta.queue_name.clone(),
            callee_call_ids: self.meta.callee_call_ids.iter().cloned().collect(),
            server_dialog_id: self.caller_dialog_id(),
            transferred: self.meta.transferred,
            leg_timeline: self.meta.leg_timeline.clone(),
            metadata,
            media_quality,
            recording_segments: self.completed_recording_segments.clone(),
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
    /// Register this session in the cluster-wide session registry (which node
    /// owns this call). Called once after the local handle registration; the
    /// RAII [`SessionGuard`] unregisters on drop. Failure is non-fatal — the
    /// registry is a routing aid, not a call-state store.
    pub async fn register_in_session_registry(&mut self) {
        if self.session_registry_guard.is_some() {
            return;
        }
        let node_id = self
            .server
            .cluster_self_addr
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "local".to_string());
        let info = crate::call::runtime::SessionInfo {
            call_id: self.id.to_string(),
            node_id,
            caller: self.context.original_caller.clone(),
            callee: self.context.original_callee.clone(),
            direction: self.context.dialplan.direction.to_string(),
            target_session_id: None,
            started_at: chrono::Utc::now(),
        };
        match crate::call::runtime::SessionGuard::register(
            self.server.session_registry.clone(),
            info,
        )
        .await
        {
            Ok(guard) => self.session_registry_guard = Some(guard),
            Err(e) => {
                crate::db_report::report_db_write_failure(
                    "cluster_sessions",
                    "insert",
                    Some(self.id.0.as_str()),
                    &e,
                );
                self.record_trace(
                    crate::call_errors::TraceEvent::new(
                        crate::call_errors::TraceKind::Error,
                        "Session registry registration failed (cluster routing degraded)",
                    )
                    .severity(crate::call_errors::ErrSeverity::Error)
                    .detail(serde_json::json!({ "error": e.to_string() })),
                );
            }
        }
    }

    pub async fn execute_command(
        &mut self,
        mut command: CallCommand,
        callee_state_rx: Option<&mut mpsc::UnboundedReceiver<DialogState>>,
    ) -> CommandResult {
        let capability_check = self.check_capability(&command);

        match capability_check {
            MediaCapabilityCheck::Denied { reason } => {
                warn!(session_id = %self.id, reason = %reason, "Media capability denied");
                return CommandResult::success();
            }
            MediaCapabilityCheck::Degraded { reason } => {
                warn!(session_id = %self.id, reason = %reason, "Executing in degraded mode");
            }
            MediaCapabilityCheck::Allowed => {}
        }

        match &mut command {
            CallCommand::Bridge { leg_a, leg_b, .. } => {
                *leg_a = self.resolve_transfer_leg(leg_a.clone());
                *leg_b = self.resolve_transfer_leg(leg_b.clone());
            }
            CallCommand::Play { leg_id: Some(id), .. }
            | CallCommand::StopPlayback { leg_id: Some(id) }
            | CallCommand::Hold { leg_id: id, .. }
            | CallCommand::Unhold { leg_id: id }
            | CallCommand::JoinMixerLeg { leg_id: id, .. }
            | CallCommand::LegRemove { leg_id: id } => *id = self.resolve_transfer_leg(id.clone()),
            _ => {}
        }

        self.process_command(command, callee_state_rx).await
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
                    match self.accept_call(None, answer_sdp).await {
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
                    CommandResult::success()
                } else {
                    CommandResult::failure("Cannot bridge: one or both legs not found")
                }
            }

            CallCommand::Unbridge { .. } => {
                self.clear_bridge().await;
                CommandResult::success()
            }

            CallCommand::MarkTransferred => {
                if !self.meta.transferred {
                    info!(session_id = %self.id,
                        "Call marked as transferred (post-call survey suppressed)"
                    );
                }
                self.mark_transferred_with(None);
                CommandResult::success()
            }

            CallCommand::HangupAgentLeg => {
                // Resolve the original queue/direct agent, excluding added dial targets.
                let agent = self.resolve_transfer_leg(LegId::from("callee"));
                if !self.legs.get(&agent).is_some_and(|leg|
                    matches!(leg.state, LegState::Connected | LegState::Hold))
                {
                    return CommandResult::success();
                }
                let mut ctx = self.session_hook_ctx();
                if let Err(error) = self.handle_remove_leg(agent).await {
                    return CommandResult::failure(error.to_string());
                }
                self.mark_transferred_with(None);
                ctx.transferred = true;
                for hook in self.server.session_hooks.iter() {
                    hook.on_agent_disconnected(&ctx, &*self.app_runtime).await;
                }
                CommandResult::success()
            }

            CallCommand::ResumeMedia => {
                self.update_media_path().await;
                CommandResult::success()
            }

            CallCommand::RelayArmFailure => {
                // One-shot: the bridge latch is sticky and duplicate monitors
                // may each deliver the command — the first handling forces
                // transcode mode permanently, repeats are no-ops.
                if self.meta.relay_arm_failure_handled {
                    debug!(session_id = %self.id, "relay arm failure already handled; ignoring duplicate");
                    return CommandResult::success();
                }
                self.meta.relay_arm_failure_handled = true;
                info!(session_id = %self.id, "fast-path relay arming failed — falling back to transcoding");
                Self::ok_or_failure(self.handle_relay_arm_failure().await)
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
                // A new app replaces any active realtime bridge (one
                // media-driving app per session).
                self.teardown_realtime_bridge("app replaced");
                // Command-path queue start: RWI `app.start` / outbound
                // `on_answer: enqueue` carry the queue target in params
                // (`{"queue": "skill-group:<id>" | "sip:…", "priority": n}`)
                // instead of a dialplan-resolved `PendingQueuePlan`. Route
                // them through the shared queue-start path so these legs get
                // the full ACD behavior (agent resolution, `queue_joined`,
                // escalation), not just the static dial strategy.
                if app_name == "queue" && params.as_ref().is_some_and(|p| p.get("queue").is_some())
                {
                    match self.start_queue_app_from_params(params.as_ref()).await {
                        Ok(()) => {
                            self.sync_rtp_timeout_pause();
                            CommandResult::success()
                        }
                        Err(e) => CommandResult::failure(e.to_string()),
                    }
                } else {
                                match self
                        .app_runtime
                        .start_app(&app_name, params, auto_answer)
                        .await
                    {
                        Ok(()) => {
                            self.sync_rtp_timeout_pause();
                            CommandResult::success()
                        }
                        Err(e) => CommandResult::failure(e.to_string()),
                    }
                }
            }

            CallCommand::StopApp { reason } => {
                self.teardown_realtime_bridge("app stopped");
                match self.app_runtime.stop_app(reason).await {
                    Ok(()) => {
                        self.sync_rtp_timeout_pause();
                        CommandResult::success()
                    }
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::RealtimeStart { params } => {
                Self::ok_or_failure(self.handle_realtime_start(params).await)
            }

            CallCommand::RealtimeStop { reason } => {
                Self::ok_or_failure(self.handle_realtime_stop(reason).await)
            }

            CallCommand::InjectAppEvent { event } => {
                let event_value = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                match self.app_runtime.inject_event(event_value) {
                    Ok(()) => CommandResult::success(),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::Play {
                leg_id,
                source,
                options,
            } => {
                let plays_to_caller = leg_id
                    .as_ref()
                    .is_none_or(|leg| leg == &LegId::from("caller") || leg == &LegId::from("both"));
                let caller_dialog_ready = self
                    .caller_dialog
                    .as_ref()
                    .is_some_and(|d| d.state().is_confirmed() || d.state().waiting_ack());
                if plays_to_caller
                    && self.app_runtime.current_app().as_deref() == Some("queue")
                    && (!caller_dialog_ready || self.media_leg(&LegId::from("caller")).is_none())
                {
                    self.prepare_queue_playback_media().await;
                    let caller_dialog_ready = self
                        .caller_dialog
                        .as_ref()
                        .is_some_and(|d| d.state().is_confirmed() || d.state().waiting_ack());
                    if !caller_dialog_ready || self.media_leg(&LegId::from("caller")).is_none() {
                        return CommandResult::failure(
                            "Queue playback could not establish caller media".to_string(),
                        );
                    }
                    self.update_leg_state(&LegId::from("caller"), LegState::Connected);
                    self.update_media_path().await;
                }
                Self::ok_or_failure(self.handle_play(leg_id, source, options).await)
            }

            CallCommand::StopPlayback { leg_id } => {
                Self::ok_or_failure(self.handle_stop_playback(leg_id).await)
            }

            CallCommand::StartRecording { config } => {
                let result = async {
                    // Capture is attached when the caller peer is built,
                    // so recording can be activated on demand at any
                    // stage (IVR node, agent connect, API command) regardless
                    // of the routing-time recording policy.
                    let segment_type = config
                        .segment_type
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| "segment".to_string());
                    let segment_id = config
                        .segment_id
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string());
                    let label = self.recording_label(config.label.as_deref(), &segment_type);
                    // Every start is a new segment in the logical call; the
                    // auto-generated name additionally bumps past any file
                    // collision left by segments of other legs (transfers).
                    self.recording_seq = self.recording_seq.saturating_add(1);
                    let mut seq = self.recording_seq;
                    let path = if config.path.trim().is_empty() {
                        let (resolved_seq, resolved_path) = crate::callrecord::segmented_wav_path(
                            &self.recording_root_dir(),
                            &self.root_session_id_str(),
                            seq,
                            &label,
                        );
                        seq = resolved_seq;
                        self.recording_seq = resolved_seq;
                        resolved_path.to_string_lossy().into_owned()
                    } else {
                        config.path.clone()
                    };
                    if let Some(parent) = std::path::Path::new(&path).parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    let notify_app = config.notify_app.unwrap_or(true);
                    // Honor a caller-minted id (RWI `record.start` replies with
                    // it immediately) or mint one here for the auto paths.
                    let unique_id = config
                        .unique_id
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let profile = self.media_leg(&LegId::from("caller")).and_then(|peer| peer.negotiated())
                        .ok_or_else(|| anyhow!("No caller media profile for recording"))?;
                    let option = self.recorder_option(path.clone());
                    self.media.recording
                        .start_recording(
                            profile,
                            option,
                            config.channels.unwrap_or(2),
                            config.mono_caller_only.unwrap_or(false),
                            config
                                .max_duration_secs
                                .map(|seconds| Duration::from_secs(seconds as u64)),
                        )
                        .await?;
                    self.active_recording = Some(crate::callrecord::ActiveRecording {
                        path,
                        segment_type: segment_type.clone(),
                        source: crate::callrecord::RecordingSource::classify(&segment_type),
                        segment_id,
                        seq,
                        label,
                        started_at: chrono::Utc::now(),
                        notify_app,
                        unique_id: unique_id.clone(),
                        discard: config.discard.unwrap_or(false),
                    });
                    if config.beep {
                        self.handle_play(
                            None,
                            crate::call::domain::MediaSource::file("beep.wav"),
                            None,
                        )
                        .await?;
                    }
                    Ok(unique_id)
                }
                .await;
                let result: anyhow::Result<String> = result;
                match result {
                    Ok(unique_id) => CommandResult {
                        success: true,
                        message: None,
                        affected_leg: None,
                        data: Some(serde_json::json!({ "unique_id": unique_id })),
                    },
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::StopRecording => {
                let outcome = self.media.recording.stop_recording().await;
                match outcome {
                    Ok(Some(result)) => {
                        self.publish_recording_complete(result);
                        CommandResult::success()
                    }
                    Ok(None) => CommandResult::success(),
                    Err(error) => CommandResult::failure(error.to_string()),
                }
            }

            CallCommand::StopRecordingSegment { label } => {
                // Ownership check runs HERE, inside the session loop — the
                // sender (CC hook, inline in `on_agent_disconnected`) cannot
                // probe first without deadlocking against this very loop.
                // Only the agent segment we started ourselves
                // (`segment_type = "agent"`, label = agent id) is stopped; a
                // foreign recorder that happens to be active (policy
                // full-call recording, manual RWI record, survey segment)
                // is never touched. The core hangup finalize remains the
                // backstop when this hook never fires.
                let ours = self.active_recording.as_ref().is_some_and(|active| {
                    active.segment_type == "agent" && active.label == label
                });
                if !ours {
                    debug!(session_id = %self.id, %label,
                        "StopRecordingSegment: active recorder is not our agent segment — leaving it alone");
                    return CommandResult::success();
                }
                let outcome = self.media.recording.stop_recording().await;
                match outcome {
                    Ok(Some(result)) => {
                        self.publish_recording_complete(result);
                        CommandResult::success()
                    }
                    Ok(None) => CommandResult::success(),
                    Err(error) => CommandResult::failure(error.to_string()),
                }
            }

            CallCommand::QueryRecorderStatus { reply } => {
                tracing::debug!(session_id = %self.id, "QueryRecorderStatus: handler entered");
                let result = self.media.recording.recorder_status().await;
                tracing::debug!(session_id = %self.id, ok = ?result.as_ref().map(|s| s.active), "QueryRecorderStatus: handler done");
                let command_result = match &result {
                    Ok(_) => CommandResult::success(),
                    Err(error) => CommandResult::failure(error.to_string()),
                };
                let _ = reply.send(result);
                command_result
            }

            CallCommand::QueryLegByDialog { dialog_id, reply } => {
                let leg = self.leg_id_for_dialog(&dialog_id);
                let _ = reply.send(leg);
                CommandResult::success()
            }

            CallCommand::Trace { event } => {
                self.record_trace(event);
                CommandResult::success()
            }

            CallCommand::ReportCallError {
                stage,
                app,
                code,
                severity,
                message,
                sip_status,
                detail,
            } => {
                let call_id = self.context.session_id.clone();
                match severity {
                    crate::call_errors::ErrSeverity::Info => tracing::info!(
                        call_id = %call_id, stage = %stage, app = %app, code = %code,
                        detail = ?detail, "{}", message
                    ),
                    crate::call_errors::ErrSeverity::Warn => tracing::warn!(
                        call_id = %call_id, stage = %stage, app = %app, code = %code,
                        detail = ?detail, "{}", message
                    ),
                    crate::call_errors::ErrSeverity::Error => tracing::error!(
                        call_id = %call_id, stage = %stage, app = %app, code = %code,
                        detail = ?detail, "{}", message
                    ),
                }
                let mut trace = crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::Error,
                    message.clone(),
                )
                .severity(severity)
                .code(&code);
                if let Some(detail) = detail.clone() {
                    trace = trace.detail(detail.clone());
                }
                self.record_trace(trace);
                self.emit_typed_rwi_event(&crate::rwi::CallError {
                    call_id: call_id.clone(),
                    session_id: Some(call_id),
                    stage,
                    app,
                    code,
                    severity: severity.as_str().to_string(),
                    message,
                    sip_status,
                    detail,
                });
                CommandResult::success()
            }

            CallCommand::UpdateQueueMeta {
                queue_name,
                queue_label,
                skill_group_id,
            } => {
                if let Some(q) = queue_name.clone().filter(|s| !s.is_empty()) {
                    self.meta.queue_name = Some(q);
                }
                if let Some(label) = queue_label.clone().filter(|s| !s.is_empty()) {
                    self.meta.queue_label = Some(label);
                }
                if let Some(sg) = skill_group_id.clone().filter(|s| !s.is_empty()) {
                    self.meta.skill_group_id = Some(sg);
                }
                // Merge into ActiveCallContextMeta: only the queue fields are
                // overwritten; ivr_node_id / ticket_id / customer_id must
                // survive the overflow switch.
                let registry = self.server.active_call_registry.clone();
                let session_id = self.context.session_id.clone();
                let mut ctx = registry.get_context_meta(&session_id).unwrap_or_default();
                if let Some(q) = queue_name.clone().filter(|s| !s.is_empty()) {
                    ctx.queue_id = Some(q);
                    if let Some(label) = queue_label.clone().filter(|s| !s.is_empty()) {
                        ctx.queue_name = Some(label);
                    }
                }
                if let Some(sg) = skill_group_id.clone().filter(|s| !s.is_empty()) {
                    ctx.skill_group_id = Some(sg);
                }
                registry.set_context_meta(session_id, ctx);
                CommandResult::success()
            }

            CallCommand::PauseRecording => Self::ok_or_failure(
                self.media.recording.pause_recording(),
            ),

            CallCommand::ResumeRecording => Self::ok_or_failure(
                self.media.recording.resume_recording(),
            ),

            CallCommand::StartTranscription { language } => {
                // Reference-counted: bump the count when already running.
                if let Some(lt) = self.live_transcription.as_mut() {
                    lt.refs += 1;
                    return CommandResult::success();
                }
                match self.start_live_transcription(language).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => {
                        // Surface the failure to subscribers (SSE / webhook).
                        self.emit_typed_rwi_event(&crate::rwi::TranscriptError {
                            call_id: self.context.session_id.clone(),
                            side: None,
                            error: e.to_string(),
                        });
                        CommandResult::failure(e.to_string())
                    }
                }
            }

            CallCommand::StopTranscription => match self.live_transcription.as_mut() {
                None => CommandResult::success(),
                Some(lt) => {
                    lt.refs = lt.refs.saturating_sub(1);
                    if lt.refs == 0 {
                        self.stop_live_transcription("stopped").await;
                    }
                    CommandResult::success()
                }
            },

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
                    self.handle_transfer(
                        leg_id,
                        target,
                        attended,
                        transfer::TransferDisposition::Detach,
                        callee_state_rx,
                        HashMap::new(),
                    )
                    .await,
                )
            }

            CallCommand::TransferAwaitResult {
                leg_id,
                target,
                headers,
            } => {
                let Some(callee_state_rx) = callee_state_rx.as_deref_mut() else {
                    self.meta.pending_transfer_outcome =
                        Some(crate::call::domain::TransferOutcome::NotConnected);
                    self.deliver_pending_transfer_result();
                    return CommandResult::failure(
                        "No callee state receiver available for transfer".to_string(),
                    );
                };
                let result = self
                    .handle_transfer(
                        leg_id,
                        target,
                        false,
                        transfer::TransferDisposition::AwaitResult,
                        callee_state_rx,
                        headers,
                    )
                    .await;
                Self::ok_or_failure(result)
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

            CallCommand::JoinMixerLeg { mixer_id, leg_id } => {
                Self::ok_or_failure(self.handle_join_mixer_leg(mixer_id, leg_id).await)
            }

            CallCommand::JoinConference { conf_id } => {
                // Room dial-in: by command-ordering, the Answer command that
                // preceded this one has completed, so the caller leg is
                // Connected and its media tracks exist — the join below will
                // find real senders/receivers (unlike joining immediately
                // after ctrl.answer() merely *initiates* the answer).
                self.join_conference_mixer(&conf_id).await;
                CommandResult::success()
            }

            CallCommand::LeaveMixer => Self::ok_or_failure(self.handle_leave_mixer().await),

            CallCommand::LegAdd {
                source_leg,

                target,
                leg_id,
                headers,
            } => {
                let headers: Vec<rsipstack::sip::Header> = headers
                    .into_iter()
                    .map(|(name, value)| rsipstack::sip::headers::make_header(&name, value))
                    .collect();
                let source_leg = source_leg.map(|id| self.resolve_transfer_leg(id));
                match self.handle_add_leg(target, leg_id, headers, source_leg).await {
                    Ok(new_leg_id) => CommandResult::success_with_leg(new_leg_id),
                    Err(e) => CommandResult::failure(e.to_string()),
                }
            }

            CallCommand::LegRemove { leg_id } => {
                Self::ok_or_failure(self.handle_remove_leg(leg_id).await)
            }

            CallCommand::LegRinging { leg_id } => {
                info!(session_id = %self.id, %leg_id, "Leg ringing async notification");
                // The agent leg is ringing — fire on_call_ringing session hooks
                // so the CC addon publishes the agent attribution (agent Idle
                // → Ringing) before the enriched `call_ringing` is emitted.
                let early_media = self.media_leg(&leg_id).is_some_and(|peer| peer.negotiated().is_some());
                self.update_leg_state(&leg_id, if early_media { LegState::EarlyMedia } else { LegState::Ringing });
                if !self.legs.contains_key(&leg_id) { return CommandResult::success(); }
                let dial_source = self.legs.get(&leg_id).and_then(|leg| leg.source_leg.clone());
                if dial_source.is_some() || leg_id.as_str() == "consult" {
                    if early_media {
                        self.update_media_path().await;
                    } else {
                        let recipient = dial_source.unwrap_or_else(|| self.resolve_transfer_leg(LegId::from("callee")));
                        if let Some(peer) = self.media_leg(&recipient) {
                            let configured = self.context.dialplan.audio_profile.as_ref().and_then(|p| p.ring.clone());
                            if let Some(path) = configured {
                                match crate::media::audio_source::FileAudioSource::new(Self::resolve_audio_file_path(&path), true).await {
                                    Ok(source) => {
                                        if let Err(error) = peer.play_media(Box::new(source), true).await {
                                            warn!(%recipient, %error, "Could not play dial ringback");
                                        }
                                    }
                                    Err(error) => {
                                        warn!(%recipient, %path, %error, "Could not load dial ringback");
                                    }
                                }
                            }
                        }
                    }
                    self.emit_typed_rwi_event(&crate::rwi::CallRinging {
                        leg_id: None,
                        call_id: self.context.session_id.clone(), early_media,
                    });
                    return CommandResult::success();
                }
                self.fire_on_call_ringing_hooks(false).await;
                // Notify the running queue app that the agent is ringing so
                // it can track per-leg state and emit QueueAgentOffered.
                let agent_uri = self.legs.get(&leg_id).and_then(|l| l.endpoint.clone());
                if let Some(ref agent_uri) = agent_uri {
                    // Identify the agent from THIS leg first: sequential
                    // fallback dials a different agent than the session-level
                    // `resolved_agent_id`, which stays pinned to the first
                    // resolved agent.
                    let agent_id = self.leg_agent_id(Some(agent_uri)).await;
                    self.app_event_bridge.send_app_event(
                        crate::call::app::ControllerEvent::Custom(
                            "agent_ringing".to_string(),
                            serde_json::json!({
                                "leg_id": leg_id.0,
                                "agent_uri": agent_uri,
                                "agent_id": agent_id,
                            }),
                        ),
                    );
                }
                self.emit_typed_rwi_event(&crate::rwi::CallRinging {
                    leg_id: None,
                    call_id: self.context.session_id.clone(),
                    early_media: false,
                });
                CommandResult::success()
            }

            CallCommand::LegConnected {
                leg_id,
                answer_sdp,
                dialog_id,
            } => {
                info!(session_id = %self.id, %leg_id, "Leg connected async notification");
                if !self.legs.contains_key(&leg_id) {
                    // The dial task can receive 200 OK and queue LegConnected
                    // behind a Cancel/LegRemove command. That command removes
                    // the leg before this notification is handled, but the
                    // answered SIP dialog still needs BYE. Do not reattach it.
                    // Retain a guard so session teardown also cleans it up if
                    // the queued hangup has not run yet.
                    if let Some(call_id) = dialog_id.as_deref() {
                        for dialog in self.server.dialog_layer.get_client_dialog_by_call_id(call_id) {
                            self.pending_hangup.insert(dialog.id());
                            self.callee_guards.push(ClientDialogGuard::new(self.server.dialog_layer.clone(), dialog.id()));
                        }
                    }
                    return CommandResult::success();
                }

                // Contract §3.3: CTI `{call_id}` is the B-leg SIP Call-ID.
                // Map this leg's dialog Call-ID onto the session handle so
                // `/cc/calls/{call_id}/...` resolves it via `get_handle_by_dialog`.
                // Covers the main callee leg, fork winners, and dynamic
                // (queue-agent / consult) legs alike.
                if let Some(call_id) = &dialog_id {
                    if let Some(handle) = self
                        .server
                        .active_call_registry
                        .get_handle(&self.id.to_string())
                    {
                        self.server
                            .active_call_registry
                            .register_dialog(call_id.clone(), handle);
                    }
                    // Cluster: also register dialog Call-ID → session owner so
                    // CTI / in-dialog SIP arriving on another node can resolve.
                    let node_id = self
                        .server
                        .cluster_self_addr
                        .as_ref()
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "local".to_string());
                    let alias = crate::call::runtime::SessionInfo::dialog_alias(
                        call_id.clone(),
                        self.id.to_string(),
                        node_id,
                    );
                    let registry = self.server.session_registry.clone();
                    crate::utils::spawn(async move {
                        if let Err(e) = registry.register(&alias).await {
                            // Incident 2026-09-17: this used to be a debug! —
                            // every alias insert was silently failing on
                            // MySQL ("Data too long for column 'direction'")
                            // and cross-node dialog resolution was dead
                            // without anyone noticing.
                            crate::db_report::report_db_write_failure_with_detail(
                                "cluster_sessions",
                                "upsert",
                                None,
                                &e,
                                Some(serde_json::json!({
                                    "stage": "dialog_alias",
                                    "dialog_call_id": alias.call_id,
                                    "target_session_id": alias.canonical_session_id(),
                                })),
                                crate::db_report::THROTTLE_COOLDOWN,
                            );
                        }
                    });
                }

                // In UAC mode, a leg added via `leg_add` with leg_id="callee"
                // should attach its answered dialog + SDP onto the MediaBridge
                // B side so it can be bridged with the A (caller) leg.
                if leg_id == LegId::from("callee") && self.legs.media_leg(&leg_id).is_none() {
                    if let Some(call_id) = dialog_id {
                        if let Some(invite) = self
                            .server
                            .dialog_layer
                            .get_client_dialog_by_call_id(&call_id)
                            .into_iter()
                            .next()
                        {
                            self.attach_callee_dialog(invite, answer_sdp.clone()).await;
                        }
                    }
                } else if let Some(ref call_id) = dialog_id {
                    // Dynamic legs (queue agents): store the INVITE dialog on the leg
                    // AND register a RAII ClientDialogGuard so the agent leg is
                    // automatically hung up (BYE) when the session is destroyed —
                    // these legs are not tracked in callee_dialogs.
                    if let Some(invite) = self
                        .server
                        .dialog_layer
                        .get_client_dialog_by_call_id(call_id)
                        .into_iter()
                        .next()
                    {
                        let dlg_id = invite.id();
                        self.legs.set_dialog(
                            leg_id.clone(),
                            rsipstack::dialog::dialog::Dialog::Invite(invite),
                        );
                        self.callee_guards.push(ClientDialogGuard::new(
                            self.server.dialog_layer.clone(),
                            dlg_id,
                        ));
                    }
                }

                // Forward to running app before processing so the app can react
                let agent_uri = self.legs.get(&leg_id).and_then(|l| l.endpoint.clone());
                if let Some(ref agent_uri) = agent_uri {
                    // The leg that actually answered identifies the agent.
                    // Overwrite the session-level `resolved_agent_id` (pinned
                    // to the FIRST resolved agent at resolve time) with the
                    // answering leg's user-part — validated against the agent
                    // registry — so app events and CC session hooks attribute
                    // connect/ended to the right agent. This is critical when
                    // sequential fallback dialed a different agent than the
                    // primary one. Gated to queue context so direct (non-
                    // queue) calls keep deriving the agent from parties.
                    let leg_agent_id = self.leg_agent_id(Some(agent_uri)).await;
                    if let Some(ref id) = leg_agent_id
                        && Some(id.as_str()) != self.session_ext_get("resolved_agent_id").as_deref()
                        && self.in_queue_context()
                    {
                        let mut ext = self.extensions.write();
                        match ext.get_mut::<std::collections::HashMap<String, String>>() {
                            Some(map) => {
                                map.insert("resolved_agent_id".to_string(), id.clone());
                            }
                            None => {
                                let mut map = std::collections::HashMap::new();
                                map.insert("resolved_agent_id".to_string(), id.clone());
                                ext.insert(map);
                            }
                        }
                    }
                    let resolved_agent_id =
                        leg_agent_id.or_else(|| self.session_ext_get("resolved_agent_id"));
                    self.app_event_bridge.send_app_event(
                        crate::call::app::ControllerEvent::Custom(
                            "agent_connected".to_string(),
                            serde_json::json!({
                                "leg_id": leg_id.0,
                                "agent_uri": agent_uri,
                                "agent_id": resolved_agent_id,
                            }),
                        ),
                    );
                }
                if let Some(sdp) = answer_sdp.clone() {
                    self.legs.set_answer(leg_id.clone(), sdp);
                }

                if let Some(peer) = self.legs.media_leg(&leg_id) {
                    if let Some(sdp) = answer_sdp.as_deref() {
                        if let Err(error) = peer.apply_sdp(sdp, rustrtc::SdpType::Answer).await {
                            return CommandResult::failure(format!("Failed to apply leg SDP: {error}"));
                        }
                    }
                    peer.accept();

                    // A real callee/agent leg answered — record it so the
                    // queue-abandon detector can tell "served then hung up"
                    // apart from "hung up while waiting" (same flag
                    // accept_call maintains for the direct-answer path).
                    // Without this, a caller hangup after a dynamic-leg
                    // dispatch was misclassified as a queue abandon.
                    self.meta.ever_connected_callee = true;
                    if let Some(endpoint) = self.legs.get(&leg_id).and_then(|l| l.endpoint.clone())
                    {
                        self.meta.connected_callee = Some(endpoint);
                    }

                    // The queue-agent leg answering IS the call-connect moment
                    // for the agent (the caller was already answered by the
                    // IVR/queue app, so accept_call's hook never sees this
                    // transition). Fire the session lifecycle hooks here —
                    // CcCallSessionHook publishes the agent attribution and
                    // moves the agent Ringing → Busy. Without this the CC
                    // layer never learns the agent connected (agent stuck in
                    // Ringing). Then emit the core `call_answered` (the
                    // unified replacement of the addon-emitted `cc_answered`),
                    // enriched with the agent context.
                    if !self.server.session_hooks.is_empty() {
                        let ctx = self.session_hook_ctx();
                        for hook in self.server.session_hooks.iter() {
                            hook.on_call_connected(&ctx).await;
                        }
                        self.sync_agent_context_to_rwi_meta();
                    }
                    // One-shot: the queue-app answer path (accept_call) may
                    // have already emitted under an app-startup race — see
                    // `answered_event_emitted`.
                    if !self.answered_event_emitted {
                        self.answered_event_emitted = true;
                        self.emit_typed_rwi_event(&crate::rwi::CallAnswered {
                            leg_id: None,
                            call_id: self.context.session_id.clone(),
                        });
                    }
                }

                // Owner-anchored consult leg ("C answered" signal): fire the
                // session hooks even when the queue media bridge is gone by
                // this point (the queue app exits at agent connect, and the
                // consult leg carries its own media peer) — the CC layer
                // blocks transfer merge/complete until this fires.
                if leg_id == LegId::from("consult") && self.legs.media_leg(&leg_id).is_none() && !self.server.session_hooks.is_empty() {
                    let ctx = self.session_hook_ctx();
                    for hook in self.server.session_hooks.iter() {
                        hook.on_call_connected(&ctx).await;
                    }
                    self.sync_agent_context_to_rwi_meta();
                }

                self.update_leg_state(&leg_id, LegState::Connected);
                if self.bridge.active
                    && self.bridge.contains_leg(&leg_id)
                    && self.bridge.legs.len() == 2
                {
                    let pair = self.bridge.legs.clone();
                    if !self.setup_bridge(pair[0].clone(), pair[1].clone()).await {
                        return CommandResult::failure("Failed to connect requested media legs");
                    }
                }
                self.update_media_path().await;
                CommandResult::success()
            }

            CallCommand::LegFailed { leg_id, reason } => {
                warn!(%leg_id, %reason, "Leg failed async notification");
                if !self.legs.contains_key(&leg_id) { return CommandResult::success(); }
                self.emit_rwi_leg_hangup(&leg_id, Some(reason.clone()));
                let result = {
                    // Bridge-based trigger (legacy): the failed leg was paired
                    // with the caller inside an armed media bridge.
                    let bridge_paired_leg = self
                        .legs
                        .get(&leg_id)
                        .is_some_and(|leg| leg.state == LegState::Connected || leg.source_leg.is_some())
                        && self.bridge.active
                        && self.bridge.contains_leg(&LegId::from("caller"))
                        && self.bridge.contains_leg(&leg_id);
                    // B-leg trigger: the failed leg IS the session's current
                    // B-leg toward the caller even when the media bridge never
                    // armed (e.g. ICE never completed on the agent leg). This
                    // mirrors the unconditional cascade in handle_callee_state
                    // for tracked callee dialogs. resolve_transfer_leg excludes
                    // consult legs and dial-source legs, so a consult hangup
                    // (a normal flow event) never triggers the cascade — and a
                    // ringing/no-answer failure never does either (the queue
                    // keeps dialing; production 2026-09-17).
                    let current_b_leg = self
                        .legs
                        .get(&leg_id)
                        .is_some_and(|leg| matches!(leg.state, LegState::Connected | LegState::Hold))
                        && self.resolve_transfer_leg(LegId::from("callee")) == leg_id;
                    let connected_bridge_leg = bridge_paired_leg || current_b_leg;
                    // Forward to running app before removing the leg (so we can get the URI)
                    let agent_uri = self.legs.get(&leg_id).and_then(|l| l.endpoint.clone());
                    let event_name =
                        if reason.contains("486") || reason.to_lowercase().contains("busy") {
                            "agent_busy"
                        } else {
                            "agent_no_answer"
                        };
                    // Resolve the canonical agent_id from the failing LEG first
                    // (sequential fallback dials a different agent than the
                    // session-level value; validated against the registry so
                    // WebRTC contact user-parts are not mistaken for agent ids),
                    // then fall back to session extensions so the queue app can
                    // update the correct agent's presence.
                    let resolved_agent_id = self
                        .leg_agent_id(agent_uri.as_deref())
                        .await
                        .unwrap_or_default();
                    let agent_id = if !resolved_agent_id.is_empty() {
                        resolved_agent_id.clone()
                    } else {
                        agent_uri
                            .as_deref()
                            .and_then(Self::uri_user_part)
                            .unwrap_or_else(|| "unknown".to_string())
                    };
                    {
                        self.app_event_bridge.send_app_event(
                            crate::call::app::ControllerEvent::Custom(
                                event_name.to_string(),
                                serde_json::json!({
                                    "leg_id": leg_id.0,
                                    "agent_uri": agent_uri,
                                    "agent_id": agent_id,
                                    "reason": reason,
                                }),
                            ),
                        );
                    }

                    // Surface agent rejection / no-answer in the call trace so
                    // operator-facing call records show *which* agent and *why*
                    // the queue could not connect (e.g. 486 from off-hours phone).
                    let in_queue = self.in_queue_context();
                    if in_queue {
                        let status = reason
                            .strip_prefix("Rejected with ")
                            .map(str::to_string)
                            .unwrap_or_else(|| reason.clone());
                        let queue_name =
                            crate::proxy::proxy_call::call_meta::effective_queue_name(&self.meta)
                                .unwrap_or_default();
                        let (msg, severity) = if event_name == "agent_busy" {
                            (
                                format!("Agent {} rejected ({})", agent_id, status),
                                crate::call_errors::ErrSeverity::Warn,
                            )
                        } else {
                            (
                                format!("Agent {} no answer", agent_id),
                                crate::call_errors::ErrSeverity::Warn,
                            )
                        };
                        let ev = crate::call_errors::TraceEvent::new(
                            crate::call_errors::TraceKind::Queue,
                            msg,
                        )
                        .severity(severity)
                        .detail(serde_json::json!({
                            "agent": agent_id,
                            "status": status,
                            "reason": reason,
                            "queue_name": queue_name,
                        }));
                        self.record_trace(ev);
                    }

                    self.update_leg_state(&leg_id, LegState::Ended);
                    if let Err(error) = self.handle_remove_leg(leg_id.clone()).await {
                        return CommandResult::failure(error.to_string());
                    }
                    // The dial leg is gone: lift the RTP-watchdog suppression
                    // that the transfer dial armed (`sync_rtp_timeout_pause`
                    // re-evaluates from current leg state). Without this a
                    // ringing transfer leg that fails outright leaves the
                    // watchdog paused for the rest of the call.
                    if self.meta.transfer_in_progress {
                        self.meta.transfer_in_progress = false;
                        self.sync_rtp_timeout_pause();
                    }
                    // A departed dial source cannot continue its private call.
                    let dial_source_ended = self.legs.values().any(|leg|
                        leg.source_leg.as_ref() == Some(&leg_id));
                    if (connected_bridge_leg || dial_source_ended)
                        && self
                            .caller_dialog
                            .as_ref()
                            .is_none_or(|d| !d.state().is_terminated())
                    {
                        // Defer to the unified post-disconnect handler (CSAT first,
                        // then return_app, then hangup).  Call directly since we
                        // are already inside execute_command.
                        self.handle_start_return_app().await;
                        info!(
                            session_id = %self.id,
                            %leg_id,
                            "Connected dynamic leg ended; post-disconnect handler ran"
                        );
                    }
                    CommandResult::failure(reason.clone())
                };
                let ctx = self.session_hook_ctx();
                for hook in self.server.session_hooks.iter() {
                    if let Some(spec) = hook.on_leg_failed(&ctx, leg_id.as_str(), &reason).await {
                        if let Err(error) = self
                            .handle_send_info(spec.leg_id, spec.content_type, spec.body)
                            .await
                        {
                            warn!(session_id = %self.id, %error, "Failed to send leg failure notification");
                        }
                    }
                }
                result
            }

            CallCommand::AppExited => self.handle_app_exited().await,

            CallCommand::StartReturnApp => self.handle_start_return_app().await,

            CallCommand::VoipBridgeClosed => {
                // The TTS/voip WS bridge ended. Drop the handle (cancelling its
                // token) and re-evaluate the media path: while the bridge was
                // up, `update_media_path()` deliberately stayed out of the way;
                // a route that became pending meanwhile (e.g. a queue agent leg
                // connecting) must be established now that it is gone.
                if self.voip_bridge.take().is_some() {
                    debug!(session_id = %self.id, "Voip bridge closed; re-evaluating media path");
                }
                self.update_media_path().await;
                CommandResult::success()
            }

            CallCommand::SendInfo {
                leg_id,
                content_type,
                body,
            } => Self::ok_or_failure(self.handle_send_info(leg_id, content_type, body).await),

            _ => CommandResult::failure("Command not yet implemented".to_string()),
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
                    if let Err(e) = self
                        .propagate_unhold_to_side(LegSide::B)
                        .await
                    {
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

    /// Unified post-disconnect handler: decide what to do with the caller after
    /// the connected B-leg (agent / bridge) terminates.
    ///
    /// Precedence (CSAT-first):
    /// 1. `on_agent_disconnected` hooks (e.g. CSAT survey) — first hook that
    ///    returns `true` takes over and this method returns.
    /// 2. `meta.transfer_return_app` — start the stored return app.
    /// 3. Neither — queue a normal hangup.
    ///
    /// Called from three disconnect paths (B2BUA callee termination, dynamic-
    /// leg failure, Bridge monitor) via `CallCommand::StartReturnApp`.
    async fn handle_start_return_app(&mut self) -> CommandResult {
        let caller_alive = !self
            .caller_dialog
            .as_ref()
            .is_none_or(|d| d.state().is_terminated());

        if !caller_alive {
            // Caller died while the flow was suspended on a bridge/queue —
            // the suspended IVR will never resume, so emit the compensating
            // session_end trace it suppressed at hand-off time.
            if self.meta.ivr_flow_suspended {
                self.meta.ivr_flow_suspended = false;
                self.emit_suspended_flow_session_end(
                    crate::call::app::ivr::provider::SessionEndReason {
                        reason: crate::call::app::ivr::provider::SessionEndTag::UserHangup,
                        detail: None,
                    },
                )
                .await;
            }
            self.meta.pending_transfer_outcome = None;
            return CommandResult::success();
        }

        // A supervisor takeover intentionally ended the agent (B) leg — the
        // customer stays alive in the takeover conference with the supervisor,
        // so the regular B-leg-disconnect cascade (return app / caller hangup)
        // must not run.
        if self.meta.supervisor_takeover_active {
            info!(session_id = %self.id,
                "B-leg ended by supervisor takeover; keeping caller in takeover conference"
            );
            return CommandResult::success();
        }

        if self.deliver_pending_transfer_result() {
            return CommandResult::success();
        }

        // 1. CSAT / session hooks take precedence.
        let ctx = self.session_hook_ctx();
        for hook in self.server.session_hooks.iter() {
            if hook.on_agent_disconnected(&ctx, &*self.app_runtime).await {
                return CommandResult::success();
            }
        }

        // 2. Return app from CallMeta.
        if let Some(mut spec) = self.meta.transfer_return_app.take() {
            info!(session_id = %self.id,
                app = %spec.app_name,
                "B‑leg disconnected; starting return app"
            );
            // Replay DTMF captured during the bridge into the return IVR's
            // params so the resumed flow (and its step provider) can see the
            // keys the caller pressed while the bridge owned the media.
            let digits: Vec<String> = std::mem::take(&mut *self.bridge_dtmf_digits.lock());
            if !digits.is_empty()
                && spec.app_name == "ivr"
                && let Some(obj) = spec.params.as_object_mut()
            {
                let ivr_params = obj
                    .entry("ivr_params")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(ip) = ivr_params.as_object_mut() {
                    ip.insert(
                        "bridge_dtmf_digits".to_string(),
                        serde_json::Value::String(digits.join(",")),
                    );
                    // Originating bridge-step context (serialized —
                    // `with_ivr_params` keeps string values only): the
                    // resumed executor reports the suppressed bridge-DTMF
                    // trace itself (with `next_node_id`) once the successor
                    // node resolves.
                    if let Some(ctx) = self.bridge_trace_context.lock().clone()
                        && let Ok(s) = serde_json::to_string(&ctx)
                    {
                        ip.insert("bridge_step_ctx".to_string(), serde_json::Value::String(s));
                    }
                }
            }
            self.bridge.clear();
            let label = format!("Return app '{}'", spec.app_name);
            match self
                .ensure_app_running(&spec.app_name, Some(spec.params), &label)
                .await
            {
                Ok(()) => {
                    // Flow resumed — the successor app owns the lifecycle now.
                    self.meta.ivr_flow_suspended = false;
                    return CommandResult::success();
                }
                Err(e) => {
                    warn!(session_id = %self.id,
                        app = %spec.app_name,
                        error = %e,
                        "Failed to start return app; falling through to hangup"
                    );
                }
            }
        }

        // 3. Neither hook nor return app — hang up the caller.
        self.pending_hangup.insert(self.caller_dialog_id());
        CommandResult::success()
    }

    /// Emit the compensating `session_end` `ivr_step_trace` for an IVR flow
    /// that died while suspended (caller hangup during a bridge/queue
    /// hand-off, a JumpIvr target that failed to start, or session teardown).
    ///
    /// The step-IVR executor suppresses its own session_end trace for
    /// resumable hand-offs, so this synthetic event guarantees consumers
    /// still see exactly one session_end per logical flow — carrying the
    /// REAL end reason instead of a premature `transfer`. Node context comes
    /// from the bridge trace context when the suspension was a voip_bridge.
    pub(crate) async fn emit_suspended_flow_session_end(
        &self,
        end_reason: crate::call::app::ivr::provider::SessionEndReason,
    ) {
        super::util::emit_suspended_flow_session_end(
            &self.context.session_id,
            &self.context.original_caller,
            &self.context.original_callee,
            &self.server.rwi_gateway,
            &self.bridge_trace_context,
            super::util::trace_sip_headers(&self.app_runtime).await,
            end_reason,
        );
    }

    pub(crate) fn deliver_pending_transfer_result(&mut self) -> bool {
        let caller_alive = !self
            .caller_dialog
            .as_ref()
            .is_none_or(|dialog| dialog.state().is_terminated());
        if !caller_alive {
            self.meta.pending_transfer_outcome = None;
            return false;
        }
        let Some(outcome) = self.meta.pending_transfer_outcome.take() else {
            return false;
        };
        if outcome == crate::call::domain::TransferOutcome::TargetEnded {
            self.bridge.clear();
        }
        self.app_event_bridge
            .send_app_event(crate::call::app::ControllerEvent::TransferResult(outcome))
    }

    /// Send a SIP INFO request to the dialog identified by the given leg.
    /// Supports `leg_id = "caller"` (the inbound caller dialog) and
    /// `leg_id = "callee"` (the connected callee dialog).
    async fn handle_send_info(
        &self,
        leg_id: LegId,
        content_type: String,
        body: Vec<u8>,
    ) -> Result<()> {
        let leg_id = self.resolve_transfer_leg(leg_id);
        let dialog_id = self.legs.get_dialog(&leg_id).map(|dialog| dialog.id()).or_else(|| {
            match leg_id.as_str() {
                "caller" => self.caller_dialog.as_ref().map(|_| self.caller_dialog_id()),
                "callee" => self.meta.connected_callee_dialog_id.clone(),
                _ => None,
            }
        });

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

    pub(super) async fn handle_hangup(&mut self, cmd: &HangupCommand) -> CommandResult {
        self.meta.pending_transfer_outcome = None;
        // Kill any realtime (AI voice) bridge — media tasks must not outlive
        // the call.
        self.teardown_realtime_bridge("call hangup");
        let cascade = &cmd.cascade;

        // Record the system hangup reason (e.g. RtpTimeout from the RTP
        // inactivity watchdog) unless a more specific reason has already been
        // established by dialog signalling (ByCaller / ByCallee / etc.). This
        // is the only place an explicit command reason reaches the CDR.
        if cmd.reason.is_some() && self.meta.hangup_reason.is_none() {
            self.meta.hangup_reason = cmd.reason.clone();
        }

        // RTP-inactivity watchdog: surface exactly which side went silent in
        // the call trace and CDR metadata. Recorded unconditionally (even when
        // a higher-level reason like an IVR end won the CDR `hangup_reason`)
        // so the RTP timeout is never lost from the diagnostic timeline.
        if cmd.reason == Some(crate::callrecord::CallRecordHangupReason::RtpTimeout) {
            self.meta.rtp_timeout_fired = true;
            if self.meta.error_code.is_none() {
                self.meta.error_code = Some(&crate::proxy::proxy_call::error_catalog::RTP_TIMEOUT);
            }
            if let Some(side) = cmd.rtp_timeout_side {
                if self.meta.rtp_timeout_side.is_none() {
                    self.meta.rtp_timeout_side = Some(side);
                    self.meta.rtp_timeout_leg = Some(self.rtp_timeout_leg_label(side));
                }
            }
            let side_str = match self.meta.rtp_timeout_side {
                Some(crate::call::domain::RtpTimeoutSide::Caller) => "caller",
                Some(crate::call::domain::RtpTimeoutSide::Callee) => "callee",
                None => "unknown",
            };
            let leg_label = self
                .meta
                .rtp_timeout_leg
                .clone()
                .unwrap_or_else(|| "unknown leg".to_string());
            let detail = serde_json::json!({
                "side": side_str,
                "leg": leg_label,
            });
            self.record_trace(
                crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::RtpTimeout,
                    format!("RTP inactivity timeout: no media from {}", leg_label),
                )
                .severity(crate::call_errors::ErrSeverity::Warn)
                .code(crate::proxy::proxy_call::error_catalog::RTP_TIMEOUT.code)
                .detail(detail),
            );
        }

        let mut ended_legs: Vec<LegId> = Vec::new();
        for leg in self.legs.values_mut() {
            let should_hangup = match cascade {
                HangupCascade::All => true,
                HangupCascade::None => false,
                HangupCascade::AllExcept(exclude) => !exclude.contains(&leg.id),
                HangupCascade::Other => true,
            };

            if should_hangup && leg.state != LegState::Ended {
                leg.state = LegState::Ended;
                ended_legs.push(leg.id.clone());
            }
        }

        // Queue the SIP BYE(s) now so the main loop's pending_hangup drain
        // sends them on its next iteration. Without this the legs are merely
        // marked Ended and the cancel token fires a 3s shutdown drain — the
        // remote only sees the BYE after that delay (or never, when it hangs
        // up first), which is user-visible as "app requested hangup but the
        // call stays up".
        for leg_id in &ended_legs {
            if let Some(dialog) = self.legs.get_dialog(leg_id) {
                self.pending_hangup.insert(dialog.id());
            }
        }
        // The caller dialog is tracked on the session (not necessarily via
        // the leg registry) — queue it too when the caller leg ended. For
        // UAC sessions the caller dialog id falls back to the primary callee
        // dialog, which is the correct BYE target as well.
        if ended_legs.iter().any(|id| id.0 == "caller")
            || self.legs.values().all(|leg| leg.state == LegState::Ended)
        {
            self.pending_hangup.insert(self.caller_dialog_id());
        }

        self.sync_state();
        // Apply the cascade first: All marks the caller Ended, while a
        // hangup that leaves the caller alive must preserve the session.
        if self
            .legs
            .get(&LegId::from("caller"))
            .is_some_and(|leg| leg.state != LegState::Ended)
        {
            return CommandResult::success();
        }
        self.bridge.clear();

        // Compensating session_end: an IVR flow suspended on a resumable
        // hand-off (bridge/queue return pending) dies with the session when
        // the caller hangs up before the return app runs — emit the
        // session_end trace its executor suppressed (exactly-once contract).
        if self.meta.ivr_flow_suspended {
            self.meta.ivr_flow_suspended = false;
            self.emit_suspended_flow_session_end(super::util::map_suspended_flow_end(
                cmd.reason.as_ref(),
            ))
            .await;
        }

        if self.app_runtime.is_running() {
            let reason_str = cmd.reason.as_ref().map(|r| r.to_string());
            if let Err(e) = self.app_runtime.stop_app(reason_str).await {
                error!(session_id = %self.id, error = %e, "Failed to stop app during hangup");
            }
        }

        self.cancel_token.cancel();

        CommandResult::success()
    }

    fn emit_rwi_leg_hangup(&self, id: &LegId, reason: Option<String>) {
        let sip_status = reason.as_deref().and_then(|r| r.strip_prefix("Rejected with "))
            .and_then(|s| s.parse().ok());
        self.emit_typed_rwi_event(&crate::rwi::CallHangup {
            call_id: self.context.session_id.clone(), leg_id: Some(id.to_string()),
            reason, sip_status, hangup_by: None, duration_secs: None,
        });
    }

    pub(crate) fn update_leg_state(&mut self, leg_id: &LegId, new_state: LegState) -> bool {
        if let Some(leg) = self.legs.get_mut(leg_id) {
            let changed = leg.state != new_state;
            leg.state = new_state;
            if changed {
                match new_state {
                    LegState::Ringing | LegState::EarlyMedia => self.emit_typed_rwi_event(&crate::rwi::CallRinging {
                        call_id: self.context.session_id.clone(), leg_id: Some(leg_id.to_string()),
                        early_media: new_state == LegState::EarlyMedia,
                    }),
                    LegState::Connected => self.emit_typed_rwi_event(&crate::rwi::CallAnswered {
                        call_id: self.context.session_id.clone(), leg_id: Some(leg_id.to_string()),
                    }),
                    _ => {}
                }
            }
            self.sync_state();
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

    /// Fire `on_call_held` / `on_call_unheld` session hooks and emit the core
    /// `call_held` / `call_unheld` events when a leg transitions to/from
    /// [`LegState::Hold`].
    ///
    /// This centralizes hold detection so it works regardless of whether the
    /// transition is caused by an explicit `CallCommand::Hold/Unhold` or by an
    /// inbound re-INVITE carrying `sendonly`/`inactive` (or back to
    /// `sendrecv`). No-op when there is no transition.
    async fn fire_hold_transition_hooks(
        &mut self,
        leg_id: &LegId,
        prev: Option<LegState>,
        new: LegState,
    ) {
        let entered_hold = !matches!(prev, Some(LegState::Hold)) && new == LegState::Hold;
        let left_hold = matches!(prev, Some(LegState::Hold)) && new != LegState::Hold;
        if !entered_hold && !left_hold {
            return;
        }
        if entered_hold {
            self.record_trace(
                crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::Hold,
                    format!("Leg {} placed on hold", leg_id),
                )
                .severity(crate::call_errors::ErrSeverity::Info),
            );
        } else {
            self.record_trace(
                crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::Resume,
                    format!("Leg {} resumed from hold", leg_id),
                )
                .severity(crate::call_errors::ErrSeverity::Info),
            );
        }
        if self.server.session_hooks.is_empty() {
            // No hooks — still emit the core hold/unheld events (agent
            // attribution comes from the meta already published at
            // ringing/connected time).
            let event_call_id = self.context.session_id.clone();
            let leg_id_str = leg_id.to_string();
            if entered_hold {
                self.emit_typed_rwi_event(&crate::rwi::CallHeld {
                    call_id: event_call_id,
                    leg_id: leg_id_str,
                });
            } else {
                self.emit_typed_rwi_event(&crate::rwi::CallUnheld {
                    call_id: event_call_id,
                    leg_id: leg_id_str,
                });
            }
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
        if entered_hold {
            self.emit_typed_rwi_event(&crate::rwi::CallHeld {
                call_id: ctx.session_id.clone(),
                leg_id: leg_id_str.clone(),
            });
        } else {
            self.emit_typed_rwi_event(&crate::rwi::CallUnheld {
                call_id: ctx.session_id.clone(),
                leg_id: leg_id_str.clone(),
            });
        }
    }

    /// Emit a typed call lifecycle event via the new generic gateway API.
    pub(crate) fn emit_typed_rwi_event<E: crate::rwi::RwiEventSpec>(&self, event: &E) {
        if let Some(ref gw) = self.server.rwi_gateway {
            let g = gw.read();
            g.send_to_owner(event);
        }
    }

    /// Header names the SIP stack manages itself on every outbound INVITE
    /// (or that are REGISTER-dialog semantics). The registrar stores nearly
    /// the full REGISTER header set on the location, so without this filter
    /// those captured headers leak into a new leg's INVITE and duplicate
    /// the stack-generated ones (e.g. Contact ×3, User-Agent ×3).
    const LEG_INVITE_BLOCKED_HEADERS: &[&str] = &[
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
        "Allow",
        "Supported",
        "Expires",
        "Min-Expires",
        "Path",
        "Service-Route",
        "Max-Forwards",
        "Session-Expires",
        "Min-SE",
        "Session-ID",
        // Compact forms of stack-managed SIP headers.
        "v",
        "f",
        "t",
        "i",
        "m",
        "l",
        "c",
        "e",
        "k",
    ];

    fn is_leg_invite_header_name_allowed(name: &str) -> bool {
        if name
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Content-"))
        {
            return false;
        }
        !Self::LEG_INVITE_BLOCKED_HEADERS
            .iter()
            .any(|excluded| name.eq_ignore_ascii_case(excluded))
    }

    fn is_leg_invite_header_allowed(header: &rsipstack::sip::Header) -> bool {
        Self::is_leg_invite_header_name_allowed(header.name())
    }

    fn validate_transfer_headers(
        headers: &HashMap<String, String>,
    ) -> Result<Vec<(String, String)>, String> {
        let mut validated = Vec::with_capacity(headers.len());
        let mut seen = HashSet::with_capacity(headers.len());
        for (name, value) in headers {
            if !Self::is_sip_token(name) {
                return Err("invalid SIP header name".to_string());
            }
            if !Self::is_leg_invite_header_name_allowed(name) {
                return Err("SIP stack-managed header is not allowed".to_string());
            }
            if value.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0)) {
                return Err("invalid control character in SIP header value".to_string());
            }
            if !seen.insert(name.to_ascii_lowercase()) {
                return Err("duplicate SIP header name".to_string());
            }
            validated.push((name.clone(), value.clone()));
        }
        validated.sort_by(|left, right| {
            left.0
                .to_ascii_lowercase()
                .cmp(&right.0.to_ascii_lowercase())
        });
        Ok(validated)
    }

    fn is_sip_token(value: &str) -> bool {
        !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'-' | b'.' | b'!' | b'%' | b'*' | b'_' | b'+' | b'`' | b'\'' | b'~'
                    )
            })
    }

    /// Merge caller-supplied INVITE headers over location-derived headers
    /// (see `handle_add_leg`).
    ///
    /// Both sets are filtered against protocol-managed header names (see
    /// [`Self::LEG_INVITE_BLOCKED_HEADERS`]) — the stack generates its own
    /// Contact / User-Agent / Via / … on the outbound INVITE. The merge
    /// then dedupes by header name: caller (queue-enricher) headers come
    /// FIRST so they win; with no caller headers the filtered location set
    /// is returned unchanged.
    pub(crate) fn merge_leg_invite_headers(
        caller_headers: Vec<rsipstack::sip::Header>,
        location_headers: Option<Vec<rsipstack::sip::Header>>,
    ) -> Option<Vec<rsipstack::sip::Header>> {
        if caller_headers.is_empty() {
            return location_headers.map(|headers| {
                headers
                    .into_iter()
                    .filter(Self::is_leg_invite_header_allowed)
                    .collect::<Vec<_>>()
            });
        }
        let mut merged: Vec<rsipstack::sip::Header> = Vec::new();
        for header in caller_headers
            .into_iter()
            .chain(location_headers.into_iter().flatten())
            .filter(Self::is_leg_invite_header_allowed)
        {
            if merged
                .iter()
                .any(|existing| existing.name().eq_ignore_ascii_case(header.name()))
            {
                continue;
            }
            merged.push(header);
        }
        Some(merged)
    }

    async fn handle_add_leg(
        &mut self,
        target: String,
        leg_id: Option<LegId>,
        headers: Vec<rsipstack::sip::Header>,
        source_leg: Option<LegId>,

    ) -> Result<LegId> {
        let leg_id = leg_id.unwrap_or_else(|| LegId::new(format!("leg-{}", uuid::Uuid::new_v4())));
        let outcome = self.handle_add_leg_inner(target, Some(leg_id.clone()), headers, source_leg).await;
        match outcome {
            Ok(id) => Ok(id),
            Err(e) => {
                self.emit_rwi_leg_hangup(&leg_id, Some(e.to_string()));
                let in_queue = self.in_queue_context();
                let kind = if in_queue {
                    crate::call_errors::TraceKind::Queue
                } else {
                    crate::call_errors::TraceKind::Transfer
                };
                self.record_trace(
                    crate::call_errors::TraceEvent::new(
                        kind,
                        format!("Failed to add SIP leg: {}", e),
                    )
                    .severity(crate::call_errors::ErrSeverity::Error),
                );
                Err(e)
            }
        }
    }

    /// Add a new leg to the session dynamically.
    async fn handle_add_leg_inner(
        &mut self,
        target: String,
        leg_id: Option<LegId>,
        headers: Vec<rsipstack::sip::Header>,
        source_leg: Option<LegId>,

    ) -> Result<LegId> {
        let new_leg_id =
            leg_id.unwrap_or_else(|| LegId::new(format!("leg-{}", uuid::Uuid::new_v4())));

        info!(session_id = %self.id,
            %new_leg_id,
            %target,
            "Adding new SIP leg to session"
        );

        if let Some(source) = source_leg.as_ref() {
            self.require_leg(source)?;
        }
        if self.legs.get(&new_leg_id).is_some_and(|leg| leg.state != LegState::Ended) {
            return Err(anyhow!("Leg already exists: {}", new_leg_id));
        }

        // Normalize bare-user targets ("1002") with the selected realm —
        // same normalization the blind-transfer B-leg dial uses — so the
        // locator can match the registered AOR (user@realm). A hostless
        // URI silently misses the locator and the INVITE would go to the
        // default port where nobody listens.
        let realm = self.server.proxy_config.load().select_realm("");
        let normalized_target = crate::call::build_sip_uri(&target, &realm);
        let uri = parse_dial_target(&normalized_target)
            .or_else(|_| parse_dial_target(&target))
            .map_err(|e| anyhow!("Invalid SIP URI '{}': {}", target, e))?;
        let mut location = crate::call::Location {
            aor: uri.clone(),
            ..Default::default()
        };
        let mut registered = false;
        match self.server.locator.lookup(&uri).await {
            Ok(registered_locations) => {
                if let Some(registered_location) = registered_locations.into_iter().next() {
                    info!(
                        target = %uri,
                        registered_contact = %registered_location.aor,
                        webrtc = registered_location.supports_webrtc,
                        transport = ?registered_location.transport,
                        "Resolved dynamic leg target through locator"
                    );
                    location = registered_location;
                    registered = true;
                }
            }
            Err(error) => {
                warn!(
                    target = %uri,
                    %error,
                    "Failed to resolve dynamic leg target through locator; using bare SIP target"
                );
            }
        }

        // Not a registered internal contact — if routing for originated calls is
        // enabled, run it through the route table (match/rewrite/trunk).
        if !registered {
            match self.route_originated_leg(&location).await {
                Ok((routed, hints)) => {
                    location = routed;
                    self.track_routed_leg_hints(hints);
                }
                Err(e) => {
                    warn!(session_id = %self.id, target = %uri, error = %e, "Route lookup failed for dynamic leg; dialing directly");
                }
            }
        }

        // Merge caller-supplied INVITE headers (queue location enricher:
        // Call-Info / User-to-User) over any location-derived headers.
        if !headers.is_empty() {
            location.headers = Self::merge_leg_invite_headers(headers, location.headers.take());
        }

        if self.app_runtime.current_app().as_deref() == Some("queue") && self.media_leg(&LegId::from("caller")).is_none()
        {
            self.prepare_app_caller_media_bridge()
                .await
                .ok_or_else(|| anyhow!("Queue could not prepare caller media before dialing"))?;

        }

        let mut leg = crate::call::domain::Leg::new(new_leg_id.clone()).with_endpoint(target.clone());
        leg.source_leg = source_leg;
        self.legs.insert(new_leg_id.clone(), leg);
        self.update_leg_state(&new_leg_id, LegState::Initializing);
        self.record_leg_event(
            &new_leg_id.0,
            crate::callrecord::LegTimelineEventType::Added,
            None,
            Some(serde_json::json!({ "target": target })),
        );

        // Create peer and initiate INVITE in background
        if let Err(e) = self.initiate_sip_leg(&new_leg_id, location).await {
            warn!(
                session_id = %self.id,
                error = %e,
                "Failed to initiate SIP leg, cleaning up"
            );
            self.legs.remove(&new_leg_id);
            self.record_leg_event(
                &new_leg_id.0,
                crate::callrecord::LegTimelineEventType::Removed,
                None,
                Some(serde_json::json!({ "reason": "invite_failed", "error": e.to_string() })),
            );
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
    pub(super) async fn handle_remove_leg(&mut self, leg_id: LegId) -> Result<()> {
        info!(session_id = %self.id,
            %leg_id,
            "Removing leg from session"
        );

        if let Some(conf_id) = self.conference_bridge.conf_id.as_deref() {
            let _ = self.server.conference_server.remove_participant(
                &crate::call::runtime::ConferenceId::from(conf_id), &self.participant_leg(&leg_id),
            ).await;
        }
        // A normal proxy call can already have a media route without a
        // command-level BridgeConfig. Detach the actual selected peer.
        if self.conference_bridge.conf_id.is_none()
            && self.bridge().is_some_and(|bridge| bridge.side_for_leg(&leg_id).is_some())
        {
            if let Some(bridge) = self.bridge_mut() { bridge.unbridge().await?; }
        }
        let dialog_id = self.legs.get_dialog(&leg_id).map(|dialog| dialog.id())
            .or_else(|| (leg_id == self.resolve_transfer_leg(LegId::from("callee")))
                .then(|| self.meta.connected_callee_dialog_id.clone()).flatten());
        if let Some(dialog_id) = dialog_id {
            self.pending_hangup.insert(dialog_id.clone());
            self.callee_dialogs.remove(&dialog_id);
            if self.meta.connected_callee_dialog_id.as_ref() == Some(&dialog_id) {
                self.meta.connected_callee_dialog_id = None;
            }
        }
        if self.legs.remove(&leg_id).is_some() {
            info!(session_id = %self.id, %leg_id, "Leg removed");
            self.record_leg_event(
                &leg_id.0,
                crate::callrecord::LegTimelineEventType::Removed,
                None,
                None,
            );
        }

        self.update_media_path().await;
        Ok(())
    }

    /// Negotiate a leg independently of the currently selected bridge pair.
    async fn create_leg_peer(
        &self,
        leg_id: &LegId,
        mode: rustrtc::TransportMode,
    ) -> Result<(crate::media::leg::Leg, String)> {
        let source_sdp = self.legs.get(leg_id).and_then(|leg| leg.source_leg.as_ref())
            .and_then(|id| self.media_leg(id).and_then(|peer| peer.pc().remote_description())
                .map(|sdp| sdp.to_sdp_string())
                .or_else(|| self.legs.get_answer(id).map(str::to_owned)));
        let allow_codecs = self.resolve_effective_codecs();
        let mut codecs = if let Some(offer) = source_sdp.as_ref().or(self.media.caller_offer.as_ref()) {
            MediaNegotiator::build_callee_codec_offer_with_allow(offer, &allow_codecs)
        } else {
            let defaults = if mode == rustrtc::TransportMode::WebRtc {
                MediaNegotiator::default_webrtc_codecs()
            } else { MediaNegotiator::default_rtp_codecs() };
            MediaNegotiator::build_local_rtp_codec_offer(if allow_codecs.is_empty() {
                &defaults
            } else { &allow_codecs })
        };
        if mode == rustrtc::TransportMode::WebRtc {
            if let Some(offer) = source_sdp.as_ref().or(self.media.caller_offer.as_ref()) {
                codecs = MediaNegotiator::filter_webrtc_offer_codecs(offer, codecs);
            }
        }
        let mut video = if self.video_relay_enabled() {
            source_sdp.as_ref().or(self.media.caller_offer.as_ref()).map(|offer| self.video_caps_from_sdp(offer))
                .unwrap_or_default()
        } else { Vec::new() };
        if mode == rustrtc::TransportMode::WebRtc {
            MediaNegotiator::remap_bundle_video_payload_types(
                &mut video, codecs.iter().map(|codec| codec.payload_type),
            )?;
        }
        let cfg = self.build_leg_config(mode, codecs, video);
        let peer = crate::media::leg::LegInner::new(leg_id.as_str(), &cfg, None)?;
        let offer = peer.create_offer().await?;
        Ok((peer, offer))
    }

    /// Initiate a SIP INVITE for a dynamic leg.
    async fn initiate_sip_leg(
        &mut self,
        leg_id: &LegId,
        location: crate::call::Location,
    ) -> Result<()> {
        let callee_is_webrtc = Self::callee_supports_webrtc(&location);
        let transport_mode = self.callee_transport_mode(callee_is_webrtc);
        // Dialing owns a transport; bridge membership is selected only when
        // connecting. No queue/RWI distinction and no reserved B slot.
        let (peer, sdp_offer) = self.create_leg_peer(leg_id, transport_mode.clone()).await?;
        self.legs.set_media_leg(leg_id, peer.clone());
        self.legs.set_transport(leg_id.clone(), transport_mode);

        let self_ident = self.self_ident_addr();
        let route_via_home_proxy = Self::route_via_home_proxy(
            &location,
            !self.server.cluster_peer_ips.is_empty(),
            self_ident.as_ref(),
        );
        let callee_uri = Self::resolve_outbound_callee_uri(&location, route_via_home_proxy);

        info!(
            %leg_id,
            %callee_uri,
            callee_is_webrtc,
            sdp_len = %sdp_offer.len(),
            "Initiating SIP leg"
        );

        // Build INVITE option
        let caller = self.legs.get(leg_id).and_then(|leg| leg.source_leg.as_ref())
            .filter(|id| id.as_str() != "caller")
            .and_then(|id| self.legs.get_dialog(id).map(|dialog| dialog.to().uri)
                .or_else(|| self.legs.get(id)?.endpoint.as_ref()?.parse().ok()))
            .or_else(|| self.context.dialplan.caller.clone())
            .unwrap_or_else(|| callee_uri.clone());
        let contact = self
            .context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .or_else(|| self.server.contact_uri_for_location_with_sip_contact(
                &location, self.context.dialplan.media.sip_contact.as_ref(),
            ))
            .unwrap_or_else(|| caller.clone());

        // A reused logical leg (e.g. consult after rejection) is a new SIP call.
        // Linphone remembers declined Call-IDs and rejects attempts reusing one.
        let bleg_call_id = format!("{}-{}-{}", self.id.0, leg_id, uuid::Uuid::new_v4());
        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: callee_uri.clone(),
            caller: caller.clone(),
            contact: contact.clone(),
            content_type: Some("application/sdp".to_string()),
            offer: Some(sdp_offer.into_bytes()),
            destination: if route_via_home_proxy {
                None
            } else {
                location.destination.clone()
            },
            credential: location.credential.clone(),
            headers: location.headers.clone(),
            call_id: Some(bleg_call_id.clone()),
            // RFC 7989: queue/consult agent legs inherit the global session id.
            session_id: self.session_id_for_outgoing_leg(),
            ..Default::default()
        };

        // B-leg SIP destination for the CDR `calleePeer` field (see
        // build_target_invite_option; last dial wins). Bare `ip:port`;
        // URI-routed legs fall back to the request-URI host.
        self.meta.callee_peer = invite_option
            .destination
            .as_ref()
            .map(|d| d.addr.to_string())
            .or_else(|| Some(callee_uri.host_with_port.to_string()));

        // Register the B-leg SIP Call-ID as soon as the INVITE is built so
        // ringing-time CTI (`GET /cc/calls/{call_id}/context`) resolves before
        // the 200 OK / LegConnected notification.
        if let Some(handle) = self
            .server
            .active_call_registry
            .get_handle(&self.id.to_string())
        {
            self.server
                .active_call_registry
                .register_dialog(bleg_call_id, handle);
        }

        let dialog_layer = self.server.dialog_layer.clone();
        let leg_id_for_spawn = leg_id.clone();
        let session_id = self.id.to_string();
        let cmd_tx = self
            .cmd_tx
            .clone()
            .ok_or_else(|| anyhow!("No command sender available"))?;
        let cancel_token = self.cancel_token.child_token();

        let ring_timeout = Self::effective_ring_timeout(&self.context.dialplan, &self.server);

        // Spawn background task to handle INVITE response
        let invite_handle = crate::utils::spawn(async move {
            let leg_id = leg_id_for_spawn;
            let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

            let mut result: Result<InviteDialog, String> = Err("not started".to_string());
            let mut answered_guard = None;
            let mut state_rx_open = true;
            let deadline = tokio::time::sleep(ring_timeout.unwrap_or(Duration::from_secs(60)));
            tokio::pin!(deadline);

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => break,
                    _ = &mut deadline, if ring_timeout.is_some() => {
                        let _ = cmd_tx.send(CallCommand::LegFailed {
                            leg_id: leg_id.clone(), reason: "Ring timeout".to_string(),
                        }).await;
                        break;
                    }
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
                                            Some(sdp)
                                        } else {
                                            None
                                        };

                                        // Own the answered dialog before publishing it to the session.
                                        // Removing the leg aborts this task, including before
                                        // LegConnected has been processed.
                                        answered_guard = Some(ClientDialogGuard::new(dialog_layer.clone(), dialog.id()));
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
                                let body = resp.body();
                                if !body.is_empty() {
                                    info!(session_id = %session_id, %leg_id, "SIP leg early media (183)");
                                    let sdp = String::from_utf8_lossy(body).to_string();
                                    if let Err(error) = peer.apply_sdp(&sdp, rustrtc::SdpType::Pranswer).await {
                                        warn!(%leg_id, %error, "Failed to apply early media SDP");
                                    }
                                }
                                if !body.is_empty() || resp.status_code == rsipstack::sip::StatusCode::Ringing {
                                    info!(session_id = %session_id, %leg_id, "SIP leg ringing (180)");
                                    let _ = cmd_tx.send(CallCommand::LegRinging {
                                        leg_id: leg_id.clone(),
                                    }).await;
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
                let _guard = answered_guard;
                {
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
                }
            }
        });

        self.legs.push_task(leg_id.clone(), invite_handle);

        Ok(())
    }

    /// Keep only the explicitly requested pair.
    /// Additional legs wait for explicit selection; only mixer APIs create a
    /// conference.
    async fn update_media_path(&mut self) {
        if self.conference_bridge.conf_id.is_some() {
            return;
        }
        let requested = self.bridge.legs.clone();
        if requested.len() == 2 && requested.iter().all(|id| self.legs.get(id).is_some_and(|leg| {
            !matches!(leg.state, LegState::Ending | LegState::Ended)
        })) {
            // Hold changes audio output, not the selected conversation partner.
            // A playback completion or unmute while held must neither reconnect
            // media nor discard the pair needed by the subsequent unhold.
            if requested.iter().any(|id| self.legs.get(id).is_some_and(|leg| leg.state == LegState::Hold)) {
                return;
            }
            self.setup_bridge(requested[0].clone(), requested[1].clone()).await;
            return;
        }
        if self.bridge.active {
            self.clear_bridge().await;
        }
    }

}

/// Bridges a session's legs into a multi-party conference. Delegates the
/// per-leg audio wiring to the session's existing media-bridge glue and lets
/// [`crate::call::runtime::ConferenceServer`] own the participant lifecycle.
#[async_trait::async_trait]
impl crate::call::runtime::LegMediaBridger for SipSession {
    async fn bridge_into(&mut self, conf_id: &str, leg_id: &LegId) -> Result<()> {
        self.try_start_and_store_bridge(conf_id, leg_id, "automatic conference bridge")
            .await
    }

    async fn unbridge(&mut self, conf_id: &str, leg_id: &LegId) -> Result<()> {
        let prefix = format!("{}-", self.id);
        let local_leg = LegId::from(
            leg_id
                .as_str()
                .strip_prefix(&prefix)
                .unwrap_or(leg_id.as_str()),
        );
        drop(self.legs.remove_conference_bridge_handle(&local_leg));
        let _ = self
            .server
            .conference_server
            .leave_conference(conf_id, &self.participant_leg(&local_leg))
            .await;

        Ok(())
    }
}

impl SipSession {
    /// Append DTMF digits to the session's digit history, bounded so a very
    /// long call cannot grow this Vec without limit.
    fn record_dtmf_digits(&mut self, digits: &[char]) {
        const MAX_DTMF_DIGITS: usize = 512;
        let room = MAX_DTMF_DIGITS.saturating_sub(self.dtmf_digits.len());
        if room > 0 {
            self.dtmf_digits.extend(digits.iter().take(room));
        }
    }

    pub(crate) async fn setup_bridge(&mut self, leg_a: LegId, leg_b: LegId) -> bool {
        if leg_a == leg_b || !self.legs.contains_key(&leg_a) || !self.legs.contains_key(&leg_b) {
            return false;
        }
        self.bridge = BridgeConfig::bridge(leg_a.clone(), leg_b.clone());
        self.record_leg_event(
            &leg_a.0,
            crate::callrecord::LegTimelineEventType::Bridged,
            Some(leg_b.0.clone()),
            None,
        );
        self.record_leg_event(
            &leg_b.0,
            crate::callrecord::LegTimelineEventType::Bridged,
            Some(leg_a.0.clone()),
            None,
        );

        for id in [leg_a.clone(), leg_b.clone()] {
            if let Some(peer) = self.media_leg(&id) { self.legs.set_media_leg(&id, peer); }
        }
        if let (Some(a), Some(b)) = (self.legs.media_leg(&leg_a), self.legs.media_leg(&leg_b)) {
            if a.negotiated().is_none() || b.negotiated().is_none()
                || ![&leg_a, &leg_b].iter().all(|id| self.legs.get(id).is_some_and(|leg| matches!(leg.state, LegState::Connected | LegState::EarlyMedia))) {
                // Incident 2026-09-17: an answered agent leg whose ICE never
                // completed kept deferring here silently — the call showed
                // "answered" with no audio and no log trail. Make the
                // pathological case (legs answered but media never negotiated)
                // visible at warn; mid-call transitions stay debug.
                let legs_ready = ![&leg_a, &leg_b].iter().all(|id| self.legs.get(id).is_some_and(|leg| matches!(leg.state, LegState::Connected | LegState::EarlyMedia)));
                let a_neg = a.negotiated().is_some();
                let b_neg = b.negotiated().is_some();
                if legs_ready && (!a_neg || !b_neg) {
                    warn!(session_id = %self.id,
                        leg_a = %leg_a, leg_b = %leg_b,
                        a_negotiated = a_neg, b_negotiated = b_neg,
                        "Media bridge deferred: legs answered but peer not negotiated (ICE incomplete?)"
                    );
                } else {
                    debug!(session_id = %self.id, leg_a = %leg_a, leg_b = %leg_b,
                        "Media bridge deferred until usable media arrives");
                }
                self.sync_rtp_timeout_pause();
                return true; // Keep the requested pair until usable media arrives.
            }
            if self.media.bridge.is_none() {
                self.media.bridge = Some(MediaBridge::new(self.id.to_string()));
            }
            if !self.meta.media_health_monitor_spawned {
                Self::arm_media_health_monitor(
                    self.bridge().unwrap(),
                    self.cmd_tx.clone(),
                    &self.context.session_id,
                    self.context.dialplan.media.stall_detect_secs,
                    self.context.dialplan.media.media_trace_interval_secs,
                );
                self.meta.media_health_monitor_spawned = true;
            }
            if !self.meta.relay_arm_monitor_spawned {
                Self::arm_relay_arm_failure_monitor(self.bridge().unwrap(), self.cmd_tx.clone(), &self.context.session_id);
                self.meta.relay_arm_monitor_spawned = true;
            }
            let result = if let Some(mb) = self.bridge_mut() {
                match mb.select_pair(a, b).await {
                    Ok(()) => {
                        mb.accept(LegSide::A).await;
                        mb.accept(LegSide::B).await;
                        mb.bridge().await
                    }
                    Err(error) => Err(error),
                }
            } else { Err(anyhow!("No MediaBridge")) };
            if let Err(error) = result {
                warn!(session_id = %self.id, %error, "Bridge selection failed");
                self.bridge.clear();
                return false;
            }
            if let Some(bridge) = self.bridge() {
                Self::arm_bridged_rtp_timeouts(bridge, self.rtp_timeout_config(), self.cmd_tx.clone(), &self.context.session_id);
            }
            self.sync_rtp_timeout_pause();
            return true;
        }

        // Calls without media anchoring only maintain the logical pair.
        // An anchored call must have both independently negotiated peers.
        if !self.bypasses_local_media() {
            self.bridge.clear();
            return false;
        }
        true
    }

    async fn clear_bridge(&mut self) {
        let bridged: Vec<LegId> = self.bridge.legs.clone();
        for leg_id in &bridged {
            self.record_leg_event(
                &leg_id.0,
                crate::callrecord::LegTimelineEventType::Unbridged,
                None,
                None,
            );
        }
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

    /// Re-derive the session-level [`SessionState`] from the current leg states
    /// and refresh the externally-visible snapshot cache when it changes.
    ///
    /// This is the **single entry point** that keeps `self.state` and
    /// [`update_snapshot_cache`] in sync with leg reality. Called automatically
    /// by [`update_leg_state`] on every leg transition, so callers that mutate
    /// legs through the normal path never need to think about session state.
    fn sync_state(&mut self) {
        let new_state = Self::derive_state(&self.legs);
        if new_state != self.state {
            self.state = new_state;
            self.update_snapshot_cache();
        }
    }

    /// Resolve when playback finishes (natural EOF or interrupted), or when
    /// `cancel` fires (e.g. the caller hung up). Returns `None` when cancelled
    /// before the media layer signalled completion. Biased so a cancel that is
    /// already in flight wins over a simultaneous completion signal.
    async fn await_playback_done(
        mut done: tokio::sync::oneshot::Receiver<crate::media::media_bridge::PlaybackResult>,
        cancel: &CancellationToken,
    ) -> Option<crate::media::media_bridge::PlaybackResult> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            res = &mut done => res.ok(),
        }
    }

    /// Centralised "playback finished" side-effects: notify the app event loop
    /// (`AudioComplete`), emit the RWI `MediaPlayFinished` legacy event, and
    /// restore the media route. Shared by the awaited and fire-and-forget
    /// playback paths.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_playback_completion(
        app_event_bridge: &crate::proxy::proxy_call::state::AppEventBridge,
        rwi_gateway: &Option<crate::rwi::RwiGatewayRef>,
        session_id: &SessionId,
        event_leg_id_str: &Option<String>,
        track_id: &str,
        interrupted: bool,
        handle_for_restore: &SipSessionHandle,
    ) {
        let _ = app_event_bridge.send_app_event(crate::call::app::ControllerEvent::AudioComplete {
            track_id: track_id.to_string(),
            interrupted,
        });
        if let Some(gw) = rwi_gateway {
            gw.read().send_to_owner(&crate::rwi::MediaPlayFinished {
                call_id: session_id.to_string(),
                leg_id: event_leg_id_str.clone(),
                track_id: track_id.to_string(),
                interrupted,
            });
        }
        let _ = handle_for_restore.send_command(CallCommand::ResumeMedia);
    }

    pub(crate) async fn handle_play(
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
        let track_id = options
            .as_ref()
            .and_then(|o| o.track_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let file_path = match source {
            crate::call::domain::MediaSource::File { path } => Self::resolve_audio_file_path(&path),
            crate::call::domain::MediaSource::Url { url } => url,
            _ => return Err(anyhow!("Only file/URL playback supported")),
        };

        let target = leg_id.clone().unwrap_or_else(|| LegId::from("caller"));
        let in_ivr_exec = self.extensions.read()
            .get::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecState>().is_some();
        let single_peer = in_ivr_exec || options.as_ref().is_some_and(|o| o.side_only);
        let mut peers = Vec::new();
        if target.as_str() == "both" {
            // "Both" means the selected pair, or the lone caller before bridging.
            if let Some(mb) = self.bridge() {
                peers.extend([LegSide::A, LegSide::B].into_iter().filter_map(|side| mb.leg(side)));
            } else if let Some(peer) = self.media_leg(&LegId::from("caller")) {
                peers.push(peer);
            }
        } else {
            peers.push(self.media_leg(&target).ok_or_else(|| anyhow!("No media peer for {}", target))?);
            if !single_peer && self.media_side_for_leg(&target).is_some() {
                if let Some(other) = self.bridge().and_then(|mb| {
                    mb.side_for_leg(&target).and_then(|side| mb.leg(side.opposite()))
                }) { peers.push(other); }
            }
        }
        if peers.is_empty() { return Err(anyhow!("No media peer for playback")); }
        // Load sources before changing the active connection.
        let mut sources = Vec::new();
        for _ in &peers {
            sources.push(crate::media::audio_source::FileAudioSource::new(file_path.clone(), loop_playback).await?);
        }
        if peers.iter().any(|peer| self.media_side_for_leg(peer.id()).is_some()) {
            if let Some(mb) = self.bridge_mut() {
                if single_peer { mb.invalidate_route_cache(); }
                else { mb.unbridge().await?; }
            }
        }
        let mut handles = Vec::new();
        for (peer, audio) in peers.into_iter().zip(sources) {
            handles.push(peer.play_media(Box::new(audio), loop_playback).await?);
        }
        let events = self.app_event_bridge.clone();
        let gateway = self.server.rwi_gateway.clone();
        let session_id = self.id.clone();
        let target_id = leg_id.map(|id| id.to_string());
        let handle = self.handle.clone();
        let cancel = self.cancel_token.clone();
        let complete = async move {
            let mut interrupted = false;
            for playback in handles {
                interrupted |= Self::await_playback_done(playback.done, &cancel).await
                    .map(|result| result.interrupted).unwrap_or(true);
            }
            Self::dispatch_playback_completion(&events, &gateway, &session_id,
                &target_id, &track_id, interrupted, &handle);
        };
        if await_completion && !loop_playback { complete.await; }
        else { crate::utils::spawn(complete); }
        Ok(())
    }

    async fn handle_stop_playback(&mut self, leg_id: Option<LegId>) -> Result<()> {
        let ids: Vec<_> = match leg_id {
            None => self.legs.keys().cloned().collect(),
            Some(id) if id.as_str() == "both" => self.legs.keys().cloned().collect(),
            Some(id) => vec![id],
        };
        for id in ids {
            if let Some(peer) = self.media_leg(&id) {
                peer.stop_playback().await?;
            }
        }
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

        if let Some(dialog) = self.caller_dialog.as_ref() {
            if let Err(e) = dialog.ringing(None, None) {
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
        // A service-initiated DTMF (CTI / WS API) represents a keypress on
        // this call, so a running app (IVR menu, CSAT collect, …) must see it
        // exactly like an inbound RTP/SIP-INFO digit would be injected.
        let mut app_notified = false;
        for d in &valid_digits {
            app_notified |= super::util::inject_dtmf_into_app(
                *d,
                leg_id.as_str(),
                &self.id.to_string(),
                &self.app_runtime,
                &self.server.rwi_gateway,
            );
        }
        if let Some(peer) = self.media_leg(&leg_id) {
            match peer.send_dtmf(&digit_str).await {
                Ok(()) => {
                    self.record_dtmf_digits(&valid_digits);
                    info!(session_id = %self.id, %leg_id, digits = %digit_str, "DTMF sent via RTP RFC 2833 telephone-events");
                    return Ok(());
                }
                Err(error) => {
                    warn!(session_id = %self.id, %leg_id, %error, "RTP DTMF failed; falling back to SIP INFO");
                }
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
        } else if app_notified {
            return Ok(());
        } else {
            return Err(anyhow!("No dialog for leg: {}", leg_id));
        };

        match info_result {
            Ok(()) => {
                self.record_dtmf_digits(&valid_digits);
                info!(session_id = %self.id, %leg_id, digits = %digit_str, "DTMF sent via SIP INFO");
            }
            Err(e) => {
                if app_notified {
                    // The running app already consumed the digits; the far
                    // end simply does not support SIP INFO delivery.
                    info!(session_id = %self.id, error = %e, "SIP INFO DTMF failed; app already received digits");
                    return Ok(());
                }
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

        let id = self.legs.keys().find(|id| {
            track_id == id.as_str()
                || (id.as_str() == "caller" && track_id == Self::CALLER_TRACK_ID)
                || (id.as_str() == "callee" && track_id == Self::CALLEE_TRACK_ID)
                || track_id == format!("leg-{}-{}", self.id, id)
        }).cloned().ok_or_else(|| anyhow!("Unknown media track: {}", track_id))?;
        let peer = self.media_leg(&id).ok_or_else(|| anyhow!("No media for leg {}", id))?;
        if muted {
            if let Some(mb) = self.bridge_mut() {
                if mb.side_for_leg(&id).is_some() { mb.invalidate_route_cache(); }
            }
            peer.set_egress_source(crate::media::egress::EgressSource::Silence).await?;
        } else {
            self.update_media_path().await;
        }
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
        match server_dialog.message(Some(headers), Some(body_bytes)).await {
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
        match server_dialog.notify(Some(headers), Some(body_bytes)).await {
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

        // Fall back through the standard hold-music chain (X-Hold-Music
        // header/extension -> [proxy].hold_music -> built-in default) so the
        // held party always hears hold audio instead of silence.
        let music = music.or_else(|| self.resolve_hold_music(&[]));

        self.update_leg_state(&leg_id, LegState::Hold);

        let hold_sdp = self.generate_sdp_for_side(&leg_id, true)?;

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

                // Core hold event — agent attribution (agent_id / agent_name /
                // queue_id) is injected from the RWI call meta published at
                // ringing/connected time (replaces the former `cc_held`).
                self.emit_typed_rwi_event(&crate::rwi::CallHeld {
                    call_id: self.context.session_id.clone(),
                    leg_id: leg_id.to_string(),
                });

                // Switch the held leg's media egress to hold music (looping) or
                // CNG/silence. Mirrors propagate_hold_to_callee / _to_caller: without
                // this the leg stays on EgressSource::RewriteRelay, which parks the
                // ptime-paced egress loop and emits nothing to the held party.
                let peer = self.media_leg(&leg_id);
                if let Some(peer) = peer {
                    if self.bridge().is_some_and(|mb| mb.side_for_leg(&leg_id).is_some()) {
                        if let Some(mb) = self.bridge_mut() { mb.unbridge().await?; }
                    }
                    peer.pause_rtp_timeout();
                    let source = match &music {
                        Some(crate::call::domain::MediaSource::File { path })
                        | Some(crate::call::domain::MediaSource::Url { url: path }) => {
                            let resolved = if path.starts_with("http://") || path.starts_with("https://") {
                                path.clone()
                            } else { Self::resolve_audio_file_path(path) };
                            match crate::media::audio_source::FileAudioSource::new(resolved, true).await {
                                Ok(audio) => crate::media::egress::EgressSource::Media {
                                    audio: Box::new(audio), loop_playback: true, on_end: None,
                                },
                                Err(error) => {
                                    warn!(session_id = %self.id, %leg_id, %error, "Hold music failed; using silence");
                                    crate::media::egress::EgressSource::Silence
                                }
                            }
                        }
                        _ => crate::media::egress::EgressSource::Silence,
                    };
                    match source {
                        crate::media::egress::EgressSource::Media { audio, loop_playback, .. } => {
                            peer.play_media(audio, loop_playback).await?;
                        }
                        source => peer.set_egress_source(source).await?,
                    }
                }

                Ok(())
            }
            Err(e) => {
                warn!(session_id = %self.id, %leg_id, error = %e, "Failed to send hold re-INVITE");
                Ok(())
            }
        }
    }

    pub(crate) async fn handle_unhold(&mut self, leg_id: LegId) -> Result<()> {
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

        let unhold_sdp = self.generate_sdp_for_side(&leg_id, false)?;
        self.send_reinvite_to_leg(&leg_id, unhold_sdp).await?;

        // Joining Charlie already selected mixer output. Resume direct A/B
        // only when consultation was cancelled and the mixer was left.
        self.update_leg_state(&leg_id, LegState::Connected);
        if self.conference_bridge.conf_id.is_none() {
            self.update_media_path().await;
        }
        if let Some(peer) = self.media_leg(&leg_id) { peer.resume_rtp_timeout(); }
        self.update_leg_state(&leg_id, LegState::Connected);
        if !self.server.session_hooks.is_empty() {
            let ctx = self.session_hook_ctx();
            let leg_id_str = leg_id.to_string();
            for hook in self.server.session_hooks.iter() {
                hook.on_call_unheld(&ctx, &leg_id_str).await;
            }
        }
        // Core unhold event (replaces the former `cc_unheld`).
        self.emit_typed_rwi_event(&crate::rwi::CallUnheld {
            call_id: self.context.session_id.clone(),
            leg_id: leg_id.to_string(),
        });
        Ok(())
    }

    /// Send a re-INVITE (e.g. hold/unhold SDP) to the dialog of the target leg
    /// ("caller" → the primary caller dialog; "callee" → the callee dialog).
    async fn send_reinvite_to_leg(&self, leg_id: &LegId, sdp: String) -> Result<()> {
        let headers = Self::sdp_headers();
        let dialog = if leg_id.0 != "caller" {
            self.legs
                .get_dialog(leg_id)
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
        match dialog.reinvite(Some(headers), Some(sdp.into_bytes())).await {
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

impl Drop for SipSession {
    fn drop(&mut self) {
        self.cancel_token.cancel();

        // Cancel the running app's event loop synchronously. On the normal
        // path cleanup() → stop_app() already cancelled it (so this is a
        // no-op), but on abnormal teardown (task abort/panic) the app loop
        // would otherwise keep blocking on its event channel and retain
        // ApplicationContext.
        self.app_runtime.cancel_sync();

        // Safety net for task abort, panic, or runtime shutdown before cleanup.
        self.concurrent_call_lease.release_all();
        // Routed-leg leases release their permits when dropped (each permit is
        // an OwnedSemaphorePermit). Drain explicitly to free slots promptly.
        self.transient_leases.clear();

        self.callee_guards.clear();

        self.callee_event_tx = None;

        self.callee_dialogs.clear();
        self.meta.connected_callee_dialog_id = None;
        self.timers.clear();
        self.timer_queue.clear();
        self.timer_keys.clear();

        if let Some(conf_id) = self.conference_bridge.conf_id.clone()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let server = self.server.conference_server.clone();
            let participants: Vec<_> = self
                .legs
                .keys()
                .map(|id| self.participant_leg(id))
                .collect();
            runtime.spawn(async move {
                let conf_id = crate::call::runtime::ConferenceId::from(conf_id.as_str());
                for participant in participants {
                    let _ = server.remove_participant(&conf_id, &participant).await;
                }
            });
        }
        // Stop conference bridges (safety net — cancel only, since we can't
        // .await in Drop)
        self.conference_bridge.stop_bridge();
        self.voip_bridge = None;
        self.legs.stop_all_conference_bridge_handles();

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

        // RWI cleanup is owned by the CallRecord guard created by the reporter
        // above, or by the originate task for UAC sessions without a reporter.
    }
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;

#[cfg(test)]
mod route_via_home_proxy_tests {
    use super::*;
    use rsipstack::sip::HostWithPort;

    fn sip_addr(addr: &str) -> SipAddr {
        SipAddr {
            r#type: None,
            addr: HostWithPort::try_from(addr).unwrap(),
        }
    }

    fn target_with_home(home: &str) -> Location {
        Location {
            home_proxy: Some(sip_addr(home)),
            ..Default::default()
        }
    }

    #[test]
    fn peer_home_routes_via_home_node_even_when_connections_know_the_address() {
        // Regression: the self check used live `endpoint.get_addrs()`, which
        // mixes in remote addresses of active connections — a peer node's
        // home address this node once connected to looked "local" and the
        // INVITE was pushed into another node's WebSocket (black-holed).
        // Self detection now relies on the deterministic cluster_self_addr.
        let target = target_with_home("172.25.224.232:15060");
        let cluster_self = sip_addr("172.25.225.2:15060");

        assert!(SipSession::route_via_home_proxy(
            &target,
            true,
            Some(&cluster_self)
        ));
    }

    #[test]
    fn own_cluster_address_delivers_locally() {
        let target = target_with_home("172.25.225.2:15060");
        let cluster_self = sip_addr("172.25.225.2:15060");

        assert!(!SipSession::route_via_home_proxy(
            &target,
            true,
            Some(&cluster_self)
        ));
    }

    #[test]
    fn fallback_identity_mirrors_registrar_stamping() {
        // When cluster self resolution fails the registrar stamps
        // default_contact_uri (the NAT/external address) as home_proxy, so
        // the fallback identity must be that same address: a home equal to
        // it is OUR registration (deliver locally), a peer's external home
        // is remote — regardless of any address this node happens to hold
        // connections to.
        let own_contact = sip_addr("116.62.75.161:15060");
        assert!(!SipSession::route_via_home_proxy(
            &target_with_home("116.62.75.161:15060"),
            true,
            Some(&own_contact)
        ));
        assert!(SipSession::route_via_home_proxy(
            &target_with_home("116.62.250.247:15060"),
            true,
            Some(&own_contact)
        ));
    }

    #[test]
    fn unknown_identity_routes_to_home_node() {
        // Without any identity the home node is authoritative for its own
        // registrations — forward there instead of guessing.
        assert!(SipSession::route_via_home_proxy(
            &target_with_home("172.25.224.232:15060"),
            true,
            None
        ));
    }

    #[test]
    fn no_cluster_or_no_home_proxy_never_routes_away() {
        let target = target_with_home("172.25.224.232:15060");
        let cluster_self = sip_addr("172.25.225.2:15060");

        assert!(!SipSession::route_via_home_proxy(
            &target,
            false,
            Some(&cluster_self)
        ));

        let no_home = Location::default();
        assert!(!SipSession::route_via_home_proxy(
            &no_home,
            true,
            Some(&cluster_self)
        ));
    }
}
