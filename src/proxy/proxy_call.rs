use crate::{
    call::{DialStrategy, Dialplan, FailureAction, Location, TransactionCookie},
    callrecord::{
        CallRecord, CallRecordHangupReason, CallRecordPersistArgs, CallRecordSender,
        extract_sip_username, persist_call_record,
    },
    config::{MediaProxyMode, RouteResult},
    event::{EventReceiver, EventSender, create_event_sender},
    media::{
        stream::{MediaStream, MediaStreamBuilder},
        track::{Track, TrackConfig, file::FileTrack, rtp::RtpTrackBuilder, webrtc::WebrtcTrack},
    },
    net_tool::is_private_ip,
    proxy::server::SipServerRef,
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use rsip::{StatusCode, Uri};
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
use serde_json::{Map as JsonMap, Number as JsonNumber, Value};
use std::{
    collections::{HashMap, HashSet},
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
    server: SipServerRef,
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

    pub fn build(self, server: SipServerRef) -> ProxyCall {
        let dialplan = self.dialplan;
        let cancel_token = self.cancel_token.unwrap_or_default();
        let session_id = dialplan
            .session_id
            .as_ref()
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let dialog_layer = server.dialog_layer.clone();
        ProxyCall {
            server,
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
    external_addr: Option<IpAddr>,  // Optional external IP address for media
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
        dialog_id: Option<DialogId>,
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
        external_addr: Option<IpAddr>,
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
            external_addr,
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
                self.media_stream.cancel_token.clone(),
                track_id.clone(),
                config,
                None,
            ))
        } else {
            let track = RtpTrackBuilder::new(track_id.clone(), config)
                .with_cancel_token(self.media_stream.cancel_token.clone());
            if let Some(ref addr) = self.external_addr {
                Box::new(track.with_external_addr(addr.clone()).build().await?)
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
        let (offer, track) = if !is_webrtc {
            let track = RtpTrackBuilder::new(track_id.clone(), config)
                .with_cancel_token(self.media_stream.cancel_token.clone());
            let track = if let Some(ref addr) = self.external_addr {
                track.with_external_addr(addr.clone()).build().await?
            } else {
                track.build().await?
            };
            (
                track.local_description()?,
                Box::new(track) as Box<dyn Track>,
            )
        } else {
            let mut track = WebrtcTrack::new(
                self.media_stream.cancel_token.clone(),
                track_id.clone(),
                config,
                None,
            );
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

    fn has_error(&self) -> bool {
        self.last_error.is_some()
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
        self.connected_callee = Some(callee);
        self.answer_time = Some(Instant::now());

        info!(
            server_dialog_id = %self.server_dialog.id(),
            use_media_proxy = self.use_media_proxy,
            has_answer = self.answer.is_some(),
            "Call answered"
        );
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
            let (code, reason) = self
                .last_error
                .clone()
                .unwrap_or((StatusCode::Decline, None));

            info!(code = %code, reason = ?reason, id = %self.server_dialog.id(), "Call not answered, sending rejection if possible");
            if let Err(e) = self
                .server_dialog
                .reject(Some(code.clone()), reason.clone())
            {
                info!(error = %e, "Failed to send rejection to caller");
            }
        }
        if let Err(e) = self.server_dialog.bye().await {
            info!(error = %e, "Failed to send BYE to server dialog");
        }
        dialog_layer.remove_dialog(&self.server_dialog.id());

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

    fn local_contact_uri(&self) -> Option<Uri> {
        self.dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .or_else(|| self.server.default_contact_uri())
    }

    fn should_forward_header(header: &rsip::Header) -> bool {
        use rsip::Header;

        match header {
            Header::Via(_)
            | Header::Contact(_)
            | Header::From(_)
            | Header::To(_)
            | Header::CallId(_)
            | Header::CSeq(_)
            | Header::MaxForwards(_)
            | Header::ContentLength(_)
            | Header::ContentType(_)
            | Header::Authorization(_)
            | Header::ProxyAuthorization(_)
            | Header::ProxyAuthenticate(_)
            | Header::WwwAuthenticate(_)
            | Header::Route(_)
            | Header::RecordRoute(_) => false,
            Header::Other(name, _) => {
                let lower = name.to_ascii_lowercase();
                !matches!(
                    lower.as_str(),
                    "via"
                        | "from"
                        | "to"
                        | "contact"
                        | "call-id"
                        | "cseq"
                        | "max-forwards"
                        | "content-length"
                        | "content-type"
                        | "route"
                        | "record-route"
                        | "authorization"
                        | "proxy-authorization"
                        | "proxy-authenticate"
                        | "www-authenticate"
                )
            }
            _ => true,
        }
    }

    fn build_invite_headers(&self, target: &Location) -> Option<Vec<rsip::Header>> {
        let mut headers = target.headers.clone().unwrap_or_default();

        let mut custom_names: HashSet<String> = headers
            .iter()
            .filter_map(|h| match h {
                rsip::Header::Other(name, _) => Some(name.to_ascii_lowercase()),
                _ => None,
            })
            .collect();

        if self.dialplan.with_original_headers {
            for header in self.dialplan.original.headers.iter() {
                if !Self::should_forward_header(header) {
                    continue;
                }
                if let rsip::Header::Other(name, _) = header {
                    let key = name.to_ascii_lowercase();
                    if !custom_names.insert(key) {
                        continue;
                    }
                }
                headers.push(header.clone());
            }
        }

        if headers.is_empty() {
            None
        } else {
            Some(headers)
        }
    }

    fn merge_invite_headers(
        mut existing: Option<Vec<rsip::Header>>,
        base: Option<&[rsip::Header]>,
    ) -> Option<Vec<rsip::Header>> {
        let mut headers = existing.take().unwrap_or_default();

        let mut other_indexes: HashMap<String, usize> = headers
            .iter()
            .enumerate()
            .filter_map(|(idx, header)| match header {
                rsip::Header::Other(name, _) => Some((name.to_ascii_lowercase(), idx)),
                _ => None,
            })
            .collect();

        if let Some(base_headers) = base {
            for header in base_headers {
                match header {
                    rsip::Header::Other(name, _) => {
                        let key = name.to_ascii_lowercase();
                        if let Some(idx) = other_indexes.get(&key).cloned() {
                            headers[idx] = header.clone();
                        } else {
                            other_indexes.insert(key, headers.len());
                            headers.push(header.clone());
                        }
                    }
                    _ => {
                        if !headers.iter().any(|h| h == header) {
                            headers.push(header.clone());
                        }
                    }
                }
            }
        }

        if headers.is_empty() {
            None
        } else {
            Some(headers)
        }
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
                let is_webrtc_sdp = Self::is_webrtc_sdp(offer_sdp);
                let all_webrtc_target = self.dialplan.all_webrtc_target();
                if is_webrtc_sdp && all_webrtc_target {
                    return false; // Both offer and all targets are WebRTC, no proxy needed
                }

                if (is_webrtc_sdp && !all_webrtc_target) || (!is_webrtc_sdp && all_webrtc_target) {
                    return true;
                }

                if let Some(ip) = self.dialplan.media.external_ip.as_ref() {
                    if let Ok(ip_addr) = IpAddr::from_str(ip) {
                        return !is_private_ip(&ip_addr) && self.contains_private_ip(offer_sdp);
                    }
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
        let (state_tx, state_rx) = mpsc::unbounded_channel();
        let local_contact = self.local_contact_uri();
        let mut server_dialog = self.dialog_layer.get_or_create_server_invite(
            tx,
            state_tx,
            None,
            local_contact.clone(),
        )?;

        // Extract initial offer SDP
        let initial_request = server_dialog.initial_request();
        let offer_sdp = String::from_utf8_lossy(initial_request.body()).to_string();
        let use_media_proxy = self.needs_media_proxy(&offer_sdp);
        let all_webrtc_target = self.dialplan.all_webrtc_target();

        info!(
            session_id = %self.session_id,
            server_dialog_id = %server_dialog.id(),
            use_media_proxy,
            all_webrtc_target,
            "starting proxy call processing"
        );

        let external_addr = self
            .dialplan
            .media
            .external_ip
            .as_ref()
            .map(|ip| ip.parse().ok())
            .flatten();
        let mut session = CallSession::new(
            self.cancel_token.child_token(),
            self.session_id.clone(),
            server_dialog.clone(),
            use_media_proxy,
            self.event_sender.clone(),
            external_addr,
        );
        if use_media_proxy {
            session.caller_offer = Some(offer_sdp);
            session.callee_offer = session.create_callee_track(all_webrtc_target).await.ok();
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
                DialogState::Terminated(dialog_id, reason) => {
                    info!(session_id = %self.session_id, reason = ?reason, %dialog_id, "Server dialog terminated");
                    break;
                }
                DialogState::Info(dialog_id, _request) => {
                    debug!(session_id = %self.session_id, %dialog_id, "Received INFO on server dialog");
                }
                DialogState::Updated(dialog_id, _request) => {
                    debug!(session_id = %self.session_id, %dialog_id, "Received UPDATE on server dialog");
                }
                DialogState::Notify(dialog_id, _request) => {
                    debug!(session_id = %self.session_id, %dialog_id, "Received NOTIFY on server dialog");
                }
                DialogState::Calling(dialog_id) => {
                    debug!(session_id = %self.session_id, %dialog_id, "Server dialog in calling state");
                }
                DialogState::Trying(dialog_id) => {
                    debug!(session_id = %self.session_id, %dialog_id, "Server dialog in trying state");
                }
                DialogState::Early(dialog_id, _) => {
                    debug!(session_id = %self.session_id, %dialog_id, "Server dialog in early state");
                }
                DialogState::WaitAck(dialog_id, _) => {
                    debug!(session_id = %self.session_id, %dialog_id, "Server dialog in wait-ack state");
                }
                DialogState::Confirmed(dialog_id, _) => {
                    debug!(session_id = %self.session_id, %dialog_id, "Server dialog in confirmed state");
                }
                other_state => {
                    debug!(
                        session_id = %self.session_id,
                        "Received other state on server dialog: {}", other_state
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
                session_id = %self.session_id, index, %target,
                "trying sequential target"
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

        let local_contact = self.local_contact_uri();

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

            let base_headers = self.build_invite_headers(target);
            let mut invite_option = InviteOption {
                callee: target.aor.clone(),
                caller: caller.clone(),
                content_type,
                offer,
                destination: target.destination.clone(),
                contact: local_contact.clone().unwrap_or_else(|| caller.clone()),
                credential: target.credential.clone(),
                headers: base_headers.clone(),
                ..Default::default()
            };

            invite_option.headers = Self::merge_invite_headers(
                invite_option.headers.take(),
                base_headers.as_ref().map(Vec::as_slice),
            );

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
                                        dialog_id: Some(dlg.id()),
                                        reason,
                                    });
                                }
                            } else {
                                let _ = ev_tx_c.send(ParallelEvent::Failed {
                                    code: StatusCode::RequestTerminated,
                                    dialog_id: Some(dlg.id()),
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
                            let _ = ev_tx_c.send(ParallelEvent::Failed {
                                code,
                                reason,
                                dialog_id: None,
                            });
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
                    session.add_callee_dialog(dialog_id.clone());
                    if accepted_idx.is_none() {
                        if let Err(e) = session.accept_call(dialog_id, aor, answer).await {
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
                ParallelEvent::Failed {
                    code,
                    reason,
                    dialog_id,
                } => {
                    failures += 1;
                    if let Some(id) = dialog_id {
                        session.add_callee_dialog(id);
                    }
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
        let caller_display_name = self.dialplan.caller_display_name.as_ref();

        let offer = match session.callee_offer.as_ref() {
            Some(sdp) if !sdp.trim().is_empty() => Some(sdp.clone().into_bytes()),
            _ => None,
        };

        let content_type = if offer.is_some() {
            Some("application/sdp".to_string())
        } else {
            None
        };

        let local_contact = self.local_contact_uri();

        let base_headers = self.build_invite_headers(target);

        let invite_option = InviteOption {
            caller_display_name: caller_display_name.cloned(),
            callee: target.aor.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination: target.destination.clone(),
            contact: local_contact.clone().unwrap_or_else(|| caller.clone()),
            credential: target.credential.clone(),
            headers: base_headers.clone(),
            ..Default::default()
        };

        let mut invite_option = if let Some(ref route_invite) = self.dialplan.route_invite {
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
                    info!(session_id = self.session_id, %target,
                        "Routing function returned NotHandled"
                    );
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

        invite_option.headers = Self::merge_invite_headers(
            invite_option.headers.take(),
            base_headers.as_ref().map(Vec::as_slice),
        );

        let callee_uri = &invite_option.callee;
        let callee_realm = callee_uri.host().to_string();
        if self.server.is_same_realm(&callee_realm).await {
            let dialplan = &self.dialplan;
            let locations = self.server.locator.lookup(&callee_uri).await.map_err(|e| {
                (
                    rsip::StatusCode::TemporarilyUnavailable,
                    Some(e.to_string()),
                )
            })?;

            if locations.is_empty() {
                match self
                    .server
                    .user_backend
                    .get_user(&callee_uri.user().unwrap_or_default(), Some(&callee_realm))
                    .await
                {
                    Ok(Some(_)) => {
                        info!(session_id = ?dialplan.session_id, callee = %callee_uri, "user offline in locator, abort now");
                        return Err((
                            rsip::StatusCode::TemporarilyUnavailable,
                            Some("User offline".to_string()),
                        ));
                    }
                    Ok(None) => {
                        info!(session_id = ?dialplan.session_id, callee = %callee_uri, "user not found in auth backend, continue");
                    }
                    Err(e) => {
                        warn!(session_id = ?dialplan.session_id, callee = %callee_uri, "failed to lookup user in auth backend: {}", e);
                        return Err((rsip::StatusCode::ServerInternalError, Some(e.to_string())));
                    }
                }
            } else {
                invite_option.destination = locations[0].destination.clone();
            }
        }

        let destination = invite_option
            .destination
            .as_ref()
            .map(|d| d.to_string())
            .unwrap_or_else(|| "?".to_string());

        debug!(
            session_id = %self.session_id, %caller, %target, destination,
            "Sending INVITE to callee"
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
                        session.add_callee_dialog(dlg.id());
                        if let Some(resp) = resp {
                            if resp.status_code.kind() != rsip::StatusCodeKind::Successful {
                                let reason = resp.reason_phrase().clone().map(Into::into);
                                info!(session_id = %self.session_id, code = %resp.status_code, "Invite failed with non-successful response");
                                match dlg.state() {
                                    DialogState::Terminated(_, term_reason) => {
                                        session.callee_terminated(term_reason).await;
                                    }
                                    _ => {}
                                }
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
                        "Received state in established call: {}",
                        other_state
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
                DialogState::Info(_, request) => {
                    session.callee_dialog_request(request).await.ok();
                }
                DialogState::Updated(_, request) => {
                    session.callee_dialog_request(request).await.ok();
                }
                DialogState::Notify(_, request) => {
                    session.callee_dialog_request(request).await.ok();
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
                if !session.has_error() {
                    session.set_error(status_code, None);
                }
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
        let now = Utc::now();
        let start_time = now - chrono::Duration::from_std(self.elapsed()).unwrap_or_default();

        let ring_time = session.ring_time.map(|rt| {
            start_time
                + chrono::Duration::from_std(rt.duration_since(self.start_time)).unwrap_or_default()
        });

        let answer_time = session.answer_time.map(|at| {
            start_time
                + chrono::Duration::from_std(at.duration_since(self.start_time)).unwrap_or_default()
        });

        let status_code = session
            .last_error
            .as_ref()
            .map(|(code, _)| u16::from(code.clone()))
            .unwrap_or(200);

        let hangup_reason = session.hangup_reason.clone().or_else(|| {
            if session.last_error.is_some() {
                Some(CallRecordHangupReason::Failed)
            } else if session.answer_time.is_some() {
                Some(CallRecordHangupReason::BySystem)
            } else {
                Some(CallRecordHangupReason::Failed)
            }
        });

        let caller = self
            .dialplan
            .caller
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_default();
        let callee = session
            .connected_callee
            .as_ref()
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        let mut extras_map: HashMap<String, Value> = HashMap::new();
        extras_map.insert(
            "status_code".to_string(),
            Value::Number(JsonNumber::from(status_code)),
        );
        if let Some(reason) = hangup_reason.as_ref() {
            extras_map.insert(
                "hangup_reason".to_string(),
                Value::String(reason.to_string()),
            );
        }
        if let Some((code, reason)) = session.last_error.as_ref() {
            extras_map.insert(
                "last_error_code".to_string(),
                Value::Number(JsonNumber::from(u16::from(code.clone()))),
            );
            if let Some(reason) = reason {
                extras_map.insert(
                    "last_error_reason".to_string(),
                    Value::String(reason.clone()),
                );
            }
        }

        let extras = if extras_map.is_empty() {
            None
        } else {
            Some(extras_map.clone())
        };

        let record = CallRecord {
            call_type: crate::call::ActiveCallType::Sip,
            option: None,
            call_id: self.session_id.clone(),
            start_time,
            ring_time,
            answer_time,
            end_time: now,
            caller: caller.clone(),
            callee: callee.clone(),
            status_code,
            offer: session.caller_offer.clone(),
            answer: session.answer.clone(),
            hangup_reason: hangup_reason.clone(),
            recorder: Vec::new(),
            extras,
            dump_event_file: None,
            refer_callrecord: None,
        };

        if let Some(db) = self.server.database.as_ref() {
            let direction = match self.dialplan.direction {
                crate::call::DialDirection::Inbound => "inbound".to_string(),
                crate::call::DialDirection::Outbound => "outbound".to_string(),
                crate::call::DialDirection::Internal => "internal".to_string(),
            };
            let status = resolve_call_status(session);
            let from_number = extract_sip_username(&caller);
            let to_number = extract_sip_username(&callee);

            let trunk_name = self.cookie.get_source_trunk();
            let (sip_gateway, sip_trunk_id) = if let Some(ref name) = trunk_name {
                let trunks = self.server.data_context.trunks_snapshot().await;
                let trunk_id = trunks.get(name).and_then(|config| config.id);
                (Some(name.clone()), trunk_id)
            } else {
                (None, None)
            };

            let metadata_value = if extras_map.is_empty() {
                None
            } else {
                let mut map = JsonMap::new();
                for (key, value) in extras_map.iter() {
                    map.insert(key.clone(), value.clone());
                }
                Some(Value::Object(map))
            };

            let persist_args = CallRecordPersistArgs {
                direction,
                status,
                from_number,
                to_number,
                caller_name: None,
                agent_name: None,
                queue: None,
                department_id: None,
                extension_id: None,
                sip_trunk_id,
                route_id: None,
                sip_gateway,
                recording_url: None,
                recording_duration_secs: None,
                has_transcript: false,
                transcript_status: "pending".to_string(),
                transcript_language: None,
                tags: None,
                quality_mos: None,
                quality_latency_ms: None,
                quality_jitter_ms: None,
                quality_packet_loss_percent: None,
                analytics: None,
                metadata: metadata_value,
            };

            if let Err(err) = persist_call_record(db, &record, persist_args).await {
                warn!(
                    session_id = %self.session_id,
                    error = %err,
                    "Failed to persist call record"
                );
            }
        }

        if let Some(ref sender) = self.call_record_sender {
            if let Err(e) = sender.send(record.clone()) {
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

fn resolve_call_status(session: &CallSession) -> String {
    if session.answer_time.is_some() {
        return "completed".to_string();
    }

    match session.hangup_reason.as_ref() {
        Some(CallRecordHangupReason::NoAnswer)
        | Some(CallRecordHangupReason::Autohangup)
        | Some(CallRecordHangupReason::Canceled) => "missed".to_string(),
        _ => "failed".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::ProxyCall;
    use rsip::Header;

    #[test]
    fn merge_invite_headers_prefers_base_custom_headers() {
        let existing = Some(vec![Header::Other("X-Param".into(), "old".into())]);
        let base = vec![Header::Other("X-Param".into(), "new".into())];

        let merged = ProxyCall::merge_invite_headers(existing, Some(base.as_slice())).unwrap();

        assert!(merged.iter().any(|header| match header {
            Header::Other(name, value) => name == "X-Param" && value == "new",
            _ => false,
        }));
    }

    #[test]
    fn merge_invite_headers_keeps_existing_headers() {
        let existing = Some(vec![Header::Other(
            "P-Asserted-Identity".into(),
            "<sip:alice@example.com>".into(),
        )]);
        let base = vec![Header::Other("X-Param".into(), "custom".into())];

        let merged = ProxyCall::merge_invite_headers(existing, Some(base.as_slice())).unwrap();

        assert!(merged.iter().any(|header| match header {
            Header::Other(name, value) =>
                name == "P-Asserted-Identity" && value == "<sip:alice@example.com>",
            _ => false,
        }));
        assert!(merged.iter().any(|header| match header {
            Header::Other(name, value) => name == "X-Param" && value == "custom",
            _ => false,
        }));
    }
}
