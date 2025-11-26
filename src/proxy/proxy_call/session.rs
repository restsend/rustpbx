#[derive(Clone, Default)]
pub(super) struct IvrTrace {
    reference: Option<String>,
    plan_id: Option<String>,
    exit: Option<String>,
    detail: Option<String>,
}

#[derive(Clone)]
pub(super) struct CallSessionRecordSnapshot {
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub last_error: Option<(StatusCode, Option<String>)>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<CallRecordHangupMessage>,
    pub original_caller: Option<String>,
    pub original_callee: Option<String>,
    pub routed_caller: Option<String>,
    pub routed_callee: Option<String>,
    pub connected_callee: Option<String>,
    pub routed_contact: Option<String>,
    pub routed_destination: Option<String>,
    pub last_queue_name: Option<String>,
    pub ivr_trace: Option<IvrTrace>,
    pub caller_offer: Option<String>,
    pub answer: Option<String>,
    pub callee_dialogs: Vec<DialogId>,
    pub server_dialog_id: DialogId,
}

struct CallSession {
    server_dialog: ServerInviteDialog,
    callee_dialogs: HashSet<DialogId>,
    last_error: Option<(StatusCode, Option<String>)>,
    connected_callee: Option<String>,
    ring_time: Option<Instant>,
    answer_time: Option<Instant>,
    caller_offer: Option<String>,
    callee_offer: Option<String>,
    answer: Option<String>,
    hangup_reason: Option<CallRecordHangupReason>,
    hangup_messages: Vec<SessionHangupMessage>,
    callee_hangup_reason: Option<TerminatedReason>,
    shared: CallSessionShared,
    early_media_sent: bool,
    media_stream: Arc<MediaStream>,
    event_sender: EventSender,
    use_media_proxy: bool,
    media_config: MediaConfig,
    original_caller: Option<String>,
    original_callee: Option<String>,
    routed_caller: Option<String>,
    routed_callee: Option<String>,
    routed_contact: Option<String>,
    routed_destination: Option<String>,
    queue_hold_active: bool,
    queue_passthrough_ringback: bool,
    queue_hold_audio_file: Option<String>,
    queue_hold_loop_cancel: Option<CancellationToken>,
    queue_hold_loop_handle: Option<JoinHandle<()>>,
    last_queue_name: Option<String>,
    ivr_trace: Option<IvrTrace>,
}

impl CallSession {
    const QUEUE_HOLD_TRACK_ID: &'static str = "queue-hold-track";
    const QUEUE_HOLD_PLAY_ID: &'static str = "queue-hold";
    const IVR_PROMPT_TRACK_ID: &'static str = "ivr-prompt-track";
    const CALLEE_TRACK_ID: &'static str = "callee-track";

    fn new(
        cancel_token: CancellationToken,
        session_id: String,
        server_dialog: ServerInviteDialog,
        use_media_proxy: bool,
        event_sender: EventSender,
        media_config: MediaConfig,
        recorder_option: Option<RecorderOption>,
        shared: CallSessionShared,
    ) -> Self {
        let mut builder = MediaStreamBuilder::new(event_sender.clone())
            .with_id(session_id.clone())
            .with_cancel_token(cancel_token);
        if let Some(option) = recorder_option {
            builder = builder.with_recorder_config(option);
        }
        let stream = builder.build();
        let initial = server_dialog.initial_request();
        let original_caller = initial
            .from_header()
            .ok()
            .and_then(|header| header.uri().ok())
            .map(|uri| uri.to_string());
        let original_callee = initial
            .to_header()
            .ok()
            .and_then(|header| header.uri().ok())
            .map(|uri| uri.to_string());
        Self {
            server_dialog,
            callee_dialogs: HashSet::new(),
            last_error: None,
            connected_callee: None,
            ring_time: None,
            answer_time: None,
            caller_offer: None,
            callee_offer: None,
            answer: None,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            callee_hangup_reason: None,
            shared,
            early_media_sent: false,
            media_stream: Arc::new(stream),
            event_sender,
            use_media_proxy,
            media_config,
            original_caller,
            original_callee,
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            queue_hold_active: false,
            queue_passthrough_ringback: false,
            queue_hold_audio_file: None,
            queue_hold_loop_cancel: None,
            queue_hold_loop_handle: None,
            last_queue_name: None,
            ivr_trace: None,
        }
    }

    fn note_attempt_failure(
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
        self.shared.emit_custom_event(ProxyCallEvent::TargetFailed {
            session_id: self.shared.session_id(),
            target,
            code: Some(u16::from(code)),
            reason,
        });
    }

