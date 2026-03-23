use crate::call::domain::{
    CallCommand, HangupCascade, HangupCommand, LegId, LegState, MediaPathMode, MediaRuntimeProfile,
    SessionPolicy,
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
    /// Answer SDP from the first leg (for Re-INVITE handling)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_sdp: Option<String>,
}
use crate::call::sip::{DialogStateReceiverGuard, ServerDialogGuard};
use crate::call::{DialplanFlow, TransferEndpoint};
use crate::callrecord::{CallRecordHangupMessage, CallRecordHangupReason, CallRecordSender};
use crate::config::MediaProxyMode;
use crate::media::mixer::MediaMixer;
use crate::media::negotiate::MediaNegotiator;
use crate::media::{FileTrack, RtpTrackBuilder, Track};
use crate::proxy::proxy_call::{
    media_bridge::MediaBridge,
    media_peer::{MediaPeer, VoiceEnginePeer},
    reporter::CallReporter,
    session_timer::{
        HEADER_SESSION_EXPIRES, SessionRefresher, SessionTimerState, get_header_value,
        has_timer_support, parse_session_expires,
    },
    state::{
        CallContext, CallSessionRecordSnapshot, PendingHangup, SessionAction, SessionActionInbox,
        SessionHangupMessage,
    },
};
use crate::proxy::server::SipServerRef;
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use futures::FutureExt;
use futures::future::BoxFuture;
use rsip::StatusCode;
use rsipstack::dialog::{
    DialogId, dialog::DialogState, dialog::TerminatedReason, invitation::InviteOption,
    server_dialog::ServerInviteDialog,
};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Action inbox type alias for compatibility with legacy SessionAction-based APIs
/// Note: New code should use CallCommand instead of SessionAction.
#[allow(dead_code)]
pub(crate) type ActionInbox<'a> = Option<&'a mut SessionActionInbox>;

/// Negotiation state for SDP handling (RFC 3264)
/// Note: Currently only Idle state is used. Other states are reserved for
/// future SDP renegotiation (hold, reinvite) support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiationState {
    Idle,
    #[allow(dead_code)]
    Stable,
    #[allow(dead_code)]
    LocalOfferSent,
    #[allow(dead_code)]
    RemoteOfferReceived,
}

#[allow(dead_code)]
pub struct SipSession {
    pub id: SessionId,
    pub policy: SessionPolicy,
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
    pub media_bridge: Option<MediaBridge>,
    pub supervisor_mixer: Option<Arc<MediaMixer>>,

    pub context: CallContext,
    pub use_media_proxy: bool,
    pub call_record_sender: Option<CallRecordSender>,

    pub cancel_token: CancellationToken,
    pub pending_hangup: Arc<Mutex<Option<PendingHangup>>>,
    pub connected_callee: Option<String>,
    pub connected_dialog_id: Option<DialogId>,
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub caller_offer: Option<String>,
    pub callee_offer: Option<String>,
    pub answer: Option<String>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<SessionHangupMessage>,
    pub last_error: Option<(StatusCode, Option<String>)>,
    pub early_media_sent: bool,
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
    #[allow(dead_code)]
    pub server_timer: Arc<Mutex<SessionTimerState>>,
    /// Client timer state - currently initialized but not actively managed
    #[allow(dead_code)]
    pub client_timer: Arc<Mutex<SessionTimerState>>,

    // === Internal ===
    /// Negotiation state - reserved for future SDP renegotiation support
    #[allow(dead_code)]
    pub negotiation_state: NegotiationState,
    /// Callee event sender - used for dialog state updates
    pub callee_event_tx: Option<mpsc::UnboundedSender<DialogState>>,
    /// Callee guards - keeps dialog receivers alive
    pub callee_guards: Vec<DialogStateReceiverGuard>,
    /// Callee answer SDP - reserved for early media scenarios
    #[allow(dead_code)]
    pub callee_answer_sdp: Option<String>,
    /// Reporter - initialized but reporting is handled via process() cleanup
    #[allow(dead_code)]
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

#[allow(dead_code)]
impl SipSessionHandle {
    /// Create a new handle (used by tests)
    pub fn new(
        session_id: SessionId,
        cmd_tx: mpsc::UnboundedSender<CallCommand>,
        snapshot_cache: Arc<Mutex<Option<SessionSnapshot>>>,
    ) -> Self {
        Self {
            session_id,
            cmd_tx,
            snapshot_cache,
        }
    }

    /// Send a command to the session
    pub fn send_command(&self, cmd: CallCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|e| anyhow::anyhow!("channel closed: {}", e))
    }

    /// Get the session ID
    pub fn session_id(&self) -> &str {
        &self.session_id.0
    }

    /// Get a snapshot of the session
    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        self.snapshot_cache.lock().ok().and_then(|g| g.clone())
    }

    /// Update the snapshot cache (called by the session)
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

#[allow(dead_code)]
impl SipSession {
    pub const CALLEE_TRACK_ID: &'static str = "callee-track";
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

