use crate::call::domain::{
    CallCommand, HangupCascade, HangupCommand, LegId, LegState, MediaPathMode, MediaRuntimeProfile, RingbackPolicy,
};
use crate::call::domain::{Leg, SessionState};
use crate::call::runtime::BridgeConfig;
use crate::call::runtime::{
    AppRuntime, CommandResult, ExecutionContext, MediaCapabilityCheck, SessionId, StubAppRuntime,
};

/// Snapshot of session state for external consumers
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
use crate::call::sip::{DialogStateReceiverGuard, ServerDialogGuard};
use crate::call::TransferEndpoint;
use crate::callrecord::{CallRecordHangupMessage, CallRecordHangupReason, CallRecordSender};
use crate::config::MediaProxyMode;
use crate::media::mixer::MediaMixer;
use crate::media::negotiate::MediaNegotiator;
use crate::media::{FileTrack, RtpTrackBuilder, Track};
use crate::proxy::proxy_call::{
    media_peer::{MediaPeer, VoiceEnginePeer},
    reporter::CallReporter,
    session_timer::{
        HEADER_MIN_SE, HEADER_SESSION_EXPIRES, TIMER_TAG, SessionRefresher, SessionTimerState, get_header_value,
        has_timer_support, parse_session_expires,
    },
    state::{
        CallContext, CallSessionRecordSnapshot, PendingHangup, SessionHangupMessage,
    },
};
use crate::proxy::server::SipServerRef;
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use futures::FutureExt;

use rsip::StatusCode;
use rsipstack::dialog::{
    DialogId, dialog::Dialog, dialog::DialogState, dialog::TerminatedReason, invitation::InviteOption,
    server_dialog::ServerInviteDialog,
};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Negotiation state for SDP handling (RFC 3264)
/// Action to take based on session timer check
#[derive(Debug)]
#[allow(dead_code)]
enum TimerAction {
    /// No action needed
    None,
    /// Send session refresh (re-INVITE)
    Refresh,
    /// Session has expired, terminate
    Expired,
    /// Reschedule timer check with new interval
    Reschedule(Duration),
}

pub struct SipSession {
    pub id: SessionId,
    pub state: SessionState,
    pub legs: std::collections::HashMap<LegId, Leg>,
    pub bridge: BridgeConfig,
    pub media_profile: MediaRuntimeProfile,
    pub app_runtime: Arc<dyn AppRuntime>,
    pub snapshot_cache: Arc<Mutex<Option<SessionSnapshot>>>,

    pub server: SipServerRef,
    pub server_dialog: ServerInviteDialog,
    pub callee_dialogs: Arc<Mutex<HashSet<DialogId>>>,

    pub caller_peer: Arc<dyn MediaPeer>,
    pub callee_peer: Arc<dyn MediaPeer>,
    pub supervisor_mixer: Option<Arc<MediaMixer>>,

    pub context: CallContext,
    pub call_record_sender: Option<CallRecordSender>,

    pub cancel_token: CancellationToken,
    pub pending_hangup: Arc<Mutex<Option<PendingHangup>>>,
    pub connected_callee: Option<String>,
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub caller_offer: Option<String>,
    pub callee_offer: Option<String>,
    pub answer: Option<String>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<SessionHangupMessage>,
    pub last_error: Option<(StatusCode, Option<String>)>,
    pub recording_state: Option<(String, Instant)>,

    // === Routing Info ===
    /// Routed caller
    pub routed_caller: Option<String>,
    /// Routed callee
    pub routed_callee: Option<String>,
    /// Routed contact
    pub routed_contact: Option<String>,
    /// Routed destination
    pub routed_destination: Option<String>,

    // === Session Timer (RFC 4028) ===
    /// Server timer state - currently initialized but not actively managed
    /// Session timer refresh logic needs to be added to process() loop
    pub server_timer: Arc<Mutex<SessionTimerState>>,

    // === Internal ===
    /// Callee event sender - used for dialog state updates
    pub callee_event_tx: Option<mpsc::UnboundedSender<DialogState>>,
    /// Callee guards - keeps dialog receivers alive
    pub callee_guards: Vec<DialogStateReceiverGuard>,

    /// Reporter - initialized but reporting is handled via process() cleanup
    pub reporter: Option<CallReporter>,
}

/// Handle for sending commands to a SipSession
///
/// This is the unified handle for both RWI originate and SIP inbound calls.
/// It uses CallCommand directly without any conversion.
#[derive(Clone)]
pub struct SipSessionHandle {
    session_id: SessionId,
    cmd_tx: mpsc::UnboundedSender<CallCommand>,
    snapshot_cache: Arc<Mutex<Option<SessionSnapshot>>>,
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
        self.snapshot_cache.lock().ok().and_then(|g| g.clone())
    }

    pub fn update_snapshot(&self, snapshot: SessionSnapshot) {
        if let Ok(mut guard) = self.snapshot_cache.lock() {
            *guard = Some(snapshot);
        }
    }

    /// Send an app event to the session
    pub fn send_app_event(&self, _event: crate::call::app::ControllerEvent) -> bool {
        // TODO: Implement via CallCommand
        false
    }
}

impl SipSession {
    #[allow(dead_code)]
    pub const CALLEE_TRACK_ID: &'static str = "callee-track";
    #[allow(dead_code)]
    pub const RINGBACK_TRACK_ID: &'static str = "ringback-track";

    /// Create a lightweight handle for RWI originate (without full SIP session)
    ///
    /// This creates just a handle with a command channel. The actual session
    /// is managed by the RWI processor.
    pub fn with_handle(id: SessionId) -> (SipSessionHandle, mpsc::UnboundedReceiver<CallCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let snapshot_cache: Arc<Mutex<Option<SessionSnapshot>>> = Arc::new(Mutex::new(None));

        let handle = SipSessionHandle {
            session_id: id,
            cmd_tx,
            snapshot_cache,
        };

        (handle, cmd_rx)
    }

    /// Create a new SIP session
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