    fn recorded_hangup_messages(&self) -> Vec<CallRecordHangupMessage> {
        self.hangup_messages
            .iter()
            .map(CallRecordHangupMessage::from)
            .collect()
    }

    fn queue_ringback_passthrough(&self) -> bool {
        self.queue_passthrough_ringback
    }

    fn set_queue_ringback_passthrough(&mut self, enabled: bool) {
        self.queue_passthrough_ringback = enabled;
    }

    fn register_active_call(&mut self, handle: CallSessionHandle) {
        self.shared.register_active_call(handle);
    }

    fn set_queue_name(&mut self, name: Option<String>) {
        if let Some(ref queue) = name {
            self.last_queue_name = Some(queue.clone());
        }
        self.shared.set_queue_name(name);
    }

    fn last_queue_name(&self) -> Option<String> {
        self.last_queue_name.clone()
    }

    fn note_ivr_reference(&mut self, reference: Option<String>, plan_id: Option<String>) {
        let mut trace = self.ivr_trace.clone().unwrap_or_default();
        trace.reference = reference;
        trace.plan_id = plan_id;
        self.ivr_trace = Some(trace);
    }

    fn note_ivr_exit(&mut self, exit: &IvrExit) {
        let mut trace = self.ivr_trace.clone().unwrap_or_default();
        let (exit_label, detail) = match exit {
            IvrExit::Completed => ("completed".to_string(), None),
            IvrExit::Transfer(action) => ("transfer".to_string(), Some(action.target.clone())),
            IvrExit::Queue(action) => ("queue".to_string(), Some(action.queue.clone())),
            IvrExit::Webhook(action) => ("webhook".to_string(), Some(action.url.clone())),
            IvrExit::Playback(_) => ("playback".to_string(), None),
            IvrExit::Hangup(action) => ("hangup".to_string(), action.reason.clone()),
        };
        trace.exit = Some(exit_label);
        trace.detail = detail;
        self.ivr_trace = Some(trace);
    }

    fn record_snapshot(&self) -> CallSessionRecordSnapshot {
        CallSessionRecordSnapshot {
            ring_time: self.ring_time,
            answer_time: self.answer_time,
            last_error: self.last_error.clone(),
            hangup_reason: self.hangup_reason.clone(),
            hangup_messages: self.recorded_hangup_messages(),
            original_caller: self.original_caller.clone(),
            original_callee: self.original_callee.clone(),
            routed_caller: self.routed_caller.clone(),
            routed_callee: self.routed_callee.clone(),
            connected_callee: self.connected_callee.clone(),
            routed_contact: self.routed_contact.clone(),
            routed_destination: self.routed_destination.clone(),
            last_queue_name: self.last_queue_name(),
            ivr_trace: self.ivr_trace.clone(),
            caller_offer: self.caller_offer.clone(),
            answer: self.answer.clone(),
            callee_dialogs: self.callee_dialogs.iter().cloned().collect(),
            server_dialog_id: self.server_dialog.id(),
        }
    }

    fn note_invite_details(&mut self, invite: &InviteOption) {
        self.routed_caller = Some(invite.caller.to_string());
        self.routed_callee = Some(invite.callee.to_string());
        self.routed_contact = Some(invite.contact.to_string());
        self.routed_destination = invite.destination.as_ref().map(|addr| addr.to_string());
        self.refresh_active_call_parties();
        let current_target = self
            .routed_callee
            .clone()
            .or_else(|| self.original_callee.clone());
        self.shared.set_current_target(current_target);
    }

    fn refresh_active_call_parties(&self) {
        self.shared
            .update_routed_parties(self.routed_caller.clone(), self.routed_callee.clone());
    }

    fn is_webrtc_sdp(sdp: &str) -> bool {
        sdp.contains("a=fingerprint")
    }

    async fn create_caller_answer_from_offer(&mut self) -> Result<String> {
        if let Some(ref ans) = self.answer {
            return Ok(ans.clone());
        }

        let orig_offer_sdp =
            String::from_utf8_lossy(self.server_dialog.initial_request().body()).to_string();
        let track_id = "caller-track".to_string();
        let config = TrackConfig::default();
        let mut track: Box<dyn Track> = if Self::is_webrtc_sdp(&orig_offer_sdp) {
            let track = WebrtcTrack::new(
                self.media_stream.cancel_token.clone(),
                track_id.clone(),
                config,
                self.media_config.ice_servers.clone(),
            );
            if let Some(ref addr) = self.media_config.external_ip {
                Box::new(track.with_external_ip(addr.clone()))
            } else {
                Box::new(track)
            }
        } else {
            let track = RtpTrackBuilder::new(track_id.clone(), config)
                .with_cancel_token(self.media_stream.cancel_token.clone());
            if let Some(ref addr) = self.media_config.external_ip {
                Box::new(track.with_external_addr(addr.parse()?).build().await?)
            } else {
                Box::new(track.build().await?)
            }
        };

        let processed_answer = if let Some(ref offer) = self.caller_offer {
            match track.handshake(offer.clone(), None).await {
                Ok(processed) => processed,
                Err(e) => {
                    warn!("Failed to handshake caller track (from offer): {}", e);
                    String::new()
                }
            }
        } else {
            String::new()
        };
        self.media_stream.update_track(track, None).await;
        Ok(processed_answer)
    }