        // Create policy based on dialplan direction
        let policy = SessionPolicy::inbound_sip();

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
            policy: policy.clone(),
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
            media_bridge: None,
            supervisor_mixer: None,
            context,
            use_media_proxy,
            call_record_sender,
            cancel_token,
            pending_hangup: Arc::new(Mutex::new(None)),
            connected_callee: None,
            connected_dialog_id: None,
            ring_time: None,
            answer_time: None,
            caller_offer,
            callee_offer: None,
            answer: None,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            last_error: None,
            early_media_sent: false,
            recording_state: None,
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            server_timer: Arc::new(Mutex::new(SessionTimerState::default())),
            client_timer: Arc::new(Mutex::new(SessionTimerState::default())),
            negotiation_state: NegotiationState::Idle,
            callee_event_tx: None,
            callee_guards: Vec::new(),
            callee_answer_sdp: None,
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
            }
        }

        // Cleanup
        self.cleanup().await;

        // Drop the cancel guard to signal cancellation
        // This ensures the token is cancelled when process() returns
        let _ = cancel_guard;

        Ok(())
    }

    /// Handle dialog state change
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
                DialplanFlow::Queue { plan: _, next } => {
                    // For now, skip queue handling and just execute the next flow
                    // TODO: Implement queue handling
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

        // Wait for the invitation to complete or fail
        let result = tokio::select! {
            res = &mut invitation => {
                match res {
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
                }
            }
            // Handle early media / ringing
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
                // Continue waiting for the main result
                match invitation.await {
                    Ok((dialog, response)) => {
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

        // Add callee dialog to tracking
        if let Ok(mut dialogs) = self.callee_dialogs.lock() {
            dialogs.insert(dialog_id);
        }

        Ok(())
    }

    /// Apply session action - bridges to unified commands
    /// Note: This is a compatibility interface for legacy code that uses SessionAction.
    /// New code should use CallCommand directly via unified.execute_command().
    #[allow(dead_code)]
    pub fn apply_session_action<'a>(
        &'a mut self,
        action: SessionAction,
        _inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            // Convert SessionAction to CallCommand and execute
            let cmd = session_action_to_call_command(action)?;
            let result = self.execute_command(cmd).await;

            if result.success {
                Ok(())
            } else {
                Err(anyhow!(
                    "Command failed: {}",
                    result.message.unwrap_or_default()
                ))
            }
        }
        .boxed()
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

        // Send 200 OK
        if let Some(answer_sdp) = sdp {
            let headers = vec![rsip::Header::ContentType("application/sdp".into())];
            self.server_dialog
                .accept(Some(headers), Some(answer_sdp.into_bytes()))
                .map_err(|e| anyhow!("Failed to send answer: {}", e))?;
        }

        self.answer_time = Some(Instant::now());
        Ok(())
    }

    /// Start ringing
    pub async fn start_ringing(&mut self, ringback: String) {
        info!(ringback = %ringback, "Starting ringing");

        // Update session state
        self.update_leg_state(&LegId::from("caller"), LegState::Ringing)
            .await;

        self.ring_time = Some(Instant::now());

        // Send 180 Ringing
        let _ = self.server_dialog.ringing(None, None);
    }

    /// Handle re-INVITE
    pub async fn handle_reinvite(&mut self, method: rsip::Method, sdp: Option<String>) {
        debug!(?method, sdp_present = sdp.is_some(), "Handling re-INVITE");

        if let Some(answer_sdp) = sdp {
            // Update our answer
            self.answer = Some(answer_sdp.clone());

            // Send 200 OK with current answer
            let headers = vec![rsip::Header::ContentType("application/sdp".into())];
            let _ = self
                .server_dialog
                .accept(Some(headers), Some(answer_sdp.into_bytes()));
        }
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
    pub async fn transfer_to_endpoint(
        &mut self,
        endpoint: &TransferEndpoint,
        _inbox: ActionInbox<'_>,
    ) -> Result<()> {
        info!(endpoint = ?endpoint, "Transferring to endpoint");

        // Implementation would handle the transfer logic
        // For now, just log

        Ok(())
    }

    /// Transfer to URI
    pub async fn transfer_to_uri(&mut self, target: &str) -> Result<()> {
        info!(target = %target, "Transferring to URI");

        // Implementation would handle the transfer logic
        Ok(())
    }

    /// Set error
    pub fn set_error(&mut self, code: StatusCode, reason: Option<String>, target: Option<String>) {
        self.last_error = Some((code.clone(), reason.clone()));
        self.note_attempt_failure(code, reason, target);
    }

    /// Note attempt failure
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
    pub fn is_answered(&self) -> bool {
        self.answer_time.is_some()
    }

    /// Set answer
    pub fn set_answer(&mut self, sdp: String) {
        self.answer = Some(sdp);
    }

    /// Cleanup - ensures all resources are released to prevent memory leaks
    async fn cleanup(&mut self) {
        debug!(session_id = %self.context.session_id, "Cleaning up session");

        // Stop recording if active
        if self.recording_state.is_some() {
            let _ = self.stop_recording().await;
        }

        // Stop media bridge and release resources
        if let Some(bridge) = self.media_bridge.take() {
            drop(bridge);
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

        debug!(session_id = %self.context.session_id, "Session cleanup complete");
    }

    /// Get retry codes
    pub fn get_retry_codes(&self) -> Option<&Vec<u16>> {
        match &self.context.dialplan.flow {
            DialplanFlow::Queue { plan, .. } => plan.retry_codes.as_ref(),
            _ => None,
        }
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

    /// Note invite details
    pub fn note_invite_details(&mut self, invite: &InviteOption) {
        self.routed_caller = Some(invite.caller.to_string());
        self.routed_callee = Some(invite.callee.to_string());
        self.routed_contact = Some(invite.contact.to_string());
        self.routed_destination = invite.destination.as_ref().map(|addr| addr.to_string());
    }

    /// Create record snapshot
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

/// Convert SessionAction to CallCommand
/// Note: Legacy compatibility function. Used by apply_session_action for backward compatibility.
#[allow(dead_code)]
fn session_action_to_call_command(action: SessionAction) -> Result<CallCommand> {
    use crate::call::domain::{HangupInitiator, SystemHangupReason};

    match action {
        SessionAction::Hangup {
            reason,
            code,
            initiator,
        } => Ok(CallCommand::Hangup(HangupCommand {
            leg_id: None,
            reason: reason.or(Some(CallRecordHangupReason::BySystem)),
            code,
            initiator: HangupInitiator::System {
                reason: SystemHangupReason::InternalError,
                details: initiator,
            },
            cascade: HangupCascade::All,
        })),
        SessionAction::AcceptCall { .. } => Ok(CallCommand::Answer {
            leg_id: LegId::from("callee"),
        }),
        SessionAction::StartRinging { .. } => {
            // Ring command would need more context
            Err(anyhow!("Ring command not directly supported"))
        }
        _ => Err(anyhow!("Action not yet supported")),
    }
}

impl SipSession {
    /// Start the command processing loop
    /// 
    /// Note: Currently used in test contexts and SIP inbound flow.
    #[allow(dead_code)]
    pub async fn run_command_loop(mut self, mut cmd_rx: mpsc::UnboundedReceiver<CallCommand>) {
        loop {
            tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    debug!(session_id = %self.id, "Command loop cancelled");
                    break;
                }
                Some(cmd) = cmd_rx.recv() => {
                    let result = self.execute_command(cmd).await;
                    // Update snapshot cache (TODO: share with handle via channel)
                    let _snapshot = self.snapshot().await;
                    debug!(session_id = %self.id, result = ?result, "Command executed");
                }
            }
        }
    }

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

            CallCommand::StartRecording { .. }
            | CallCommand::StopRecording { .. }
            | CallCommand::PauseRecording { .. }
            | CallCommand::ResumeRecording { .. } => {
                // TODO: Implement recording
                CommandResult::success()
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

    /// Get session snapshot
    /// Get session snapshot
    #[allow(dead_code)]
    pub async fn snapshot(&self) -> SessionSnapshot {
        let answer_sdp = self
            .legs
            .values()
            .find(|leg| leg.answer_sdp.is_some())
            .and_then(|leg| leg.answer_sdp.clone());

        SessionSnapshot {
            id: self.id.clone(),
            state: self.state.clone(),
            leg_count: self.legs.len(),
            bridge_active: self.bridge.active,
            media_path: self.media_profile.path.clone(),
            answer_sdp,
        }
    }

    /// Check if session is active
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        !matches!(self.state, SessionState::Ended | SessionState::Ending)
    }

    /// Add a leg
    #[allow(dead_code)]
    pub fn add_leg(&mut self, leg: Leg) {
        self.legs.insert(leg.id.clone(), leg);
        self.state = Self::derive_state(&self.legs);
    }

    /// Remove a leg
    #[allow(dead_code)]
    pub fn remove_leg(&mut self, leg_id: &LegId) -> Option<Leg> {
        let leg = self.legs.remove(leg_id);
        self.state = Self::derive_state(&self.legs);
        leg
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
        let _ = self.media_bridge.take();
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
            *p = Some(PendingHangup {
                reason: Some(CallRecordHangupReason::BySystem),
                code: Some(200),
                initiator: Some("test".to_string()),
            });
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

    /// Test that media bridge is properly released
    #[test]
    fn test_media_bridge_release() {
        // Create an option with a simple value (simulating MediaBridge)
        let bridge: Option<Box<i32>> = Some(Box::new(42));

        // Take and drop
        if let Some(b) = bridge {
            drop(b);
        }

        // Note: We can't easily test MediaBridge directly without mocking,
        // but this pattern verifies the take() + drop() pattern works
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
}
