use crate::{
    TrackId,
    call::{DialStrategy, Dialplan, FailureAction, Location, TransactionCookie},
    callrecord::{CallRecord, CallRecordHangupReason, CallRecordSender},
    event::EventSender,
    media::{
        stream::{MediaStream, MediaStreamBuilder},
        track::{Track, TrackConfig, file::FileTrack, rtp::RtpTrackBuilder, webrtc::WebrtcTrack},
    },
    net_tool::is_private_ip,
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use rsip::StatusCode;
use rsipstack::{
    dialog::{
        DialogId,
        dialog::{DialogState, DialogStateReceiver, TerminatedReason},
        dialog_layer::DialogLayer,
        invitation::InviteOption,
        server_dialog::ServerInviteDialog,
    },
    rsip_ext::RsipResponseExt,
    transaction::transaction::Transaction,
};
use std::{
    net::IpAddr,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub struct ProxyCall {
    start_time: Instant,
    session_id: String,
    dialplan: Dialplan,
    #[allow(dead_code)]
    cookie: TransactionCookie,
    dialog_layer: Arc<DialogLayer>,
    cancel_token: CancellationToken,
    call_record_sender: Option<CallRecordSender>,
    event_sender: EventSender,
}

pub struct ProxyCallBuilder {
    cookie: TransactionCookie,
    dialplan: Option<Dialplan>,
    cancel_token: Option<CancellationToken>,
    call_record_sender: Option<CallRecordSender>,
    event_sender: EventSender,
}

impl ProxyCallBuilder {
    pub fn new(cookie: TransactionCookie, event_sender: EventSender) -> Self {
        Self {
            cookie,
            dialplan: None,
            cancel_token: None,
            call_record_sender: None,
            event_sender,
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

    pub fn with_call_record_sender(mut self, sender: CallRecordSender) -> Self {
        self.call_record_sender = Some(sender);
        self
    }

    pub fn with_original_sdp_offer(self, _sdp_offer: String) -> Self {
        self
    }

    pub fn build(self, dialog_layer: Arc<DialogLayer>) -> ProxyCall {
        let dialplan = self.dialplan.unwrap_or_default();
        let cancel_token = self.cancel_token.unwrap_or_default();
        let session_id = dialplan
            .session_id
            .as_ref()
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        ProxyCall {
            start_time: Instant::now(),
            session_id,
            dialplan,
            cookie: self.cookie,
            dialog_layer,
            cancel_token,
            call_record_sender: self.call_record_sender,
            event_sender: self.event_sender,
        }
    }
}

struct CallSession {
    server_dialog: ServerInviteDialog,
    callee_dialogs: Vec<DialogId>,
    last_error: Option<(StatusCode, Option<String>)>,
    connected_callee: Option<String>,
    ring_time: Option<Instant>,
    answer_time: Option<Instant>,
    offer: Option<String>,
    answer: Option<String>,
    hangup_reason: Option<CallRecordHangupReason>,
    callee_hangup_reason: Option<TerminatedReason>,
    early_media_sent: bool,
    media_stream: Arc<MediaStream>, // Always created now
    caller_track_id: Option<TrackId>,
    callee_track_id: Option<TrackId>,
    use_media_proxy: bool, // Flag to control whether to use media stream proxy
}

impl CallSession {
    fn new(
        server_dialog: ServerInviteDialog,
        use_media_proxy: bool,
        event_sender: EventSender,
    ) -> Self {
        // Create MediaStream by default to avoid spawning later
        let media_stream = Arc::new(MediaStreamBuilder::new(event_sender).build());

        Self {
            server_dialog,
            callee_dialogs: Vec::new(),
            last_error: None,
            connected_callee: None,
            ring_time: None,
            answer_time: None,
            offer: None,
            answer: None,
            hangup_reason: None,
            callee_hangup_reason: None,
            early_media_sent: false,
            media_stream,
            caller_track_id: None,
            callee_track_id: None,
            use_media_proxy,
        }
    }

    fn add_callee_dialog(&mut self, dialog_id: DialogId) {
        self.callee_dialogs.push(dialog_id);
    }

    fn is_webrtc_sdp(sdp: &str) -> bool {
        sdp.contains("a=ice-ufrag")
            || sdp.contains("a=ice-pwd")
            || sdp.contains("a=fingerprint")
            || sdp.contains("a=setup:")
    }

    fn determine_callee_track_type(&self, callee_aor: &str) -> bool {
        callee_aor.contains("webrtc")
            || callee_aor.contains("ws://")
            || callee_aor.contains("wss://")
    }

    async fn create_callee_track(&mut self, offer_sdp: &str, callee_aor: &str) -> Result<String> {
        let track_id = format!("callee-{}", uuid::Uuid::new_v4());
        let config = TrackConfig::default();

        let mut track: Box<dyn Track> = if self.determine_callee_track_type(callee_aor) {
            Box::new(WebrtcTrack::new(
                tokio_util::sync::CancellationToken::new(),
                track_id.clone(),
                config,
                None,
            ))
        } else {
            Box::new(
                RtpTrackBuilder::new(track_id.clone(), config)
                    .build()
                    .await?,
            )
        };

        let answer = if !offer_sdp.trim().is_empty() {
            match track.handshake(offer_sdp.to_string(), None).await {
                Ok(answer) => answer,
                Err(e) => {
                    warn!("Failed to handshake callee track: {}", e);
                    offer_sdp.to_string()
                }
            }
        } else {
            String::new()
        };

        self.media_stream.update_track(track, None).await;
        self.callee_track_id = Some(track_id);

        Ok(answer)
    }

    async fn create_caller_track(&mut self, answer_sdp: &str) -> Result<String> {
        let track_id = format!("caller-{}", uuid::Uuid::new_v4());
        let config = TrackConfig::default();

        let mut track: Box<dyn Track> = if Self::is_webrtc_sdp(answer_sdp) {
            Box::new(WebrtcTrack::new(
                tokio_util::sync::CancellationToken::new(),
                track_id.clone(),
                config,
                None,
            ))
        } else {
            Box::new(
                RtpTrackBuilder::new(track_id.clone(), config)
                    .build()
                    .await?,
            )
        };

        let processed_answer = if !answer_sdp.trim().is_empty() {
            match track.handshake(answer_sdp.to_string(), None).await {
                Ok(processed) => processed,
                Err(e) => {
                    warn!("Failed to handshake caller track: {}", e);
                    answer_sdp.to_string()
                }
            }
        } else {
            String::new()
        };

        self.media_stream.update_track(track, None).await;
        self.caller_track_id = Some(track_id);

        Ok(processed_answer)
    }

    async fn start_ringing(&mut self, answer: String, proxy_call: &ProxyCall) {
        if self.early_media_sent {
            debug!("Early media already sent, skipping ringing");
            return;
        }

        if self.ring_time.is_none() {
            self.ring_time = Some(Instant::now());
        }

        // Check if dialplan has a ringtone configured
        let should_play_ringtone = proxy_call.dialplan.ringback.audio_file.is_some();
        let has_media_content = !answer.trim().is_empty() || should_play_ringtone;

        let (headers, body, status_code) = if has_media_content {
            // Play ringtone if configured in dialplan
            if should_play_ringtone && !self.early_media_sent {
                let ringtone_file = proxy_call.dialplan.ringback.audio_file.as_ref().unwrap();
                let wait_for_completion = proxy_call
                    .dialplan
                    .ringback
                    .wait_for_completion
                    .unwrap_or(false);

                info!(
                    session_id = %proxy_call.session_id,
                    ringtone_file = %ringtone_file,
                    wait_for_completion = wait_for_completion,
                    "Playing ringtone from dialplan"
                );

                // Play ringtone with configurable wait behavior
                if let Err(e) = proxy_call
                    .play_audio_file(self, ringtone_file, wait_for_completion)
                    .await
                {
                    warn!(
                        session_id = %proxy_call.session_id,
                        error = %e,
                        "Failed to play ringtone, sending regular ringing"
                    );
                    // Fall back to regular ringing without media
                    (None, None, StatusCode::Ringing)
                } else {
                    // Ringtone was successfully started/completed
                    // play_audio_file already sent 183 Session Progress
                    return;
                }
            } else {
                // 183 Session Progress with SDP for early media
                let headers = vec![
                    rsip::Header::ContentType("application/sdp".into()),
                    rsip::Header::ContentLength((answer.len() as u32).into()),
                ];
                (
                    Some(headers),
                    Some(answer.into_bytes()),
                    StatusCode::SessionProgress,
                )
            }
        } else {
            // 180 Ringing without media
            (None, None, StatusCode::Ringing)
        };

        if let Err(e) = self.server_dialog.ringing(headers, body) {
            warn!("Failed to send {} response: {}", status_code, e);
        } else {
            debug!("Sent {} response to caller", status_code);
            if has_media_content {
                self.early_media_sent = true;
            }
        }
    }

    async fn callee_terminated(&mut self, reason: TerminatedReason) {
        debug!(reason = ?reason, "Callee dialog terminated");
        self.hangup_reason = Some(match reason {
            TerminatedReason::UasBye | TerminatedReason::UacBye => CallRecordHangupReason::ByCallee,
            TerminatedReason::UacCancel => CallRecordHangupReason::Canceled,
            TerminatedReason::Timeout => CallRecordHangupReason::NoAnswer,
            TerminatedReason::UacBusy | TerminatedReason::UasBusy => {
                CallRecordHangupReason::ByCallee
            }
            TerminatedReason::UasDecline => CallRecordHangupReason::Rejected,
            TerminatedReason::ProxyError(_)
            | TerminatedReason::ProxyAuthRequired
            | TerminatedReason::UacOther(_)
            | TerminatedReason::UasOther(_) => CallRecordHangupReason::Failed,
        });
        self.callee_hangup_reason = Some(reason);
    }

    async fn callee_dialog_request(&mut self, _request: rsip::Request) -> Result<()> {
        Ok(())
    }

    fn set_error(&mut self, code: StatusCode, reason: Option<String>) {
        self.last_error = Some((code.clone(), reason.clone()));
        self.hangup_reason = Some(CallRecordHangupReason::Failed);
        debug!(code = %code, reason = ?reason, "Call session error set");
    }

    async fn accept_call(&mut self, callee: String, answer: String) -> Result<()> {
        debug!(callee = %callee, "Call accepted, will not send reject on cleanup");
        self.connected_callee = Some(callee);
        self.answer_time = Some(Instant::now());

        let mut processed_answer = answer.clone();
        if !answer.trim().is_empty() && self.caller_track_id.is_none() {
            match self.create_caller_track(&answer).await {
                Ok(processed) => {
                    processed_answer = processed;
                    debug!("Created caller track for confirmed call");
                }
                Err(e) => {
                    warn!("Failed to create caller track for confirmed call: {}", e);
                }
            }
        }

        self.answer = Some(processed_answer.clone());

        if let Err(e) = self
            .server_dialog
            .accept(None, Some(processed_answer.into_bytes()))
        {
            error!("Failed to send 200 OK response: {}", e);
            return Err(anyhow!("Failed to send 200 OK: {}", e));
        }

        debug!("Sent 200 OK response to caller");
        Ok(())
    }

    async fn cleanup(&mut self, dialog_layer: &Arc<DialogLayer>) {
        debug!("Cleaning up call session");

        if self.answer_time.is_none() {
            if let Some((code, reason)) = &self.last_error {
                if let Err(e) = self
                    .server_dialog
                    .reject(Some(code.clone()), reason.clone())
                {
                    info!(error = %e, "Failed to send rejection to caller");
                }
            }
        }

        if let Err(e) = self.server_dialog.bye().await {
            info!(error = %e, "Failed to send BYE to server dialog");
        }

        for dialog_id in &self.callee_dialogs {
            if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
                if let Err(e) = dialog.hangup().await {
                    info!(dialog_id = %dialog_id, error = %e, "Failed to send BYE to callee dialog");
                }
            }
        }
    }
}

impl ProxyCall {
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    pub fn id(&self) -> &String {
        &self.session_id
    }

    fn is_webrtc_sdp(sdp: &str) -> bool {
        sdp.contains("a=ice-ufrag")
            || sdp.contains("a=ice-pwd")
            || sdp.contains("a=fingerprint")
            || sdp.contains("a=setup:")
    }

    fn needs_media_proxy(&self, offer_sdp: &str, target_type: &str) -> bool {
        use crate::config::MediaProxyMode;

        match self.dialplan.media.proxy_mode {
            MediaProxyMode::None => false,
            MediaProxyMode::All => true,
            MediaProxyMode::Auto => {
                // Auto: proxy when WebRTC to RTP conversion is needed
                Self::is_webrtc_sdp(offer_sdp) && target_type == "rtp"
            }
            MediaProxyMode::Nat => {
                // NAT: only proxy when private IPs are involved
                self.contains_private_ip(offer_sdp)
            }
        }
    }

    fn contains_private_ip(&self, sdp: &str) -> bool {
        for line in sdp.lines() {
            if line.starts_with("c=IN IP4 ") {
                if let Some(ip_str) = line.split_whitespace().nth(2) {
                    if let Ok(ip) = IpAddr::from_str(ip_str) {
                        if is_private_ip(&ip) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    pub async fn process(&self, tx: &mut Transaction) -> Result<()> {
        info!(session_id = %self.session_id, "Starting proxy call processing");

        let (state_tx, state_rx) = mpsc::unbounded_channel();
        let local_contact = self.dialplan.caller_contact.as_ref().map(|c| c.uri.clone());
        let mut server_dialog =
            self.dialog_layer
                .get_or_create_server_invite(tx, state_tx, None, local_contact)?;

        // Extract initial offer SDP
        let initial_request = server_dialog.initial_request();
        let offer_sdp = String::from_utf8_lossy(initial_request.body()).to_string();

        // Determine if media proxy should be used
        let use_media_proxy = self.needs_media_proxy(&offer_sdp, "rtp");
        let mut session = CallSession::new(
            server_dialog.clone(),
            use_media_proxy,
            self.event_sender.clone(),
        );

        session.offer = Some(offer_sdp.clone());

        // Configure media stream if proxy is needed
        if use_media_proxy {
            let stream = MediaStreamBuilder::new(self.event_sender.clone())
                .with_id(self.session_id.clone())
                .with_cancel_token(self.cancel_token.clone())
                .build();
            session.media_stream = Arc::new(stream);
            info!(session_id = %self.session_id, "Configured media stream for proxy call");
        }

        let media_stream = session.media_stream.clone();
        let serve_stream_loop = async move {
            if use_media_proxy {
                media_stream.serve().await.ok();
            } else {
                futures::future::pending::<()>().await;
            }
        };
        let server_dialog_loop = async {
            server_dialog.handle(tx).await.ok();
            futures::future::pending::<()>().await;
        };
        let result = tokio::select! {
            _ = self.cancel_token.cancelled() => {
                session.set_error(StatusCode::RequestTerminated, Some("Cancelled by system".to_string()));
                session.hangup_reason = Some(CallRecordHangupReason::Canceled);
                Err(anyhow!("Call cancelled"))
            }
            _ = serve_stream_loop => {Ok(())},
            _ = server_dialog_loop => {Ok(())},
            r = self.execute_dialplan(&mut session) => r,
            r = self.handle_server_events(state_rx) => r,
        };

        session.cleanup(&self.dialog_layer).await;

        self.record_call(&session).await;
        result
    }

    async fn handle_server_events(
        &self,
        mut state_rx: mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        while let Some(state) = state_rx.recv().await {
            match state {
                DialogState::Terminated(_, reason) => {
                    info!(session_id = %self.session_id, reason = ?reason, "Server dialog terminated");
                    break;
                }
                DialogState::Info(_, _request) => {
                    debug!(session_id = %self.session_id, "Received INFO on server dialog");
                }
                DialogState::Updated(_, _request) => {
                    debug!(session_id = %self.session_id, "Received UPDATE on server dialog");
                }
                DialogState::Notify(_, _request) => {
                    debug!(session_id = %self.session_id, "Received NOTIFY on server dialog");
                }
                other_state => {
                    debug!(
                        session_id = %self.session_id,
                        "Received other state on server dialog: {:?}",
                        std::mem::discriminant(&other_state)
                    );
                }
            }
        }
        Ok(())
    }

    async fn execute_dialplan(&self, session: &mut CallSession) -> Result<()> {
        if self.dialplan.is_empty() {
            session.set_error(
                StatusCode::ServerInternalError,
                Some("No targets in dialplan".to_string()),
            );
            return Err(anyhow!("Dialplan has no targets"));
        }

        info!(
            session_id = %self.session_id,
            strategy = ?self.dialplan.targets,
            "Executing dialplan"
        );

        let dial_result = match &self.dialplan.targets {
            DialStrategy::Sequential(targets) => self.dial_sequential(session, targets).await,
            DialStrategy::Parallel(targets) => self.dial_parallel(session, targets).await,
        };

        match dial_result {
            Ok(_) => {
                info!(session_id = %self.session_id, "Dialplan executed successfully");
                Ok(())
            }
            Err(e) => {
                warn!(session_id = %self.session_id, error = %e, "Dialplan execution failed");
                self.handle_failure(session).await
            }
        }
    }

    async fn dial_sequential(&self, session: &mut CallSession, targets: &[Location]) -> Result<()> {
        info!(
            session_id = %self.session_id,
            target_count = targets.len(),
            "Starting sequential dialing"
        );

        for (index, target) in targets.iter().enumerate() {
            info!(
                session_id = %self.session_id,
                target_index = index,
                target = %target.aor,
                "Trying sequential target"
            );

            match self.try_single_target(session, target).await {
                Ok(_) => {
                    info!(
                        session_id = %self.session_id,
                        target_index = index,
                        "Sequential target succeeded"
                    );
                    return Ok(());
                }
                Err((code, reason)) => {
                    warn!(
                        session_id = %self.session_id,
                        target_index = index,
                        code = %code,
                        reason = ?reason,
                        "Sequential target failed, trying next"
                    );
                    session.set_error(code, reason);
                    continue;
                }
            }
        }

        Err(anyhow!("All sequential targets failed"))
    }

    async fn dial_parallel(&self, session: &mut CallSession, targets: &[Location]) -> Result<()> {
        info!(
            session_id = %self.session_id,
            target_count = targets.len(),
            "Starting parallel dialing (simplified: using first target)"
        );

        if let Some(target) = targets.first() {
            self.try_single_target(session, target)
                .await
                .map_err(|(code, reason)| {
                    session.set_error(code, reason);
                    anyhow!("Parallel dialing failed")
                })
        } else {
            session.set_error(
                StatusCode::ServerInternalError,
                Some("No targets provided".to_string()),
            );
            Err(anyhow!("No targets provided for parallel dialing"))
        }
    }

    async fn try_single_target(
        &self,
        session: &mut CallSession,
        target: &Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let caller = self.dialplan.caller.as_ref().ok_or((
            StatusCode::ServerInternalError,
            Some("No caller specified in dialplan".to_string()),
        ))?;

        let mut offer_sdp = session
            .offer
            .as_ref()
            .map(|s| s.clone())
            .unwrap_or_default();

        if !offer_sdp.is_empty() {
            match session
                .create_callee_track(&offer_sdp, &target.aor.to_string())
                .await
            {
                Ok(processed_offer) => {
                    offer_sdp = processed_offer;
                    debug!(
                        "Created callee track and processed offer for target: {}",
                        target.aor
                    );
                }
                Err(e) => {
                    warn!("Failed to create callee track: {}", e);
                }
            }
        }

        let offer = if offer_sdp.is_empty() {
            None
        } else {
            Some(offer_sdp.as_bytes().to_vec())
        };

        let content_type = if offer.is_some() {
            Some("application/sdp".to_string())
        } else {
            None
        };

        let invite_option = InviteOption {
            callee: target.aor.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination: Some(target.destination.clone()),
            contact: self
                .dialplan
                .caller_contact
                .as_ref()
                .map(|c| c.uri.clone())
                .unwrap_or_else(|| caller.clone()),
            credential: target.credential.clone(),
            headers: target.headers.clone(),
        };

        debug!(
            session_id = %self.session_id,
            callee = %target.aor,
            caller = %caller,
            "Sending INVITE to target"
        );

        self.execute_invite(session, invite_option, &target.aor.to_string())
            .await
    }

    async fn execute_invite(
        &self,
        session: &mut CallSession,
        invite_option: InviteOption,
        callee_aor: &str,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        let dialog_layer = self.dialog_layer.clone();

        tokio::select! {
            _ = self.handle_callee_state(session, &mut state_rx) => {
                Ok(())
            },
            invite_result = dialog_layer.do_invite(invite_option, state_tx) => {
                match invite_result {
                    Ok((_, resp)) => {
                        if let Some(resp) = resp {
                            if resp.status_code.kind() != rsip::StatusCodeKind::Successful {
                                let reason = resp.reason_phrase().clone().map(Into::into);
                                info!(session_id = %self.session_id, code = %resp.status_code, "Invite failed with non-successful response");
                                return Err((resp.status_code, reason));
                            }
                            let answer = String::from_utf8_lossy(resp.body()).to_string();
                            match session.accept_call(callee_aor.to_string(), answer).await {
                                Ok(_) => {
                                    return self.monitor_established_call(session, state_rx).await;
                                }
                                _=>{}
                            }
                        }
                        Err((StatusCode::RequestTerminated, Some("Cancelled by callee".to_string())))
                    }
                    Err(e) => {
                        debug!(session_id = %self.session_id, "Invite failed: {:?}", e);
                        Err(match e {
                            rsipstack::Error::DialogError(reason, _, code) => (code, Some(reason)),
                            _ => (StatusCode::ServerInternalError, Some("Invite failed".to_string())),
                        })
                    }
                }
            }
        }
    }

    async fn monitor_established_call(
        &self,
        session: &mut CallSession,
        mut state_rx: mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<(), (StatusCode, Option<String>)> {
        info!(session_id = %self.session_id, "Monitoring established call");
        while let Some(state) = state_rx.recv().await {
            match state {
                DialogState::Terminated(dialog_id, reason) => {
                    info!(
                        session_id = %self.session_id,
                        %dialog_id,
                        ?reason,
                        "Established call terminated by callee"
                    );
                    session.callee_terminated(reason).await;
                    return Ok(());
                }
                DialogState::Info(_, request) => {
                    session.callee_dialog_request(request).await.ok();
                }
                DialogState::Updated(_, request) => {
                    session.callee_dialog_request(request).await.ok();
                }
                DialogState::Notify(_, request) => {
                    session.callee_dialog_request(request).await.ok();
                }
                other_state => {
                    debug!(
                        session_id = %self.session_id,
                        "Received state in established call: {:?}",
                        std::mem::discriminant(&other_state)
                    );
                }
            }
        }
        debug!(session_id = %self.session_id, "State channel closed for established call");
        Ok(())
    }

    async fn handle_callee_state(
        &self,
        session: &mut CallSession,
        state_rx: &mut DialogStateReceiver,
    ) -> Result<()> {
        while let Some(state) = state_rx.recv().await {
            match state {
                DialogState::Calling(dialog_id) => {
                    session.add_callee_dialog(dialog_id);
                }
                DialogState::Early(dialog_id, response) => {
                    let mut answer = String::from_utf8_lossy(response.body()).to_string();
                    info!(
                        session_id = %self.session_id,
                        dialog_id = %dialog_id,
                        status = ?response.status_code,
                        answer = %answer,
                        "Callee dialog is ringing/early"
                    );

                    if !answer.trim().is_empty() && session.caller_track_id.is_none() {
                        match session.create_caller_track(&answer).await {
                            Ok(processed_answer) => {
                                answer = processed_answer;
                                debug!("Created caller track for early media");
                            }
                            Err(e) => {
                                warn!("Failed to create caller track for early media: {}", e);
                            }
                        }
                    }

                    session.start_ringing(answer, self).await;
                }
                DialogState::Terminated(dialog_id, reason) => {
                    info!(
                        session_id = %self.session_id,
                        dialog_id = %dialog_id,
                        reason = ?reason,
                        "Callee dialog terminated before establishment"
                    );
                    session.callee_terminated(reason).await;
                    break;
                }
                _ => {
                    debug!(
                        session_id = %self.session_id,
                        "Other callee state received"
                    );
                }
            }
        }
        Ok(())
    }

    async fn handle_failure(&self, session: &mut CallSession) -> Result<()> {
        warn!(
            session_id = %self.session_id,
            failure_action = ?self.dialplan.failure_action,
            "Handling dialplan failure"
        );

        match &self.dialplan.failure_action {
            FailureAction::Hangup(code) => {
                let status_code = code
                    .as_ref()
                    .cloned()
                    .unwrap_or(StatusCode::ServiceUnavailable);
                session.set_error(status_code, Some("All targets failed".to_string()));
            }
            FailureAction::PlayThenHangup {
                audio_file,
                status_code,
            } => {
                // First, try to play the audio file to the caller, then hangup
                match self
                    .play_file_then_hangup(session, audio_file, status_code)
                    .await
                {
                    Ok(()) => {
                        // Successfully played file and hungup
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(
                            session_id = %self.session_id,
                            audio_file = %audio_file,
                            error = %e,
                            "Failed to play file, hanging up immediately"
                        );
                        session
                            .set_error(status_code.clone(), Some("All targets failed".to_string()));
                    }
                }
            }
            FailureAction::Transfer(destination) => {
                warn!(
                    session_id = %self.session_id,
                    destination = %destination,
                    "Transfer not implemented, using hangup instead"
                );
                session.set_error(
                    StatusCode::ServiceUnavailable,
                    Some("Transfer not implemented".to_string()),
                );
            }
        }

        Err(anyhow!("Dialplan execution failed"))
    }

    /// Simplified audio file playback with early media (183 Session Progress)
    async fn play_audio_file(
        &self,
        session: &mut CallSession,
        audio_file: &str,
        wait_for_completion: bool,
    ) -> Result<()> {
        if !session.use_media_proxy {
            warn!("Media proxy not enabled, cannot play audio file");
            return Err(anyhow!("Media proxy not enabled for audio playback"));
        }

        // Get answer SDP if not set
        if session.answer.is_none() {
            let offer_sdp = session.offer.as_ref().cloned().unwrap_or_default();
            if !offer_sdp.is_empty() {
                session.answer = Some(self.create_simple_answer_sdp(&offer_sdp));
            }
        }

        let answer_sdp = session
            .answer
            .as_ref()
            .ok_or_else(|| anyhow!("No SDP answer available"))?;

        // Send 183 Session Progress with answer SDP to establish early media
        if let Err(e) = session.server_dialog.ringing(
            Some(vec![
                rsip::Header::ContentType("application/sdp".into()),
                rsip::Header::ContentLength((answer_sdp.len() as u32).into()),
            ]),
            Some(answer_sdp.clone().into_bytes()),
        ) {
            error!(
                "Failed to send 183 Session Progress for audio playback: {}",
                e
            );
            return Err(anyhow!(
                "Failed to send 183 Session Progress for audio playback: {}",
                e
            ));
        }

        session.ring_time = Some(Instant::now());
        session.early_media_sent = true;

        // Create file track for audio playback
        let track_id = format!("audio-playback-{}", uuid::Uuid::new_v4());
        let file_track = FileTrack::new(track_id)
            .with_cancel_token(self.cancel_token.clone())
            .with_path(audio_file.to_string());

        session
            .media_stream
            .update_track(Box::new(file_track), None)
            .await;

        // Wait for track completion if requested
        if wait_for_completion {
            // Simple delay for audio playback - in practice, audio length should be calculated
            tokio::time::sleep(Duration::from_secs(5)).await;
            info!(
                session_id = %self.session_id,
                audio_file = %audio_file,
                "Audio file playback completed (simulated)"
            );
        }

        Ok(())
    }

    /// Create a simple SDP answer from an offer
    fn create_simple_answer_sdp(&self, _offer_sdp: &str) -> String {
        // For simplicity, return a basic RTP answer
        // In a real implementation, you would properly parse and respond to the offer
        format!(
            "v=0\r\no=rustpbx {} {} IN IP4 127.0.0.1\r\ns=RustPBX Audio Session\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0 8\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n",
            chrono::Utc::now().timestamp(),
            chrono::Utc::now().timestamp() + 1
        )
    }

    async fn play_file_then_hangup(
        &self,
        session: &mut CallSession,
        audio_file: &str,
        status_code: &StatusCode,
    ) -> Result<()> {
        info!(
            session_id = %self.session_id,
            audio_file = %audio_file,
            "Playing audio file before hangup"
        );

        // Play the audio file and wait for completion
        let result = self.play_audio_file(session, audio_file, true).await;

        match result {
            Ok(()) => {
                info!(session_id = %self.session_id, "File playback completed successfully, sending reject");
                // Send reject after playback completion
                if let Err(e) = session.server_dialog.reject(
                    Some(status_code.clone()),
                    Some("File playback completed".to_string()),
                ) {
                    warn!("Failed to send reject after playback: {}", e);
                }
                Ok(())
            }
            Err(e) => {
                warn!(session_id = %self.session_id, error = %e, "File playback failed, sending reject");
                // Send reject after playback failure
                if let Err(e) = session.server_dialog.reject(
                    Some(status_code.clone()),
                    Some("File playback failed".to_string()),
                ) {
                    warn!("Failed to send reject after playback failure: {}", e);
                }
                Err(e)
            }
        }
    }

    async fn record_call(&self, session: &CallSession) {
        if let Some(ref sender) = self.call_record_sender {
            let now = Utc::now();
            let start_time = now - chrono::Duration::from_std(self.elapsed()).unwrap_or_default();

            let ring_time = session.ring_time.map(|rt| {
                start_time
                    + chrono::Duration::from_std(rt.duration_since(self.start_time))
                        .unwrap_or_default()
            });

            let answer_time = session.answer_time.map(|at| {
                start_time
                    + chrono::Duration::from_std(at.duration_since(self.start_time))
                        .unwrap_or_default()
            });

            let record = CallRecord {
                call_type: crate::call::ActiveCallType::Sip,
                option: None,
                call_id: self.session_id.clone(),
                start_time,
                ring_time,
                answer_time,
                end_time: now,
                caller: self
                    .dialplan
                    .caller
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
                callee: session
                    .connected_callee
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string()),
                status_code: session
                    .last_error
                    .as_ref()
                    .map(|(code, _)| u16::from(code.clone()))
                    .unwrap_or(200),
                offer: session.offer.clone(),
                answer: session.answer.clone(),
                hangup_reason: session.hangup_reason.clone().or_else(|| {
                    if session.last_error.is_some() {
                        Some(CallRecordHangupReason::Failed)
                    } else if session.answer_time.is_some() {
                        Some(CallRecordHangupReason::BySystem)
                    } else {
                        Some(CallRecordHangupReason::Failed)
                    }
                }),
                recorder: Vec::new(),
                extras: None,
                dump_event_file: None,
                refer_callrecord: None,
            };

            if let Err(e) = sender.send(record) {
                warn!(session_id = %self.session_id, error = %e, "Failed to send call record");
            }
        }
    }
}

impl Drop for ProxyCall {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