    async fn create_callee_track(&mut self, is_webrtc: bool) -> Result<String> {
        let track_id = "callee-track".to_string();
        let config = TrackConfig::default();

        // Parse caller's offer to extract rtp_map for correct payload types
        let mut caller_rtp_map = Vec::new();
        if let Some(ref caller_offer) = self.caller_offer {
            use std::io::Cursor;
            use voice_engine::media::negotiate::select_peer_media;
            use webrtc::sdp::SessionDescription;

            let mut reader = Cursor::new(caller_offer.as_bytes());
            if let Ok(sdp) = SessionDescription::unmarshal(&mut reader) {
                if let Some(peer_media) = select_peer_media(&sdp, "audio") {
                    caller_rtp_map = peer_media.rtp_map;
                }
            }
        }

        let (offer, track) = if !is_webrtc {
            let track = RtpTrackBuilder::new(track_id.clone(), config)
                .with_cancel_token(self.media_stream.cancel_token.clone());
            let rtp_track = if let Some(ref addr) = self.media_config.external_ip {
                track.with_external_addr(addr.parse()?).build().await?
            } else {
                track.build().await?
            };

            // Set rtp_map from caller's offer before generating local_description
            rtp_track.set_rtp_map(caller_rtp_map);

            let offer = rtp_track.local_description()?;
            (offer, Box::new(rtp_track) as Box<dyn Track>)
        } else {
            let mut track = WebrtcTrack::new(
                self.media_stream.cancel_token.clone(),
                track_id.clone(),
                config,
                self.media_config.ice_servers.clone(),
            );
            if let Some(ref addr) = self.media_config.external_ip {
                track = track.with_external_ip(addr.clone());
            }
            (
                track.local_description().await?,
                Box::new(track) as Box<dyn Track>,
            )
        };
        self.media_stream.update_track(track, None).await;
        Ok(offer)
    }

    async fn setup_callee_track(&mut self, callee_answer_sdp: &String) -> Result<()> {
        let track_id = "callee-track".to_string();
        self.media_stream
            .update_remote_description(&track_id, callee_answer_sdp)
            .await
    }

    fn add_callee_dialog(&mut self, dialog_id: DialogId) {
        if self.callee_dialogs.contains(&dialog_id) {
            return;
        }
        self.callee_dialogs.insert(dialog_id);
    }

