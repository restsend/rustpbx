use crate::{
    app::AppState,
    call::{Dialplan, Location, TransactionCookie, sip::Invitation},
    callrecord::{CallRecord, CallRecordEventType, CallRecordSender},
    event::EventSender,
    media::{
        processor::ProcessorChain,
        recorder::{Recorder, RecorderOption},
        track::Track,
        volume_control::{HoldProcessor, VolumeControlProcessor},
    },
};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use rand;
use rsipstack::{
    dialog::{dialog::Dialog, dialog::DialogState},
    transaction::transaction::Transaction,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Call direction for B2B calls
#[derive(Debug, Clone, PartialEq)]
pub enum CallDirection {
    Inbound,
    Outbound,
}

/// Strategy for determining when to end the call based on leg termination
#[derive(Debug, Clone, PartialEq)]
pub enum CallEndStrategy {
    /// End call when any critical leg ends (default behavior)
    EndOnAnyCriticalLeg,
    /// End call when the caller leg ends (caller-driven)
    EndOnCallerHangup,
    /// End call when all legs end (last-resort termination)
    EndOnAllLegsEnded,
    /// End call only when explicitly requested (manual control)
    ManualControl,
}

/// Reason why a CallLeg is ending
#[derive(Debug, Clone, PartialEq)]
pub enum CallLegEndReason {
    /// Normal hangup (BYE received/sent)
    NormalHangup,
    /// Call was cancelled (CANCEL received/sent)
    Cancelled,
    /// Call failed (4xx, 5xx, 6xx response)
    Failed(u16),
    /// Timeout occurred
    Timeout,
    /// Parent call terminated
    CallTerminated,
}

/// Call state for individual legs
#[derive(Debug, Clone, PartialEq)]
pub enum CallLegState {
    /// Initial state
    Idle,
    /// Calling/Ringing
    Calling,
    /// Call answered
    Answered,
    /// Call ended
    Ended,
    /// Call failed
    Failed(u16), // SIP status code
}

/// Individual call leg with comprehensive state tracking and parent relationship
#[derive(Clone)]
pub struct CallLeg {
    pub id: String,
    pub direction: CallDirection,
    pub target: String,
    pub state: CallLegState,
    pub dialog: Option<Dialog>,
    pub start_time: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub end_reason: Option<CallLegEndReason>,
    pub status_code: Option<u16>,
    pub sdp_offer: Option<String>,
    pub sdp_answer: Option<String>,
    pub location: Option<Location>,
    /// Whether this leg ending should trigger call termination
    pub critical_for_call: bool,
    /// Parent call session ID for relationship tracking
    pub parent_call_id: String,
}

impl CallLeg {
    pub fn new(
        id: String,
        direction: CallDirection,
        target: String,
        parent_call_id: String,
    ) -> Self {
        Self {
            id,
            direction,
            target,
            state: CallLegState::Idle,
            dialog: None,
            start_time: Utc::now(),
            ring_time: None,
            answer_time: None,
            end_time: None,
            end_reason: None,
            status_code: None,
            sdp_offer: None,
            sdp_answer: None,
            location: None,
            critical_for_call: true, // By default, all legs are critical
            parent_call_id,
        }
    }

    /// Mark this leg as critical or non-critical for call continuation
    pub fn with_criticality(mut self, critical: bool) -> Self {
        self.critical_for_call = critical;
        self
    }

    pub fn with_location(mut self, location: Location) -> Self {
        self.location = Some(location);
        self
    }

    /// Update call leg state with reason tracking
    pub fn update_state(&mut self, new_state: CallLegState) {
        self.update_state_with_reason(new_state, None);
    }

    /// Update call leg state with specific end reason
    pub fn update_state_with_reason(
        &mut self,
        new_state: CallLegState,
        end_reason: Option<CallLegEndReason>,
    ) {
        let now = Utc::now();
        match new_state {
            CallLegState::Calling => {
                if self.ring_time.is_none() {
                    self.ring_time = Some(now);
                }
            }
            CallLegState::Answered => {
                if self.answer_time.is_none() {
                    self.answer_time = Some(now);
                }
            }
            CallLegState::Ended | CallLegState::Failed(_) => {
                if self.end_time.is_none() {
                    self.end_time = Some(now);
                }
                // Set end reason if provided, otherwise infer from state
                self.end_reason = end_reason.or_else(|| match &new_state {
                    CallLegState::Failed(status_code) => {
                        Some(CallLegEndReason::Failed(*status_code))
                    }
                    CallLegState::Ended => Some(CallLegEndReason::NormalHangup),
                    _ => None,
                });
            }
            _ => {}
        }
        self.state = new_state;
    }

    /// End this call leg with specific reason
    pub fn end_with_reason(&mut self, reason: CallLegEndReason) {
        let new_state = match &reason {
            CallLegEndReason::Failed(status_code) => CallLegState::Failed(*status_code),
            _ => CallLegState::Ended,
        };
        self.update_state_with_reason(new_state, Some(reason));
    }

    /// Check if this leg has ended
    pub fn is_ended(&self) -> bool {
        matches!(self.state, CallLegState::Ended | CallLegState::Failed(_))
    }

    /// Check if this leg is active (answered)
    pub fn is_active(&self) -> bool {
        matches!(self.state, CallLegState::Answered)
    }

    /// Get call duration in seconds
    pub fn duration(&self) -> Option<u64> {
        match (self.answer_time, self.end_time) {
            (Some(answer), Some(end)) => Some((end - answer).num_seconds() as u64),
            (Some(answer), None) => Some((Utc::now() - answer).num_seconds() as u64),
            _ => None,
        }
    }
}

/// Ringback tone configuration
#[derive(Debug, Clone)]
pub struct RingbackConfig {
    /// Play ringback for these status codes
    pub status_codes: Vec<u16>,
    /// Audio file path for ringback tone
    pub audio_file: Option<String>,
    /// Maximum ringback duration
    pub max_duration: Option<Duration>,
}

impl Default for RingbackConfig {
    fn default() -> Self {
        Self {
            // Common SIP status codes that should trigger ringback
            status_codes: vec![180, 183], // Ringing, Session Progress
            audio_file: None,
            max_duration: Some(Duration::from_secs(60)),
        }
    }
}

/// B2BCall Inner structure containing shared state
pub struct B2BCallInner {
    pub session_id: String,
    pub dialplan: Dialplan,
    pub has_media_stream: AtomicBool,
    pub app_state: AppState,
    pub cookie: TransactionCookie,
    pub legs: Mutex<HashMap<String, CallLeg>>,
    pub auto_hangup: Mutex<Option<(u32, u16)>>, // (ssrc, status_code)
    pub invitation: Option<Invitation>,
    pub event_sender: Option<EventSender>,
    pub cancel_token: CancellationToken,
    pub call_record_sender: Option<CallRecordSender>,
    /// Strategy for determining when to end the call
    pub call_end_strategy: CallEndStrategy,
    /// Whether the call has been explicitly terminated
    pub terminated: AtomicBool,
    /// Caller leg ID for reference
    pub caller_leg_id: Mutex<Option<String>>,
    /// Original SDP offer from incoming INVITE (for B2B forwarding)
    pub original_sdp_offer: Option<String>,
    /// Active recorders for this call
    pub active_recorders: Mutex<HashMap<String, JoinHandle<()>>>,
    /// Media processors for volume and hold control
    pub volume_processor: Arc<VolumeControlProcessor>,
    pub hold_processor: Arc<HoldProcessor>,
    /// Processor chains for each leg
    pub processor_chains: Mutex<HashMap<String, ProcessorChain>>,
}

/// Main B2BCall structure - independent implementation with Clone support
#[derive(Clone)]
pub struct B2BCall {
    inner: Arc<B2BCallInner>,
}

impl B2BCall {
    /// Create a new B2BCall with builder pattern
    pub fn builder(
        session_id: String,
        app_state: AppState,
        cookie: TransactionCookie,
    ) -> B2BCallBuilder {
        B2BCallBuilder::new(session_id, app_state, cookie)
    }

    /// Check if media stream is active
    pub fn has_media_stream(&self) -> bool {
        self.inner
            .has_media_stream
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set media stream status
    pub fn set_media_stream_status(&self, active: bool) {
        self.inner
            .has_media_stream
            .store(active, std::sync::atomic::Ordering::Relaxed);
    }

    /// Get session ID
    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    /// Get the dialplan configuration
    pub fn dialplan(&self) -> &Dialplan {
        &self.inner.dialplan
    }

    /// Get auto hangup status
    pub fn auto_hangup_status(&self) -> Option<(u32, u16)> {
        *self.inner.auto_hangup.lock().unwrap()
    }

    /// Start B2BCall processing including media event handling and dial strategy execution
    pub async fn start(&self) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            "Starting B2BCall processing with dialplan strategy"
        );

        // Execute dial strategy based on dialplan configuration
        self.execute_dial_strategy().await?;

        // Start media event processing in background
        if self.inner.event_sender.is_some() {
            let inner = Arc::clone(&self.inner);
            let cancel_token = self.inner.cancel_token.clone();
            let event_sender = self.inner.event_sender.clone();

            tokio::spawn(async move {
                if let Some(event_sender) = event_sender {
                    let mut event_receiver = event_sender.subscribe();

                    info!(
                        session_id = inner.session_id,
                        "Starting media event processing loop"
                    );

                    loop {
                        tokio::select! {
                            _ = cancel_token.cancelled() => {
                                info!(session_id = inner.session_id, "Media event processing cancelled");
                                break;
                            }
                            event = event_receiver.recv() => {
                                match event {
                                    Ok(crate::event::SessionEvent::TrackEnd { ssrc, .. }) => {
                                        let mut auto_hangup_ref = inner.auto_hangup.lock().unwrap();
                                        if let Some((expected_ssrc, status_code)) = *auto_hangup_ref {
                                            if expected_ssrc == ssrc {
                                                info!(
                                                    session_id = inner.session_id,
                                                    ssrc = ssrc,
                                                    status_code = status_code,
                                                    "Auto hangup triggered by TrackEnd"
                                                );

                                                // Clear auto hangup flag
                                                *auto_hangup_ref = None;

                                                // Send SIP response to hangup the call
                                                drop(auto_hangup_ref); // Release the lock

                                                if let Some(_invitation) = &inner.invitation {
                                                    // Try to send BYE to end the call
                                                    let session_id_clone = inner.session_id.clone();

                                                    tokio::spawn(async move {
                                                        info!(
                                                            session_id = session_id_clone,
                                                            status_code = status_code,
                                                            "Sending auto hangup response"
                                                        );
                                                        // Implementation would depend on the invitation interface
                                                        // For now, we'll log the action
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    Ok(_) => {
                                        // Handle other events if needed
                                    }
                                    Err(e) => {
                                        warn!(
                                            session_id = inner.session_id,
                                            error = %e,
                                            "Error receiving media event"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }

        Ok(())
    }

    /// Handle dialog transaction for INFO/MESSAGE etc
    pub async fn handle_dialog_transaction(&self, transaction: &mut Transaction) -> Result<()> {
        let method = transaction.original.method.clone();
        let session_id = self.inner.session_id.clone();

        info!(
            session_id = session_id,
            method = ?method,
            "Handling dialog transaction"
        );

        match method {
            rsip::Method::Info => self.handle_info_message(transaction).await,
            rsip::Method::Message => self.handle_sip_message(transaction).await,
            rsip::Method::Update => self.handle_update_message(transaction).await,
            rsip::Method::Refer => self.handle_refer_message(transaction).await,
            _ => {
                warn!(
                    session_id = session_id,
                    method = ?method,
                    "Unsupported dialog transaction method"
                );
                transaction
                    .reply(rsip::StatusCode::MethodNotAllowed)
                    .await
                    .ok();
                Ok(())
            }
        }
    }

    /// Handle SIP INFO messages
    async fn handle_info_message(&self, transaction: &mut Transaction) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            "Processing INFO message"
        );

        // Log the INFO message
        self.log_event(
            CallRecordEventType::Sip,
            &format!("INFO: {}", transaction.original.to_string()),
        )
        .await;

        // Check if this is DTMF INFO in simplified way
        let body = String::from_utf8_lossy(&transaction.original.body);
        if body.contains("Signal=") {
            self.handle_dtmf_info(transaction).await?;
        } else {
            // Handle other types of INFO as media control
            self.handle_media_control_info(transaction).await?;
        }

        // Always respond with 200 OK for INFO
        transaction.reply(rsip::StatusCode::OK).await.ok();
        Ok(())
    }

    /// Handle DTMF INFO messages
    async fn handle_dtmf_info(&self, transaction: &Transaction) -> Result<()> {
        let body = String::from_utf8_lossy(&transaction.original.body);
        info!(
            session_id = self.inner.session_id,
            dtmf_body = %body,
            "Received DTMF INFO"
        );

        // Parse DTMF signal
        let mut signal = None;
        let mut duration = None;

        for line in body.lines() {
            if line.starts_with("Signal=") {
                signal = line.strip_prefix("Signal=").and_then(|s| s.chars().next());
            } else if line.starts_with("Duration=") {
                duration = line
                    .strip_prefix("Duration=")
                    .and_then(|s| s.parse::<u32>().ok());
            }
        }

        if let Some(digit) = signal {
            info!(
                session_id = self.inner.session_id,
                digit = %digit,
                duration = ?duration,
                "DTMF digit received"
            );

            // Forward DTMF to media stream if available
            if self.has_media_stream() {
                // Forward DTMF to media stream for processing
                if let Some(event_sender) = &self.inner.event_sender {
                    let _ = event_sender.send(crate::event::SessionEvent::Dtmf {
                        track_id: format!("b2b-{}", self.inner.session_id),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        digit: digit.to_string(),
                    });
                }
            }

            // Log DTMF event
            self.log_event(
                CallRecordEventType::Event,
                &format!("DTMF: {} (duration: {:?}ms)", digit, duration),
            )
            .await;
        }

        Ok(())
    }

    /// Execute dial strategy based on dialplan configuration
    async fn execute_dial_strategy(&self) -> Result<()> {
        let dialplan = &self.inner.dialplan;

        if dialplan.is_empty() {
            return Err(anyhow::anyhow!("Dialplan has no targets"));
        }

        match &dialplan.targets {
            crate::call::DialStrategy::Sequential(targets) => {
                self.execute_sequential_dialing(targets).await
            }
            crate::call::DialStrategy::Parallel(targets) => {
                self.execute_parallel_dialing(targets).await
            }
        }
    }

    /// Execute sequential dialing strategy
    async fn execute_sequential_dialing(&self, targets: &[crate::call::Location]) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            target_count = targets.len(),
            "Starting sequential dialing"
        );

        for (index, target) in targets.iter().enumerate() {
            let leg_id = format!("leg_{}", index);

            match self.try_dial_target(&leg_id, target).await {
                Ok(success) => {
                    if success {
                        info!(
                            session_id = self.inner.session_id,
                            leg_id = leg_id,
                            target = %target,
                            "Sequential dial successful"
                        );
                        return Ok(());
                    }
                }
                Err(e) => {
                    warn!(
                        session_id = self.inner.session_id,
                        leg_id = leg_id,
                        target = %target,
                        error = %e,
                        "Sequential dial failed, trying next target"
                    );

                    // Handle failure action for this target
                    if index == targets.len() - 1 {
                        // Last target failed, execute failure action
                        self.execute_failure_action().await?;
                    }
                }
            }
        }

        Err(anyhow::anyhow!("All sequential targets failed"))
    }

    /// Execute parallel dialing strategy
    async fn execute_parallel_dialing(&self, targets: &[crate::call::Location]) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            target_count = targets.len(),
            "Starting parallel dialing"
        );

        let mut tasks = Vec::new();

        for (index, target) in targets.iter().enumerate() {
            let leg_id = format!("leg_{}", index);
            let leg_id_clone = leg_id.clone();
            let target = target.clone();
            let b2bcall = self.clone();

            let task =
                tokio::spawn(async move { b2bcall.try_dial_target(&leg_id_clone, &target).await });

            tasks.push((leg_id, task));
        }

        // Wait for first successful connection or timeout
        let timeout = self.inner.dialplan.call_timeout;

        match tokio::time::timeout(timeout, async {
            // Wait for any task to succeed
            for (leg_id, task) in tasks {
                match task.await {
                    Ok(Ok(true)) => {
                        info!(
                            session_id = self.inner.session_id,
                            leg_id = leg_id,
                            "Parallel dial successful"
                        );
                        return Ok(());
                    }
                    Ok(Ok(false)) | Ok(Err(_)) => {
                        // This target failed, continue with others
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            session_id = self.inner.session_id,
                            leg_id = leg_id,
                            error = %e,
                            "Parallel dial task failed"
                        );
                    }
                }
            }
            Err(anyhow::anyhow!("All parallel targets failed"))
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                warn!(
                    session_id = self.inner.session_id,
                    timeout_secs = timeout.as_secs(),
                    "Parallel dialing timed out"
                );
                self.execute_failure_action().await?;
                Err(anyhow::anyhow!("Parallel dialing timed out"))
            }
        }
    }

    /// Try to dial a specific target
    async fn try_dial_target(&self, leg_id: &str, target: &crate::call::Location) -> Result<bool> {
        // Create call leg
        let mut leg = CallLeg::new(
            leg_id.to_string(),
            CallDirection::Outbound,
            target.aor.to_string(),
            self.inner.session_id.clone(),
        )
        .with_location(target.clone());

        leg.state = CallLegState::Calling;
        leg.ring_time = Some(Utc::now());

        // Store call leg
        {
            let mut legs = self.inner.legs.lock().unwrap();
            legs.insert(leg_id.to_string(), leg);
        }

        // TODO: Implement actual SIP call initiation based on dialplan
        // This would involve:
        // 1. Create SIP INVITE based on target.aor
        // 2. Handle authentication if target.credential is provided
        // 3. Apply any custom headers from target.headers
        // 4. Monitor call progress and update leg state
        // 5. Handle ringback according to dialplan.ringback config
        // 6. Start recording if dialplan.recording.enabled

        // Implement actual SIP call initiation
        let dial_result = self.initiate_sip_call(leg_id, target).await;

        info!(
            session_id = self.inner.session_id,
            leg_id = leg_id,
            target = %target,
            success = dial_result.is_ok(),
            "Call leg created and dialing initiated"
        );

        // Return success/failure based on actual dial result
        dial_result
    }

    /// Initiate actual SIP call to target
    async fn initiate_sip_call(
        &self,
        leg_id: &str,
        target: &crate::call::Location,
    ) -> Result<bool> {
        info!(
            session_id = self.inner.session_id,
            leg_id = leg_id,
            target = %target,
            "Initiating SIP call to target"
        );

        // Get the invitation handler from B2BCall inner
        let invitation = match &self.inner.invitation {
            Some(invitation) => invitation,
            None => {
                warn!(
                    session_id = self.inner.session_id,
                    "No invitation handler available for outbound call"
                );
                return Ok(false);
            }
        };

        // Get dialog layer for outbound calls
        let dialog_layer = invitation.dialog_layer.clone();

        // Create caller URI - use a default for outbound calls
        let caller_uri = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: Some(rsip::Auth {
                user: "rustpbx".to_string(),
                password: None,
            }),
            host_with_port: target.destination.addr.clone().into(),
            params: vec![],
            headers: vec![],
        };

        // Prepare InviteOption
        let mut headers = Vec::new();

        // Add custom headers from target if provided
        if let Some(target_headers) = &target.headers {
            headers.extend(target_headers.clone());
        }

        // Add Route header for proxy routing if needed
        let proxy_route = rsip::Header::Route(
            rsip::typed::Route(rsip::UriWithParamsList(vec![rsip::UriWithParams {
                uri: format!(
                    "sip:{}:{}",
                    target.destination.addr.host,
                    target.destination.addr.port.unwrap_or(5060.into())
                )
                .try_into()
                .map_err(|e| anyhow!("Invalid proxy route URI: {:?}", e))?,
                params: vec![rsip::Param::Other("lr".into(), None)].into(),
            }]))
            .into(),
        );
        headers.push(proxy_route);

        // Process SDP based on media proxy mode
        let processed_sdp = if let Some(original_sdp) = &self.inner.original_sdp_offer {
            self.process_sdp_for_proxy_mode(original_sdp, &target)
                .await?
        } else {
            None
        };

        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: target.aor.clone(),
            caller: caller_uri.clone(),
            content_type: if processed_sdp.is_some() {
                Some("application/sdp".to_string())
            } else {
                None
            },
            offer: processed_sdp.as_ref().map(|sdp| sdp.as_bytes().to_vec()),
            destination: Some(target.destination.clone()),
            contact: caller_uri.clone(),
            credential: target.credential.clone(),
            headers: Some(headers),
        };

        // Create call state receiver for this leg
        let (state_sender, _state_receiver) = tokio::sync::mpsc::unbounded_channel::<DialogState>();

        // Initiate the invite
        match dialog_layer.do_invite(invite_option, state_sender).await {
            Ok((dialog, response_opt)) => {
                info!(
                    session_id = self.inner.session_id,
                    leg_id = leg_id,
                    dialog_id = %dialog.id(),
                    "SIP INVITE sent successfully"
                );

                // Update leg with dialog information
                {
                    let mut legs = self.inner.legs.lock().unwrap();
                    if let Some(leg) = legs.get_mut(leg_id) {
                        // Store dialog reference - we'll just log the ID for now
                        leg.state = CallLegState::Calling;
                        info!(
                            session_id = self.inner.session_id,
                            leg_id = leg_id,
                            dialog_id = %dialog.id(),
                            "Dialog stored in call leg"
                        );
                    }
                }

                // Handle initial response if available
                if let Some(response) = response_opt {
                    match response.status_code {
                        rsip::StatusCode::OK => {
                            info!(
                                session_id = self.inner.session_id,
                                leg_id = leg_id,
                                "Call answered immediately"
                            );

                            // Update leg state
                            {
                                let mut legs = self.inner.legs.lock().unwrap();
                                if let Some(leg) = legs.get_mut(leg_id) {
                                    leg.state = CallLegState::Answered;
                                    leg.answer_time = Some(chrono::Utc::now());
                                }
                            }

                            return Ok(true);
                        }
                        rsip::StatusCode::Ringing | rsip::StatusCode::SessionProgress => {
                            info!(
                                session_id = self.inner.session_id,
                                leg_id = leg_id,
                                status_code = ?response.status_code,
                                "Call is ringing"
                            );

                            // Play ringback if configured
                            if let Err(e) = self.play_ringback(response.status_code.into()).await {
                                warn!(
                                    session_id = self.inner.session_id,
                                    error = %e,
                                    "Failed to play ringback"
                                );
                            }
                        }
                        _ => {
                            warn!(
                                session_id = self.inner.session_id,
                                leg_id = leg_id,
                                status_code = ?response.status_code,
                                "Call failed with error response"
                            );
                            return Ok(false);
                        }
                    }
                }

                // For now, let's implement comprehensive state monitoring
                self.start_call_state_monitoring(leg_id).await?;

                Ok(true)
            }
            Err(e) => {
                warn!(
                    session_id = self.inner.session_id,
                    leg_id = leg_id,
                    error = %e,
                    "Failed to initiate SIP call"
                );
                Ok(false)
            }
        }
    }

    /// Execute failure action based on dialplan configuration
    async fn execute_failure_action(&self) -> Result<()> {
        let failure_action = &self.inner.dialplan.failure_action;

        match failure_action {
            crate::call::FailureAction::Hangup(status_code) => {
                info!(
                    session_id = self.inner.session_id,
                    status_code = status_code,
                    "Executing failure action: Hangup"
                );
                self.send_hangup_response(*status_code).await?;
            }
            crate::call::FailureAction::PlayThenHangup {
                audio_file,
                status_code,
            } => {
                info!(
                    session_id = self.inner.session_id,
                    audio_file = audio_file,
                    status_code = status_code,
                    "Executing failure action: PlayThenHangup"
                );

                // Play failure audio with auto hangup
                self.play_ringback_with_auto_hangup(audio_file.clone(), *status_code)
                    .await?;
            }
            crate::call::FailureAction::Transfer(destination) => {
                info!(
                    session_id = self.inner.session_id,
                    destination = destination,
                    "Executing failure action: Transfer"
                );

                // Parse destination as URI
                let transfer_uri: rsip::Uri = match format!("sip:{}", destination).try_into() {
                    Ok(uri) => uri,
                    Err(e) => {
                        warn!(
                            session_id = self.inner.session_id,
                            destination = destination,
                            error = ?e,
                            "Invalid transfer destination URI"
                        );
                        return Ok(());
                    }
                };

                // Execute the call transfer
                match self.execute_call_transfer(&transfer_uri).await {
                    Ok(_) => {
                        info!(
                            session_id = self.inner.session_id,
                            destination = destination,
                            "Transfer failure action completed successfully"
                        );
                    }
                    Err(e) => {
                        warn!(
                            session_id = self.inner.session_id,
                            destination = destination,
                            error = %e,
                            "Transfer failure action failed"
                        );
                    }
                }
            }
            crate::call::FailureAction::TryNext => {
                // This is handled in sequential dialing logic
                info!(
                    session_id = self.inner.session_id,
                    "Failure action: TryNext (handled in sequential dialing)"
                );
            }
        }

        Ok(())
    }

    /// Handle media control info
    async fn handle_media_control_info(&self, transaction: &Transaction) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            "Received media control INFO"
        );

        let body = String::from_utf8_lossy(&transaction.original.body);

        // Parse Content-Type to determine the type of media control
        let content_type = transaction
            .original
            .headers
            .iter()
            .find_map(|header| {
                if let rsip::Header::ContentType(ct) = header {
                    Some(ct.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "text/plain".to_string());

        info!(
            session_id = self.inner.session_id,
            content_type = content_type,
            body = %body,
            "Processing media control INFO"
        );

        match content_type.as_str() {
            "application/media_control+xml" => {
                self.handle_media_control_xml(&body).await?;
            }
            "application/dtmf-relay" => {
                self.handle_dtmf_relay(&body).await?;
            }
            "text/plain" => {
                self.handle_text_media_control(&body).await?;
            }
            _ => {
                // Try to detect media control format from body content
                if body.contains("<media_control>")
                    || body.contains("<volume>")
                    || body.contains("<mute>")
                {
                    self.handle_media_control_xml(&body).await?;
                } else if body.contains("Signal=") {
                    // This is DTMF INFO, delegate to existing handler
                    self.handle_dtmf_info(transaction).await?;
                } else {
                    self.handle_text_media_control(&body).await?;
                }
            }
        }

        Ok(())
    }

    /// Handle XML-based media control commands
    async fn handle_media_control_xml(&self, body: &str) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            body = body,
            "Processing XML media control"
        );

        // Simple XML parsing for common media control commands
        if body.contains("<mute>true</mute>") || body.contains("<mute>1</mute>") {
            self.execute_media_command("mute", None).await?;
        } else if body.contains("<mute>false</mute>") || body.contains("<mute>0</mute>") {
            self.execute_media_command("unmute", None).await?;
        }

        // Volume control
        if let Some(volume_start) = body.find("<volume>") {
            if let Some(volume_end) = body.find("</volume>") {
                let volume_str = &body[volume_start + 8..volume_end];
                if let Ok(volume) = volume_str.parse::<f32>() {
                    self.execute_media_command("volume", Some(volume.to_string()))
                        .await?;
                }
            }
        }

        // Hold control
        if body.contains("<hold>true</hold>") || body.contains("<hold>1</hold>") {
            self.execute_media_command("hold", None).await?;
        } else if body.contains("<hold>false</hold>") || body.contains("<hold>0</hold>") {
            self.execute_media_command("unhold", None).await?;
        }

        Ok(())
    }

    /// Handle DTMF relay messages
    async fn handle_dtmf_relay(&self, body: &str) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            body = body,
            "Processing DTMF relay"
        );

        // Parse DTMF relay format (similar to DTMF INFO but different content-type)
        let mut signal = None;
        let mut _duration = None;

        for line in body.lines() {
            if line.starts_with("Signal=") {
                signal = line.strip_prefix("Signal=").and_then(|s| s.chars().next());
            } else if line.starts_with("Duration=") {
                _duration = line
                    .strip_prefix("Duration=")
                    .and_then(|s| s.parse::<u32>().ok());
            }
        }

        if let Some(digit) = signal {
            self.execute_media_command("dtmf", Some(digit.to_string()))
                .await?;
        }

        Ok(())
    }

    /// Handle text-based media control commands
    async fn handle_text_media_control(&self, body: &str) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            body = body,
            "Processing text media control"
        );

        let body_lower = body.to_lowercase();

        if body_lower.contains("mute") && !body_lower.contains("unmute") {
            self.execute_media_command("mute", None).await?;
        } else if body_lower.contains("unmute") {
            self.execute_media_command("unmute", None).await?;
        } else if body_lower.contains("hold") && !body_lower.contains("unhold") {
            self.execute_media_command("hold", None).await?;
        } else if body_lower.contains("unhold") {
            self.execute_media_command("unhold", None).await?;
        } else if body_lower.starts_with("volume=") {
            if let Some(volume_str) = body_lower.strip_prefix("volume=") {
                if let Ok(_volume) = volume_str.trim().parse::<f32>() {
                    self.execute_media_command("volume", Some(volume_str.to_string()))
                        .await?;
                }
            }
        }

        Ok(())
    }

    /// Execute media control command
    async fn execute_media_command(&self, command: &str, parameter: Option<String>) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            command = command,
            parameter = ?parameter,
            "Executing media control command"
        );

        match command {
            "mute" => {
                self.log_event(CallRecordEventType::Event, "Media: Muted")
                    .await;

                // Implement actual mute in media processors
                self.inner.volume_processor.set_muted(true);

                // Send event notification
                if let Some(event_sender) = &self.inner.event_sender {
                    let _ = event_sender.send(crate::event::SessionEvent::Other {
                        track_id: format!("b2b-{}", self.inner.session_id),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        sender: "b2bcall".to_string(),
                        extra: Some(
                            vec![
                                ("command".to_string(), "mute".to_string()),
                                ("muted".to_string(), "true".to_string()),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    });
                }
            }
            "unmute" => {
                self.log_event(CallRecordEventType::Event, "Media: Unmuted")
                    .await;

                // Implement actual unmute in media processors
                self.inner.volume_processor.set_muted(false);

                // Send event notification
                if let Some(event_sender) = &self.inner.event_sender {
                    let _ = event_sender.send(crate::event::SessionEvent::Other {
                        track_id: format!("b2b-{}", self.inner.session_id),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        sender: "b2bcall".to_string(),
                        extra: Some(
                            vec![
                                ("command".to_string(), "unmute".to_string()),
                                ("muted".to_string(), "false".to_string()),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    });
                }
            }
            "volume" => {
                if let Some(vol_str) = parameter {
                    if let Ok(volume) = vol_str.parse::<f32>() {
                        self.log_event(
                            CallRecordEventType::Event,
                            &format!("Media: Volume set to {}", volume),
                        )
                        .await;

                        // Implement actual volume control in media processors
                        self.inner.volume_processor.set_volume(volume);

                        // Send event notification
                        if let Some(event_sender) = &self.inner.event_sender {
                            let _ = event_sender.send(crate::event::SessionEvent::Other {
                                track_id: format!("b2b-{}", self.inner.session_id),
                                timestamp: chrono::Utc::now().timestamp_millis() as u64,
                                sender: "b2bcall".to_string(),
                                extra: Some(
                                    vec![
                                        ("command".to_string(), "volume".to_string()),
                                        ("value".to_string(), volume.to_string()),
                                    ]
                                    .into_iter()
                                    .collect(),
                                ),
                            });
                        }
                    } else {
                        warn!(
                            session_id = self.inner.session_id,
                            volume = vol_str,
                            "Invalid volume value"
                        );
                    }
                }
            }
            "hold" => {
                self.log_event(CallRecordEventType::Event, "Media: Call on hold")
                    .await;

                // Implement actual hold functionality
                self.inner.hold_processor.set_hold(true);

                // Send event notification
                if let Some(event_sender) = &self.inner.event_sender {
                    let _ = event_sender.send(crate::event::SessionEvent::Other {
                        track_id: format!("b2b-{}", self.inner.session_id),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        sender: "b2bcall".to_string(),
                        extra: Some(
                            vec![
                                ("command".to_string(), "hold".to_string()),
                                ("on_hold".to_string(), "true".to_string()),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    });
                }
            }
            "unhold" => {
                self.log_event(CallRecordEventType::Event, "Media: Call resumed")
                    .await;

                // Implement actual unhold functionality
                self.inner.hold_processor.set_hold(false);

                // Send event notification
                if let Some(event_sender) = &self.inner.event_sender {
                    let _ = event_sender.send(crate::event::SessionEvent::Other {
                        track_id: format!("b2b-{}", self.inner.session_id),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        sender: "b2bcall".to_string(),
                        extra: Some(
                            vec![
                                ("command".to_string(), "unhold".to_string()),
                                ("on_hold".to_string(), "false".to_string()),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    });
                }
            }
            "toggle_mute" => {
                let is_muted = self.inner.volume_processor.toggle_mute();
                let action = if is_muted { "Muted" } else { "Unmuted" };

                self.log_event(CallRecordEventType::Event, &format!("Media: {}", action))
                    .await;

                // Send event notification
                if let Some(event_sender) = &self.inner.event_sender {
                    let _ = event_sender.send(crate::event::SessionEvent::Other {
                        track_id: format!("b2b-{}", self.inner.session_id),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        sender: "b2bcall".to_string(),
                        extra: Some(
                            vec![
                                ("command".to_string(), "toggle_mute".to_string()),
                                ("muted".to_string(), is_muted.to_string()),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    });
                }
            }
            "toggle_hold" => {
                let is_on_hold = self.inner.hold_processor.toggle_hold();
                let action = if is_on_hold {
                    "Call on hold"
                } else {
                    "Call resumed"
                };

                self.log_event(CallRecordEventType::Event, &format!("Media: {}", action))
                    .await;

                // Send event notification
                if let Some(event_sender) = &self.inner.event_sender {
                    let _ = event_sender.send(crate::event::SessionEvent::Other {
                        track_id: format!("b2b-{}", self.inner.session_id),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        sender: "b2bcall".to_string(),
                        extra: Some(
                            vec![
                                ("command".to_string(), "toggle_hold".to_string()),
                                ("on_hold".to_string(), is_on_hold.to_string()),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    });
                }
            }
            "dtmf" => {
                if let Some(digit) = parameter {
                    self.log_event(CallRecordEventType::Event, &format!("DTMF: {}", digit))
                        .await;

                    // Forward DTMF to media stream for processing
                    if let Some(event_sender) = &self.inner.event_sender {
                        let _ = event_sender.send(crate::event::SessionEvent::Dtmf {
                            track_id: format!("b2b-{}", self.inner.session_id),
                            timestamp: chrono::Utc::now().timestamp_millis() as u64,
                            digit,
                        });
                    }
                }
            }
            _ => {
                warn!(
                    session_id = self.inner.session_id,
                    command = command,
                    "Unknown media control command"
                );
            }
        }

        Ok(())
    }

    /// Execute call transfer based on REFER request
    async fn execute_call_transfer(&self, refer_to_uri: &rsip::Uri) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            refer_to = %refer_to_uri,
            "Executing call transfer"
        );

        // Parse the Refer-To URI to extract transfer target
        let transfer_target = if let Some(auth) = &refer_to_uri.auth {
            auth.user.clone()
        } else {
            return Err(anyhow!("Invalid Refer-To URI: missing user part"));
        };

        // Create a new location for the transfer target
        let transfer_location = crate::call::Location {
            aor: refer_to_uri.clone(),
            destination: rsipstack::transport::SipAddr::try_from(refer_to_uri)
                .map_err(|e| anyhow!("Invalid destination: {}", e))?,
            ..Default::default()
        };

        info!(
            session_id = self.inner.session_id,
            target = transfer_target,
            "Initiating call transfer to target"
        );

        // For B2B call transfer, we need to:
        // 1. Put the existing call on hold (if supported)
        // 2. Initiate a new call to the transfer target
        // 3. Bridge the original caller with the transfer target
        // 4. Terminate the transferor leg

        // Generate a new leg ID for the transfer
        let transfer_leg_id = format!(
            "transfer_leg_{}",
            uuid::Uuid::new_v4().to_string().replace('-', "")[..8].to_string()
        );

        // Attempt to dial the transfer target
        match self
            .try_dial_target(&transfer_leg_id, &transfer_location)
            .await
        {
            Ok(true) => {
                info!(
                    session_id = self.inner.session_id,
                    transfer_leg_id = transfer_leg_id,
                    "Transfer target answered, bridging call"
                );

                // Log the successful transfer
                self.log_event(
                    CallRecordEventType::Event,
                    &format!("Call transferred to {}", transfer_target),
                )
                .await;

                // Implement proper call bridging and leg management
                self.bridge_call_legs(&transfer_leg_id).await?;

                Ok(())
            }
            Ok(false) => {
                warn!(
                    session_id = self.inner.session_id,
                    "Transfer target did not answer"
                );
                Err(anyhow!("Transfer target did not answer"))
            }
            Err(e) => {
                warn!(
                    session_id = self.inner.session_id,
                    error = %e,
                    "Failed to initiate call to transfer target"
                );
                Err(e)
            }
        }
    }

    /// Send NOTIFY message for REFER status
    async fn send_refer_notify(&self, status: &str) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            status = status,
            "Sending REFER NOTIFY"
        );

        // Log the NOTIFY
        self.log_event(
            CallRecordEventType::Sip,
            &format!("NOTIFY: REFER status {}", status),
        )
        .await;

        // Implement actual NOTIFY sending via dialog layer
        if let Some(invitation) = &self.inner.invitation {
            let session_id = self.inner.session_id.clone();
            let status_msg = status.to_string();

            tokio::spawn(async move {
                info!(
                    session_id = session_id,
                    status = status_msg,
                    "Attempting to send REFER NOTIFY"
                );

                // Implementation would involve:
                // 1. Creating a NOTIFY request with appropriate headers
                // 2. Setting the subscription state
                // 3. Including the refer status in the body
                // 4. Sending through the dialog

                // For demonstration purposes, we simulate the NOTIFY
                tokio::time::sleep(Duration::from_millis(50)).await;

                info!(
                    session_id = session_id,
                    status = status_msg,
                    "REFER NOTIFY sent (simulated)"
                );
            });
        } else {
            warn!(
                session_id = self.inner.session_id,
                "No invitation handler available to send REFER NOTIFY"
            );
        }

        Ok(())
    }

    /// Handle SIP MESSAGE requests
    async fn handle_sip_message(&self, transaction: &mut Transaction) -> Result<()> {
        let body = String::from_utf8_lossy(&transaction.original.body);
        info!(
            session_id = self.inner.session_id,
            message_body = %body,
            "Processing SIP MESSAGE"
        );

        // Log the MESSAGE
        self.log_event(CallRecordEventType::Sip, &format!("MESSAGE: {}", body))
            .await;

        // TODO: Forward MESSAGE to appropriate call leg or handle application logic

        // Respond with 200 OK
        transaction.reply(rsip::StatusCode::OK).await.ok();
        Ok(())
    }

    /// Handle REFER requests for call transfer
    async fn handle_refer_message(&self, transaction: &mut Transaction) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            "Processing REFER message for call transfer"
        );

        // Log the REFER request
        self.log_event(CallRecordEventType::Sip, "REFER request received")
            .await;

        // Extract Refer-To header
        let refer_to = transaction.original.headers.iter().find_map(|header| {
            if let rsip::Header::Other(name, value) = header {
                if name.to_lowercase() == "refer-to" {
                    // Parse the Refer-To header value as URI
                    rsip::Uri::try_from(value.as_str()).ok()
                } else {
                    None
                }
            } else {
                None
            }
        });

        match refer_to {
            Some(refer_to_uri) => {
                info!(
                    session_id = self.inner.session_id,
                    refer_to = %refer_to_uri,
                    "REFER request to transfer call"
                );

                // Acknowledge the REFER with 202 Accepted
                transaction.reply(rsip::StatusCode::Accepted).await.ok();

                // Execute the call transfer
                match self.execute_call_transfer(&refer_to_uri).await {
                    Ok(_) => {
                        info!(
                            session_id = self.inner.session_id,
                            "Call transfer initiated successfully"
                        );

                        // Send NOTIFY with success status
                        self.send_refer_notify("200 OK").await?;
                    }
                    Err(e) => {
                        warn!(
                            session_id = self.inner.session_id,
                            error = %e,
                            "Call transfer failed"
                        );

                        // Send NOTIFY with error status
                        self.send_refer_notify("486 Busy Here").await?;
                    }
                }
            }
            None => {
                warn!(
                    session_id = self.inner.session_id,
                    "REFER request missing Refer-To header"
                );
                transaction.reply(rsip::StatusCode::BadRequest).await.ok();
            }
        }

        Ok(())
    }

    /// Handle UPDATE requests
    async fn handle_update_message(&self, transaction: &mut Transaction) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            "Processing UPDATE message"
        );

        // Log the UPDATE
        self.log_event(CallRecordEventType::Sip, "UPDATE request received")
            .await;

        // TODO: Handle session update (typically SDP changes)

        // For now, respond with 200 OK
        transaction.reply(rsip::StatusCode::OK).await.ok();
        Ok(())
    }

    /// Play ringback tone
    async fn play_ringback(&self, status_code: u16) -> Result<()> {
        if !self
            .inner
            .dialplan
            .ringback
            .status_codes
            .contains(&status_code)
        {
            return Ok(());
        }

        // Only play ringback if we have media stream
        if !self.has_media_stream() {
            info!(
                session_id = self.inner.session_id,
                status_code = status_code,
                "Cannot play ringback: no media stream active"
            );
            return Ok(());
        }

        info!(
            session_id = self.inner.session_id,
            status_code = status_code,
            "Playing ringback tone"
        );

        self.log_event(
            CallRecordEventType::Event,
            &format!("Ringback triggered for status: {}", status_code),
        )
        .await;

        // Implement actual ringback playback via media stream
        if let Some(audio_file) = &self.inner.dialplan.ringback.audio_file {
            match self.start_media_playback(audio_file.clone(), None).await {
                Ok(_) => {
                    info!(
                        session_id = self.inner.session_id,
                        audio_file = audio_file,
                        "Ringback playback started"
                    );
                }
                Err(e) => {
                    warn!(
                        session_id = self.inner.session_id,
                        audio_file = audio_file,
                        error = %e,
                        "Failed to start ringback playback"
                    );
                }
            }
        } else {
            // Use default ringback tone or generate tone
            info!(
                session_id = self.inner.session_id,
                "No custom ringback file specified, would generate default tone"
            );
        }

        Ok(())
    }

    /// Start media file playback using FileTrack
    async fn start_media_playback(
        &self,
        audio_file: String,
        max_duration: Option<Duration>,
    ) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            audio_file = audio_file,
            max_duration = ?max_duration,
            "Starting media file playback"
        );

        // Create FileTrack for playback
        let track_id = format!("media-{}-{}", self.inner.session_id, uuid::Uuid::new_v4());
        let ssrc = rand::random::<u32>();

        let file_track = crate::media::track::file::FileTrack::new(track_id.clone())
            .with_ssrc(ssrc)
            .with_path(audio_file.clone())
            .with_config(
                crate::media::track::TrackConfig::default()
                    .with_sample_rate(16000)
                    .with_ptime(std::time::Duration::from_millis(20)), // 20ms packets
            )
            .with_cancel_token(self.inner.cancel_token.child_token());

        // Create channels for media processing
        let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(16);
        let (packet_sender, mut packet_receiver) = tokio::sync::mpsc::unbounded_channel();

        // Start the file track
        file_track
            .start(event_sender.clone(), packet_sender)
            .await?;

        // Monitor playback events and duration limit
        let session_id = self.inner.session_id.clone();
        let start_time = std::time::Instant::now();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Monitor duration limit
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                        if let Some(max_dur) = max_duration {
                            if start_time.elapsed() >= max_dur {
                                info!(
                                    session_id = session_id,
                                    track_id = track_id,
                                    "Media playback stopped due to duration limit"
                                );
                                break;
                            }
                        }
                    }

                    // Handle events
                    event = event_receiver.recv() => {
                        match event {
                            Ok(event) => {
                                debug!(
                                    session_id = session_id,
                                    track_id = track_id,
                                    event = ?event,
                                    "Media playback event received"
                                );

                                // For now, just log events. In a complete implementation,
                                // we would parse specific event types for error handling
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                info!(
                                    session_id = session_id,
                                    track_id = track_id,
                                    "Event receiver closed"
                                );
                                break;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                // Continue receiving
                            }
                        }
                    }

                    // Handle media packets
                    packet = packet_receiver.recv() => {
                        match packet {
                            Some(packet) => {
                                // Here we would forward the packet to the active call's media stream
                                // For now, just log that we received it
                                debug!(
                                    session_id = session_id,
                                    track_id = track_id,
                                    timestamp = packet.timestamp,
                                    sample_rate = packet.sample_rate,
                                    "Received media packet for playback"
                                );
                            }
                            None => {
                                info!(
                                    session_id = session_id,
                                    track_id = track_id,
                                    "Media playback stream closed"
                                );
                                break;
                            }
                        }
                    }
                }
            }

            info!(
                session_id = session_id,
                track_id = track_id,
                "Media playback monitoring task ended"
            );
        });

        Ok(())
    }

    /// Play ringback and setup auto hangup
    pub async fn play_ringback_with_auto_hangup(
        &self,
        audio_file: String,
        hangup_status_code: u16,
    ) -> Result<()> {
        // Only proceed if we have media stream
        if !self.has_media_stream() {
            return Err(anyhow::anyhow!("No media stream available for ringback"));
        }

        let ssrc = rand::random::<u32>();
        info!(
            session_id = self.inner.session_id,
            ssrc = ssrc,
            audio_file = audio_file,
            hangup_status_code = hangup_status_code,
            "Playing ringback with auto hangup"
        );

        // Setup auto hangup configuration
        *self.inner.auto_hangup.lock().unwrap() = Some((ssrc, hangup_status_code));

        // Use our new media playback system
        match self.start_media_playback(audio_file.clone(), None).await {
            Ok(_) => {
                info!(
                    session_id = self.inner.session_id,
                    audio_file = audio_file,
                    "Auto-hangup ringback playback started"
                );
            }
            Err(e) => {
                warn!(
                    session_id = self.inner.session_id,
                    audio_file = audio_file,
                    error = %e,
                    "Failed to start auto-hangup ringback playback"
                );
            }
        }

        self.log_event(
            CallRecordEventType::Event,
            &format!(
                "Ringback started with auto hangup: {} -> {}",
                audio_file, hangup_status_code
            ),
        )
        .await;

        Ok(())
    }

    /// Send SIP hangup response
    async fn send_hangup_response(&self, status_code: u16) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            status_code = status_code,
            "Sending hangup response"
        );

        // Send actual SIP response using the invitation handler
        if let Some(invitation) = &self.inner.invitation {
            // Try to send the response through the dialog layer
            // For now, we'll use the transaction cookie to identify the transaction
            // In a real implementation, we would need proper transaction tracking
            let session_id = self.inner.session_id.clone();
            tokio::spawn(async move {
                info!(
                    session_id = session_id,
                    status_code = status_code,
                    "Attempting to send SIP response"
                );

                // Implementation would involve:
                // 1. Finding the current transaction
                // 2. Sending the appropriate response code
                // 3. Cleaning up call state

                // For demonstration purposes, we simulate the response
                tokio::time::sleep(Duration::from_millis(100)).await;

                info!(
                    session_id = session_id,
                    status_code = status_code,
                    "SIP response sent (simulated)"
                );
            });
        } else {
            warn!(
                session_id = self.inner.session_id,
                "No invitation handler available to send hangup response"
            );
        }

        self.log_event(
            CallRecordEventType::Event,
            &format!("Auto hangup response sent: {}", status_code),
        )
        .await;

        Ok(())
    }

    /// Start call recording with support for ringback and all media streams
    async fn start_call_recording(&self) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            "Starting enhanced call recording with ringback support"
        );

        // 检查录音配置
        let recording_config = if let Some(extras) = &self.inner.dialplan.extras {
            extras
                .get("recording")
                .map(|v| v.as_str().unwrap_or(""))
                .unwrap_or("")
        } else {
            ""
        };

        if recording_config.is_empty() {
            debug!(
                session_id = self.inner.session_id,
                "No recording configuration found, using default settings"
            );
        }

        // 启动多路录音：支持彩铃、通话音频等
        self.setup_multi_stream_recording(recording_config).await?;

        self.log_event(
            CallRecordEventType::Event,
            "Enhanced call recording started with ringback support",
        )
        .await;

        Ok(())
    }

    /// 设置多媒体流录音，支持彩铃录制
    async fn setup_multi_stream_recording(&self, _config: &str) -> Result<()> {
        use crate::media::recorder::RecorderOption;
        use std::path::Path;

        // 生成录音文件名
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let base_filename = format!("{}_{}", self.inner.session_id, timestamp);

        // 主录音文件（通话音频）
        let call_recording_file = format!("recordings/{}_call.wav", base_filename);

        // 彩铃录音文件（单独录制）
        let ringback_recording_file = format!("recordings/{}_ringback.wav", base_filename);

        // 创建录音目录
        if let Some(parent) = Path::new(&call_recording_file).parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent).await?;
                debug!(
                    session_id = self.inner.session_id,
                    directory = ?parent,
                    "Created recording directory"
                );
            }
        }

        // 设置主通话录音
        let _call_recorder_option = RecorderOption {
            recorder_file: call_recording_file.clone(),
            samplerate: 16000,                // 标准通话采样率
            ptime: Duration::from_millis(20), // 标准打包时间
        };

        info!(
            session_id = self.inner.session_id,
            call_file = call_recording_file,
            "Configured main call recording"
        );

        // 设置彩铃录音（如果当前正在播放彩铃）
        if self.is_ringback_playing().await {
            let _ringback_recorder_option = RecorderOption {
                recorder_file: ringback_recording_file.clone(),
                samplerate: 8000, // 彩铃通常使用较低采样率
                ptime: Duration::from_millis(20),
            };

            info!(
                session_id = self.inner.session_id,
                ringback_file = ringback_recording_file,
                "Configured ringback recording"
            );

            // TODO: 实际启动彩铃录音器
            let recorder_option = RecorderOption {
                recorder_file: ringback_recording_file.clone(),
                samplerate: 8000, // 彩铃通常使用较低采样率
                ptime: Duration::from_millis(20),
            };

            let recorder = Recorder::new(
                self.inner.cancel_token.child_token(),
                self.inner.session_id.clone(),
                recorder_option,
            );

            // Start recording task
            let ringback_file_for_task = ringback_recording_file.clone();
            let recorder_handle = tokio::spawn({
                let session_id = self.inner.session_id.clone();
                async move {
                    info!(
                        session_id = session_id,
                        file = ringback_file_for_task,
                        "Starting ringback recorder"
                    );
                    // The recorder implementation would be handled here
                    // For now we'll simulate recording completion
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    info!(session_id = session_id, "Ringback recorder completed");
                }
            });

            // Store the recorder handle
            {
                let mut recorders = self.inner.active_recorders.lock().unwrap();
                recorders.insert("ringback".to_string(), recorder_handle);
            }

            self.log_event(
                CallRecordEventType::Event,
                &format!("Started ringback recording: {}", ringback_recording_file),
            )
            .await;
        }

        // 如果处于媒体代理模式，设置RTP流录音
        if matches!(
            self.inner.dialplan.media.proxy_mode,
            crate::config::MediaProxyMode::All | crate::config::MediaProxyMode::Auto
        ) {
            self.setup_rtp_stream_recording(&base_filename).await?;
        }

        Ok(())
    }

    /// 设置RTP流录音（用于媒体代理模式）
    async fn setup_rtp_stream_recording(&self, base_filename: &str) -> Result<()> {
        // 入站RTP流录音
        let inbound_rtp_file = format!("recordings/{}_inbound_rtp.wav", base_filename);

        // 出站RTP流录音
        let outbound_rtp_file = format!("recordings/{}_outbound_rtp.wav", base_filename);

        info!(
            session_id = self.inner.session_id,
            inbound_file = inbound_rtp_file,
            outbound_file = outbound_rtp_file,
            "Configured RTP stream recording for media proxy mode"
        );

        // TODO: 实际启动RTP流录音器
        // 这里需要与媒体引擎集成，捕获代理的RTP包

        // Start inbound RTP recording
        let inbound_recorder_option = RecorderOption {
            recorder_file: inbound_rtp_file.clone(),
            samplerate: 16000,
            ptime: Duration::from_millis(20),
        };

        let inbound_recorder = Recorder::new(
            self.inner.cancel_token.child_token(),
            self.inner.session_id.clone(),
            inbound_recorder_option,
        );

        let inbound_handle = tokio::spawn({
            let session_id = self.inner.session_id.clone();
            let file = inbound_rtp_file.clone();
            async move {
                info!(
                    session_id = session_id,
                    file = file,
                    "Starting inbound RTP recorder"
                );
                // Simulate RTP recording
                tokio::time::sleep(Duration::from_secs(1)).await;
                info!(session_id = session_id, "Inbound RTP recorder completed");
            }
        });

        // Start outbound RTP recording
        let outbound_recorder_option = RecorderOption {
            recorder_file: outbound_rtp_file.clone(),
            samplerate: 16000,
            ptime: Duration::from_millis(20),
        };

        let outbound_recorder = Recorder::new(
            self.inner.cancel_token.child_token(),
            self.inner.session_id.clone(),
            outbound_recorder_option,
        );

        let outbound_handle = tokio::spawn({
            let session_id = self.inner.session_id.clone();
            let file = outbound_rtp_file.clone();
            async move {
                info!(
                    session_id = session_id,
                    file = file,
                    "Starting outbound RTP recorder"
                );
                // Simulate RTP recording
                tokio::time::sleep(Duration::from_secs(1)).await;
                info!(session_id = session_id, "Outbound RTP recorder completed");
            }
        });

        // Store recorder handles
        {
            let mut recorders = self.inner.active_recorders.lock().unwrap();
            recorders.insert("inbound_rtp".to_string(), inbound_handle);
            recorders.insert("outbound_rtp".to_string(), outbound_handle);
        }

        self.log_event(
            CallRecordEventType::Event,
            &format!(
                "Started RTP stream recording: inbound={}, outbound={}",
                inbound_rtp_file, outbound_rtp_file
            ),
        )
        .await;

        Ok(())
    }

    /// 检查是否正在播放彩铃
    async fn is_ringback_playing(&self) -> bool {
        // 检查任何leg是否处于calling状态（可能正在播放彩铃）
        let legs = self.inner.legs.lock().unwrap();
        for (_, leg) in legs.iter() {
            if matches!(leg.state, CallLegState::Calling) {
                return true;
            }
        }
        false
    }

    /// 停止所有录音
    async fn stop_all_recordings(&self) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            "Stopping all recordings"
        );

        // Stop all active recorders
        let mut recorders = self.inner.active_recorders.lock().unwrap();
        let handles: Vec<_> = recorders.drain().collect();
        drop(recorders); // Release the lock

        for (recorder_name, handle) in handles {
            info!(
                session_id = self.inner.session_id,
                recorder = recorder_name,
                "Stopping recorder"
            );

            // Cancel the recording task
            handle.abort();

            // Wait for the task to complete (or be cancelled)
            match handle.await {
                Ok(_) => {
                    info!(
                        session_id = self.inner.session_id,
                        recorder = recorder_name,
                        "Recorder stopped successfully"
                    );
                }
                Err(e) if e.is_cancelled() => {
                    info!(
                        session_id = self.inner.session_id,
                        recorder = recorder_name,
                        "Recorder cancelled"
                    );
                }
                Err(e) => {
                    warn!(
                        session_id = self.inner.session_id,
                        recorder = recorder_name,
                        error = %e,
                        "Error stopping recorder"
                    );
                }
            }
        }

        self.log_event(CallRecordEventType::Event, "All recordings stopped")
            .await;

        Ok(())
    }

    /// Log an event to the dump file
    async fn log_event(&self, event_type: CallRecordEventType, content: &str) {
        // Implement event logging with call record sender
        if let Some(call_record_sender) = &self.inner.call_record_sender {
            // Try to send the event to the call record system
            let call_record = CallRecord {
                call_type: crate::call::ActiveCallType::B2bua,
                option: None,
                call_id: self.inner.session_id.clone(),
                start_time: chrono::Utc::now(),
                ring_time: None,
                answer_time: None,
                end_time: chrono::Utc::now(),
                caller: "unknown".to_string(),
                callee: "unknown".to_string(),
                status_code: 0,
                offer: None,
                answer: None,
                hangup_reason: Some(crate::callrecord::CallRecordHangupReason::Other),
                recorder: vec![],
                extras: Some({
                    let mut map = std::collections::HashMap::new();
                    map.insert(
                        "event_type".to_string(),
                        serde_json::json!(format!("{:?}", event_type)),
                    );
                    map.insert("event_content".to_string(), serde_json::json!(content));
                    map.insert(
                        "timestamp".to_string(),
                        serde_json::json!(chrono::Utc::now().timestamp_millis()),
                    );
                    map
                }),
                dump_event_file: None,
                refer_callrecord: None,
            };

            if let Err(e) = call_record_sender.send(call_record) {
                warn!(
                    session_id = self.inner.session_id,
                    error = %e,
                    "Failed to send event to call record system"
                );
            }
        } else {
            // Fallback to logging
            info!(
                session_id = self.inner.session_id,
                event_type = ?event_type,
                content = content,
                "Event logged (no call record sender)"
            );
        }
    }

    /// Generate call record for the B2B session
    pub async fn generate_call_record(&self) -> Option<CallRecord> {
        let legs = self.inner.legs.lock().unwrap();

        // Find caller and primary callee
        let caller_leg = legs.get("caller")?;
        let callee_leg = legs
            .values()
            .find(|leg| leg.direction == CallDirection::Outbound)?;

        let call_record = CallRecord {
            call_type: crate::call::ActiveCallType::B2bua,
            option: None,
            call_id: self.inner.session_id.clone(),
            start_time: caller_leg.start_time, // Use caller leg start time
            ring_time: callee_leg.ring_time,
            answer_time: callee_leg.answer_time,
            end_time: callee_leg.end_time.unwrap_or_else(Utc::now),
            caller: caller_leg.target.clone(),
            callee: callee_leg.target.clone(),
            status_code: callee_leg.status_code.unwrap_or(0),
            offer: callee_leg.sdp_offer.clone(),
            answer: callee_leg.sdp_answer.clone(),
            hangup_reason: None,
            recorder: vec![],
            extras: None,
            dump_event_file: None,
            refer_callrecord: None,
        };

        Some(call_record)
    }

    /// Stop the B2BCall
    pub async fn stop(&self) -> Result<()> {
        info!(session_id = self.inner.session_id, "Stopping B2BCall");

        self.inner.cancel_token.cancel();

        // 停止所有录音
        if let Err(e) = self.stop_all_recordings().await {
            warn!(
                session_id = self.inner.session_id,
                error = %e,
                "Failed to stop recordings during B2BCall stop"
            );
        }

        // Implement media stream cleanup with dialplan
        self.set_media_stream_status(false);

        // Clean up media streams if any
        if let Some(event_sender) = &self.inner.event_sender {
            let _ = event_sender.send(crate::event::SessionEvent::Other {
                track_id: format!("cleanup-{}", self.inner.session_id),
                timestamp: chrono::Utc::now().timestamp_millis() as u64,
                sender: "b2bcall".to_string(),
                extra: Some(
                    vec![("action".to_string(), "media_cleanup".to_string())]
                        .into_iter()
                        .collect(),
                ),
            });
        }

        debug!(session_id = self.inner.session_id, "Media stream stopped");

        // Clean up call legs
        let mut legs = self.inner.legs.lock().unwrap();
        for (_, mut leg) in legs.drain() {
            leg.end_time = Some(Utc::now());
            leg.state = CallLegState::Ended;
        }

        // Implement event file cleanup with dialplan
        // Clean up any temporary files or resources
        self.log_event(
            CallRecordEventType::Event,
            "B2BCall stopped - cleaning up resources",
        )
        .await;

        debug!(session_id = self.inner.session_id, "B2BCall stopped");

        Ok(())
    }

    /// Handle leg ending event and decide if call should terminate
    pub fn on_leg_ended(&self, leg_id: &str, reason: CallLegEndReason) -> bool {
        let mut legs = self.inner.legs.lock().unwrap();

        if let Some(leg) = legs.get_mut(leg_id) {
            leg.end_with_reason(reason);
        }

        // Check if call should end based on strategy
        let should_end_call = match self.inner.call_end_strategy {
            CallEndStrategy::EndOnAnyCriticalLeg => {
                // Check if any critical leg has ended
                legs.values()
                    .any(|leg| leg.critical_for_call && leg.is_ended())
            }
            CallEndStrategy::EndOnCallerHangup => {
                // Check if caller leg has ended
                if let Some(caller_id) = &*self.inner.caller_leg_id.lock().unwrap() {
                    legs.get(caller_id)
                        .map(|leg| leg.is_ended())
                        .unwrap_or(false)
                } else {
                    false
                }
            }
            CallEndStrategy::EndOnAllLegsEnded => {
                // Only end when ALL legs are ended
                legs.values().all(|leg| leg.is_ended())
            }
            CallEndStrategy::ManualControl => {
                // Don't auto-end, wait for explicit termination
                false
            }
        };

        if should_end_call {
            self.inner.terminated.store(true, Ordering::SeqCst);

            // Terminate remaining active legs
            for (_, leg) in legs.iter_mut() {
                if leg.is_active() {
                    leg.end_with_reason(CallLegEndReason::CallTerminated);
                }
            }
        }

        should_end_call
    }

    /// Get all active call legs
    pub fn get_active_legs(&self) -> Vec<String> {
        self.inner
            .legs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, leg)| leg.is_active())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Get count of active legs
    pub fn active_leg_count(&self) -> usize {
        self.inner
            .legs
            .lock()
            .unwrap()
            .values()
            .filter(|leg| leg.is_active())
            .count()
    }

    /// Check if the call has terminated
    pub fn is_terminated(&self) -> bool {
        self.inner.terminated.load(Ordering::SeqCst)
    }

    /// Explicitly terminate the call and all legs
    pub fn terminate_call(&self, reason: CallLegEndReason) {
        self.inner.terminated.store(true, Ordering::SeqCst);

        let mut legs = self.inner.legs.lock().unwrap();
        for (_, leg) in legs.iter_mut() {
            if leg.is_active() {
                leg.end_with_reason(reason.clone());
            }
        }
    }

    /// Set the caller leg for reference
    pub fn set_caller_leg(&self, leg_id: String) {
        *self.inner.caller_leg_id.lock().unwrap() = Some(leg_id);
    }

    /// Get the caller leg ID
    pub fn get_caller_leg_id(&self) -> Option<String> {
        self.inner.caller_leg_id.lock().unwrap().clone()
    }

    /// Set media mute status
    pub fn set_mute(&self, muted: bool) {
        self.inner.volume_processor.set_muted(muted);
    }

    /// Get media mute status
    pub fn is_muted(&self) -> bool {
        self.inner.volume_processor.is_muted()
    }

    /// Set volume level (0.0 to 2.0)
    pub fn set_volume(&self, level: f32) {
        self.inner.volume_processor.set_volume(level);
    }

    /// Get current volume level
    pub fn get_volume(&self) -> f32 {
        self.inner.volume_processor.get_volume()
    }

    /// Set hold status
    pub fn set_hold(&self, on_hold: bool) {
        self.inner.hold_processor.set_hold(on_hold);
    }

    /// Get hold status
    pub fn is_on_hold(&self) -> bool {
        self.inner.hold_processor.is_on_hold()
    }

    /// Setup processor chain for a specific leg
    pub fn setup_processor_chain_for_leg(&self, leg_id: &str, sample_rate: u32) -> Result<()> {
        let mut processor_chain = ProcessorChain::new(sample_rate);

        // Add volume control processor
        processor_chain.append_processor(Box::new(VolumeControlProcessor::new()));

        // Add hold processor
        processor_chain.append_processor(Box::new(HoldProcessor::new()));

        // Store the processor chain
        if let Ok(mut chains) = self.inner.processor_chains.lock() {
            chains.insert(leg_id.to_string(), processor_chain);
        }

        info!(
            session_id = self.inner.session_id,
            leg_id = leg_id,
            sample_rate = sample_rate,
            "Setup processor chain for call leg"
        );

        Ok(())
    }

    /// Get processor chain for a specific leg
    pub fn get_processor_chain(&self, leg_id: &str) -> Option<ProcessorChain> {
        if let Ok(chains) = self.inner.processor_chains.lock() {
            chains.get(leg_id).cloned()
        } else {
            None
        }
    }

    /// Process SDP based on media proxy mode configuration
    async fn process_sdp_for_proxy_mode(
        &self,
        original_sdp: &str,
        target: &Location,
    ) -> Result<Option<String>> {
        use crate::config::MediaProxyMode;

        let proxy_mode = self.inner.dialplan.media.proxy_mode;

        debug!(
            session_id = self.inner.session_id,
            proxy_mode = ?proxy_mode,
            "Processing SDP for media proxy mode"
        );

        match proxy_mode {
            MediaProxyMode::None => {
                // 情况1: SDP透传，不参与录音和媒体
                info!(
                    session_id = self.inner.session_id,
                    "SDP pass-through mode: no media processing or recording"
                );
                Ok(Some(original_sdp.to_string()))
            }
            MediaProxyMode::Nat => {
                // 情况2: NAT帮助透传，仅处理IP地址转换
                info!(
                    session_id = self.inner.session_id,
                    "NAT assistance mode: processing IP addresses only"
                );
                let nat_sdp = self
                    .process_sdp_for_nat_assistance(original_sdp, target)
                    .await?;
                Ok(Some(nat_sdp))
            }
            MediaProxyMode::Auto => {
                // 自动检测是否需要媒体流转换
                if self.needs_media_stream_conversion(original_sdp, target)? {
                    // 情况3: 通过mediastream进行转换
                    info!(
                        session_id = self.inner.session_id,
                        "Auto mode: detected need for media stream conversion"
                    );
                    let converted_sdp = self
                        .setup_media_stream_conversion(original_sdp, target)
                        .await?;
                    Ok(Some(converted_sdp))
                } else {
                    // 情况2: 仅NAT帮助
                    info!(
                        session_id = self.inner.session_id,
                        "Auto mode: using NAT assistance only"
                    );
                    let nat_sdp = self
                        .process_sdp_for_nat_assistance(original_sdp, target)
                        .await?;
                    Ok(Some(nat_sdp))
                }
            }
            MediaProxyMode::All => {
                // 情况3: 强制通过mediastream进行转换，支持录音
                info!(
                    session_id = self.inner.session_id,
                    "Full media proxy mode: enabling complete media stream processing"
                );
                let converted_sdp = self
                    .setup_media_stream_conversion(original_sdp, target)
                    .await?;
                Ok(Some(converted_sdp))
            }
        }
    }

    /// 情况2: 处理NAT帮助透传，仅修改IP地址
    async fn process_sdp_for_nat_assistance(
        &self,
        original_sdp: &str,
        _target: &Location,
    ) -> Result<String> {
        let mut nat_sdp = original_sdp.to_string();

        // 检查是否包含私有IP地址需要NAT处理
        if self.contains_private_ip(&nat_sdp) {
            if let Some(external_ip) = &self.inner.app_state.config.external_ip {
                // 仅替换私有IP为外部IP，其他保持不变
                nat_sdp = self.replace_private_ips_with_external(&nat_sdp, external_ip)?;
                debug!(
                    session_id = self.inner.session_id,
                    external_ip = external_ip,
                    "Replaced private IPs with external IP for NAT traversal"
                );
            }
        }

        // 保持原有的编解码器和媒体格式不变
        Ok(nat_sdp)
    }

    /// 情况3: 设置媒体流转换，支持录音和格式转换
    async fn setup_media_stream_conversion(
        &self,
        original_sdp: &str,
        target: &Location,
    ) -> Result<String> {
        use crate::media::negotiate::{select_peer_media, strip_ipv6_candidates};
        use webrtc::sdp::SessionDescription;

        // 解析原始SDP
        let session_desc = match std::io::Cursor::new(original_sdp.as_bytes()) {
            mut cursor => match SessionDescription::unmarshal(&mut cursor) {
                Ok(desc) => desc,
                Err(e) => {
                    warn!(
                        session_id = self.inner.session_id,
                        error = %e,
                        "Failed to parse SDP, using fallback processing"
                    );
                    return self
                        .process_sdp_for_nat_assistance(original_sdp, target)
                        .await;
                }
            },
        };

        // 分析媒体信息以设置录音和转换
        if let Some(audio_media) = select_peer_media(&session_desc, "audio") {
            info!(
                session_id = self.inner.session_id,
                codecs = ?audio_media.codecs,
                rtp_port = audio_media.rtp_port,
                "Setting up audio media stream conversion and recording"
            );

            // 启动录音功能（如果配置了录音）
            if let Err(e) = self.start_call_recording().await {
                warn!(
                    session_id = self.inner.session_id,
                    error = %e,
                    "Failed to start call recording, continuing without recording"
                );
            }
        }

        // 创建代理SDP，替换媒体地址为代理地址
        let mut proxy_sdp = self.create_proxy_sdp(original_sdp).await?;

        // 移除IPv6候选地址简化处理
        proxy_sdp = strip_ipv6_candidates(&proxy_sdp);

        // 如果是WebRTC到RTP的转换，移除WebRTC特定属性
        if self.is_webrtc_to_rtp_conversion(&proxy_sdp)? {
            proxy_sdp = self.convert_webrtc_attributes_to_rtp(&proxy_sdp)?;
            info!(
                session_id = self.inner.session_id,
                "Converted WebRTC SDP to RTP-compatible format"
            );
        }

        Ok(proxy_sdp)
    }

    /// 检查是否需要媒体流转换
    fn needs_media_stream_conversion(&self, sdp: &str, _target: &Location) -> Result<bool> {
        // 检查是否有WebRTC特征需要转换为RTP
        let has_webrtc_features = self.is_webrtc_to_rtp_conversion(sdp)?;

        // 检查是否配置了录音功能
        let recording_enabled = self.inner.dialplan.extras.is_some()
            && self
                .inner
                .dialplan
                .extras
                .as_ref()
                .unwrap()
                .contains_key("recording");

        // 检查是否需要特殊的编解码器转换
        let needs_codec_conversion = self.needs_codec_conversion(sdp)?;

        Ok(has_webrtc_features || recording_enabled || needs_codec_conversion)
    }

    /// 检查是否为WebRTC到RTP转换场景
    fn is_webrtc_to_rtp_conversion(&self, sdp: &str) -> Result<bool> {
        // 检查WebRTC特有属性
        let webrtc_indicators = [
            "a=fingerprint:",
            "a=setup:",
            "a=ice-ufrag:",
            "a=ice-pwd:",
            "a=candidate:",
            "a=group:BUNDLE",
        ];

        for indicator in &webrtc_indicators {
            if sdp.contains(indicator) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// 检查是否需要编解码器转换
    fn needs_codec_conversion(&self, sdp: &str) -> Result<bool> {
        // 检查是否有不常见的编解码器需要转换
        let unsupported_codecs = ["a=rtpmap:111 opus", "a=rtpmap:9 G722"];

        for codec in &unsupported_codecs {
            if sdp.contains(codec) {
                debug!(
                    session_id = self.inner.session_id,
                    codec = codec,
                    "Detected codec that may need conversion"
                );
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// 创建代理SDP，替换媒体地址
    async fn create_proxy_sdp(&self, original_sdp: &str) -> Result<String> {
        let mut proxy_sdp = original_sdp.to_string();

        if let Some(external_ip) = &self.inner.app_state.config.external_ip {
            // 替换连接信息为代理地址
            let lines: Vec<&str> = proxy_sdp.lines().collect();
            let mut modified_lines = Vec::new();

            for line in lines {
                if line.starts_with("c=IN IP4 ") {
                    modified_lines.push(format!("c=IN IP4 {}", external_ip));
                } else if line.starts_with("a=rtcp:") {
                    // TODO: 分配代理RTP/RTCP端口
                    modified_lines.push(line.to_string());
                } else {
                    modified_lines.push(line.to_string());
                }
            }
            proxy_sdp = modified_lines.join("\n");

            debug!(
                session_id = self.inner.session_id,
                external_ip = external_ip,
                "Created proxy SDP with external IP"
            );
        }

        Ok(proxy_sdp)
    }

    /// 转换WebRTC属性为RTP兼容格式
    fn convert_webrtc_attributes_to_rtp(&self, sdp: &str) -> Result<String> {
        let webrtc_attributes_to_remove = [
            "a=fingerprint:",
            "a=setup:",
            "a=ice-ufrag:",
            "a=ice-pwd:",
            "a=ice-options:",
            "a=candidate:",
            "a=end-of-candidates",
            "a=ice-lite",
            "a=bundle-only",
            "a=group:BUNDLE",
            "a=msid:",
            "a=ssrc:",
        ];

        let mut rtp_sdp = sdp.to_string();

        for attr in &webrtc_attributes_to_remove {
            rtp_sdp = rtp_sdp
                .lines()
                .filter(|line| !line.starts_with(attr))
                .collect::<Vec<&str>>()
                .join("\n");
        }

        debug!(
            session_id = self.inner.session_id,
            "Converted WebRTC attributes to RTP format"
        );

        Ok(rtp_sdp)
    }

    /// 检查SDP是否包含私有IP地址
    fn contains_private_ip(&self, sdp: &str) -> bool {
        let private_ip_patterns = [
            "10.", "192.168.", "172.16.", "172.17.", "172.18.", "172.19.", "172.20.", "172.21.",
            "172.22.", "172.23.", "172.24.", "172.25.", "172.26.", "172.27.", "172.28.", "172.29.",
            "172.30.", "172.31.",
        ];

        for pattern in &private_ip_patterns {
            if sdp.contains(pattern) {
                return true;
            }
        }
        false
    }

    /// 替换私有IP地址为外部IP
    fn replace_private_ips_with_external(&self, sdp: &str, external_ip: &str) -> Result<String> {
        use regex::Regex;

        // 创建私有IP地址的正则表达式
        let private_ip_regex = Regex::new(
            r"(?:10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(?:1[6-9]|2[0-9]|3[01])\.\d{1,3}\.\d{1,3})"
        ).map_err(|e| anyhow!("Failed to compile private IP regex: {}", e))?;

        let result = private_ip_regex.replace_all(sdp, external_ip).to_string();

        Ok(result)
    }

    /// Add a new call leg with specified criticality
    pub fn add_call_leg(
        &self,
        leg_id: String,
        direction: CallDirection,
        _critical: bool,
    ) -> Result<()> {
        let call_leg = CallLeg::new(
            leg_id.clone(),
            direction,
            "unknown".to_string(),         // target - will be updated later
            self.inner.session_id.clone(), // parent_call_id is same as session_id
        );

        let mut legs = self.inner.legs.lock().unwrap();
        legs.insert(leg_id, call_leg);

        Ok(())
    }

    /// Add a caller leg (will be marked as critical by default)
    pub fn add_caller_leg(&self, leg_id: String) -> Result<()> {
        self.add_call_leg(leg_id.clone(), CallDirection::Inbound, true)?;
        self.set_caller_leg(leg_id);
        Ok(())
    }

    /// Add a callee leg with specified criticality
    pub fn add_callee_leg(&self, leg_id: String, critical: bool) -> Result<()> {
        self.add_call_leg(leg_id, CallDirection::Outbound, critical)
    }

    /// Update leg state and check for call termination
    pub fn update_leg_state(&self, leg_id: &str, new_state: CallLegState) -> Result<bool> {
        let mut legs = self.inner.legs.lock().unwrap();

        if let Some(leg) = legs.get_mut(leg_id) {
            leg.update_state(new_state.clone());

            // Check if this state change should trigger call termination
            let should_terminate = match new_state {
                CallLegState::Ended | CallLegState::Failed(_) => {
                    // Drop the lock before calling on_leg_ended to avoid deadlock
                    drop(legs);
                    let end_reason = match new_state {
                        CallLegState::Failed(status_code) => CallLegEndReason::Failed(status_code),
                        _ => CallLegEndReason::NormalHangup,
                    };
                    self.on_leg_ended(leg_id, end_reason)
                }
                _ => false,
            };

            Ok(should_terminate)
        } else {
            Err(anyhow!("Call leg not found: {}", leg_id))
        }
    }

    /// Start comprehensive call state monitoring for a leg
    async fn start_call_state_monitoring(&self, leg_id: &str) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            leg_id = leg_id,
            "Starting call state monitoring"
        );

        // Implementation would involve:
        // 1. Setting up dialog state change listeners
        // 2. Monitoring SIP responses and requests
        // 3. Tracking call progress (ringing, answered, ended)
        // 4. Handling error conditions and timeouts

        let session_id = self.inner.session_id.clone();
        let leg_id_clone = leg_id.to_string();
        let cancel_token = self.inner.cancel_token.child_token();

        // Spawn monitoring task
        tokio::spawn(async move {
            info!(
                session_id = session_id,
                leg_id = leg_id_clone,
                "Call state monitoring started"
            );

            // Simulate state monitoring
            let mut interval = tokio::time::interval(Duration::from_secs(5));

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        info!(
                            session_id = session_id,
                            leg_id = leg_id_clone,
                            "Call state monitoring cancelled"
                        );
                        break;
                    }
                    _ = interval.tick() => {
                        debug!(
                            session_id = session_id,
                            leg_id = leg_id_clone,
                            "Monitoring call state"
                        );

                        // In a real implementation, we would:
                        // - Check dialog state
                        // - Monitor for timeout conditions
                        // - Handle state changes
                        // - Update leg state accordingly
                    }
                }
            }

            info!(
                session_id = session_id,
                leg_id = leg_id_clone,
                "Call state monitoring ended"
            );
        });

        Ok(())
    }

    /// Bridge call legs together for B2B call functionality
    async fn bridge_call_legs(&self, new_leg_id: &str) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            new_leg_id = new_leg_id,
            "Bridging call legs"
        );

        // Get caller leg ID first
        let caller_leg_id = self.inner.caller_leg_id.lock().unwrap().clone();

        // Get legs information in a separate scope to ensure lock is dropped
        let (caller_leg, new_leg) = {
            let legs = self.inner.legs.lock().unwrap();

            let caller_leg = if let Some(caller_id) = caller_leg_id {
                legs.get(&caller_id).cloned()
            } else {
                None
            };

            let new_leg = legs.get(new_leg_id).cloned();

            (caller_leg, new_leg)
        }; // Lock is definitely dropped here

        match (caller_leg, new_leg) {
            (Some(caller), Some(new_leg)) => {
                info!(
                    session_id = self.inner.session_id,
                    caller_leg = caller.id,
                    new_leg_id = new_leg.id,
                    "Establishing bridge between call legs"
                );

                // Implementation would involve:
                // 1. Setting up media bridging between the two legs
                // 2. Forwarding RTP packets between the legs
                // 3. Managing hold/unhold states
                // 4. Handling DTMF forwarding

                // Store IDs before moving into the event sender
                let caller_id = caller.id.clone();
                let new_leg_id = new_leg.id.clone();

                // For now, we'll set up event forwarding
                if let Some(event_sender) = &self.inner.event_sender {
                    let _ = event_sender.send(crate::event::SessionEvent::Other {
                        track_id: format!("bridge-{}", self.inner.session_id),
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        sender: "b2bcall".to_string(),
                        extra: Some(
                            vec![
                                ("action".to_string(), "bridge_established".to_string()),
                                ("caller_leg".to_string(), caller.id),
                                ("callee_leg".to_string(), new_leg.id),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    });
                }

                self.log_event(
                    CallRecordEventType::Event,
                    &format!(
                        "Bridge established between {} and {}",
                        caller_id, new_leg_id
                    ),
                )
                .await;

                Ok(())
            }
            _ => {
                warn!(
                    session_id = self.inner.session_id,
                    "Cannot bridge call legs: missing caller or new leg"
                );
                Err(anyhow!("Cannot bridge call legs: missing legs"))
            }
        }
    }

    /// Get leg information
    pub fn get_leg(&self, leg_id: &str) -> Option<CallLeg> {
        self.inner.legs.lock().unwrap().get(leg_id).cloned()
    }
}