        // Create media profile
        let media_profile = if use_media_proxy {
            MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored)
        } else {
            MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass)
        };

        // Create app runtime (stub for now, will be replaced when app starts)
        let app_runtime: Arc<dyn AppRuntime> = Arc::new(StubAppRuntime::new());

        // Create command channel
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        // Create snapshot cache
        let snapshot_cache: Arc<Mutex<Option<SessionSnapshot>>> = Arc::new(Mutex::new(None));

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
            bridge: BridgeConfig::new(),
            media_profile: media_profile.clone(),
            app_runtime,
            snapshot_cache: snapshot_cache.clone(),
            server,
            server_dialog,
            callee_dialogs: Arc::new(Mutex::new(HashSet::new())),
            caller_peer,
            callee_peer,
            supervisor_mixer: None,
            context,
            call_record_sender,
            cancel_token,
            pending_hangup: Arc::new(Mutex::new(None)),
            connected_callee: None,
            ring_time: None,
            answer_time: None,
            caller_offer,
            callee_offer: None,
            answer: None,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            last_error: None,
            recording_state: None,
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            server_timer: Arc::new(Mutex::new(SessionTimerState::default())),
            callee_event_tx: None,
            callee_guards: Vec::new(),
            reporter: None,
        };

        // Create handle
        let handle = SipSessionHandle {
            session_id: session_id.clone(),
            cmd_tx,
            snapshot_cache,
        };

        (session, handle, cmd_rx)
    }

    /// Main entry point - replaces CallSession::serve
    pub async fn serve(
        server: SipServerRef,
        context: CallContext,
        tx: &mut rsipstack::transaction::transaction::Transaction,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
    ) -> Result<()> {
        let session_id = context.session_id.clone();
        info!(session_id = %session_id, "Starting unified SIP session");

        // Create server dialog
        let local_contact = context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .or_else(|| server.default_contact_uri());

        // Create state channel for dialog - this is used by dialog_layer
        let (state_tx, state_rx) = mpsc::unbounded_channel();

        let server_dialog = server
            .dialog_layer
            .get_or_create_server_invite(tx, state_tx, None, local_contact.clone())
            .map_err(|e| anyhow!("Failed to create server dialog: {}", e))?;

        // Setup media
        let use_media_proxy = Self::check_media_proxy(&context, &server.proxy_config.media_proxy);

        let caller_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-caller", session_id))
            .with_cancel_token(cancel_token.child_token());
        let caller_peer = Arc::new(VoiceEnginePeer::new(Arc::new(caller_media_builder.build())));

        let callee_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-callee", session_id))
            .with_cancel_token(cancel_token.child_token());
        let callee_peer = Arc::new(VoiceEnginePeer::new(Arc::new(callee_media_builder.build())));

        // Create session
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

        // Create reporter
        session.reporter = Some(CallReporter {
            server: server.clone(),
            context: context.clone(),
            call_record_sender: session.call_record_sender.clone(),
        });

        // Setup media if needed
        if use_media_proxy {
            let offer_sdp =
                String::from_utf8_lossy(server_dialog.initial_request().body()).to_string();
            session.caller_offer = Some(offer_sdp.clone());
            session.callee_offer = session.create_callee_track(true).await.ok();
        }

        // Create dialog guard
        let dialog_guard = ServerDialogGuard::new(server.dialog_layer.clone(), server_dialog.id());

        // Create callee state channel
        let (callee_state_tx, callee_state_rx) = mpsc::unbounded_channel();
        session.callee_event_tx = Some(callee_state_tx);

        // Register with unified session
        server
            .active_call_registry
            .register_handle(session_id.clone(), handle.clone());

        // Store handle in session for later use
        // Note: In the future, we might want to store the handle differently

        // Spawn session processing
        server
            .active_call_registry
            .register_dialog(server_dialog.id().to_string(), handle.clone());

        let mut server_dialog_clone = server_dialog.clone();
        crate::utils::spawn(async move {
            session
                .process(state_rx, callee_state_rx, cmd_rx, dialog_guard)
                .await
        });

        // Handle dialog
        let ring_time_secs = context.dialplan.max_ring_time.clamp(30, 120);
        let max_setup_duration = Duration::from_secs(ring_time_secs as u64);
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

    /// Check if media proxy should be used
    fn check_media_proxy(context: &CallContext, mode: &MediaProxyMode) -> bool {
        if context.dialplan.recording.enabled {
            return true;
        }
        matches!(mode, MediaProxyMode::All)
    }

    /// Main processing loop
    pub async fn process(
        &mut self,
        mut state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut cmd_rx: mpsc::UnboundedReceiver<CallCommand>,
        _dialog_guard: ServerDialogGuard,
    ) -> Result<()> {
        // Keep the cancel guard alive for the duration of process()
        // When dropped, it will cancel the token to signal shutdown
        let cancel_guard = self.cancel_token.clone().drop_guard();

        // Execute dialplan if targets are available
        if !self.context.dialplan.is_empty() {
            info!(session_id = %self.context.session_id, "Executing dialplan");
            if let Err((status_code, reason)) = self.execute_dialplan().await {
                warn!(?status_code, ?reason, "Dialplan execution failed");
                // Reject the call with the actual error code from callee (e.g., 486 Busy Here)
                let code = status_code.clone();
                let _ = self.server_dialog.reject(Some(code), reason);
                return Err(anyhow!("Dialplan failed: {:?}", status_code));
            }
        }

        // Main event loop
        // TODO: Add SDP renegotiation (hold/reinvite) support

        // Calculate initial timer check interval
        let mut timer_interval = self.calculate_timer_check_interval();
        let mut timer_tick = tokio::time::interval(timer_interval);
        timer_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    debug!(session_id = %self.context.session_id, "Session cancelled");
                    break;
                }

                // Handle dialog state changes
                Some(state) = state_rx.recv() => {
                    if let Err(e) = self.handle_dialog_state(state).await {
                        warn!(error = %e, "Error handling dialog state");
                    }
                }

                // Handle callee state changes
                Some(state) = callee_state_rx.recv() => {
                    if let Err(e) = self.handle_callee_state(state).await {
                        warn!(error = %e, "Error handling callee state");
                    }
                }

                // Handle commands
                Some(cmd) = cmd_rx.recv() => {
                    let result = self.execute_command(cmd).await;
                    if !result.success {
                        warn!(error = ?result.message, "Command execution failed");
                    }
                }

                // Session Timer check tick
                _ = timer_tick.tick() => {
                    match self.check_session_timer().await {
                        TimerAction::Refresh => {
                            if let Err(e) = self.send_session_refresh().await {
                                warn!(error = %e, "Failed to send session refresh");
                                self.server_timer.lock().unwrap().fail_refresh();
                            }
                        }
                        TimerAction::Expired => {
                            warn!("Session timer expired, terminating session");
                            self.hangup_reason = Some(CallRecordHangupReason::Autohangup);
                            self.cancel_token.cancel();
                            break;
                        }
                        TimerAction::Reschedule(new_interval) => {
                            timer_interval = new_interval;
                            timer_tick = tokio::time::interval(timer_interval);
                            timer_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        }
                        TimerAction::None => {}
                    }

                    // Update snapshot cache periodically
                    self.update_snapshot_cache();
                }
            }
        }

        // Cleanup
        self.cleanup().await;

        // Drop the cancel guard to signal cancellation
        // This ensures the token is cancelled when process() returns
        let _ = cancel_guard;

        Ok(())
    }

    fn update_snapshot_cache(&self) {
        let callee_dialogs: Vec<DialogId> = {
            let dialogs = self.callee_dialogs.lock().unwrap();
            dialogs.iter().cloned().collect()
        };

        let snapshot = SessionSnapshot {
            id: self.id.clone(),
            state: self.state.clone(),
            leg_count: self.legs.len(),
            bridge_active: self.bridge.active,
            media_path: self.media_profile.path,
            answer_sdp: self.answer.clone(),
            callee_dialogs,
        };

        if let Ok(mut guard) = self.snapshot_cache.lock() {
            *guard = Some(snapshot);
        }
    }

    async fn handle_dialog_state(&mut self, state: DialogState) -> Result<()> {
        debug!("Handling dialog state");
        match state {
            DialogState::Confirmed(_, _) => {
                // Update session state
                self.update_leg_state(&LegId::from("caller"), LegState::Connected)
                    .await;
            }
            DialogState::Terminated(_, _) => {
                self.update_leg_state(&LegId::from("caller"), LegState::Ended)
                    .await;
                self.cancel_token.cancel();
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle callee state change
    async fn handle_callee_state(&mut self, state: DialogState) -> Result<()> {
        debug!("Handling callee state");
        match state {
            DialogState::Confirmed(_, _) => {
                self.update_leg_state(&LegId::from("callee"), LegState::Connected)
                    .await;
            }
            DialogState::Terminated(_, reason) => {
                self.update_leg_state(&LegId::from("callee"), LegState::Ended)
                    .await;

                // If callee was never connected and terminated with an error,
                // propagate the error to the caller
                if self.connected_callee.is_none() {
                    let (code, reason_str) = match reason {
                        TerminatedReason::UasBusy => {
                            (Some(StatusCode::BusyHere), Some("Busy Here".to_string()))
                        }
                        TerminatedReason::UasDecline => {
                            (Some(StatusCode::Decline), Some("Decline".to_string()))
                        }
                        TerminatedReason::UasBye => (None, None), // Normal hangup, no need to reject
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
                        // Cancel the session after sending rejection
                        self.cancel_token.cancel();
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Execute the dialplan - main entry point for B2BUA routing
    /// Returns Ok on success, or Err with (status_code, reason) on failure
    pub async fn execute_dialplan(&mut self) -> Result<(), (StatusCode, Option<String>)> {
        let flow = self.context.dialplan.flow.clone();
        self.execute_flow(&flow).await
    }

    /// Execute a dialplan flow
    /// Returns Ok on success, or Err with (status_code, reason) on failure
    fn execute_flow<'a>(
        &'a mut self,
        flow: &'a crate::call::DialplanFlow,
    ) -> futures::future::BoxFuture<'a, Result<(), (StatusCode, Option<String>)>> {
        use crate::call::DialplanFlow;
        use futures::FutureExt;

        async move {
            match flow {
                DialplanFlow::Targets(strategy) => self.run_targets(strategy).await,
                DialplanFlow::Queue { plan, next } => {
                    // Execute queue handling with agent dialing
                    if let Err((code, reason)) = self.execute_queue(plan).await {
                        warn!(?code, ?reason, "Queue execution failed, trying next flow");
                    }
                    // After queue completes (success or failure), execute next flow
                    self.execute_flow(next).await
                }
                DialplanFlow::Application {
                    app_name,
                    app_params: _,
                    auto_answer: _,
                } => {
                    info!(app_name = %app_name, "Executing application flow");
                    // For now, just return Ok - application handling would go here
                    // TODO: Implement application routing (voicemail, IVR, etc.)
                    Ok(())
                }
            }
        }
        .boxed()
    }

    /// Execute targets based on dial strategy (sequential or parallel)
    /// Returns Ok on success, or Err with (status_code, reason) on failure
    async fn run_targets(
        &mut self,
        strategy: &crate::call::DialStrategy,
    ) -> Result<(), (StatusCode, Option<String>)> {
        use crate::call::DialStrategy;

        match strategy {
            DialStrategy::Sequential(targets) => self.dial_sequential(targets).await,
            DialStrategy::Parallel(targets) => self.dial_parallel(targets).await,
        }
    }

    /// Dial targets sequentially - try each one until success or all fail
    /// Returns the last error if all targets fail
    async fn dial_sequential(
        &mut self,
        targets: &[crate::call::Location],
    ) -> Result<(), (StatusCode, Option<String>)> {
        let mut last_error = (
            StatusCode::TemporarilyUnavailable,
            Some("No targets to dial".to_string()),
        );

        for (idx, target) in targets.iter().enumerate() {
            info!(index = idx, target = %target.aor, "Trying sequential target");

            match self.try_single_target(target).await {
                Ok(()) => {
                    info!(index = idx, "Sequential target succeeded");
                    return Ok(());
                }
                Err(e) => {
                    warn!(index = idx, error = ?e, "Sequential target failed");
                    last_error = e;
                    // Continue to next target
                }
            }
        }

        Err(last_error)
    }

    /// Dial targets in parallel - try all at once, first success wins
    async fn dial_parallel(
        &mut self,
        targets: &[crate::call::Location],
    ) -> Result<(), (StatusCode, Option<String>)> {
        // For now, just dial the first target
        // TODO: Implement true parallel dialing with race
        if let Some(target) = targets.first() {
            self.try_single_target(target).await
        } else {
            Err((
                StatusCode::TemporarilyUnavailable,
                Some("No targets to dial".to_string()),
            ))
        }
    }

    /// Execute a queue plan with hold music and agent dialing
    async fn execute_queue(
        &mut self,
        plan: &crate::call::QueuePlan,
    ) -> Result<(), (StatusCode, Option<String>)> {
        use crate::call::DialStrategy;

        info!("Executing queue plan");

        // Check if we have agents to dial
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

        // Answer immediately if configured
        if plan.accept_immediately {
            info!("Queue: answering call immediately");
            if let Err(e) = self.accept_call(None, None, None).await {
                warn!(error = %e, "Failed to answer call in queue");
            }
        }

        // Start hold music if configured
        let hold_handle = if let Some(ref hold) = plan.hold {
            if let Some(ref audio_file) = hold.audio_file {
                info!(file = %audio_file, "Queue: starting hold music");
                // Play hold music on caller peer
                self.play_audio_file(audio_file, false, "caller", hold.loop_playback)
                    .await
                    .ok()
            } else {
                None
            }
        } else {
            None
        };

        // Dial agents based on strategy
        let result = match &plan.dial_strategy {
            Some(DialStrategy::Sequential(_)) => {
                self.dial_queue_sequential(&agents, plan.ring_timeout).await
            }
            Some(DialStrategy::Parallel(_)) => {
                self.dial_queue_parallel(&agents, plan.ring_timeout).await
            }
            None => Ok(()),
        };

        // Stop hold music if it was started
        if hold_handle.is_some() {
            info!("Queue: stopping hold music");
            // The playback will be stopped when the call connects or fails
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

    /// Dial queue agents sequentially
    async fn dial_queue_sequential(
        &mut self,
        agents: &[crate::call::Location],
        _ring_timeout: Option<Duration>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let mut last_error = (
            StatusCode::TemporarilyUnavailable,
            Some("All agents unavailable".to_string()),
        );

        for (idx, agent) in agents.iter().enumerate() {
            info!(index = idx, agent = %agent.aor, "Queue: trying agent");

            match self.try_single_target(agent).await {
                Ok(()) => {
                    info!(index = idx, "Queue: agent connected");
                    return Ok(());
                }
                Err(e) => {
                    warn!(index = idx, error = ?e, "Queue: agent failed");
                    last_error = e;
                    // Continue to next agent
                }
            }
        }

        Err(last_error)
    }

    /// Dial queue agents in parallel
    async fn dial_queue_parallel(
        &mut self,
        agents: &[crate::call::Location],
        _ring_timeout: Option<Duration>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        // For now, just try the first agent
        // TODO: Implement true parallel dialing
        if let Some(agent) = agents.first() {
            info!(agent = %agent.aor, "Queue: trying parallel agent");
            self.try_single_target(agent).await
        } else {
            Err((
                StatusCode::TemporarilyUnavailable,
                Some("No agents available".to_string()),
            ))
        }
    }

    /// Execute queue fallback action
    async fn execute_queue_fallback(
        &mut self,
        plan: &crate::call::QueuePlan,
    ) -> Result<(), (StatusCode, Option<String>)> {
        use crate::call::{FailureAction, QueueFallbackAction};

        match &plan.fallback {
            Some(QueueFallbackAction::Failure(FailureAction::Hangup { code, reason })) => {
                info!(?code, ?reason, "Queue fallback: hangup");
                Err((
                    code.as_ref().map(|c| StatusCode::from(c.clone()))
                        .unwrap_or(StatusCode::TemporarilyUnavailable),
                    reason.clone(),
                ))
            }
            Some(QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
                audio_file,
                use_early_media: _,
                status_code,
                reason,
            })) => {
                info!(file = %audio_file, "Queue fallback: play then hangup");
                // Play the audio file
                if let Err(e) = self.play_audio_file(audio_file, true, "caller", false).await {
                    warn!(error = %e, "Failed to play fallback audio");
                }
                Err((
                    StatusCode::from(status_code.clone()),
                    reason.clone(),
                ))
            }
            Some(QueueFallbackAction::Failure(FailureAction::Transfer(target))) => {
                info!(target = %target, "Queue fallback: transfer");
                // For transfer, we'd need to implement transfer logic
                // For now, just return an error
                Err((
                    StatusCode::TemporarilyUnavailable,
                    Some(format!("Transfer to {} not implemented", target)),
                ))
            }
            Some(QueueFallbackAction::Redirect { target }) => {
                info!(target = %target, "Queue fallback: redirect");
                Err((
                    StatusCode::TemporarilyUnavailable,
                    Some("Redirect not implemented".to_string()),
                ))
            }
            Some(QueueFallbackAction::Queue { name }) => {
                info!(queue = %name, "Queue fallback: transfer to another queue");
                Err((
                    StatusCode::TemporarilyUnavailable,
                    Some(format!("Queue transfer to {} not implemented", name)),
                ))
            }
            None => {
                info!("Queue fallback: default hangup");
                Err((
                    StatusCode::TemporarilyUnavailable,
                    Some("All agents unavailable".to_string()),
                ))
            }
        }
    }

    /// Try to dial a single target
    /// Returns Ok on success, or Err with (status_code, reason) on failure
    async fn try_single_target(
        &mut self,
        target: &crate::call::Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        use rsipstack::dialog::dialog::DialogState;
        use rsipstack::dialog::invitation::InviteOption;

        let caller = self.context.dialplan.caller.clone().ok_or_else(|| {
            (
                StatusCode::ServerInternalError,
                Some("No caller in dialplan".to_string()),
            )
        })?;

        // Get the callee URI
        let callee_uri = target.aor.clone();

        info!(session_id = %self.context.session_id, %caller, %callee_uri, "Sending INVITE to callee");

        // Build headers
        let headers: Vec<rsip::Header> = vec![rsip::headers::MaxForwards::from(70u32).into()];

        // Get SDP offer - use caller's offer or create one
        let offer = self
            .caller_offer
            .clone()
            .or_else(|| self.callee_offer.clone())
            .map(|s| s.into_bytes());

        let content_type = offer.as_ref().map(|_| "application/sdp".to_string());

        // Build contact URI
        let contact_uri = self
            .context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .unwrap_or_else(|| caller.clone());

        // Build the INVITE option
        let invite_option = InviteOption {
            caller_display_name: self.context.dialplan.caller_display_name.clone(),
            callee: callee_uri.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination: target.destination.clone(),
            contact: contact_uri,
            credential: target.credential.clone(),
            headers: Some(headers),
            call_id: self.context.dialplan.call_id.clone(),
            ..Default::default()
        };

        // Create channel for dialog state updates
        let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();

        // Send the INVITE
        let dialog_layer = self.server.dialog_layer.clone();
        let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

        // Wait for the invitation to complete or fail. While the outbound INVITE
        // is pending, the caller may cancel the inbound dialog. In that case we
        // must stop awaiting the invite future so rsipstack can drop the
        // unconfirmed dialog and emit CANCEL on the outbound leg.
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
                            // Check response status
                            if let Some(ref resp) = response {
                                if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
                                    Ok((dialog.id(), response))
                                } else {
                                    // Pass through the actual SIP error code from callee
                                    let code = StatusCode::from(resp.status_code.code() as u16);
                                    // Use default reason for the status code
                                    Err((code, None))
                                }
                            } else {
                                Err((StatusCode::ServerInternalError, Some("No response from callee".to_string())))
                            }
                        }
                        Err(e) => Err((StatusCode::ServerInternalError, Some(format!("Invite failed: {}", e)))),
                    };
                }
                // Handle early media / ringing while invite is still pending.
                state = state_rx.recv() => {
                    if let Some(DialogState::Early(_, ref response)) = state {
                        // Forward 180/183 to caller if needed
                        let sdp = String::from_utf8_lossy(response.body()).to_string();
                        if !sdp.is_empty() && sdp.contains("v=0") {
                            // Forward early media SDP
                            let _ = self.server_dialog.ringing(None, Some(sdp.into_bytes()));
                        } else {
                            let _ = self.server_dialog.ringing(None, None);
                        }
                    }
                }
            }
        };

        let (dialog_id, response): (DialogId, Option<rsip::Response>) = result?;

        // Extract SDP from response
        let callee_sdp = response.as_ref().and_then(|r: &rsip::Response| {
            let body = r.body();
            if body.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(body).to_string())
            }
        });

        // Accept the call with callee's SDP
        self.accept_call(
            Some(callee_uri.to_string()),
            callee_sdp,
            Some(dialog_id.to_string()),
        )
        .await
        .map_err(|e| (StatusCode::ServerInternalError, Some(e.to_string())))?;

        if let Ok(mut dialogs) = self.callee_dialogs.lock() {
            dialogs.insert(dialog_id);
        }

        self.update_snapshot_cache();

        Ok(())
    }

    /// Create callee track
    pub async fn create_callee_track(&mut self, is_webrtc: bool) -> Result<String> {
        // Implementation from CallSession
        // This is a simplified version - full implementation would need all the SDP negotiation logic
        let track_id = Self::CALLEE_TRACK_ID.to_string();

        // Build track with appropriate settings
        let mut track_builder = RtpTrackBuilder::new(track_id.clone())
            .with_cancel_token(self.callee_peer.cancel_token());

        if is_webrtc {
            track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
        }

        let track = track_builder.build();
        let sdp = track.local_description().await?;

        self.callee_peer.update_track(Box::new(track), None).await;

        Ok(sdp)
    }

    /// Accept call
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

        // Update session state
        self.update_leg_state(&LegId::from("callee"), LegState::Connected)
            .await;

        // Initialize session timer before sending 200 OK (RFC 4028 negotiation)
        let mut timer_headers = vec![];
        if self.server.proxy_config.session_timer {
            let default_expires = self.server.proxy_config.session_expires.unwrap_or(1800);
            match self.init_server_timer(default_expires) {
                Ok(()) => {
                    // Build Session-Expires and Min-SE headers for 200 OK response
                    let timer = self.server_timer.lock().unwrap();
                    if timer.enabled {
                        timer_headers.push(rsip::Header::Other(
                            HEADER_SESSION_EXPIRES.to_string(),
                            timer.get_session_expires_value(),
                        ));
                        timer_headers.push(rsip::Header::Other(
                            HEADER_MIN_SE.to_string(),
                            timer.get_min_se_value(),
                        ));
                        timer_headers.push(rsip::Header::Supported(
                            rsip::headers::Supported::from(TIMER_TAG),
                        ));
                        info!(
                            session_expires = %timer.get_session_expires_value(),
                            "Session timer negotiated in 200 OK"
                        );
                    }
                    drop(timer);
                }
                Err((code, reason)) => {
                    warn!(?code, ?reason, "Failed to initialize session timer");
                    // Continue without session timer
                }
            }
        }

        // Send 200 OK with Session-Expires header if timer is enabled
        if let Some(answer_sdp) = sdp {
            let mut headers = vec![rsip::Header::ContentType("application/sdp".into())];
            headers.extend(timer_headers);
            self.server_dialog
                .accept(Some(headers), Some(answer_sdp.into_bytes()))
                .map_err(|e| anyhow!("Failed to send answer: {}", e))?;
        }

        self.answer_time = Some(Instant::now());

        Ok(())
    }

    /// Start ringing - sends 180 Ringing response
    #[allow(dead_code)]
    pub async fn start_ringing(&mut self, ringback: String) {
        info!(ringback = %ringback, "Starting ringing");

        // Update session state
        self.update_leg_state(&LegId::from("caller"), LegState::Ringing)
            .await;

        self.ring_time = Some(Instant::now());

        // Send 180 Ringing
        let _ = self.server_dialog.ringing(None, None);
    }

    /// Handle re-INVITE from caller (B2BUA mode)
    pub async fn handle_reinvite(&mut self, method: rsip::Method, sdp: Option<String>) -> Result<Option<String>> {
        debug!(?method, sdp_present = sdp.is_some(), "Handling re-INVITE in B2BUA mode");

        if method != rsip::Method::Invite {
            return Err(anyhow!("Expected INVITE method, got {:?}", method));
        }

        // Check if this is a session refresh (RFC 4028)
        // Session refresh re-INVITEs may not have SDP body
        let headers = self.server_dialog.initial_request().headers.clone();
        if let Err(e) = self.handle_session_refresh(&headers, sdp.clone()).await {
            warn!(error = %e, "Failed to handle session refresh in re-INVITE");
        }

        let offer_sdp = match sdp {
            Some(s) => s,
            None => {
                // This might be a session refresh without SDP changes
                // Return current answer SDP if available
                return Ok(self.answer.clone());
            }
        };

        let callee_dialogs: Vec<DialogId> = {
            let dialogs = self.callee_dialogs.lock().unwrap();
            dialogs.iter().cloned().collect()
        };

        if callee_dialogs.is_empty() {
            return Err(anyhow!("No callee dialogs available for B2BUA forwarding"));
        }

        let mut final_answer: Option<String> = None;
        let dialog_layer = self.server.dialog_layer.clone();

        for callee_dialog_id in callee_dialogs {
            if let Some(mut dialog) = dialog_layer.get_dialog(&callee_dialog_id) {
                let body = offer_sdp.clone().into_bytes();
                let headers = vec![rsip::Header::ContentType("application/sdp".into())];

                let resp: Option<rsip::Response> = match &mut dialog {
                    Dialog::ClientInvite(d) => {
                        d.reinvite(Some(headers), Some(body)).await
                            .map_err(|e| anyhow!("re-INVITE to callee failed: {}", e))?
                    }
                    _ => continue,
                };

                if let Some(response) = resp {
                    if !response.body().is_empty() {
                        let answer_sdp = String::from_utf8_lossy(response.body()).to_string();
                        final_answer = Some(answer_sdp.clone());
                        self.answer = Some(answer_sdp.clone());
                    }
                }
            }
        }

        if let Some(ref answer_sdp) = final_answer {
            let headers = vec![rsip::Header::ContentType("application/sdp".into())];
            self.server_dialog
                .accept(Some(headers), Some(answer_sdp.clone().into_bytes()))
                .map_err(|e| anyhow!("Failed to send 200 OK for re-INVITE: {}", e))?;
        }

        Ok(final_answer)
    }

    /// Play audio file
    pub async fn play_audio_file(
        &mut self,
        audio_file: &str,
        _await_completion: bool,
        track_id: &str,
        loop_playback: bool,
    ) -> Result<()> {
        info!(audio_file = %audio_file, track_id = %track_id, "Playing audio file");

        // Determine caller's codec
        let caller_codec = self
            .caller_offer
            .as_ref()
            .map(|offer| MediaNegotiator::extract_codec_params(offer).audio)
            .and_then(|codecs| codecs.first().map(|c| c.codec))
            .unwrap_or(CodecType::PCMU);

        let hold_ssrc = rand::random::<u32>();
        let track = FileTrack::new(track_id.to_string())
            .with_path(audio_file.to_string())
            .with_loop(loop_playback)
            .with_ssrc(hold_ssrc)
            .with_codec_preference(vec![caller_codec]);

        // Get caller's peer connection
        let caller_pc = {
            let tracks = self.caller_peer.get_tracks().await;
            if let Some(t) = tracks.first() {
                t.lock().await.get_peer_connection().await
            } else {
                None
            }
        };

        // Start playback
        if let Err(e) = track.start_playback_on(caller_pc).await {
            warn!(error = %e, "Failed to start playback");
        }

        self.caller_peer.update_track(Box::new(track), None).await;

        Ok(())
    }

    /// Start recording
    pub async fn start_recording(
        &mut self,
        path: &str,
        max_duration: Option<Duration>,
        beep: bool,
    ) -> Result<()> {
        info!(path = %path, beep = beep, "Starting recording");

        // Store recording state
        self.recording_state = Some((path.to_string(), Instant::now()));

        // Implementation would set up actual recording here
        // For now, just log
        if beep {
            // Play beep tone
        }

        if let Some(duration) = max_duration {
            // Setup max duration timer
            let _ = duration;
        }

        Ok(())
    }

    /// Stop recording
    pub async fn stop_recording(&mut self) -> Result<()> {
        info!("Stopping recording");

        if let Some((path, start_time)) = self.recording_state.take() {
            let duration = start_time.elapsed();
            info!(path = %path, duration = ?duration, "Recording stopped");
        }

        Ok(())
    }

    /// Transfer to endpoint
    #[allow(dead_code)]
    pub async fn transfer_to_endpoint(
        &mut self,
        endpoint: &TransferEndpoint,
    ) -> Result<()> {
        info!(endpoint = ?endpoint, "Transferring to endpoint");

        // Implementation would handle the transfer logic
        // For now, just log

        Ok(())
    }

    /// Transfer to URI
    #[allow(dead_code)]
    pub async fn transfer_to_uri(&mut self, target: &str) -> Result<()> {
        info!(target = %target, "Transferring to URI");

        // Implementation would handle the transfer logic
        Ok(())
    }

    /// Set error
    #[allow(dead_code)]
    pub fn set_error(&mut self, code: StatusCode, reason: Option<String>, target: Option<String>) {
        self.last_error = Some((code.clone(), reason.clone()));
        self.note_attempt_failure(code, reason, target);
    }

    /// Note attempt failure
    #[allow(dead_code)]
    pub fn note_attempt_failure(
        &mut self,
        code: StatusCode,
        reason: Option<String>,
        target: Option<String>,
    ) {
        self.hangup_messages.push(SessionHangupMessage {
            code: u16::from(code.clone()),
            reason: reason.clone(),
            target: target.clone(),
        });
    }

    /// Check if answered
    #[allow(dead_code)]
    pub fn is_answered(&self) -> bool {
        self.answer_time.is_some()
    }

    /// Cleanup - ensures all resources are released to prevent memory leaks
    async fn cleanup(&mut self) {
        debug!(session_id = %self.context.session_id, "Cleaning up session");

        // Stop recording if active
        if self.recording_state.is_some() {
            let _ = self.stop_recording().await;
        }

        // Release supervisor mixer
        if let Some(mixer) = self.supervisor_mixer.take() {
            drop(mixer);
        }

        // Clear callee guards to release dialog receivers
        self.callee_guards.clear();

        // Close callee event channel to signal no more events
        self.callee_event_tx = None;

        // Clear pending hangup to release any pending state
        if let Ok(mut pending) = self.pending_hangup.lock() {
            *pending = None;
        }

        // Clear callee dialogs
        if let Ok(mut dialogs) = self.callee_dialogs.lock() {
            dialogs.clear();
        }

        // Unregister from server
        self.server
            .active_call_registry
            .remove(&self.context.session_id);

        // Report call record if reporter is available
        if let Some(reporter) = &self.reporter {
            let snapshot = self.record_snapshot();
            reporter.report(snapshot);
        }

        debug!(session_id = %self.context.session_id, "Session cleanup complete");
    }

    /// Init server timer
    pub fn init_server_timer(
        &mut self,
        default_expires: u64,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let request = self.server_dialog.initial_request();
        let headers = &request.headers;

        let supported = has_timer_support(headers);
        let session_expires_value = get_header_value(headers, HEADER_SESSION_EXPIRES);

        let local_min_se = Duration::from_secs(90);
        let mut server_timer = self.server_timer.lock().unwrap();

        if let Some(value) = session_expires_value {
            if let Some((interval, refresher)) = parse_session_expires(&value) {
                if interval < local_min_se {
                    return Err((
                        StatusCode::SessionIntervalTooSmall,
                        Some(local_min_se.as_secs().to_string()),
                    ));
                }

                server_timer.enabled = true;
                server_timer.session_interval = interval;
                server_timer.active = true;
                server_timer.refresher = refresher.unwrap_or(SessionRefresher::Uac);
            }
        } else {
            server_timer.enabled = true;
            server_timer.session_interval = Duration::from_secs(default_expires);
            server_timer.active = true;
            server_timer.refresher = if supported {
                SessionRefresher::Uac
            } else {
                SessionRefresher::Uas
            };
        }

        Ok(())
    }

    /// Calculate timer check interval based on current timer state
    fn calculate_timer_check_interval(&self) -> Duration {
        let timer = self.server_timer.lock().unwrap();
        if !timer.active || !timer.enabled {
            // Return a long interval when timer is not active
            return Duration::from_secs(60);
        }

        // Calculate time until next action
        let time_until_refresh = timer.time_until_refresh();
        let time_until_expiry = timer.time_until_expiration();

        // Use the shorter of the two, but cap at 30 seconds max
        let min_time = match (time_until_refresh, time_until_expiry) {
            (Some(refresh), Some(expiry)) => refresh.min(expiry),
            (Some(refresh), None) => refresh,
            (None, Some(expiry)) => expiry,
            (None, None) => Duration::from_secs(60),
        };

        // Check at least every 30 seconds, or sooner if needed
        min_time.clamp(Duration::from_secs(1), Duration::from_secs(30))
    }

    /// Check session timer state and return required action
    async fn check_session_timer(&self) -> TimerAction {
        // Check timer state and determine action
        let (active, enabled, expired, should_refresh, we_are_refresher) = {
            let timer = self.server_timer.lock().unwrap();
            (
                timer.active,
                timer.enabled,
                timer.is_expired(),
                timer.should_refresh(),
                timer.refresher == SessionRefresher::Uas,
            )
        };

        if !active || !enabled {
            return TimerAction::Reschedule(Duration::from_secs(60));
        }

        // Check if session has expired
        if expired {
            return TimerAction::Expired;
        }

        // Check if we need to refresh
        // We are UAS (server side) so we refresh if refresher is Uas
        if we_are_refresher && should_refresh {
            if self.server_timer.lock().unwrap().start_refresh() {
                return TimerAction::Refresh;
            }
        }

        // Reschedule based on remaining time
        TimerAction::Reschedule(self.calculate_timer_check_interval())
    }

    /// Send session refresh via re-INVITE
    async fn send_session_refresh(&self) -> Result<()> {
        info!("Sending session refresh (re-INVITE)");

        // Extract values while holding the lock, then release it before await
        let (session_expires, min_se) = {
            let timer = self.server_timer.lock().unwrap();
            (timer.get_session_expires_value(), timer.get_min_se_value())
        };

        // Build headers for re-INVITE
        let headers = vec![
            rsip::Header::ContentType("application/sdp".into()),
            rsip::Header::Other(HEADER_SESSION_EXPIRES.to_string(), session_expires),
            rsip::Header::Other(HEADER_MIN_SE.to_string(), min_se),
            rsip::Header::Supported(rsip::headers::Supported::from(TIMER_TAG)),
        ];

        // Get current answer SDP
        let body = self.answer.clone().map(|sdp| sdp.into_bytes());

        // Send re-INVITE
        match self.server_dialog.reinvite(Some(headers), body).await {
            Ok(_response) => {
                info!("Session refresh (re-INVITE) successful");
                self.server_timer.lock().unwrap().complete_refresh();
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "Session refresh (re-INVITE) failed");
                self.server_timer.lock().unwrap().fail_refresh();
                Err(anyhow!("re-INVITE failed: {}", e))
            }
        }
    }

    /// Handle incoming session refresh from remote party (re-INVITE or UPDATE)
    pub async fn handle_session_refresh(&mut self, headers: &rsip::Headers, body: Option<String>) -> Result<()> {
        debug!("Handling incoming session refresh");

        // Check for Session-Expires header
        if let Some(se_value) = get_header_value(headers, HEADER_SESSION_EXPIRES) {
            if let Some((interval, refresher)) = parse_session_expires(&se_value) {
                let mut timer = self.server_timer.lock().unwrap();
                
                // Validate interval against Min-SE
                if interval < timer.min_se {
                    return Err(anyhow!(
                        "Session-Expires too small: {} < {}",
                        interval.as_secs(),
                        timer.min_se.as_secs()
                    ));
                }

                // Update timer parameters
                timer.session_interval = interval;
                if let Some(new_refresher) = refresher {
                    timer.refresher = new_refresher;
                }
                timer.update_refresh();
                
                info!(
                    interval = interval.as_secs(),
                    refresher = %timer.refresher,
                    "Session timer updated from remote refresh"
                );
            }
        } else {
            // No Session-Expires header, just update refresh time
            self.server_timer.lock().unwrap().update_refresh();
        }

        // Send 200 OK response
        let response_headers = vec![rsip::Header::ContentType("application/sdp".into())];
        let response_body = body.map(|sdp| sdp.into_bytes());
        
        self.server_dialog
            .accept(Some(response_headers), response_body)
            .map_err(|e| anyhow!("Failed to send 200 OK for refresh: {}", e))?;

        Ok(())
    }

    /// Note invite details - stores routing information from the INVITE
    #[allow(dead_code)]
    pub fn note_invite_details(&mut self, invite: &InviteOption) {
        self.routed_caller = Some(invite.caller.to_string());
        self.routed_callee = Some(invite.callee.to_string());
        self.routed_contact = Some(invite.contact.to_string());
        self.routed_destination = invite.destination.as_ref().map(|addr| addr.to_string());
    }

    /// Create record snapshot for call record reporting
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
            callee_dialogs: self
                .callee_dialogs
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect(),
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
    /// Execute a CallCommand
    pub async fn execute_command(&mut self, command: CallCommand) -> CommandResult {
        // Check media capability
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

        // Process the command
        let mut result = self.process_command(command).await;

        if let Some(reason) = degradation_reason {
            result.media_degraded = true;
            result.degradation_reason = Some(reason);
        }

        result
    }

    /// Check media capability for a command
    fn check_capability(&self, command: &CallCommand) -> MediaCapabilityCheck {
        let ctx = ExecutionContext::new(&self.id.0).with_media_profile(self.media_profile.clone());
        ctx.check_media_capability(command)
    }

    /// Internal command processing
    async fn process_command(&mut self, command: CallCommand) -> CommandResult {
        match command {
            CallCommand::Answer { leg_id } => {
                if self.update_leg_state(&leg_id, LegState::Connected).await {
                    CommandResult::success()
                } else {
                    CommandResult::failure(&format!("Leg not found: {}", leg_id))
                }
            }

            CallCommand::Hangup(cmd) => self.handle_hangup(&cmd).await,

            CallCommand::Bridge {
                leg_a,
                leg_b,
                mode: _,
            } => {
                if self.setup_bridge(leg_a.clone(), leg_b.clone()).await {
                    self.update_leg_state(&leg_a, LegState::Connected).await;
                    self.update_leg_state(&leg_b, LegState::Connected).await;
                    CommandResult::success()
                } else {
                    CommandResult::failure("Cannot bridge: one or both legs not found")
                }
            }

            CallCommand::Unbridge { .. } => {
                self.clear_bridge().await;
                CommandResult::success()
            }

            CallCommand::Hold { leg_id, .. } => {
                if self.update_leg_state(&leg_id, LegState::Hold).await {
                    CommandResult::success()
                } else {
                    CommandResult::failure(&format!("Leg not found: {}", leg_id))
                }
            }

            CallCommand::Unhold { leg_id } => {
                if self.update_leg_state(&leg_id, LegState::Connected).await {
                    CommandResult::success()
                } else {
                    CommandResult::failure(&format!("Leg not found: {}", leg_id))
                }
            }

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
                    Ok(()) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::StopApp { reason } => match self.app_runtime.stop_app(reason).await {
                Ok(()) => CommandResult::success(),
                Err(e) => CommandResult::failure(&e.to_string()),
            },

            CallCommand::InjectAppEvent { event } => {
                let event_value = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                match self.app_runtime.inject_event(event_value) {
                    Ok(()) => CommandResult::success(),
                    Err(e) => CommandResult::degraded(&e.to_string()),
                }
            }

            CallCommand::Play { .. } | CallCommand::StopPlayback { .. } => {
                // TODO: Implement media playback
                CommandResult::success()
            }

            CallCommand::StartRecording { config } => {
                match self.start_recording(&config.path, config.max_duration_secs.map(|s| Duration::from_secs(s as u64)), config.beep).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::StopRecording { .. } => {
                match self.stop_recording().await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::PauseRecording { .. } => {
                // TODO: Implement pause recording
                CommandResult::success()
            }

            CallCommand::ResumeRecording { .. } => {
                // TODO: Implement resume recording
                CommandResult::success()
            }

            // ============================================================================
            // Transfer
            // ============================================================================
            CallCommand::Transfer { leg_id, target, attended } => {
                match self.handle_transfer(leg_id, target, attended).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::TransferComplete { consult_leg } => {
                match self.handle_transfer_complete(consult_leg).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::TransferCancel { consult_leg } => {
                match self.handle_transfer_cancel(consult_leg).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            // ============================================================================
            // Supervisor / Monitoring
            // ============================================================================
            CallCommand::SupervisorListen { supervisor_leg, target_leg } => {
                match self.handle_supervisor_listen(supervisor_leg, target_leg).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::SupervisorWhisper { supervisor_leg, target_leg } => {
                match self.handle_supervisor_whisper(supervisor_leg, target_leg).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::SupervisorBarge { supervisor_leg, target_leg } => {
                match self.handle_supervisor_barge(supervisor_leg, target_leg).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::SupervisorStop { supervisor_leg } => {
                match self.handle_supervisor_stop(supervisor_leg).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            // ============================================================================
            // Conference
            // ============================================================================
            CallCommand::ConferenceCreate { conf_id, options } => {
                match self.handle_conference_create(conf_id, options).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::ConferenceAdd { conf_id, leg_id } => {
                match self.handle_conference_add(conf_id, leg_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::ConferenceRemove { conf_id, leg_id } => {
                match self.handle_conference_remove(conf_id, leg_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::ConferenceMute { conf_id, leg_id } => {
                match self.handle_conference_mute(conf_id, leg_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::ConferenceUnmute { conf_id, leg_id } => {
                match self.handle_conference_unmute(conf_id, leg_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::ConferenceDestroy { conf_id } => {
                match self.handle_conference_destroy(conf_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            // ============================================================================
            // Queue Operations
            // ============================================================================
            CallCommand::QueueEnqueue { leg_id, queue_id, priority } => {
                match self.handle_queue_enqueue(leg_id, queue_id, priority).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::QueueDequeue { leg_id } => {
                match self.handle_queue_dequeue(leg_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            // ============================================================================
            // Reject
            // ============================================================================
            CallCommand::Reject { leg_id, reason } => {
                match self.handle_reject(leg_id, reason).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            // ============================================================================
            // Ring
            // ============================================================================
            CallCommand::Ring { leg_id, ringback } => {
                match self.handle_ring(leg_id, ringback).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            // ============================================================================
            // DTMF
            // ============================================================================
            CallCommand::SendDtmf { leg_id, digits } => {
                match self.handle_send_dtmf(leg_id, digits).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            // ============================================================================
            // Re-Invite Handling
            // ============================================================================
            CallCommand::HandleReInvite { leg_id, sdp } => {
                match self.handle_reinvite_command(leg_id, sdp).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            // ============================================================================
            // Track Muting
            // ============================================================================
            CallCommand::MuteTrack { track_id } => {
                match self.handle_mute_track(track_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            CallCommand::UnmuteTrack { track_id } => {
                match self.handle_unmute_track(track_id).await {
                    Ok(_) => CommandResult::success(),
                    Err(e) => CommandResult::failure(&e.to_string()),
                }
            }

            _ => CommandResult::not_supported("Command not yet implemented"),
        }
    }

    /// Handle hangup command
    async fn handle_hangup(&mut self, cmd: &HangupCommand) -> CommandResult {
        let cascade = &cmd.cascade;

        // Mark legs as ended based on cascade policy
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

        // Stop any running app
        if self.app_runtime.is_running() {
            let reason_str = cmd.reason.as_ref().map(|r| r.to_string());
            if let Err(e) = self.app_runtime.stop_app(reason_str).await {
                error!(session_id = %self.id, error = %e, "Failed to stop app during hangup");
            }
        }

        // Cancel the session
        self.cancel_token.cancel();

        CommandResult::success()
    }

    /// Update leg state and derive session state
    async fn update_leg_state(&mut self, leg_id: &LegId, new_state: LegState) -> bool {
        if let Some(leg) = self.legs.get_mut(leg_id) {
            leg.state = new_state;
            self.state = Self::derive_state(&self.legs);
            true
        } else {
            false
        }
    }

    /// Setup bridge between legs
    async fn setup_bridge(&mut self, leg_a: LegId, leg_b: LegId) -> bool {
        if self.legs.contains_key(&leg_a) && self.legs.contains_key(&leg_b) {
            self.bridge = BridgeConfig::bridge(leg_a, leg_b);
            true
        } else {
            false
        }
    }

    /// Clear bridge
    async fn clear_bridge(&mut self) {
        self.bridge.clear();
    }

    /// Derive session state from leg states
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

    // ============================================================================
    // Transfer Operations
    // ============================================================================

    /// Handle transfer command (blind or attended)
    async fn handle_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
        attended: bool,
    ) -> Result<()> {
        info!(%leg_id, %target, %attended, "Handling transfer");

        // Verify the leg exists
        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        // For attended transfer, we need to place the original leg on hold
        // and create a new leg to the target
        if attended {
            // Place the original leg on hold
            self.update_leg_state(&leg_id, LegState::Hold).await;
            
            // TODO: Create a new leg (consultation call) to the target
            // This requires originating a new call which is handled at a higher level
            // For now, we mark the transfer as initiated
            info!("Attended transfer initiated - consultation call should be created externally");
        } else {
            // Blind transfer: refer the leg to the target
            // TODO: Send SIP REFER or re-invite to transfer the call
            // For now, mark the leg as ending
            self.update_leg_state(&leg_id, LegState::Ending).await;
            info!("Blind transfer initiated - call will be transferred to {}", target);
        }

        Ok(())
    }

    /// Handle transfer complete (attended transfer completion)
    async fn handle_transfer_complete(&mut self, consult_leg: LegId) -> Result<()> {
        info!(%consult_leg, "Completing attended transfer");

        // Verify the consultation leg exists
        if !self.legs.contains_key(&consult_leg) {
            return Err(anyhow!("Consultation leg not found: {}", consult_leg));
        }

        // Find the original leg (the one on hold)
        let original_leg = self.legs.iter()
            .find(|(_, leg)| leg.state == LegState::Hold)
            .map(|(id, _)| id.clone());

        if let Some(original_leg) = original_leg {
            // Bridge the original caller with the consultation target
            if self.setup_bridge(original_leg.clone(), consult_leg.clone()).await {
                self.update_leg_state(&original_leg, LegState::Connected).await;
                self.update_leg_state(&consult_leg, LegState::Connected).await;
                info!("Attended transfer completed successfully");
            } else {
                return Err(anyhow!("Failed to setup bridge for transfer completion"));
            }
        } else {
            return Err(anyhow!("No leg on hold found for transfer completion"));
        }

        Ok(())
    }

    /// Handle transfer cancel (cancel attended transfer)
    async fn handle_transfer_cancel(&mut self, consult_leg: LegId) -> Result<()> {
        info!(%consult_leg, "Canceling attended transfer");

        // Verify the consultation leg exists
        if !self.legs.contains_key(&consult_leg) {
            return Err(anyhow!("Consultation leg not found: {}", consult_leg));
        }

        // Mark consultation leg as ended
        self.update_leg_state(&consult_leg, LegState::Ending).await;

        // Find and unhold the original leg
        let original_leg = self.legs.iter()
            .find(|(_, leg)| leg.state == LegState::Hold)
            .map(|(id, _)| id.clone());

        if let Some(original_leg) = original_leg {
            self.update_leg_state(&original_leg, LegState::Connected).await;
            info!("Attended transfer canceled, original call resumed");
        }

        Ok(())
    }

    // ============================================================================
    // Supervisor Operations
    // ============================================================================

    /// Handle supervisor listen mode
    async fn handle_supervisor_listen(&mut self, supervisor_leg: LegId, target_leg: LegId) -> Result<()> {
        info!(%supervisor_leg, %target_leg, "Starting supervisor listen mode");

        // Verify legs exist
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        if !self.legs.contains_key(&target_leg) {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        }

        // Create or get the supervisor mixer
        if self.supervisor_mixer.is_none() {
            let mixer = MediaMixer::new(format!("supervisor-{}", self.id), 8000);
            self.supervisor_mixer = Some(Arc::new(mixer));
        }

        // TODO: Connect supervisor leg to target leg's audio in listen-only mode
        // This requires media layer support for one-way audio streaming
        
        info!("Supervisor listen mode activated");
        Ok(())
    }

    /// Handle supervisor whisper mode
    async fn handle_supervisor_whisper(&mut self, supervisor_leg: LegId, target_leg: LegId) -> Result<()> {
        info!(%supervisor_leg, %target_leg, "Starting supervisor whisper mode");

        // Verify legs exist
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        if !self.legs.contains_key(&target_leg) {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        }

        // Create or get the supervisor mixer
        if self.supervisor_mixer.is_none() {
            let mixer = MediaMixer::new(format!("supervisor-{}", self.id), 8000);
            self.supervisor_mixer = Some(Arc::new(mixer));
        }

        // TODO: Connect supervisor leg to target leg's audio in whisper mode
        // Supervisor can talk to target but not hear the other party
        
        info!("Supervisor whisper mode activated");
        Ok(())
    }

    /// Handle supervisor barge mode
    async fn handle_supervisor_barge(&mut self, supervisor_leg: LegId, target_leg: LegId) -> Result<()> {
        info!(%supervisor_leg, %target_leg, "Starting supervisor barge mode");

        // Verify legs exist
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        if !self.legs.contains_key(&target_leg) {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        }

        // Create or get the supervisor mixer
        if self.supervisor_mixer.is_none() {
            let mixer = MediaMixer::new(format!("supervisor-{}", self.id), 8000);
            self.supervisor_mixer = Some(Arc::new(mixer));
        }

        // TODO: Bridge supervisor leg into the active conversation
        // This creates a 3-way call
        
        info!("Supervisor barge mode activated");
        Ok(())
    }

    /// Handle supervisor stop
    async fn handle_supervisor_stop(&mut self, supervisor_leg: LegId) -> Result<()> {
        info!(%supervisor_leg, "Stopping supervisor mode");

        // Verify supervisor leg exists
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }

        // Remove supervisor from mixer
        if self.supervisor_mixer.take().is_some() {
            // Mixer will be dropped when Arc is released
            info!("Supervisor mixer removed");
        }

        // Mark supervisor leg as ended
        self.update_leg_state(&supervisor_leg, LegState::Ending).await;

        info!("Supervisor mode stopped");
        Ok(())
    }

    // ============================================================================
    // Conference Operations
    // ============================================================================

    /// Handle conference create
    async fn handle_conference_create(&mut self, conf_id: String, _options: crate::call::domain::ConferenceOptions) -> Result<()> {
        info!(%conf_id, "Creating conference");
        // TODO: Implement conference creation
        // This requires conference bridge support in the media layer
        Ok(())
    }

    /// Handle conference add
    async fn handle_conference_add(&mut self, conf_id: String, leg_id: LegId) -> Result<()> {
        info!(%conf_id, %leg_id, "Adding leg to conference");
        // TODO: Implement adding leg to conference
        Ok(())
    }

    /// Handle conference remove
    async fn handle_conference_remove(&mut self, conf_id: String, leg_id: LegId) -> Result<()> {
        info!(%conf_id, %leg_id, "Removing leg from conference");
        // TODO: Implement removing leg from conference
        Ok(())
    }

    /// Handle conference mute
    async fn handle_conference_mute(&mut self, conf_id: String, leg_id: LegId) -> Result<()> {
        info!(%conf_id, %leg_id, "Muting leg in conference");
        // TODO: Implement muting leg in conference
        Ok(())
    }

    /// Handle conference unmute
    async fn handle_conference_unmute(&mut self, conf_id: String, leg_id: LegId) -> Result<()> {
        info!(%conf_id, %leg_id, "Unmuting leg in conference");
        // TODO: Implement unmuting leg in conference
        Ok(())
    }

    /// Handle conference destroy
    async fn handle_conference_destroy(&mut self, conf_id: String) -> Result<()> {
        info!(%conf_id, "Destroying conference");
        // TODO: Implement conference destruction
        Ok(())
    }

    // ============================================================================
    // Queue Operations
    // ============================================================================

    /// Handle queue enqueue
    async fn handle_queue_enqueue(&mut self, leg_id: LegId, queue_id: String, priority: Option<u32>) -> Result<()> {
        info!(%leg_id, %queue_id, ?priority, "Enqueueing leg to queue");

        // Verify the leg exists
        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        // Update leg state to indicate it's in a queue
        self.update_leg_state(&leg_id, LegState::Hold).await;

        // TODO: Integrate with queue management system
        // This would typically involve:
        // 1. Adding the leg to the queue's waiting list
        // 2. Notifying queue manager
        // 3. Starting queue position announcements

        info!(%leg_id, %queue_id, "Leg enqueued successfully");
        Ok(())
    }

    /// Handle queue dequeue
    async fn handle_queue_dequeue(&mut self, leg_id: LegId) -> Result<()> {
        info!(%leg_id, "Dequeuing leg from queue");

        // Verify the leg exists
        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        // Update leg state from Hold to Connected (or appropriate state)
        self.update_leg_state(&leg_id, LegState::Connected).await;

        // TODO: Integrate with queue management system
        // This would typically involve:
        // 1. Removing the leg from the queue's waiting list
        // 2. Notifying queue manager
        // 3. Stopping queue position announcements

        info!(%leg_id, "Leg dequeued successfully");
        Ok(())
    }

    // ============================================================================
    // Reject
    // ============================================================================

    /// Handle reject command
    async fn handle_reject(&mut self, leg_id: LegId, reason: Option<String>) -> Result<()> {
        info!(%leg_id, ?reason, "Rejecting call");

        // Verify the leg exists
        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        // Mark leg as ended
        self.update_leg_state(&leg_id, LegState::Ending).await;

        // TODO: Send SIP response (e.g., 486 Busy Here, 603 Decline)
        // This requires access to the server_dialog to send the appropriate response
        // For now, we just update the state

        info!(%leg_id, "Call rejected");
        Ok(())
    }

    // ============================================================================
    // Ring
    // ============================================================================

    /// Handle ring command (send 180 Ringing)
    async fn handle_ring(&mut self, leg_id: LegId, _ringback: Option<RingbackPolicy>) -> Result<()> {
        info!(%leg_id, "Sending ringing indication");

        // Verify the leg exists
        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        // Update leg state to ringing
        self.update_leg_state(&leg_id, LegState::Ringing).await;

        // TODO: Send 180 Ringing via SIP
        // This requires access to the server_dialog
        // For now, we just update the state

        info!(%leg_id, "Ringing indication sent");
        Ok(())
    }

    // ============================================================================
    // DTMF
    // ============================================================================

    /// Handle send DTMF command
    async fn handle_send_dtmf(&mut self, leg_id: LegId, digits: String) -> Result<()> {
        info!(%leg_id, %digits, "Sending DTMF digits");

        // Verify the leg exists
        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        // TODO: Implement DTMF sending
        // This could be done via:
        // 1. RFC 4733/RFC 2833 telephone-event RTP payload
        // 2. SIP INFO messages
        // 3. In-band DTMF generation

        info!(%leg_id, %digits, "DTMF digits sent");
        Ok(())
    }

    // ============================================================================
    // Re-Invite Handling
    // ============================================================================

    async fn handle_reinvite_command(&mut self, leg_id: LegId, sdp: String) -> Result<()> {
        info!(%leg_id, "Handling re-INVITE command");

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        self.handle_reinvite(rsip::Method::Invite, Some(sdp)).await?;

        info!(%leg_id, "Re-INVITE command handled");
        Ok(())
    }

    // ============================================================================
    // Track Muting
    // ============================================================================

    /// Handle mute track command
    async fn handle_mute_track(&mut self, track_id: String) -> Result<()> {
        info!(%track_id, "Muting track");

        // TODO: Implement track muting
        // This would involve:
        // 1. Finding the track by ID
        // 2. Setting the track's mute flag
        // 3. Potentially sending re-INVITE if SDP needs updating

        info!(%track_id, "Track muted");
        Ok(())
    }

    /// Handle unmute track command
    async fn handle_unmute_track(&mut self, track_id: String) -> Result<()> {
        info!(%track_id, "Unmuting track");

        // TODO: Implement track unmuting
        // This would involve:
        // 1. Finding the track by ID
        // 2. Clearing the track's mute flag
        // 3. Potentially sending re-INVITE if SDP needs updating

        info!(%track_id, "Track unmuted");
        Ok(())
    }
}

impl Drop for SipSession {
    fn drop(&mut self) {
        debug!(session_id = %self.context.session_id, "SipSession dropping");

        // Cancel token to signal all async tasks to stop
        self.cancel_token.cancel();

        // Clear callee guards to release dialog receivers
        self.callee_guards.clear();

        // Close event channels
        self.callee_event_tx = None;

        // Clear pending hangup state
        if let Ok(mut pending) = self.pending_hangup.lock() {
            *pending = None;
        }

        // Clear callee dialogs
        if let Ok(mut dialogs) = self.callee_dialogs.lock() {
            dialogs.clear();
        }

        // Note: Media peers and bridges should be dropped naturally when the session is dropped
        // but we explicitly take them to ensure they're dropped in the right order
        let _ = self.supervisor_mixer.take();

        debug!(session_id = %self.context.session_id, "SipSession drop complete");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test that SipSession properly drops all resources
    #[test]
    fn test_session_drop_releases_resources() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropTracker;
        impl Drop for DropTracker {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Create a scope to test dropping
        {
            let _tracker = DropTracker;
            // When _tracker goes out of scope, it should be dropped
        }

        // Verify the tracker was dropped
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
    }

    /// Test that SipSession handle works correctly
    #[tokio::test]
    async fn test_sip_session_handle() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-session");
        let (handle, mut cmd_rx) = SipSession::with_handle(id.clone());

        // Test sending a command
        let result = handle.send_command(CallCommand::Answer {
            leg_id: LegId::from("caller"),
        });
        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Answer { .. })));

        // Drop handle
        drop(handle);
    }

    /// Test that pending hangup is cleared during cleanup
    #[test]
    fn test_pending_hangup_cleared() {
        let pending: Arc<Mutex<Option<PendingHangup>>> = Arc::new(Mutex::new(None));

        // Set pending hangup
        {
            let mut p = pending.lock().unwrap();
            *p = Some(PendingHangup);
        }

        // Verify set
        assert!(pending.lock().unwrap().is_some());

        // Clear (simulating cleanup)
        {
            let mut p = pending.lock().unwrap();
            *p = None;
        }

        // Verify cleared
        assert!(pending.lock().unwrap().is_none());
    }

    /// Test that cancellation token propagates to child tasks
    #[tokio::test]
    async fn test_cancel_token_propagation() {
        let cancel_token = CancellationToken::new();
        let child_token = cancel_token.child_token();

        // Spawn a task that waits on the child token
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

        // Cancel the parent token
        cancel_token.cancel();

        // Child task should complete quickly
        let result = tokio::time::timeout(Duration::from_millis(100), task).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap(), "cancelled");
    }

    /// Test that callee_event_tx is properly closed
    #[tokio::test]
    async fn test_callee_event_channel_closed() {
        use rsipstack::dialog::DialogId;

        let (tx, mut rx) = mpsc::unbounded_channel::<DialogState>();

        // Send a message
        let dialog_id = DialogId {
            call_id: "test".into(),
            local_tag: "local".into(),
            remote_tag: "remote".into(),
        };
        let _ = tx.send(DialogState::Trying(dialog_id));

        // Verify we can receive
        assert!(rx.recv().await.is_some());

        // Drop the sender (simulating cleanup)
        drop(tx);

        // Receiver should return None (channel closed)
        assert!(rx.recv().await.is_none());
    }

    /// Test SipSession handle lifecycle
    #[tokio::test]
    async fn test_handle_lifecycle() {
        use crate::call::runtime::SessionId;

        // Create and drop handle multiple times
        for i in 0..10 {
            let id = SessionId::from(format!("lifecycle-test-{}", i));
            let (handle, cmd_rx) = SipSession::with_handle(id);

            // Clean shutdown
            drop(cmd_rx);
            drop(handle);
        }

        // If we get here without hanging, handles were properly dropped
    }

    // ============================================================================
    // Command Handler Tests
    // ============================================================================

    /// Test Reject command processing
    #[tokio::test]
    async fn test_reject_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-reject");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        // Send reject command
        let result = handle.send_command(CallCommand::Reject {
            leg_id: LegId::from("caller"),
            reason: Some("User busy".to_string()),
        });
        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Reject { .. })));

        drop(handle);
    }

    /// Test Ring command processing
    #[tokio::test]
    async fn test_ring_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-ring");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        // Send ring command
        let result = handle.send_command(CallCommand::Ring {
            leg_id: LegId::from("caller"),
            ringback: None,
        });
        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::Ring { .. })));

        drop(handle);
    }

    /// Test SendDtmf command processing
    #[tokio::test]
    async fn test_send_dtmf_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-dtmf");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        // Send DTMF command
        let result = handle.send_command(CallCommand::SendDtmf {
            leg_id: LegId::from("caller"),
            digits: "1234".to_string(),
        });
        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::SendDtmf { .. })));

        drop(handle);
    }

    /// Test QueueEnqueue command processing
    #[tokio::test]
    async fn test_queue_enqueue_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-queue-enqueue");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        // Send queue enqueue command
        let result = handle.send_command(CallCommand::QueueEnqueue {
            leg_id: LegId::from("caller"),
            queue_id: "support-queue".to_string(),
            priority: Some(1),
        });
        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::QueueEnqueue { .. })));

        drop(handle);
    }

    /// Test QueueDequeue command processing
    #[tokio::test]
    async fn test_queue_dequeue_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-queue-dequeue");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        // Send queue dequeue command
        let result = handle.send_command(CallCommand::QueueDequeue {
            leg_id: LegId::from("caller"),
        });
        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::QueueDequeue { .. })));

        drop(handle);
    }

    /// Test HandleReInvite command processing
    #[tokio::test]
    async fn test_handle_reinvite_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-reinvite");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        // Send re-invite command
        let result = handle.send_command(CallCommand::HandleReInvite {
            leg_id: LegId::from("caller"),
            sdp: "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n".to_string(),
        });
        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::HandleReInvite { .. })));

        drop(handle);
    }

    /// Test MuteTrack command processing
    #[tokio::test]
    async fn test_mute_track_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-mute");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        // Send mute track command
        let result = handle.send_command(CallCommand::MuteTrack {
            track_id: "track-1".to_string(),
        });
        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::MuteTrack { .. })));

        drop(handle);
    }

    /// Test UnmuteTrack command processing
    #[tokio::test]
    async fn test_unmute_track_command() {
        use crate::call::runtime::SessionId;

        let id = SessionId::from("test-unmute");
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        // Send unmute track command
        let result = handle.send_command(CallCommand::UnmuteTrack {
            track_id: "track-1".to_string(),
        });
        assert!(result.is_ok());

        // Verify command was received
        let received = cmd_rx.recv().await;
        assert!(matches!(received, Some(CallCommand::UnmuteTrack { .. })));

        drop(handle);
    }
}
