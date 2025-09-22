use crate::{
    call::{DialStrategy, Dialplan, FailureAction, Location, TransactionCookie},
    callrecord::{CallRecord, CallRecordHangupReason, CallRecordSender},
    config::{MediaProxyMode, RouteResult},
    event::{EventReceiver, EventSender, create_event_sender},
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
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

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
    dialplan: Dialplan,
    cancel_token: Option<CancellationToken>,
    call_record_sender: Option<CallRecordSender>,
}

impl ProxyCallBuilder {
    pub fn new(cookie: TransactionCookie, dialplan: Dialplan) -> Self {
        Self {
            cookie,
            dialplan,
            cancel_token: None,
            call_record_sender: None,
        }
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub fn with_call_record_sender(mut self, sender: Option<CallRecordSender>) -> Self {
        self.call_record_sender = sender;
        self
    }

    pub fn build(self, dialog_layer: Arc<DialogLayer>) -> ProxyCall {
        let dialplan = self.dialplan;
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
            event_sender: create_event_sender(),
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
    caller_offer: Option<String>,
    callee_offer: Option<String>,
    answer: Option<String>,
    hangup_reason: Option<CallRecordHangupReason>,
    callee_hangup_reason: Option<TerminatedReason>,
    early_media_sent: bool,
    media_stream: Arc<MediaStream>, // Always created now
    use_media_proxy: bool,          // Flag to control whether to use media stream proxy
}

#[derive(Debug)]
enum ParallelEvent {
    Calling {
        idx: usize,
        dialog_id: DialogId,
    },
    Early {
        sdp: Option<String>,
    },
    Accepted {
        idx: usize,
        dialog_id: DialogId,
        answer: String,
        aor: String,
    },
    Failed {
        code: StatusCode,
        reason: Option<String>,
    },
    Terminated {
        idx: usize,
    },
    Cancelled,
}
impl CallSession {
    fn new(
        cancel_token: CancellationToken,
        session_id: String,
        server_dialog: ServerInviteDialog,
        use_media_proxy: bool,
        event_sender: EventSender,
    ) -> Self {
        let stream = MediaStreamBuilder::new(event_sender)
            .with_id(session_id)
            .with_cancel_token(cancel_token)
            .build();
        Self {
            server_dialog,
            callee_dialogs: Vec::new(),
            last_error: None,
            connected_callee: None,
            ring_time: None,
            answer_time: None,
            caller_offer: None,
            callee_offer: None,
            answer: None,
            hangup_reason: None,
            callee_hangup_reason: None,
            early_media_sent: false,
            media_stream: Arc::new(stream),
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

    async fn create_caller_answer_from_offer(&mut self) -> Result<String> {
        if let Some(ref ans) = self.answer {
            return Ok(ans.clone());
        }

        let orig_offer_sdp =
            String::from_utf8_lossy(self.server_dialog.initial_request().body()).to_string();
        let track_id = "caller-track".to_string();
        let config = TrackConfig::default();

        let mut track: Box<dyn Track> = if Self::is_webrtc_sdp(&orig_offer_sdp) {
            Box::new(WebrtcTrack::new(
                CancellationToken::new(),
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

    async fn create_callee_track(&mut self) -> Result<String> {
        let track_id = "callee-track".to_string();
        let config = TrackConfig::default();
        let track = RtpTrackBuilder::new(track_id.clone(), config)
            .build()
            .await?;
        let offer = track.local_description()?;
        self.media_stream.update_track(Box::new(track), None).await;
        Ok(offer)
    }

    async fn setup_callee_track(&mut self, callee_answer_sdp: &String) -> Result<()> {
        let track_id = "callee-track".to_string();
        self.media_stream
            .update_remote_description(&track_id, callee_answer_sdp)
            .await
    }

    async fn start_ringing(&mut self, mut answer: String, proxy_call: &ProxyCall) {
        if self.early_media_sent {
            debug!("Early media already sent, skipping ringing");
            return;
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
                        if let Some(ref file_name) = proxy_call.dialplan.ringback.audio_file {
                            let mut track = FileTrack::new("callee-track".to_string());
                            track = track.with_path(file_name.clone());
                            self.media_stream.update_track(Box::new(track), None).await;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to create caller answer from offer: {}", e);
                    }
                };
            }
        };

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

    async fn play_ringtone(
        &mut self,
        audio_file: &String,
        event_rx: Option<EventReceiver>,
    ) -> Result<()> {
        let answer_for_caller = self.create_caller_answer_from_offer().await?;
        self.answer = Some(answer_for_caller);
        let hangup_ssrc = rand::random::<u32>();

        let track = FileTrack::new("callee-track".to_string())
            .with_path(audio_file.clone())
            .with_ssrc(hangup_ssrc);
        self.media_stream.update_track(Box::new(track), None).await;

        if let Some(mut rx) = event_rx {
            let wait_for_completion = async {
                while let Ok(event) = rx.recv().await {
                    match event {
                        crate::event::SessionEvent::TrackEnd { ssrc, .. } => {
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
        dialog_id: DialogId,
        callee: String,
        answer: String,
    ) -> Result<()> {
        // retain pending dialog id
        let mut pending_dialog_id = dialog_id.clone();
        pending_dialog_id.to_tag = String::new();
        self.callee_dialogs.retain(|id| *id != pending_dialog_id);

        self.callee_dialogs.push(dialog_id);
        debug!(callee = %callee, "Call accepted, will not send reject on cleanup");
        self.connected_callee = Some(callee);
        self.answer_time = Some(Instant::now());

        if self.use_media_proxy {
            if self.answer.is_none() {
                let answer_for_caller = self.create_caller_answer_from_offer().await?;
                self.answer = Some(answer_for_caller);
            }
            self.setup_callee_track(&answer).await?;
        } else {
            self.answer = Some(answer);
        }

        let headers = vec![rsip::Header::ContentType("application/sdp".into())];
        if let Err(e) = self.server_dialog.accept(
            Some(headers),
            self.answer.clone().map(|sdp| sdp.into_bytes()),
        ) {
            return Err(anyhow!("Failed to send 200 OK: {}", e));
        }
        Ok(())
    }

    async fn cleanup(&mut self, dialog_layer: &Arc<DialogLayer>) {
        let callee_count = self.callee_dialogs.len();
        let call_answered = self.answer_time.is_some();
        debug!(call_answered, callee_count, "Cleaning up call session");

        // Only send a reject if the call was NOT answered
        if !call_answered {
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

    fn needs_media_proxy(&self, offer_sdp: &str) -> bool {
        if self.dialplan.recording.enabled {
            return true;
        }
        match self.dialplan.media.proxy_mode {
            MediaProxyMode::None => false,
            MediaProxyMode::All => true,
            MediaProxyMode::Auto => {
                if let Some(ip) = self.dialplan.media.external_ip.as_ref() {
                    if let Ok(ip_addr) = IpAddr::from_str(ip) {
                        return !is_private_ip(&ip_addr) && self.contains_private_ip(offer_sdp);
                    }
                }
                let is_webrtc_sdp = Self::is_webrtc_sdp(offer_sdp);
                let all_webrtc_target = self.dialplan.all_webrtc_target();
                if (is_webrtc_sdp && !all_webrtc_target) || (!is_webrtc_sdp && all_webrtc_target) {
                    return true;
                }
                return false;
            }
            MediaProxyMode::Nat => {
                if let Some(ip) = self.dialplan.media.external_ip.as_ref() {
                    if let Ok(ip_addr) = IpAddr::from_str(ip) {
                        return !is_private_ip(&ip_addr) && self.contains_private_ip(offer_sdp);
                    }
                }
                return false;
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
        let use_media_proxy = self.needs_media_proxy(&offer_sdp);

        let mut session = CallSession::new(
            self.cancel_token.child_token(),
            self.session_id.clone(),
            server_dialog.clone(),
            use_media_proxy,
            self.event_sender.clone(),
        );
        if use_media_proxy {
            session.caller_offer = Some(offer_sdp);
            session.callee_offer = session.create_callee_track().await.ok();
        } else {
            session.caller_offer = Some(offer_sdp.clone());
            session.callee_offer = Some(offer_sdp);
        }
        let media_stream = session.media_stream.clone();

        let (_, result) = tokio::join!(server_dialog.handle(tx), async {
            let result = tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    session.set_error(StatusCode::RequestTerminated, Some("Cancelled by system".to_string()));
                    session.hangup_reason = Some(CallRecordHangupReason::Canceled);
                    Err(anyhow!("Call cancelled"))
                }
                _ = media_stream.serve() => {Ok(())},
                r = self.execute_dialplan(&mut session) => r,
                r = self.handle_server_events(state_rx) => r,
            };
            session.cleanup(&self.dialog_layer).await;
            result
        });

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
            strategy = %self.dialplan.targets,
            media_proxy = session.use_media_proxy,
            "executing dialplan"
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
            Err(_) => self.handle_failure(session).await,
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
                    info!(
                        session_id = %self.session_id,
                        target_index = index,
                        code = %code,
                        reason,
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
            "Starting parallel dialing"
        );

        if targets.is_empty() {
            session.set_error(
                StatusCode::ServerInternalError,
                Some("No targets provided".to_string()),
            );
            return Err(anyhow!("No targets provided for parallel dialing"));
        }

        if !session.early_media_sent {
            session.start_ringing(String::new(), self).await;
        }

        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<ParallelEvent>();
        let dialog_layer = self.dialog_layer.clone();
        let cancel_token = self.cancel_token.clone();
        let mut join_set = JoinSet::<()>::new();

        let mut known_dialogs: Vec<Option<DialogId>> = vec![None; targets.len()];
        let caller = match self.dialplan.caller.as_ref() {
            Some(c) => c.clone(),
            None => {
                session.set_error(
                    StatusCode::ServerInternalError,
                    Some("No caller specified in dialplan".to_string()),
                );
                return Err(anyhow!("No caller specified in dialplan"));
            }
        };

        for (idx, target) in targets.iter().enumerate() {
            let (state_tx, mut state_rx) = mpsc::unbounded_channel();
            let ev_tx_c = ev_tx.clone();
            let aor = target.aor.to_string();

            let offer = match session.callee_offer.as_ref() {
                Some(sdp) if !sdp.trim().is_empty() => Some(sdp.clone().into_bytes()),
                _ => None,
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
                destination: target.destination.clone(),
                contact: self
                    .dialplan
                    .caller_contact
                    .as_ref()
                    .map(|c| c.uri.clone())
                    .unwrap_or_else(|| caller.clone()),
                credential: target.credential.clone(),
                headers: target.headers.clone(),
            };

            // Forward dialog state events to aggregator
            join_set.spawn({
                let ev_tx_c = ev_tx_c.clone();
                async move {
                    while let Some(state) = state_rx.recv().await {
                        match state {
                            DialogState::Calling(dialog_id) => {
                                let _ = ev_tx_c.send(ParallelEvent::Calling { idx, dialog_id });
                            }
                            DialogState::Early(_, response) => {
                                let sdp = if !response.body().is_empty() {
                                    Some(String::from_utf8_lossy(response.body()).to_string())
                                } else {
                                    None
                                };
                                let _ = ev_tx_c.send(ParallelEvent::Early { sdp });
                            }
                            DialogState::Terminated(_, _) => {
                                let _ = ev_tx_c.send(ParallelEvent::Terminated { idx });
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            });

            // Perform INVITE and report result
            join_set.spawn({
                let dialog_layer = dialog_layer.clone();
                let ev_tx_c = ev_tx_c.clone();
                async move {
                    let invite_result = dialog_layer.do_invite(invite_option, state_tx).await;
                    match invite_result {
                        Ok((dlg, resp_opt)) => {
                            if let Some(resp) = resp_opt {
                                if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
                                    let answer = String::from_utf8_lossy(resp.body()).to_string();
                                    let _ = ev_tx_c.send(ParallelEvent::Accepted {
                                        idx,
                                        dialog_id: dlg.id(),
                                        answer,
                                        aor,
                                    });
                                } else {
                                    let reason = resp.reason_phrase().clone().map(Into::into);
                                    let _ = ev_tx_c.send(ParallelEvent::Failed {
                                        code: resp.status_code,
                                        reason,
                                    });
                                }
                            } else {
                                let _ = ev_tx_c.send(ParallelEvent::Failed {
                                    code: StatusCode::RequestTerminated,
                                    reason: Some("Cancelled by callee".to_string()),
                                });
                            }
                        }
                        Err(e) => {
                            let (code, reason) = match e {
                                rsipstack::Error::DialogError(reason, _, code) => {
                                    (code, Some(reason))
                                }
                                _ => (
                                    StatusCode::ServerInternalError,
                                    Some("Invite failed".to_string()),
                                ),
                            };
                            let _ = ev_tx_c.send(ParallelEvent::Failed { code, reason });
                        }
                    }
                }
            });
        }

        // spawn a cancellation watcher that notifies the main event loop
        {
            let ev_tx_c = ev_tx.clone();
            let cancel_token = cancel_token.clone();
            join_set.spawn(async move {
                cancel_token.cancelled().await;
                let _ = ev_tx_c.send(ParallelEvent::Cancelled);
            });
        }

        drop(ev_tx);

        let mut failures = 0usize;
        let mut accepted_idx: Option<usize> = None;

        while let Some(event) = ev_rx.recv().await {
            match event {
                ParallelEvent::Calling { idx, dialog_id } => {
                    known_dialogs[idx] = Some(dialog_id);
                    if let Some(id) = &known_dialogs[idx] {
                        session.add_callee_dialog(id.clone());
                    }
                }
                ParallelEvent::Early { sdp } => {
                    if let Some(answer) = sdp {
                        session.start_ringing(answer, self).await;
                    } else if !session.early_media_sent {
                        session.start_ringing(String::new(), self).await;
                    }
                }
                ParallelEvent::Accepted {
                    idx,
                    dialog_id,
                    answer,
                    aor,
                } => {
                    if accepted_idx.is_none() {
                        if let Err(e) = session.accept_call(dialog_id.clone(), aor, answer).await {
                            warn!(session_id = %self.session_id, error = %e, "Failed to accept call on parallel branch");
                            continue;
                        }
                        accepted_idx = Some(idx);
                        for (j, maybe_id) in known_dialogs.iter().enumerate() {
                            if j != idx {
                                if let Some(id) = maybe_id.clone() {
                                    if let Some(dlg) = self.dialog_layer.get_dialog(&id) {
                                        dlg.hangup().await.ok();
                                    }
                                }
                            }
                        }
                    }
                }
                ParallelEvent::Failed { code, reason } => {
                    failures += 1;
                    session.set_error(code, reason);
                    if failures >= targets.len() && accepted_idx.is_none() {
                        return Err(anyhow!("All parallel targets failed"));
                    }
                }
                ParallelEvent::Terminated { idx } => {
                    if Some(idx) == accepted_idx {
                        return Ok(());
                    }
                }
                ParallelEvent::Cancelled => {
                    for maybe_id in known_dialogs.iter().filter_map(|o| o.as_ref()) {
                        if let Some(dlg) = self.dialog_layer.get_dialog(maybe_id) {
                            dlg.hangup().await.ok();
                        }
                    }
                    return Err(anyhow!("Caller cancelled"));
                }
            }
        }

        if accepted_idx.is_some() {
            Ok(())
        } else {
            Err(anyhow!("Parallel dialing concluded without success"))
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

        let offer = match session.callee_offer.as_ref() {
            Some(sdp) if !sdp.trim().is_empty() => Some(sdp.clone().into_bytes()),
            _ => None,
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
            destination: target.destination.clone(),
            contact: self
                .dialplan
                .caller_contact
                .as_ref()
                .map(|c| c.uri.clone())
                .unwrap_or_else(|| caller.clone()),
            credential: target.credential.clone(),
            headers: target.headers.clone(),
        };

        let invite_option = if let Some(ref route_invite) = self.dialplan.route_invite {
            let route_result = route_invite
                .route_invite(
                    invite_option,
                    &self.dialplan.original,
                    &self.dialplan.direction,
                )
                .await
                .map_err(|e| {
                    warn!(session_id = %self.session_id, error = %e, "Routing function error");
                    (
                        StatusCode::ServerInternalError,
                        Some("Routing function error".to_string()),
                    )
                })?;
            match route_result {
                RouteResult::NotHandled(option) => {
                    info!(
                        session_id = self.session_id,
                        "Routing function returned NotHandled"
                    );
                    if let Some(ref code) = target.abort_on_route_invite_missing {
                        return Err((code.clone(), None));
                    }
                    option
                }
                RouteResult::Forward(option) => option,
                RouteResult::Abort(code, reason) => {
                    warn!(session_id = self.session_id, %code, ?reason, "route abort");
                    return Err((code, reason));
                }
            }
        } else {
            invite_option
        };

        debug!(
            session_id = %self.session_id,
            callee = %target.aor,
            caller = %caller,
            "Sending INVITE to target"
        );

        self.execute_invite(session, invite_option, &target).await
    }

    async fn execute_invite(
        &self,
        session: &mut CallSession,
        invite_option: InviteOption,
        target: &Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        let dialog_layer = self.dialog_layer.clone();

        tokio::select! {
            _ = self.handle_callee_state(session, &mut state_rx) => {
                Ok(())
            },
            invite_result = dialog_layer.do_invite(invite_option, state_tx) => {
                match invite_result {
                    Ok((dlg, resp)) => {
                        if let Some(resp) = resp {
                            if resp.status_code.kind() != rsip::StatusCodeKind::Successful {
                                let reason = resp.reason_phrase().clone().map(Into::into);
                                info!(session_id = %self.session_id, code = %resp.status_code, "Invite failed with non-successful response");
                                return Err((resp.status_code, reason));
                            }
                            let answer = String::from_utf8_lossy(resp.body()).to_string();
                            match session.accept_call(dlg.id(), target.aor.to_string(), answer).await {
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
                    let answer = String::from_utf8_lossy(response.body()).to_string();
                    info!(
                        session_id = %self.session_id,
                        dialog_id = %dialog_id,
                        status = ?response.status_code,
                        answer = %answer,
                        "Callee dialog is ringing/early"
                    );
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
                session.set_error(status_code.clone(), None);
                session
                    .play_ringtone(audio_file, Some(self.event_sender.subscribe()))
                    .await
                    .ok();
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
        Ok(())
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
                offer: session.caller_offer.clone(),
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