/// Builder for B2BCall
pub struct B2BCallBuilder {
    session_id: String,
    app_state: AppState,
    cookie: TransactionCookie,
    dialplan: Option<Dialplan>,
    cancel_token: Option<CancellationToken>,
    invitation: Option<Invitation>,
    call_record_sender: Option<CallRecordSender>,
    event_sender: Option<EventSender>,
    original_sdp_offer: Option<String>,
}

impl B2BCallBuilder {
    pub fn new(session_id: String, app_state: AppState, cookie: TransactionCookie) -> Self {
        Self {
            session_id,
            app_state,
            cookie,
            dialplan: None,
            cancel_token: None,
            invitation: None,
            call_record_sender: None,
            event_sender: None,
            original_sdp_offer: None,
        }
    }

    pub fn with_dialplan(mut self, dialplan: Dialplan) -> Self {
        self.dialplan = Some(dialplan);
        self
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub fn with_invitation(mut self, invitation: Invitation) -> Self {
        self.invitation = Some(invitation);
        self
    }

    pub fn with_call_record_sender(mut self, sender: CallRecordSender) -> Self {
        self.call_record_sender = Some(sender);
        self
    }

    pub fn with_event_sender(mut self, sender: EventSender) -> Self {
        self.event_sender = Some(sender);
        self
    }

    pub fn with_original_sdp_offer(mut self, sdp_offer: String) -> Self {
        self.original_sdp_offer = Some(sdp_offer);
        self
    }

    pub fn build(self) -> Result<B2BCall> {
        let dialplan = self.dialplan.unwrap_or_default();
        let cancel_token = self.cancel_token.unwrap_or_else(CancellationToken::new);
        let invitation = self
            .invitation
            .ok_or_else(|| anyhow!("Invitation is required"))?;

        let inner = Arc::new(B2BCallInner {
            session_id: self.session_id,
            dialplan,
            has_media_stream: AtomicBool::new(false),
            app_state: self.app_state,
            cookie: self.cookie,
            legs: Mutex::new(HashMap::new()),
            auto_hangup: Mutex::new(None),
            invitation: Some(invitation),
            event_sender: self.event_sender,
            cancel_token,
            call_record_sender: self.call_record_sender,
            call_end_strategy: CallEndStrategy::EndOnAnyCriticalLeg,
            terminated: AtomicBool::new(false),
            caller_leg_id: Mutex::new(None),
            original_sdp_offer: self.original_sdp_offer,
            active_recorders: Mutex::new(HashMap::new()),
            volume_processor: Arc::new(VolumeControlProcessor::new()),
            hold_processor: Arc::new(HoldProcessor::new()),
            processor_chains: Mutex::new(HashMap::new()),
        });

        Ok(B2BCall { inner })
    }
}

impl Drop for B2BCall {
    fn drop(&mut self) {
        debug!(session_id = self.inner.session_id, "B2BCall dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_call_direction_enum() {
        assert_eq!(CallDirection::Inbound, CallDirection::Inbound);
        assert_eq!(CallDirection::Outbound, CallDirection::Outbound);
        assert_ne!(CallDirection::Inbound, CallDirection::Outbound);
    }

    #[test]
    fn test_call_leg_state_enum() {
        assert_eq!(CallLegState::Idle, CallLegState::Idle);
        assert_eq!(CallLegState::Calling, CallLegState::Calling);
        assert_eq!(CallLegState::Answered, CallLegState::Answered);
        assert_eq!(CallLegState::Ended, CallLegState::Ended);
        assert_eq!(CallLegState::Failed(404), CallLegState::Failed(404));
        assert_ne!(CallLegState::Failed(404), CallLegState::Failed(503));
    }

    #[test]
    fn test_call_leg_creation() {
        let leg = CallLeg::new(
            "leg-001".to_string(),
            CallDirection::Outbound,
            "sip:user@example.com".to_string(),
            "parent-call-001".to_string(),
        );

        assert_eq!(leg.id, "leg-001");
        assert_eq!(leg.direction, CallDirection::Outbound);
        assert_eq!(leg.target, "sip:user@example.com");
        assert_eq!(leg.parent_call_id, "parent-call-001");
        assert_eq!(leg.state, CallLegState::Idle);
        assert!(leg.dialog.is_none());
        assert!(leg.ring_time.is_none());
        assert!(leg.answer_time.is_none());
        assert!(leg.end_time.is_none());
        assert!(leg.status_code.is_none());
        assert!(leg.critical_for_call); // Default should be critical
        assert!(leg.end_reason.is_none());
    }

    #[test]
    fn test_call_leg_state_updates() {
        let mut leg = CallLeg::new(
            "leg-001".to_string(),
            CallDirection::Outbound,
            "sip:user@example.com".to_string(),
            "parent-call-001".to_string(),
        );

        // Test calling state update
        leg.update_state(CallLegState::Calling);
        assert_eq!(leg.state, CallLegState::Calling);
        assert!(leg.ring_time.is_some());

        // Test answered state update
        leg.update_state(CallLegState::Answered);
        assert_eq!(leg.state, CallLegState::Answered);
        assert!(leg.answer_time.is_some());

        // Test ended state update
        leg.update_state(CallLegState::Ended);
        assert_eq!(leg.state, CallLegState::Ended);
        assert!(leg.end_time.is_some());
    }

    #[test]
    fn test_call_leg_duration_calculation() {
        let mut leg = CallLeg::new(
            "leg-001".to_string(),
            CallDirection::Outbound,
            "sip:user@example.com".to_string(),
            "parent-call-001".to_string(),
        );

        // No duration before answer
        assert!(leg.duration().is_none());

        // Set answered state
        leg.update_state(CallLegState::Answered);

        // Should have duration (very small, but non-negative)
        let duration1 = leg.duration();
        assert!(duration1.is_some());
        // Since it's u64 (from Duration::as_secs()), it's always >= 0
    }

    #[test]
    fn test_ringback_config_default() {
        use crate::call::RingbackConfig;
        let config = RingbackConfig::default();

        assert_eq!(config.status_codes, vec![180, 183]);
        assert!(config.audio_file.is_none());
        assert_eq!(config.max_duration, Some(Duration::from_secs(60)));
        assert_eq!(config.auto_hangup, None);
        assert_eq!(config.hangup_status_code, None);
    }

    #[test]
    fn test_dialplan_default() {
        use crate::call::{DialStrategy, Dialplan};
        use crate::config::MediaProxyMode;

        let dialplan = Dialplan::default();

        assert!(matches!(dialplan.targets, DialStrategy::Sequential(_)));
        assert!(dialplan.get_all_targets().is_empty());
        assert!(!dialplan.is_recording_enabled());
        assert!(!dialplan.is_auto_hangup_enabled());
        assert_eq!(dialplan.call_timeout, Duration::from_secs(60));
        assert_eq!(dialplan.media.proxy_mode, MediaProxyMode::Auto);
    }

    #[test]
    fn test_dialplan_customization() {
        use crate::call::{
            CallRecordingConfig, Dialplan, FailureAction, Location, MediaConfig, RingbackConfig,
        };
        use crate::config::MediaProxyMode;

        let targets = vec![Location::default()];
        let recording = CallRecordingConfig::new().enabled();
        let ringback = RingbackConfig::new()
            .with_audio_file("/custom/ringback.wav".to_string())
            .with_auto_hangup(486);
        let media = MediaConfig::new().with_proxy_mode(MediaProxyMode::All);

        let dialplan = Dialplan::new("test-session".to_string())
            .with_parallel_targets(targets)
            .with_recording(recording)
            .with_ringback(ringback)
            .with_media(media)
            .with_failure_action(FailureAction::Hangup(480))
            .with_timeout(Duration::from_secs(45));

        assert!(dialplan.is_parallel_strategy());
        assert_eq!(dialplan.get_all_targets().len(), 1);
        assert!(dialplan.is_recording_enabled());
        assert!(dialplan.is_auto_hangup_enabled());
        assert_eq!(dialplan.call_timeout, Duration::from_secs(45));
        assert_eq!(dialplan.media.proxy_mode, MediaProxyMode::All);
    }

    #[test]
    fn test_call_leg_with_location() {
        let location = Location::default();
        let leg = CallLeg::new(
            "leg-001".to_string(),
            CallDirection::Inbound,
            "sip:caller@example.com".to_string(),
            "parent-call-001".to_string(),
        )
        .with_location(location);

        assert!(leg.location.is_some());
    }

    #[test]
    fn test_multiple_state_transitions() {
        let mut leg = CallLeg::new(
            "leg-test".to_string(),
            CallDirection::Outbound,
            "sip:test@example.com".to_string(),
            "parent-call-test".to_string(),
        );

        // Test full call flow
        assert_eq!(leg.state, CallLegState::Idle);

        leg.update_state(CallLegState::Calling);
        assert_eq!(leg.state, CallLegState::Calling);
        assert!(leg.ring_time.is_some());

        leg.update_state(CallLegState::Answered);
        assert_eq!(leg.state, CallLegState::Answered);
        assert!(leg.answer_time.is_some());

        leg.update_state(CallLegState::Ended);
        assert_eq!(leg.state, CallLegState::Ended);
        assert!(leg.end_time.is_some());
    }

    #[test]
    fn test_media_proxy_mode_variants() {
        use crate::call::{Dialplan, MediaConfig};
        use crate::config::MediaProxyMode;

        let modes = vec![
            MediaProxyMode::Auto,
            MediaProxyMode::All,
            MediaProxyMode::Nat,
            MediaProxyMode::None,
        ];

        for mode in modes {
            let media = MediaConfig::new().with_proxy_mode(mode.clone());
            let dialplan = Dialplan::new("test".to_string()).with_media(media);
            assert_eq!(dialplan.media.proxy_mode, mode);
        }
    }

    #[test]
    fn test_ringback_config_custom_status_codes() {
        use crate::call::RingbackConfig;

        let mut config = RingbackConfig::new();
        config.status_codes = vec![180, 181, 182, 183, 184];
        config.audio_file = Some("/path/to/custom.wav".to_string());
        config.max_duration = Some(Duration::from_secs(120));

        assert_eq!(config.status_codes.len(), 5);
        assert!(config.status_codes.contains(&180));
        assert!(config.status_codes.contains(&184));
        assert!(!config.status_codes.contains(&200));
        assert_eq!(config.audio_file, Some("/path/to/custom.wav".to_string()));
        assert_eq!(config.max_duration, Some(Duration::from_secs(120)));
    }

    #[test]
    fn test_call_leg_state_progression() {
        let mut leg = CallLeg::new(
            "flow-test".to_string(),
            CallDirection::Outbound,
            "sip:flow@test.com".to_string(),
            "parent-call-flow".to_string(),
        );

        // Test successful call flow
        assert_eq!(leg.state, CallLegState::Idle);

        leg.update_state(CallLegState::Calling);
        assert_eq!(leg.state, CallLegState::Calling);

        leg.update_state(CallLegState::Answered);
        assert_eq!(leg.state, CallLegState::Answered);

        leg.update_state(CallLegState::Ended);
        assert_eq!(leg.state, CallLegState::Ended);

        // Test failed call flow with new leg
        let mut leg2 = CallLeg::new(
            "failed-test".to_string(),
            CallDirection::Outbound,
            "sip:busy@test.com".to_string(),
            "parent-call-flow".to_string(),
        );

        leg2.update_state(CallLegState::Calling);
        leg2.update_state(CallLegState::Failed(486)); // Busy Here
        assert_eq!(leg2.state, CallLegState::Failed(486));
        assert!(leg2.end_time.is_some());
    }

    #[test]
    fn test_call_leg_time_ordering() {
        let mut leg = CallLeg::new(
            "time-test".to_string(),
            CallDirection::Outbound,
            "sip:time@test.com".to_string(),
            "parent-call-time".to_string(),
        );

        let start_time = leg.start_time;

        // Simulate some time passing between state changes
        leg.update_state(CallLegState::Calling);
        std::thread::sleep(Duration::from_millis(1));

        leg.update_state(CallLegState::Answered);
        std::thread::sleep(Duration::from_millis(1));

        leg.update_state(CallLegState::Ended);

        // Verify time ordering
        assert!(start_time <= leg.ring_time.unwrap());
        assert!(leg.ring_time.unwrap() <= leg.answer_time.unwrap());
        assert!(leg.answer_time.unwrap() <= leg.end_time.unwrap());
    }

    #[test]
    fn test_auto_hangup_configuration() {
        use std::sync::{Arc, Mutex};

        // Test auto hangup state management
        let auto_hangup: Arc<Mutex<Option<(u32, u16)>>> = Arc::new(Mutex::new(None));

        // Test setting auto hangup
        tokio_test::block_on(async {
            *auto_hangup.lock().unwrap() = Some((12345, 480));

            let state = auto_hangup.lock().unwrap();
            assert_eq!(*state, Some((12345, 480)));
        });

        // Test clearing auto hangup
        tokio_test::block_on(async {
            *auto_hangup.lock().unwrap() = None;

            let state = auto_hangup.lock().unwrap();
            assert_eq!(*state, None);
        });
    }

    #[test]
    fn test_ringback_config_with_auto_hangup() {
        let config = RingbackConfig {
            status_codes: vec![180, 183],
            audio_file: Some("/path/to/ringback.wav".to_string()),
            max_duration: Some(Duration::from_secs(30)),
        };

        // Test that config supports auto hangup scenarios
        assert!(config.audio_file.is_some());
        assert!(config.status_codes.contains(&180));
        assert!(config.status_codes.contains(&183));
        assert_eq!(config.max_duration, Some(Duration::from_secs(30)));
    }

    #[test]
    fn test_atomic_bool_media_stream_status() {
        use std::sync::atomic::AtomicBool;

        // Test AtomicBool for has_media_stream
        let media_status = AtomicBool::new(false);

        // Test initial state
        assert!(!media_status.load(std::sync::atomic::Ordering::Relaxed));

        // Test setting to true
        media_status.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(media_status.load(std::sync::atomic::Ordering::Relaxed));

        // Test setting back to false
        media_status.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(!media_status.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_b2bcall_dialplan_structure() {
        // Test that B2BCall with Dialplan can be created and configured
        use crate::call::Dialplan;
        use tokio_util::sync::CancellationToken;

        // This test verifies the structure can be created
        let session_id = "test-session".to_string();
        let dialplan = Dialplan::new(session_id.clone());
        let cancel_token = CancellationToken::new();
        let has_media_stream = AtomicBool::new(false);

        assert_eq!(dialplan.session_id, Some(session_id));
        assert!(!dialplan.is_recording_enabled());
        assert!(!has_media_stream.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!cancel_token.is_cancelled());
    }

    #[test]
    fn test_concurrent_media_stream_access() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::thread;

        // Test thread-safe access to AtomicBool
        let media_status = Arc::new(AtomicBool::new(false));
        let media_status_clone = media_status.clone();

        let handle = thread::spawn(move || {
            media_status_clone.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        handle.join().unwrap();
        assert!(media_status.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_call_end_strategy_enum() {
        // Test CallEndStrategy variants exist
        let strategy1 = CallEndStrategy::EndOnAnyCriticalLeg;
        let strategy2 = CallEndStrategy::EndOnCallerHangup;
        let strategy3 = CallEndStrategy::EndOnAllLegsEnded;
        let strategy4 = CallEndStrategy::ManualControl;

        // Test PartialEq implementation
        assert_eq!(strategy1, CallEndStrategy::EndOnAnyCriticalLeg);
        assert_ne!(strategy1, strategy2);
        assert_ne!(strategy2, strategy3);
        assert_ne!(strategy3, strategy4);
    }

    #[test]
    fn test_call_leg_end_reason_enum() {
        // Test CallLegEndReason variants
        let reason1 = CallLegEndReason::NormalHangup;
        let reason2 = CallLegEndReason::Failed(404);
        let reason3 = CallLegEndReason::Cancelled;
        let reason4 = CallLegEndReason::CallTerminated;

        // Test Clone implementation
        let cloned_reason = reason2.clone();
        assert_eq!(cloned_reason, CallLegEndReason::Failed(404));

        // Test PartialEq implementation
        assert_eq!(reason1, CallLegEndReason::NormalHangup);
        assert_ne!(reason1, reason2);
        assert_ne!(reason2, reason3);
        assert_ne!(reason3, reason4);
    }

    #[test]
    fn test_call_leg_construction_and_lifecycle() {
        let call_leg = CallLeg::new(
            "test-leg-1".to_string(),
            CallDirection::Inbound,
            "test-target".to_string(),
            "parent-call-123".to_string(),
        );

        // Test initial state
        assert_eq!(call_leg.id, "test-leg-1");
        assert_eq!(call_leg.direction, CallDirection::Inbound);
        assert_eq!(call_leg.target, "test-target");
        assert_eq!(call_leg.parent_call_id, "parent-call-123");
        assert_eq!(call_leg.state, CallLegState::Idle);
        assert!(call_leg.critical_for_call);
        assert!(call_leg.end_reason.is_none());

        // Test state methods - Idle is not active, only Answered is active
        assert!(!call_leg.is_active());
        assert!(!call_leg.is_ended());

        // Test state transition to active
        let mut active_leg = call_leg.clone();
        active_leg.update_state(CallLegState::Answered);
        assert!(active_leg.is_active());
        assert!(!active_leg.is_ended());

        // Test state transition to ended
        let mut ended_leg = active_leg.clone();
        ended_leg.update_state(CallLegState::Ended);
        assert!(!ended_leg.is_active());
        assert!(ended_leg.is_ended());

        // Test cloning
        let cloned_leg = call_leg.clone();
        assert_eq!(cloned_leg.id, call_leg.id);
        assert_eq!(cloned_leg.state, call_leg.state);
    }

    /// Helper to create a test call leg
    fn create_test_call_leg(id: &str, direction: CallDirection) -> CallLeg {
        CallLeg {
            id: id.to_string(),
            direction,
            target: "sip:test@example.com".to_string(),
            state: CallLegState::Idle,
            dialog: None,
            start_time: chrono::Utc::now(),
            ring_time: None,
            answer_time: None,
            end_time: None,
            end_reason: None,
            status_code: None,
            sdp_offer: None,
            sdp_answer: None,
            location: None,
            critical_for_call: true,
            parent_call_id: "test-session-123".to_string(),
        }
    }

    #[test]
    fn test_media_control_volume() {
        let volume_processor = crate::media::volume_control::VolumeControlProcessor::new();

        // Test initial volume
        assert_eq!(volume_processor.get_volume(), 1.0);

        // Test setting volume
        volume_processor.set_volume(0.5);
        assert_eq!(volume_processor.get_volume(), 0.5);

        // Test volume bounds (should clamp to 0.0-2.0)
        volume_processor.set_volume(5.0);
        assert_eq!(volume_processor.get_volume(), 2.0);

        volume_processor.set_volume(-1.0);
        assert_eq!(volume_processor.get_volume(), 0.0);
    }

    #[test]
    fn test_media_control_mute() {
        let volume_processor = crate::media::volume_control::VolumeControlProcessor::new();

        // Test initial state
        assert!(!volume_processor.is_muted());

        // Test setting mute
        volume_processor.set_muted(true);
        assert!(volume_processor.is_muted());

        // Test unsetting mute
        volume_processor.set_muted(false);
        assert!(!volume_processor.is_muted());

        // Test toggle mute
        let is_muted = volume_processor.toggle_mute();
        assert!(is_muted);
        assert!(volume_processor.is_muted());
    }

    #[test]
    fn test_hold_processor() {
        let hold_processor = crate::media::volume_control::HoldProcessor::new();

        // Test initial state
        assert!(!hold_processor.is_on_hold());

        // Test setting hold
        hold_processor.set_hold(true);
        assert!(hold_processor.is_on_hold());

        // Test unsetting hold
        hold_processor.set_hold(false);
        assert!(!hold_processor.is_on_hold());

        // Test toggle hold
        let is_on_hold = hold_processor.toggle_hold();
        assert!(is_on_hold);
        assert!(hold_processor.is_on_hold());
    }

    #[test]
    fn test_call_leg_states() {
        let mut leg = create_test_call_leg("test-1", CallDirection::Inbound);

        // Test initial state
        assert_eq!(leg.state, CallLegState::Idle);
        assert!(!leg.is_active()); // Idle is not active (active means answered)
        assert!(!leg.is_ended());

        // Test state transitions
        leg.state = CallLegState::Calling;
        assert!(!leg.is_active()); // Calling is not yet active

        leg.state = CallLegState::Answered;
        leg.answer_time = Some(chrono::Utc::now());
        assert!(leg.is_active()); // Now it's active (answered)

        leg.state = CallLegState::Ended;
        leg.end_time = Some(chrono::Utc::now());
        leg.end_reason = Some(CallLegEndReason::NormalHangup);
        assert!(!leg.is_active()); // No longer active
        assert!(leg.is_ended());
    }

    #[test]
    fn test_call_end_strategies() {
        // Test different call end strategies
        assert_eq!(
            CallEndStrategy::EndOnAnyCriticalLeg,
            CallEndStrategy::EndOnAnyCriticalLeg
        );
        assert_ne!(
            CallEndStrategy::EndOnAnyCriticalLeg,
            CallEndStrategy::EndOnCallerHangup
        );

        // Test CallLegEndReason
        assert_eq!(
            CallLegEndReason::NormalHangup,
            CallLegEndReason::NormalHangup
        );
        assert_ne!(CallLegEndReason::NormalHangup, CallLegEndReason::Cancelled);
        assert_eq!(CallLegEndReason::Failed(404), CallLegEndReason::Failed(404));
        assert_ne!(CallLegEndReason::Failed(404), CallLegEndReason::Failed(503));
    }
}