    async fn start_ringing(&mut self, mut answer: String, proxy_call: &ProxyCall) {
        let call_answered = self.answer_time.is_some();
        if self.early_media_sent && !call_answered {
            debug!("Early media already sent, skipping ringing");
            return;
        }

        self.shared.transition_to_ringing(!answer.is_empty());

        if self.queue_hold_active && !answer.is_empty() {
            if self.queue_passthrough_ringback {
                debug!("Stopping queue hold audio to passthrough remote ringback");
                self.stop_queue_hold().await;
            } else {
                debug!("Queue hold audio active, suppressing remote early-media ringback");
                return;
            }
        }

        if self.ring_time.is_none() {
            self.ring_time = Some(Instant::now());
        }

        if !answer.is_empty() {
            self.early_media_sent = true;
            if self.use_media_proxy {
                self.setup_callee_track(&answer).await.ok();
                match self.create_caller_answer_from_offer().await {
                    Ok(answer_for_caller) => {
                        self.answer = Some(answer_for_caller.clone());
                        answer = answer_for_caller;
                        if !call_answered {
                            if let Some(ref file_name) = proxy_call.dialplan.ringback.audio_file {
                                let mut track = FileTrack::new("callee-track".to_string());
                                track = track.with_path(file_name.clone());
                                self.media_stream.update_track(Box::new(track), None).await;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to create caller answer from offer: {}", e);
                    }
                };
            }
        };

        if call_answered {
            return;
        }

        let status_code = if !answer.is_empty() {
            StatusCode::SessionProgress
        } else {
            StatusCode::Ringing
        };

        let (headers, body) = if !answer.is_empty() {
            let headers = vec![rsip::Header::ContentType("application/sdp".into())];
            (Some(headers), Some(answer.into_bytes()))
        } else {
            (None, None)
        };

        if let Err(e) = self.server_dialog.ringing(headers, body) {
            warn!("Failed to send {} response: {}", status_code, e);
            return;
        }

        if self.early_media_sent && proxy_call.dialplan.ringback.audio_file.is_some() {
            if !proxy_call
                .dialplan
                .ringback
                .wait_for_completion
                .unwrap_or(false)
            {
                return;
            }
            // wait for done
        }
    }

    async fn callee_terminated(&mut self, reason: TerminatedReason) {
        debug!(reason = ?reason, "Callee dialog terminated");
        let hangup_reason = match reason {
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
        };
        self.hangup_reason = Some(hangup_reason.clone());
        self.shared.mark_hangup(hangup_reason);
        self.callee_hangup_reason = Some(reason);
    }

    async fn callee_dialog_request(&mut self, _request: rsip::Request) -> Result<()> {
        Ok(())
    }

    fn has_error(&self) -> bool {
        self.last_error.is_some()
    }

    fn set_error(&mut self, code: StatusCode, reason: Option<String>, target: Option<String>) {
        debug!(code = %code, reason = ?reason, target = ?target, "Call session error set");
        self.last_error = Some((code.clone(), reason.clone()));
        self.hangup_reason = Some(CallRecordHangupReason::Failed);
        self.note_attempt_failure(code.clone(), reason.clone(), target);
        self.shared.note_failure(code, reason);
    }

    fn is_answered(&self) -> bool {
        self.answer_time.is_some()
    }

    async fn play_ringtone(
        &mut self,
        audio_file: &String,
        event_rx: Option<EventReceiver>,
        send_progress: bool,
    ) -> Result<()> {
        let answer_for_caller = self.create_caller_answer_from_offer().await?;
        self.answer = Some(answer_for_caller);
        let hangup_ssrc = rand::random::<u32>();

        let track = FileTrack::new("callee-track".to_string())
            .with_path(audio_file.clone())
            .with_ssrc(hangup_ssrc);
        self.media_stream.update_track(Box::new(track), None).await;

        if send_progress && self.answer_time.is_none() {
            let headers = vec![rsip::Header::ContentType("application/sdp".into())];
            let body = self.answer.clone().map(|sdp| sdp.into_bytes());
            if let Err(err) = self.server_dialog.ringing(Some(headers), body) {
                return Err(anyhow!("Failed to send 183 Session Progress: {}", err));
            }
            self.early_media_sent = true;
        }

        if let Some(mut rx) = event_rx {
            let wait_for_completion = async {
                while let Ok(event) = rx.recv().await {
                    match event {
                        SessionEvent::TrackEnd { ssrc, .. } => {
                            if ssrc == hangup_ssrc {
                                info!("Ringtone playback completed");
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(32), wait_for_completion).await?;
        }
        Ok(())
    }

    async fn accept_call(
        &mut self,
        callee: Option<String>,
        callee_answer: Option<String>,
    ) -> Result<()> {
        // Ensure queue hold tones cease immediately once the call is answered.
        self.stop_queue_hold().await;

        let first_answer = self.answer_time.is_none();
        if let Some(callee_addr) = callee {
            let resolved_callee = self.routed_callee.clone().unwrap_or(callee_addr);
            self.connected_callee = Some(resolved_callee);
        }
        if first_answer {
            self.answer_time = Some(Instant::now());
        }
        info!(
            server_dialog_id = %self.server_dialog.id(),
            use_media_proxy = self.use_media_proxy,
            has_answer = self.answer.is_some(),
            "Call answered"
        );

        if self.answer.is_none() {
            let answer_for_caller = self.create_caller_answer_from_offer().await?;
            self.answer = Some(answer_for_caller);
        }

        if self.use_media_proxy {
            if let Some(answer) = callee_answer.as_ref() {
                self.setup_callee_track(answer).await?;
            }
        } else if let Some(answer) = callee_answer {
            self.answer = Some(answer);
        }

        let headers = if self.answer.is_some() {
            Some(vec![rsip::Header::ContentType("application/sdp".into())])
        } else {
            None
        };

        if let Err(e) = self
            .server_dialog
            .accept(headers, self.answer.clone().map(|sdp| sdp.into_bytes()))
        {
            return Err(anyhow!("Failed to send 200 OK: {}", e));
        }
        self.mark_active_call_answered();
        if first_answer {
            let callee = self
                .connected_callee
                .clone()
                .or_else(|| self.routed_callee.clone());
            self.shared
                .emit_custom_event(ProxyCallEvent::TargetAnswered {
                    session_id: self.shared.session_id(),
                    callee,
                });
        }
        Ok(())
    }

    fn mark_active_call_answered(&self) {
        self.shared.transition_to_answered();
    }

    async fn start_queue_hold(&mut self, hold: QueueHoldConfig) -> Result<()> {
        if self.queue_hold_active {
            return Ok(());
        }

        let audio_file = hold
            .audio_file
            .clone()
            .ok_or_else(|| anyhow!("Queue hold requires an audio file"))?;

        // When the call has not been answered we must still provide the caller with an SDP
        // answer so early media (the hold music) can flow. Otherwise the queued caller would
        // never hear the track we are about to start.
        let need_early_media = self.answer_time.is_none() && !self.early_media_sent;
        if need_early_media || self.answer.is_none() {
            let answer = self.create_caller_answer_from_offer().await?;
            self.answer = Some(answer.clone());
            if need_early_media {
                let headers = vec![rsip::Header::ContentType("application/sdp".into())];
                let body = Some(answer.into_bytes());
                if let Err(err) = self.server_dialog.ringing(Some(headers), body) {
                    return Err(anyhow!(
                        "Failed to send 183 Session Progress for queue hold: {}",
                        err
                    ));
                }
                self.early_media_sent = true;
            }
        }

        Self::play_queue_hold_track(self.media_stream.clone(), audio_file.clone()).await;
        self.queue_hold_active = true;
        self.queue_hold_audio_file = Some(audio_file.clone());

        if hold.loop_playback {
            let cancel = CancellationToken::new();
            let loop_handle = self.spawn_queue_hold_loop(audio_file, cancel.clone());
            self.queue_hold_loop_cancel = Some(cancel);
            self.queue_hold_loop_handle = Some(loop_handle);
        }

        if !self.queue_passthrough_ringback {
            self.media_stream
                .suppress_forwarding(&Self::CALLEE_TRACK_ID.to_string())
                .await;
        }
        Ok(())
    }

    async fn stop_queue_hold(&mut self) {
        if !self.queue_hold_active {
            return;
        }
        if let Some(cancel) = self.queue_hold_loop_cancel.take() {
            cancel.cancel();
        }
        if let Some(handle) = self.queue_hold_loop_handle.take() {
            match tokio::time::timeout(Duration::from_secs(2), handle).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    warn!("queue hold loop task error: {}", err);
                }
                Err(_) => {
                    warn!("queue hold loop task did not stop in time");
                }
            }
        }
        self.media_stream
            .remove_track(&Self::QUEUE_HOLD_TRACK_ID.to_string(), false)
            .await;
        self.media_stream
            .resume_forwarding(&Self::CALLEE_TRACK_ID.to_string())
            .await;
        self.queue_hold_active = false;
        self.queue_hold_audio_file = None;
    }

    fn spawn_queue_hold_loop(
        &self,
        audio_file: String,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let mut event_rx = self.event_sender.subscribe();
        let media_stream = self.media_stream.clone();
        let track_id = Self::QUEUE_HOLD_TRACK_ID.to_string();
        let stream_token = media_stream.cancel_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stream_token.cancelled() => break,
                    _ = cancel.cancelled() => break,
                    event = event_rx.recv() => {
                        match event {
                            Ok(SessionEvent::TrackEnd { track_id: ended_id, .. }) if ended_id == track_id => {
                                if cancel.is_cancelled() {
                                    break;
                                }
                                Self::play_queue_hold_track(media_stream.clone(), audio_file.clone()).await;
                            }
                            Ok(SessionEvent::Error { track_id: errored_id, .. }) if errored_id == track_id => {
                                if cancel.is_cancelled() {
                                    break;
                                }
                                Self::play_queue_hold_track(media_stream.clone(), audio_file.clone()).await;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                            _ => {}
                        }
                    }
                }
            }
        })
    }

    async fn play_queue_hold_track(media_stream: Arc<MediaStream>, audio_file: String) {
        let track = FileTrack::new(Self::QUEUE_HOLD_TRACK_ID.to_string())
            .with_path(audio_file)
            .with_ssrc(rand::random::<u32>())
            .with_cancel_token(media_stream.cancel_token.child_token());
        media_stream
            .update_track(Box::new(track), Some(Self::QUEUE_HOLD_PLAY_ID.to_string()))
            .await;
    }
}

impl Drop for CallSession {
    fn drop(&mut self) {
        self.shared.unregister();
    }
}
