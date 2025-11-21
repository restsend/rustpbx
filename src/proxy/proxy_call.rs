use crate::{
    call::{
        CallForwardingConfig, CallForwardingMode, DialDirection, DialStrategy, Dialplan,
        DialplanFlow, DialplanIvrConfig, FailureAction, Location, MediaConfig, QueueFallbackAction,
        QueueHoldConfig, QueuePlan, TransactionCookie, TransferEndpoint,
        ivr::{
            InputEvent, InputStep, IvrExecutor, IvrExit, IvrPlan, IvrRuntime, PromptMedia,
            PromptPlayback, PromptSource, ResolvedTransferAction,
        },
        sip::{DialogStateReceiverGuard, ServerDialogGuard},
    },
    callrecord::{
        CallRecord, CallRecordHangupMessage, CallRecordHangupReason, CallRecordMedia,
        CallRecordPersistArgs, CallRecordSender, apply_record_file_extras, extract_sip_username,
        extras_map_to_metadata, extras_map_to_option, persist_and_dispatch_record,
        sipflow::SipMessageItem,
    },
    config::{MediaProxyMode, RouteResult},
    event::{EventReceiver, EventSender, SessionEvent, create_event_sender},
    media::{
        recorder::RecorderOption,
        stream::{MediaStream, MediaStreamBuilder},
        track::{Track, TrackConfig, file::FileTrack, rtp::RtpTrackBuilder, webrtc::WebrtcTrack},
    },
    net_tool::is_private_ip,
    proxy::{
        active_call_registry::{
            ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus,
            normalize_direction,
        },
        server::SipServerRef,
    },
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{FutureExt, future::BoxFuture};
use rsip::{Param, StatusCode, Uri, headers::UntypedHeader, prelude::HeadersExt};
use rsipstack::{
    dialog::{
        DialogId,
        dialog::{DialogState, TerminatedReason},
        dialog_layer::DialogLayer,
        invitation::InviteOption,
        server_dialog::ServerInviteDialog,
    },
    rsip_ext::RsipResponseExt,
    transaction::transaction::Transaction,
    transport::SipConnection,
};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    net::IpAddr,
    str::FromStr,
    sync::{Arc, mpsc as std_mpsc},
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, mpsc},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
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

const MAX_QUEUE_CHAIN_DEPTH: usize = 4;

pub struct ProxyCallBuilder {
    cookie: TransactionCookie,
    dialplan: Dialplan,
    cancel_token: Option<CancellationToken>,
    call_record_sender: Option<CallRecordSender>,
}

#[derive(Clone, Debug)]
struct SessionHangupMessage {
    code: u16,
    reason: Option<String>,
    target: Option<String>,
}

impl From<&SessionHangupMessage> for CallRecordHangupMessage {
    fn from(message: &SessionHangupMessage) -> Self {
        Self {
            code: message.code,
            reason: message.reason.clone(),
            target: message.target.clone(),
        }
    }
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
    session_id: String,
    started_at: DateTime<Utc>,
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
    queue_hold_audio_file: Option<String>,
    queue_hold_loop_cancel: Option<CancellationToken>,
    queue_hold_loop_handle: Option<JoinHandle<()>>,
    active_registry: Option<Arc<ActiveProxyCallRegistry>>,
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
        answer: String,
        aor: String,
        caller_uri: String,
        callee_uri: String,
        contact: String,
        destination: Option<String>,
    },
    Failed {
        idx: usize,
        code: StatusCode,
        reason: Option<String>,
        target: Option<String>,
    },
    Terminated {
        idx: usize,
    },
    Cancelled,
}

impl CallSession {
    const QUEUE_HOLD_TRACK_ID: &'static str = "queue-hold-track";
    const QUEUE_HOLD_PLAY_ID: &'static str = "queue-hold";
    const IVR_PROMPT_TRACK_ID: &'static str = "ivr-prompt-track";

    fn new(
        cancel_token: CancellationToken,
        session_id: String,
        server_dialog: ServerInviteDialog,
        use_media_proxy: bool,
        event_sender: EventSender,
        media_config: MediaConfig,
        recorder_option: Option<RecorderOption>,
        active_registry: Option<Arc<ActiveProxyCallRegistry>>,
    ) -> Self {
        let started_at = Utc::now();
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
            session_id,
            started_at,
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
            queue_hold_audio_file: None,
            queue_hold_loop_cancel: None,
            queue_hold_loop_handle: None,
            active_registry,
        }
    }

    fn note_attempt_failure(
        &mut self,
        code: StatusCode,
        reason: Option<String>,
        target: Option<String>,
    ) {
        self.hangup_messages.push(SessionHangupMessage {
            code: u16::from(code),
            reason,
            target,
        });
    }

    fn recorded_hangup_messages(&self) -> Vec<CallRecordHangupMessage> {
        self.hangup_messages
            .iter()
            .map(CallRecordHangupMessage::from)
            .collect()
    }

    fn register_active_call(&mut self, direction: &DialDirection) {
        if let Some(registry) = self.active_registry.as_ref() {
            let entry = ActiveProxyCallEntry {
                session_id: self.session_id.clone(),
                caller: self.routed_caller.clone().or(self.original_caller.clone()),
                callee: self.routed_callee.clone().or(self.original_callee.clone()),
                direction: normalize_direction(direction),
                started_at: self.started_at,
                answered_at: None,
                status: ActiveProxyCallStatus::Ringing,
            };
            registry.upsert(entry);
        }
    }

    fn refresh_active_call_parties(&self) {
        if let Some(registry) = self.active_registry.as_ref() {
            let caller = self.routed_caller.clone().or(self.original_caller.clone());
            let callee = self.routed_callee.clone().or(self.original_callee.clone());
            registry.update(&self.session_id, |entry| {
                if caller.is_some() {
                    entry.caller = caller.clone();
                }
                if callee.is_some() {
                    entry.callee = callee.clone();
                }
            });
        }
    }

    fn mark_active_call_answered(&self) {
        if let Some(registry) = self.active_registry.as_ref() {
            let resolved_callee = self
                .connected_callee
                .clone()
                .or(self.routed_callee.clone())
                .or(self.original_callee.clone());
            let answered_at = Utc::now();
            registry.update(&self.session_id, |entry| {
                entry.status = ActiveProxyCallStatus::Talking;
                entry.answered_at = Some(answered_at);
                if resolved_callee.is_some() {
                    entry.callee = resolved_callee.clone();
                }
            });
        }
    }

    fn note_invite_details(&mut self, invite: &InviteOption) {
        self.routed_caller = Some(invite.caller.to_string());
        self.routed_callee = Some(invite.callee.to_string());
        self.routed_contact = Some(invite.contact.to_string());
        self.routed_destination = invite.destination.as_ref().map(|addr| addr.to_string());
        self.refresh_active_call_parties();
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
            use crate::media::negotiate::select_peer_media;
            use std::io::Cursor;
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
        if self.early_media_sent {
            debug!("Early media already sent, skipping ringing");
            return;
        }

        if self.queue_hold_active && !answer.is_empty() {
            debug!("Queue hold audio active, suppressing remote early-media ringback");
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

    fn set_error(&mut self, code: StatusCode, reason: Option<String>, target: Option<String>) {
        debug!(code = %code, reason = ?reason, target = ?target, "Call session error set");
        self.last_error = Some((code.clone(), reason.clone()));
        self.hangup_reason = Some(CallRecordHangupReason::Failed);
        self.note_attempt_failure(code, reason, target);
    }

    fn is_answered(&self) -> bool {
        self.answer.is_some() || self.answer_time.is_some()
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

        if let Some(callee_addr) = callee {
            let resolved_callee = self.routed_callee.clone().unwrap_or(callee_addr);
            self.connected_callee = Some(resolved_callee);
        }
        if self.answer_time.is_none() {
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
        Ok(())
    }

    async fn start_queue_hold(&mut self, hold: QueueHoldConfig) -> Result<()> {
        if self.queue_hold_active {
            return Ok(());
        }

        let audio_file = hold
            .audio_file
            .clone()
            .ok_or_else(|| anyhow!("Queue hold requires an audio file"))?;

        Self::play_queue_hold_track(self.media_stream.clone(), audio_file.clone()).await;
        self.queue_hold_active = true;
        self.queue_hold_audio_file = Some(audio_file.clone());

        if hold.loop_playback {
            let cancel = CancellationToken::new();
            let loop_handle = self.spawn_queue_hold_loop(audio_file, cancel.clone());
            self.queue_hold_loop_cancel = Some(cancel);
            self.queue_hold_loop_handle = Some(loop_handle);
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
        if let Some(registry) = self.active_registry.as_ref() {
            registry.remove(&self.session_id);
        }
    }
}

impl ProxyCall {
    fn forwarding_config(&self) -> Option<&CallForwardingConfig> {
        self.dialplan.call_forwarding.as_ref()
    }

    fn immediate_forwarding_config(&self) -> Option<&CallForwardingConfig> {
        self.forwarding_config().and_then(|config| {
            if matches!(config.mode, CallForwardingMode::Always) {
                Some(config)
            } else {
                None
            }
        })
    }

    fn forwarding_timeout(&self) -> Option<Duration> {
        self.forwarding_config().and_then(|config| {
            if matches!(config.mode, CallForwardingMode::WhenNoAnswer) {
                Some(config.timeout)
            } else {
                None
            }
        })
    }

    fn failure_is_busy(&self, session: &CallSession) -> bool {
        session
            .last_error
            .as_ref()
            .map(|(code, _)| {
                matches!(
                    code,
                    StatusCode::BusyHere
                        | StatusCode::BusyEverywhere
                        | StatusCode::Decline
                        | StatusCode::RequestTerminated
                )
            })
            .unwrap_or(false)
    }

    fn failure_is_no_answer(&self, session: &CallSession) -> bool {
        if session.is_answered() {
            return false;
        }
        session
            .last_error
            .as_ref()
            .map(|(code, _)| {
                matches!(
                    code,
                    StatusCode::RequestTimeout
                        | StatusCode::TemporarilyUnavailable
                        | StatusCode::ServerTimeOut
                )
            })
            .unwrap_or(false)
    }

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

    fn is_webrtc_sdp(sdp: &str) -> bool {
        sdp.contains("a=fingerprint")
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

    fn sync_server_dialog_remote_target(&self, server_dialog: &ServerInviteDialog) {
        let request = server_dialog.initial_request();
        if let Some((uri, remote_contact)) = compute_remote_target_from_request(request) {
            if let Some(dialog) = self.dialog_layer.get_dialog(&server_dialog.id()) {
                dialog.set_remote_target(uri, Some(remote_contact));
            }
        }
    }

    fn update_remote_target_from_response(&self, dialog_id: &DialogId, response: &rsip::Response) {
        let contact_header = match response.contact_header() {
            Ok(header) => header,
            Err(_) => return,
        };

        let remote_contact =
            rsip::headers::untyped::Contact::from(contact_header.value().to_string());
        let uri = match remote_contact.uri() {
            Ok(uri) => uri,
            Err(_) => return,
        };

        if let Some(dialog) = self.dialog_layer.get_dialog(dialog_id) {
            info!(
                session_id = %self.session_id,
                dialog_id = %dialog_id,
                remote_uri = %uri,
                "Updating server dialog remote target from response Contact header"
            );
            dialog.set_remote_target(uri, Some(remote_contact));
        }
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
        self.sync_server_dialog_remote_target(&server_dialog);
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

        let recorder_option =
            if self.dialplan.recording.enabled && self.dialplan.recording.auto_start {
                self.dialplan.recording.option.clone()
            } else {
                None
            };

        let mut session = CallSession::new(
            self.cancel_token.child_token(),
            self.session_id.clone(),
            server_dialog.clone(),
            use_media_proxy,
            self.event_sender.clone(),
            self.dialplan.media.clone(),
            recorder_option,
            Some(self.server.active_call_registry.clone()),
        );
        session.register_active_call(&self.dialplan.direction);
        if use_media_proxy {
            session.caller_offer = Some(offer_sdp);
            session.callee_offer = session.create_callee_track(all_webrtc_target).await.ok();
        } else {
            session.caller_offer = Some(offer_sdp.clone());
            session.callee_offer = Some(offer_sdp);
        }
        let media_stream = session.media_stream.clone();
        let dialog_guard = ServerDialogGuard::new(self.dialog_layer.clone(), server_dialog.id());

        let (_, result) = tokio::join!(server_dialog.handle(tx), async {
            let result = tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    session.set_error(
                        StatusCode::RequestTerminated,
                        Some("Cancelled by system".to_string()),
                        None,
                    );
                    session.hangup_reason = Some(CallRecordHangupReason::Canceled);
                    Err(anyhow!("Call cancelled"))
                }
                _ = media_stream.serve() => {Ok(())},
                r = self.execute_dialplan(&mut session) => r,
                r = self.handle_server_events(state_rx) => r,
            };
            drop(dialog_guard);
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
                None,
            );
            return Err(anyhow!("Dialplan has no targets"));
        }
        if self.try_forwarding_before_dial(session).await? {
            return Ok(());
        }
        self.execute_flow(session, &self.dialplan.flow).await
    }

    async fn try_forwarding_before_dial(&self, session: &mut CallSession) -> Result<bool> {
        let Some(config) = self.immediate_forwarding_config() else {
            return Ok(false);
        };
        info!(
            session_id = %self.session_id,
            endpoint = ?config.endpoint,
            "Call forwarding (always) engaged"
        );
        match self.transfer_to_endpoint(session, &config.endpoint).await {
            Ok(()) => Ok(true),
            Err(err) => {
                warn!(
                    session_id = %self.session_id,
                    error = %err,
                    "Call forwarding (always) failed, continuing with dialplan"
                );
                Ok(false)
            }
        }
    }

    async fn try_call_forwarding_on_failure(&self, session: &mut CallSession) -> Result<bool> {
        let Some(config) = self.forwarding_config() else {
            return Ok(false);
        };
        let should_forward = match config.mode {
            CallForwardingMode::Always => false,
            CallForwardingMode::WhenBusy => self.failure_is_busy(session),
            CallForwardingMode::WhenNoAnswer => self.failure_is_no_answer(session),
        };
        if !should_forward {
            return Ok(false);
        }
        info!(
            session_id = %self.session_id,
            mode = ?config.mode,
            endpoint = ?config.endpoint,
            "Call forwarding engaged after failure"
        );
        match self.transfer_to_endpoint(session, &config.endpoint).await {
            Ok(()) => Ok(true),
            Err(err) => {
                warn!(
                    session_id = %self.session_id,
                    error = %err,
                    "Call forwarding after failure failed"
                );
                Ok(false)
            }
        }
    }

    fn transfer_to_endpoint<'a>(
        &'a self,
        session: &'a mut CallSession,
        endpoint: &'a TransferEndpoint,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match endpoint {
                TransferEndpoint::Uri(uri) => self.transfer_to_uri(session, uri).await,
                TransferEndpoint::Queue(name) => self.transfer_to_queue(session, name).await,
                TransferEndpoint::Ivr(reference) => self.transfer_to_ivr(session, reference).await,
            }
        }
        .boxed()
    }

    async fn transfer_to_uri(&self, session: &mut CallSession, uri: &str) -> Result<()> {
        let parsed = Uri::try_from(uri)
            .map_err(|err| anyhow!("invalid forwarding uri '{}': {}", uri, err))?;
        let mut location = Location::default();
        location.aor = parsed.clone();
        location.contact_raw = Some(parsed.to_string());
        match self.try_single_target(session, &location).await {
            Ok(_) => Ok(()),
            Err((code, reason)) => {
                let message = reason.unwrap_or_else(|| code.to_string());
                Err(anyhow!("forwarding to {} failed: {}", uri, message))
            }
        }
    }

    async fn transfer_to_queue(&self, session: &mut CallSession, reference: &str) -> Result<()> {
        let queue_plan = self.load_queue_plan(reference).await?;
        let strategy = queue_plan
            .dial_strategy()
            .cloned()
            .ok_or_else(|| anyhow!("queue '{}' has no dial targets", reference))?;
        let next_flow = DialplanFlow::Targets(strategy);
        self.execute_queue_plan(session, &queue_plan, &next_flow, 0, false)
            .await
    }

    async fn load_queue_plan(&self, reference: &str) -> Result<QueuePlan> {
        if reference.trim().is_empty() {
            return Err(anyhow!("queue reference cannot be empty"));
        }
        if let Some(config) = self
            .server
            .data_context
            .resolve_queue_config(reference)
            .await?
        {
            return config.to_queue_plan();
        }
        if let Some(config) = self.server.data_context.load_queue_file(reference).await? {
            return config.to_queue_plan();
        }
        Err(anyhow!("queue reference '{}' not found", reference))
    }

    async fn transfer_to_ivr(&self, session: &mut CallSession, reference: &str) -> Result<()> {
        if reference.trim().is_empty() {
            return Err(anyhow!("ivr reference cannot be empty"));
        }
        let config = self
            .server
            .data_context
            .load_ivr_file(reference)
            .await?
            .ok_or_else(|| anyhow!("ivr reference '{}' not found", reference))?;
        let dialplan_config = config.to_dialplan_config()?;
        self.run_ivr(session, &dialplan_config).await
    }

    fn execute_flow<'a>(
        &'a self,
        session: &'a mut CallSession,
        flow: &'a DialplanFlow,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match flow {
                DialplanFlow::Targets(strategy) => self.execute_targets(session, strategy).await,
                DialplanFlow::Queue { plan, next } => {
                    self.execute_queue_plan(session, plan, next, 0, true).await
                }
                DialplanFlow::Ivr(config) => self.run_ivr(session, config).await,
            }
        }
        .boxed()
    }

    async fn execute_targets(
        &self,
        session: &mut CallSession,
        strategy: &DialStrategy,
    ) -> Result<()> {
        let run_future = self.run_targets(session, strategy);
        let result = if let Some(timeout_duration) = self.forwarding_timeout() {
            match timeout(timeout_duration, run_future).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    session.set_error(
                        StatusCode::RequestTimeout,
                        Some("Call forwarding timeout elapsed".to_string()),
                        None,
                    );
                    Err(anyhow!("forwarding timeout elapsed"))
                }
            }
        } else {
            run_future.await
        };

        match result {
            Ok(_) => {
                info!(session_id = %self.session_id, "Dialplan executed successfully");
                Ok(())
            }
            Err(_) => self.handle_failure(session).await,
        }
    }

    async fn run_targets(&self, session: &mut CallSession, strategy: &DialStrategy) -> Result<()> {
        info!(
            session_id = %self.session_id,
            strategy = %strategy,
            media_proxy = session.use_media_proxy,
            "executing dialplan"
        );

        match strategy {
            DialStrategy::Sequential(targets) => self.dial_sequential(session, targets).await,
            DialStrategy::Parallel(targets) => self.dial_parallel(session, targets).await,
        }
    }

    async fn handle_ivr_exit(&self, session: &mut CallSession, exit: IvrExit) -> Result<()> {
        match exit {
            IvrExit::Completed => Ok(()),
            IvrExit::Transfer(action) => self.handle_ivr_transfer(session, action).await,
            IvrExit::Hangup(hangup) => {
                let status = hangup.code.map(StatusCode::from).unwrap_or(StatusCode::OK);
                session.set_error(status, hangup.reason.clone(), None);
                Ok(())
            }
            IvrExit::Queue(_) => Err(anyhow!("Queue actions from IVR are not supported yet")),
            IvrExit::Webhook(_) => Err(anyhow!("Webhook actions from IVR are not supported yet")),
            IvrExit::Playback(_) => Err(anyhow!("Playback-only exits are not supported yet")),
        }
    }

    async fn handle_ivr_transfer(
        &self,
        session: &mut CallSession,
        action: ResolvedTransferAction,
    ) -> Result<()> {
        let uri = Uri::try_from(action.target.as_str())?;
        let mut location = Location::default();
        location.aor = uri.clone();
        location.contact_raw = Some(uri.to_string());
        if !action.headers.is_empty() {
            let mut headers = Vec::new();
            for (name, value) in action.headers {
                headers.push(rsip::Header::Other(name, value));
            }
            location.headers = Some(headers);
        }
        if let Some(caller_id) = action.caller_id {
            session.routed_caller = Some(caller_id);
        }
        self.try_single_target(session, &location)
            .await
            .map_err(|(code, reason)| {
                session.set_error(code.clone(), reason.clone(), None);
                anyhow!(
                    "IVR transfer failed: {}",
                    reason.unwrap_or_else(|| code.to_string())
                )
            })
    }

    async fn run_ivr(&self, session: &mut CallSession, config: &DialplanIvrConfig) -> Result<()> {
        session.stop_queue_hold().await;
        let plan = config
            .plan
            .as_ref()
            .ok_or_else(|| anyhow!("IVR config requires inline plan data"))?
            .clone();
        let plan = Arc::new(plan);

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let availability_override = config.availability_override;
        let executor_plan = plan.clone();
        let executor_handle = tokio::task::spawn_blocking(move || {
            let mut runtime = BlockingIvrRuntime::new(cmd_tx);
            let mut executor = IvrExecutor::new(executor_plan.as_ref(), &mut runtime)
                .with_availability_override(availability_override);
            executor.run()
        });

        {
            let event_rx = session.event_sender.subscribe();
            let mut processor = IvrCommandProcessor::new(session, event_rx);
            while let Some(command) = cmd_rx.recv().await {
                if let Err(err) = processor.process(command).await {
                    warn!(
                        session_id = %self.session_id,
                        error = %err,
                        "failed to process IVR command"
                    );
                    return Err(err);
                }
            }
        }

        let exit = executor_handle
            .await
            .map_err(|e| anyhow!("IVR executor panic: {e}"))??;
        self.handle_ivr_exit(session, exit).await
    }

    async fn execute_queue_plan(
        &self,
        session: &mut CallSession,
        plan: &QueuePlan,
        next: &DialplanFlow,
        depth: usize,
        propagate_failure: bool,
    ) -> Result<()> {
        if depth >= MAX_QUEUE_CHAIN_DEPTH {
            warn!(
                session_id = %self.session_id,
                depth,
                "queue chain exceeded maximum depth"
            );
            return self.handle_failure(session).await;
        }
        if plan.accept_immediately {
            session.accept_call(None, None).await?;
        } else if !session.early_media_sent {
            session.start_ringing(String::new(), self).await;
        }

        if let Some(hold) = plan.hold.clone() {
            if let Err(e) = session.start_queue_hold(hold).await {
                warn!(session_id = %self.session_id, error = %e, "Failed to start queue hold track");
            }
        }

        let dial_future = self.execute_flow(session, next);
        let dial_result = if let Some(timeout_duration) = plan.ring_timeout {
            match timeout(timeout_duration, dial_future).await {
                Ok(result) => result,
                Err(_) => {
                    warn!(
                        session_id = %self.session_id,
                        timeout = ?timeout_duration,
                        "queue dial attempt timed out"
                    );
                    Err(anyhow!("queue dial timed out after {:?}", timeout_duration))
                }
            }
        } else {
            dial_future.await
        };

        session.stop_queue_hold().await;

        match dial_result {
            Ok(_) => Ok(()),
            Err(_) => {
                if let Some(fallback) = &plan.fallback {
                    self.execute_queue_fallback(session, fallback, next, depth, propagate_failure)
                        .await
                } else if propagate_failure {
                    self.handle_failure(session).await
                } else {
                    Err(anyhow!("queue plan execution failed"))
                }
            }
        }
    }

    fn execute_queue_fallback<'a>(
        &'a self,
        session: &'a mut CallSession,
        fallback: &'a QueueFallbackAction,
        next: &'a DialplanFlow,
        depth: usize,
        propagate_failure: bool,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match fallback {
                QueueFallbackAction::Failure(action) => {
                    self.execute_failure_action(session, action).await
                }
                QueueFallbackAction::Redirect { target } => {
                    let mut location = Location::default();
                    location.aor = target.clone();
                    match self.try_single_target(session, &location).await {
                        Ok(_) => Ok(()),
                        Err(_) => {
                            if propagate_failure {
                                self.handle_failure(session).await
                            } else {
                                Err(anyhow!("queue fallback redirect target failed to answer"))
                            }
                        }
                    }
                }
                QueueFallbackAction::Queue { name } => {
                    let config = match self.server.data_context.resolve_queue_config(name).await {
                        Ok(Some(cfg)) => cfg,
                        Ok(None) => {
                            warn!(
                                session_id = %self.session_id,
                                queue = name,
                                "queue fallback references unknown queue"
                            );
                            return self.handle_failure(session).await;
                        }
                        Err(err) => {
                            warn!(
                                session_id = %self.session_id,
                                queue = name,
                                error = %err,
                                "failed to load queue fallback config"
                            );
                            return self.handle_failure(session).await;
                        }
                    };
                    match config.to_queue_plan() {
                        Ok(plan) => {
                            self.execute_queue_plan(
                                session,
                                &plan,
                                next,
                                depth + 1,
                                propagate_failure,
                            )
                            .await
                        }
                        Err(err) => {
                            warn!(
                                session_id = %self.session_id,
                                queue = name,
                                error = %err,
                                "failed to build fallback queue plan"
                            );
                            if propagate_failure {
                                self.handle_failure(session).await
                            } else {
                                Err(anyhow!("queue fallback plan failed"))
                            }
                        }
                    }
                }
            }
        }
        .boxed()
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
                    session.note_attempt_failure(
                        code.clone(),
                        reason.clone(),
                        Some(target.aor.to_string()),
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
                None,
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
                    None,
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

            let invite_option = InviteOption {
                callee: target.aor.clone(),
                caller: caller.clone(),
                content_type,
                offer,
                destination: target.destination.clone(),
                contact: local_contact.clone().unwrap_or_else(|| caller.clone()),
                credential: target.credential.clone(),
                headers: self.dialplan.build_invite_headers(&target),
                ..Default::default()
            };
            let invite_caller = invite_option.caller.to_string();
            let invite_callee = invite_option.callee.to_string();
            let invite_contact = invite_option.contact.to_string();
            let invite_destination = invite_option
                .destination
                .as_ref()
                .map(|addr| addr.to_string());

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
                let caller_snapshot = invite_caller.clone();
                let callee_snapshot = invite_callee.clone();
                let contact_snapshot = invite_contact.clone();
                let destination_snapshot = invite_destination.clone();
                async move {
                    let invite_result = dialog_layer.do_invite(invite_option, state_tx).await;
                    match invite_result {
                        Ok((_, resp_opt)) => {
                            if let Some(resp) = resp_opt {
                                if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
                                    let answer = String::from_utf8_lossy(resp.body()).to_string();
                                    let _ = ev_tx_c.send(ParallelEvent::Accepted {
                                        idx,
                                        answer,
                                        aor,
                                        caller_uri: caller_snapshot,
                                        callee_uri: callee_snapshot,
                                        contact: contact_snapshot,
                                        destination: destination_snapshot,
                                    });
                                } else {
                                    let reason = resp.reason_phrase().clone().map(Into::into);
                                    let _ = ev_tx_c.send(ParallelEvent::Failed {
                                        idx,
                                        code: resp.status_code,
                                        reason,
                                        target: Some(invite_callee.clone()),
                                    });
                                }
                            } else {
                                let _ = ev_tx_c.send(ParallelEvent::Failed {
                                    idx,
                                    code: StatusCode::RequestTerminated,
                                    reason: Some("Cancelled by callee".to_string()),
                                    target: Some(invite_callee.clone()),
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
                                idx,
                                code,
                                reason,
                                target: Some(invite_callee.clone()),
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
                    answer,
                    aor,
                    caller_uri,
                    callee_uri,
                    contact,
                    destination,
                } => {
                    session.routed_caller = Some(caller_uri.clone());
                    session.routed_callee = Some(callee_uri.clone());
                    session.routed_contact = Some(contact.clone());
                    session.routed_destination = destination.clone();
                    if accepted_idx.is_none() {
                        if let Err(e) = session.accept_call(Some(aor), Some(answer)).await {
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
                    target,
                    ..
                } => {
                    failures += 1;
                    session.set_error(code, reason, target);
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

        let enforced_contact = self.local_contact_uri();
        let invite_option = InviteOption {
            caller_display_name: caller_display_name.cloned(),
            callee: target.aor.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination: target.destination.clone(),
            contact: enforced_contact.clone().unwrap_or_else(|| caller.clone()),
            credential: target.credential.clone(),
            headers: self.dialplan.build_invite_headers(&target),
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
                RouteResult::Forward(option)
                | RouteResult::Queue { option, .. }
                | RouteResult::Ivr { option, .. } => option,
                RouteResult::Abort(code, reason) => {
                    warn!(session_id = self.session_id, %code, ?reason, "route abort");
                    return Err((code, reason));
                }
            }
        } else {
            invite_option
        };

        if let Some(contact_uri) = enforced_contact {
            // Preserve the PBX contact even if routing logic rewrites the From header.
            invite_option.contact = contact_uri;
        }

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
        let (state_tx, state_rx) = mpsc::unbounded_channel();
        let dialog_layer = self.dialog_layer.clone();
        session.note_invite_details(&invite_option);

        let mut state_rx_guard = DialogStateReceiverGuard::new(dialog_layer.clone(), state_rx);

        let invite_loop = async {
            match dialog_layer.do_invite(invite_option, state_tx).await {
                Ok((_, resp)) => {
                    if let Some(resp) = resp {
                        if resp.status_code.kind() != rsip::StatusCodeKind::Successful {
                            let reason = resp.reason_phrase().clone().map(Into::into);
                            Err((resp.status_code, reason))
                        } else {
                            Ok(())
                        }
                    } else {
                        Err((
                            StatusCode::RequestTerminated,
                            Some("Cancelled by callee".to_string()),
                        ))
                    }
                }
                Err(e) => {
                    debug!(session_id = %self.session_id, "Invite failed: {:?}", e);
                    Err(match e {
                        rsipstack::Error::DialogError(reason, _, code) => (code, Some(reason)),
                        _ => (
                            StatusCode::ServerInternalError,
                            Some("Invite failed".to_string()),
                        ),
                    })
                }
            }
        };
        let (_, r) = tokio::join!(
            state_rx_guard.handle_proxy_call_state(&self, session, target),
            invite_loop
        );
        r
    }

    async fn handle_failure(&self, session: &mut CallSession) -> Result<()> {
        if self.try_call_forwarding_on_failure(session).await? {
            return Ok(());
        }
        self.execute_failure_action(session, &self.dialplan.failure_action)
            .await
    }

    async fn execute_failure_action(
        &self,
        session: &mut CallSession,
        action: &FailureAction,
    ) -> Result<()> {
        match action {
            FailureAction::Hangup { code, reason } => {
                let status_code = code
                    .as_ref()
                    .cloned()
                    .unwrap_or(StatusCode::ServiceUnavailable);
                if !session.has_error() {
                    session.set_error(status_code, reason.clone(), None);
                }
            }
            FailureAction::PlayThenHangup {
                audio_file,
                status_code,
                reason,
            } => {
                session.set_error(status_code.clone(), reason.clone(), None);
                if !session.is_answered() {
                    if let Err(err) = session.accept_call(None, None).await {
                        warn!(
                            session_id = %self.session_id,
                            error = %err,
                            "failed to accept call before playing failure prompt"
                        );
                        return Err(err);
                    }
                }
                if let Err(err) = session
                    .play_ringtone(audio_file, Some(self.event_sender.subscribe()))
                    .await
                {
                    warn!(
                        session_id = %self.session_id,
                        error = %err,
                        "failed to play failure prompt"
                    );
                    return Err(err);
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
                    None,
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

        let original_caller = session
            .original_caller
            .clone()
            .or_else(|| self.dialplan.caller.as_ref().map(|c| c.to_string()))
            .unwrap_or_default();

        let original_callee = session
            .original_callee
            .clone()
            .or_else(|| {
                self.dialplan
                    .original
                    .to_header()
                    .ok()
                    .and_then(|to_header| to_header.uri().ok().map(|uri| uri.to_string()))
            })
            .or_else(|| {
                self.dialplan
                    .first_target()
                    .map(|location| location.aor.to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());

        let caller = session
            .routed_caller
            .clone()
            .unwrap_or_else(|| original_caller.clone());

        let callee = session
            .routed_callee
            .clone()
            .or_else(|| session.connected_callee.clone())
            .unwrap_or_else(|| original_callee.clone());

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

        let mut hangup_messages = session.recorded_hangup_messages();
        if hangup_messages.is_empty() {
            if let Some((code, reason)) = session.last_error.as_ref() {
                hangup_messages.push(CallRecordHangupMessage {
                    code: u16::from(code.clone()),
                    reason: reason.clone(),
                    target: None,
                });
            }
        }
        if !hangup_messages.is_empty() {
            if let Ok(value) = serde_json::to_value(&hangup_messages) {
                extras_map.insert("hangup_messages".to_string(), value);
            }
        }

        let mut rewrite_payload = JsonMap::new();
        rewrite_payload.insert(
            "caller_original".to_string(),
            Value::String(original_caller.clone()),
        );
        rewrite_payload.insert("caller_final".to_string(), Value::String(caller.clone()));
        rewrite_payload.insert(
            "callee_original".to_string(),
            Value::String(original_callee.clone()),
        );
        rewrite_payload.insert("callee_final".to_string(), Value::String(callee.clone()));
        if let Some(contact) = session.routed_contact.as_ref() {
            rewrite_payload.insert("contact".to_string(), Value::String(contact.clone()));
        }
        if let Some(destination) = session.routed_destination.as_ref() {
            rewrite_payload.insert(
                "destination".to_string(),
                Value::String(destination.clone()),
            );
        }
        extras_map.insert("rewrite".to_string(), Value::Object(rewrite_payload));

        let mut sip_flows_map: HashMap<String, Vec<SipMessageItem>> = HashMap::new();
        let server_dialog_id = session.server_dialog.id();
        let mut call_ids: HashSet<String> = HashSet::new();
        call_ids.insert(server_dialog_id.call_id.clone());

        for dialog_id in &session.callee_dialogs {
            call_ids.insert(dialog_id.call_id.clone());
        }

        let mut sip_leg_roles: HashMap<String, String> = HashMap::new();
        sip_leg_roles.insert(server_dialog_id.call_id.clone(), "primary".to_string());
        for dialog_id in &session.callee_dialogs {
            sip_leg_roles
                .entry(dialog_id.call_id.clone())
                .or_insert_with(|| "b2bua".to_string());
        }

        for call_id in call_ids.iter() {
            if let Some(items) = self.server.drain_sip_flow(call_id) {
                if !items.is_empty() {
                    sip_flows_map.insert(call_id.clone(), items);
                }
            }
        }

        let mut record = CallRecord {
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
            hangup_messages: hangup_messages.clone(),
            recorder: Vec::new(),
            extras: None,
            dump_event_file: None,
            refer_callrecord: None,
            sip_flows: sip_flows_map,
            sip_leg_roles,
        };

        if self.dialplan.recording.enabled {
            if let Some(recorder_config) = self.dialplan.recording.option.as_ref() {
                if !recorder_config.recorder_file.is_empty() {
                    let size = fs::metadata(&recorder_config.recorder_file)
                        .map(|meta| meta.len())
                        .unwrap_or(0);
                    record.recorder.push(CallRecordMedia {
                        track_id: "mixed".to_string(),
                        path: recorder_config.recorder_file.clone(),
                        size,
                        extra: None,
                    });
                }
            }
        }

        let recording_path_for_db = record.recorder.first().map(|media| media.path.clone());

        apply_record_file_extras(&record, &mut extras_map);
        record.extras = extras_map_to_option(&extras_map);

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

        let metadata_value = extras_map_to_metadata(&extras_map);

        let mut persist_args = CallRecordPersistArgs::default();
        persist_args.direction = direction;
        persist_args.status = status;
        persist_args.from_number = from_number;
        persist_args.to_number = to_number;
        persist_args.sip_trunk_id = sip_trunk_id;
        persist_args.sip_gateway = sip_gateway;
        persist_args.metadata = metadata_value;
        persist_args.recording_url = recording_path_for_db;

        let (persist_error, send_error) = persist_and_dispatch_record(
            self.server.database.as_ref(),
            self.call_record_sender.as_ref(),
            record,
            persist_args,
        )
        .await;

        if let Some(err) = persist_error {
            warn!(
                session_id = %self.session_id,
                error = %err,
                "Failed to persist call record"
            );
        }
        if let Some(err) = send_error {
            warn!(
                session_id = %self.session_id,
                error = %err,
                "Failed to send call record"
            );
        }
    }
}

fn compute_remote_target_from_request(
    request: &rsip::Request,
) -> Option<(Uri, rsip::headers::untyped::Contact)> {
    let via = request.via_header().ok()?;
    let (via_transport, via_received) = SipConnection::parse_target_from_via(via).ok()?;
    let contact_header = request.contact_header().ok()?;

    let original_contact =
        rsip::headers::untyped::Contact::from(contact_header.value().to_string());
    let mut uri = original_contact.uri().ok()?;

    uri.host_with_port = via_received;
    uri.params.retain(|p| !matches!(p, Param::Transport(_)));
    if via_transport != rsip::transport::Transport::Udp {
        uri.params.push(Param::Transport(via_transport));
    }

    let remote_contact = original_contact
        .clone()
        .with_uri(uri.clone())
        .unwrap_or(original_contact);

    Some((uri, remote_contact))
}

impl Drop for ProxyCall {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

type PromptResponseSender = std_mpsc::Sender<Result<PromptPlayback>>;
type InputResponseSender = std_mpsc::Sender<Result<InputEvent>>;

enum IvrRuntimeCommand {
    PlayPrompt {
        step_id: String,
        prompt: PromptMedia,
        allow_barge_in: bool,
        responder: PromptResponseSender,
    },
    CollectInput {
        step_id: String,
        input: InputStep,
        attempt: u32,
        responder: InputResponseSender,
    },
}

struct BlockingIvrRuntime {
    command_tx: mpsc::UnboundedSender<IvrRuntimeCommand>,
}

impl BlockingIvrRuntime {
    fn new(command_tx: mpsc::UnboundedSender<IvrRuntimeCommand>) -> Self {
        Self { command_tx }
    }

    fn send(&self, command: IvrRuntimeCommand) -> Result<()> {
        self.command_tx
            .send(command)
            .map_err(|_| anyhow!("IVR command processor dropped"))
    }

    fn await_response<T>(receiver: std_mpsc::Receiver<Result<T>>) -> Result<T> {
        Ok(receiver
            .recv()
            .map_err(|_| anyhow!("IVR command response channel closed"))??)
    }
}

impl IvrRuntime for BlockingIvrRuntime {
    fn play_prompt(
        &mut self,
        _plan: &IvrPlan,
        step_id: &str,
        prompt: &PromptMedia,
        allow_barge_in: bool,
    ) -> Result<PromptPlayback> {
        let (tx, rx) = std_mpsc::channel();
        self.send(IvrRuntimeCommand::PlayPrompt {
            step_id: step_id.to_string(),
            prompt: prompt.clone(),
            allow_barge_in,
            responder: tx,
        })?;
        Self::await_response(rx)
    }

    fn collect_input(
        &mut self,
        _plan: &IvrPlan,
        step_id: &str,
        input: &InputStep,
        attempt: u32,
    ) -> Result<InputEvent> {
        let (tx, rx) = std_mpsc::channel();
        self.send(IvrRuntimeCommand::CollectInput {
            step_id: step_id.to_string(),
            input: input.clone(),
            attempt,
            responder: tx,
        })?;
        Self::await_response(rx)
    }
}

struct IvrCommandProcessor<'a> {
    session: &'a mut CallSession,
    events: IvrEventStream,
    prompt_counter: u64,
}

impl<'a> IvrCommandProcessor<'a> {
    fn new(session: &'a mut CallSession, receiver: EventReceiver) -> Self {
        Self {
            session,
            events: IvrEventStream::new(receiver),
            prompt_counter: 0,
        }
    }

    async fn process(&mut self, command: IvrRuntimeCommand) -> Result<()> {
        match command {
            IvrRuntimeCommand::PlayPrompt {
                prompt,
                allow_barge_in,
                responder,
                ..
            } => {
                let result = self.handle_play_prompt(prompt, allow_barge_in).await;
                let _ = responder.send(result);
            }
            IvrRuntimeCommand::CollectInput {
                input, responder, ..
            } => {
                let result = self.handle_collect_input(input).await;
                let _ = responder.send(result);
            }
        }
        Ok(())
    }

    async fn ensure_answered(&mut self) -> Result<()> {
        if !self.session.is_answered() {
            self.session.accept_call(None, None).await?;
        }
        Ok(())
    }

    fn next_play_id(&mut self) -> String {
        self.prompt_counter += 1;
        format!("ivr-prompt-{}", self.prompt_counter)
    }

    async fn handle_play_prompt(
        &mut self,
        prompt: PromptMedia,
        allow_barge_in: bool,
    ) -> Result<PromptPlayback> {
        self.ensure_answered().await?;
        let repeat = prompt.loop_count.unwrap_or(1).max(1) as usize;
        let mut last = PromptPlayback::Completed;
        for _ in 0..repeat {
            let play_id = self.next_play_id();
            self.start_prompt_track(&prompt, &play_id).await?;
            let result = self
                .events
                .wait_for_prompt(play_id.clone(), allow_barge_in)
                .await;
            self.stop_prompt_track().await;
            last = result?;
            if matches!(last, PromptPlayback::BargeIn) {
                break;
            }
        }
        Ok(last)
    }

    async fn start_prompt_track(&mut self, prompt: &PromptMedia, play_id: &str) -> Result<()> {
        match &prompt.source {
            PromptSource::File { file } => {
                let track = FileTrack::new(CallSession::IVR_PROMPT_TRACK_ID.to_string())
                    .with_path(file.path.clone())
                    .with_ssrc(rand::random::<u32>())
                    .with_cancel_token(self.session.media_stream.cancel_token.child_token());
                self.session
                    .media_stream
                    .update_track(Box::new(track), Some(play_id.to_string()))
                    .await;
                Ok(())
            }
            PromptSource::Tts { .. } => Err(anyhow!("TTS prompts are not supported yet")),
            PromptSource::Url { .. } => Err(anyhow!("URL prompts are not supported yet")),
        }
    }

    async fn stop_prompt_track(&mut self) {
        self.session
            .media_stream
            .remove_track(&CallSession::IVR_PROMPT_TRACK_ID.to_string(), false)
            .await;
    }

    async fn handle_collect_input(&mut self, input: InputStep) -> Result<InputEvent> {
        self.ensure_answered().await?;
        let mut digits = String::new();
        self.events
            .drain_pending(&mut digits, input.max_digits.unwrap_or(usize::MAX));
        if let Some(max) = input.max_digits {
            if digits.len() >= max {
                return Ok(InputEvent::Digits(digits));
            }
        }

        loop {
            let timeout_ms = input.timeout_ms.unwrap_or(5_000);
            match self
                .events
                .next_digit(Some(Duration::from_millis(timeout_ms)))
                .await?
            {
                DigitWaitResult::Digit(digit) => {
                    digits.push_str(&digit);
                    if let Some(max) = input.max_digits {
                        if digits.len() >= max {
                            return Ok(InputEvent::Digits(digits));
                        }
                    }
                }
                DigitWaitResult::Timeout => {
                    if digits.is_empty() {
                        return Ok(InputEvent::Timeout);
                    }
                    return Ok(InputEvent::Digits(digits));
                }
                DigitWaitResult::Cancelled => return Ok(InputEvent::Cancel),
            }
        }
    }
}

struct IvrEventStream {
    receiver: EventReceiver,
    pending_digits: VecDeque<String>,
}

impl IvrEventStream {
    fn new(receiver: EventReceiver) -> Self {
        Self {
            receiver,
            pending_digits: VecDeque::new(),
        }
    }

    fn drain_pending(&mut self, buffer: &mut String, max: usize) {
        while buffer.len() < max {
            if let Some(digit) = self.pending_digits.pop_front() {
                buffer.push_str(&digit);
            } else {
                break;
            }
        }
    }

    async fn wait_for_prompt(
        &mut self,
        play_id: String,
        allow_barge_in: bool,
    ) -> Result<PromptPlayback> {
        loop {
            match self.receiver.recv().await {
                Ok(SessionEvent::TrackEnd {
                    play_id: Some(id), ..
                }) if id == play_id => {
                    return Ok(PromptPlayback::Completed);
                }
                Ok(SessionEvent::Dtmf { digit, .. }) => {
                    self.pending_digits.push_back(digit.clone());
                    if allow_barge_in {
                        return Ok(PromptPlayback::BargeIn);
                    }
                }
                Ok(SessionEvent::Hangup { .. }) => {
                    return Err(anyhow!("call ended during IVR prompt"));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(anyhow!("event channel closed"));
                }
                Err(broadcast::error::RecvError::Lagged(_)) | Ok(_) => {
                    continue;
                }
            }
        }
    }

    async fn next_digit(&mut self, timeout: Option<Duration>) -> Result<DigitWaitResult> {
        if let Some(digit) = self.pending_digits.pop_front() {
            return Ok(DigitWaitResult::Digit(digit));
        }

        if let Some(duration) = timeout {
            match tokio::time::timeout(duration, self.receiver.recv()).await {
                Ok(result) => self.handle_digit_event(result),
                Err(_) => Ok(DigitWaitResult::Timeout),
            }
        } else {
            let event = self.receiver.recv().await;
            self.handle_digit_event(event)
        }
    }

    fn handle_digit_event(
        &mut self,
        event: Result<SessionEvent, broadcast::error::RecvError>,
    ) -> Result<DigitWaitResult> {
        match event {
            Ok(SessionEvent::Dtmf { digit, .. }) => Ok(DigitWaitResult::Digit(digit)),
            Ok(SessionEvent::Hangup { .. }) => Ok(DigitWaitResult::Cancelled),
            Ok(_) => Ok(DigitWaitResult::Timeout),
            Err(broadcast::error::RecvError::Lagged(_)) => Ok(DigitWaitResult::Timeout),
            Err(broadcast::error::RecvError::Closed) => Ok(DigitWaitResult::Cancelled),
        }
    }
}

enum DigitWaitResult {
    Digit(String),
    Timeout,
    Cancelled,
}

#[async_trait]
trait ProxyCallDialogStateReceiverGuard {
    async fn handle_proxy_call_state(
        &mut self,
        proxy_call: &ProxyCall,
        session: &mut CallSession,
        target: &Location,
    ) -> Result<()>;
}

#[async_trait]
impl ProxyCallDialogStateReceiverGuard for DialogStateReceiverGuard {
    async fn handle_proxy_call_state(
        &mut self,
        proxy_call: &ProxyCall,
        session: &mut CallSession,
        target: &Location,
    ) -> Result<()> {
        while let Some(state) = self.recv().await {
            match state {
                DialogState::Terminated(dialog_id, reason) => {
                    info!(
                        session_id = proxy_call.session_id,
                        %dialog_id,
                        ?reason,
                        "Established call terminated by callee"
                    );
                    session.add_callee_dialog(dialog_id);
                    session.callee_terminated(reason).await;
                    return Ok(());
                }
                DialogState::Calling(dialog_id) => {
                    session.add_callee_dialog(dialog_id);
                }
                DialogState::Early(dialog_id, response) => {
                    proxy_call.update_remote_target_from_response(&dialog_id, &response);
                    let answer = String::from_utf8_lossy(response.body()).to_string();
                    info!(
                        session_id = proxy_call.session_id,
                        %dialog_id,
                        status = ?response.status_code,
                        %answer,
                        "Callee dialog is ringing/early"
                    );
                    session.start_ringing(answer, proxy_call).await;
                }
                DialogState::Confirmed(dialog_id, response) => {
                    info!(
                        session_id = proxy_call.session_id,
                        %dialog_id,
                        status = ?response.status_code,
                        "Callee dialog confirmed"
                    );
                    proxy_call.update_remote_target_from_response(&dialog_id, &response);
                    session.add_callee_dialog(dialog_id);
                    let answer = String::from_utf8_lossy(response.body()).to_string();
                    if let Err(e) = session
                        .accept_call(Some(target.aor.to_string()), Some(answer))
                        .await
                    {
                        warn!(
                            session_id = proxy_call.session_id,
                            error = %e,
                            "Failed to accept call on confirmed callee dialog"
                        );
                        break;
                    }
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
                        session_id = proxy_call.session_id,
                        "Received state in established call: {}", other_state
                    );
                }
            }
        }
        Ok(())
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
    use std::net::SocketAddr;

    use super::compute_remote_target_from_request;
    use crate::call::Dialplan;
    use rsip::headers::UntypedHeader;
    use rsip::{Headers, Param};
    use rsipstack::{
        EndpointBuilder,
        dialog::{dialog_layer::DialogLayer, invitation::InviteOption},
        transport::{TransportLayer, udp::UdpConnection},
    };
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_build_target_headers() -> Result<(), anyhow::Error> {
        let req = rsip::Request {
            method: rsip::Method::Invite,
            uri: "sip:bob@localhost".try_into().expect("uri"),
            version: rsip::Version::V2,
            headers: Headers::default(),
            body: vec![],
        };
        let plan = Dialplan::new(
            "mock".to_string(),
            req,
            crate::call::DialDirection::Outbound,
        );
        let loc = crate::call::Location {
            aor: "sip:1001@domain.com".try_into().expect("uri"),
            headers: Some(vec![
                rsip::Header::Other("X-Custom-Header".to_string(), "CustomValue-1".to_string()),
                rsip::Header::Other("X-Custom-Header".to_string(), "CustomValue-2".to_string()),
            ]),
            ..Default::default()
        };
        let headers = plan.build_invite_headers(&loc).expect("build headers");
        assert_eq!(headers.len(), 2);
        assert!(headers.iter().any(|h| match h {
            rsip::Header::Other(name, value) => {
                name == "X-Custom-Header" && value == "CustomValue-1"
            }
            _ => false,
        }));
        let transport_layer = TransportLayer::new(CancellationToken::new());
        let local_addr = format!("127.0.0.1:0").parse::<SocketAddr>()?;

        // Setup transport
        let connection = UdpConnection::create_connection(local_addr, None, None)
            .await
            .expect("create udp connection");
        transport_layer.add_transport(connection.into());

        let endpoint = EndpointBuilder::new()
            .with_transport_layer(transport_layer)
            .build();
        let dialog_layer = DialogLayer::new(endpoint.inner.clone());
        let invite_option = InviteOption {
            caller: "sip:hello@test".try_into().expect("uri"),
            callee: loc.aor.clone(),
            headers: plan.build_invite_headers(&loc),
            ..Default::default()
        };
        let req = dialog_layer
            .make_invite_request(&invite_option)
            .expect("build invite request");
        assert!(req.headers.iter().any(|h| match h {
            rsip::Header::Other(name, value) => {
                name == "X-Custom-Header" && value == "CustomValue-1"
            }
            _ => false,
        }));
        assert!(req.headers.iter().any(|h| match h {
            rsip::Header::Other(name, value) => {
                name == "X-Custom-Header" && value == "CustomValue-2"
            }
            _ => false,
        }));
        Ok(())
    }

    #[test]
    fn test_compute_remote_target_rewrites_using_received_rport() {
        let request = rsip::Request {
            method: rsip::Method::Invite,
            uri: rsip::Uri::try_from("sip:bob@rustpbx.com").expect("uri"),
            version: rsip::Version::V2,
            headers: vec![
                rsip::headers::Via::new(
                    "SIP/2.0/UDP proxy.rustpbx.com:5060;branch=z9hG4bK-1;received=198.51.100.5;rport=5090",
                )
                .into(),
                rsip::headers::Contact::new(
                    "<sip:alice@10.0.0.5:5060;transport=udp>",
                )
                .into(),
                rsip::headers::From::new("Alice <sip:alice@rustpbx.com>;tag=from-tag").into(),
                rsip::headers::To::new("Bob <sip:bob@rustpbx.com>").into(),
                rsip::headers::CallId::new("callid-1").into(),
                rsip::headers::CSeq::new("1 INVITE").into(),
                rsip::headers::MaxForwards::new("70").into(),
            ]
            .into(),
            body: Vec::new(),
        };

        let (uri, contact) =
            compute_remote_target_from_request(&request).expect("computed remote target");

        assert_eq!(uri.host_with_port.host.to_string(), "198.51.100.5");
        assert_eq!(
            uri.host_with_port.port.as_ref().map(|p| p.to_string()),
            Some("5090".to_string())
        );
        assert!(!uri.params.iter().any(|p| matches!(p, Param::Transport(_))));

        let contact_uri = contact.uri().expect("contact uri");
        assert_eq!(contact_uri.host_with_port.host.to_string(), "198.51.100.5");
        assert_eq!(
            contact_uri
                .host_with_port
                .port
                .as_ref()
                .map(|p| p.to_string()),
            Some("5090".to_string())
        );
        assert!(
            !contact_uri
                .params
                .iter()
                .any(|p| matches!(p, Param::Transport(_)))
        );
    }

    #[test]
    fn test_compute_remote_target_adds_transport_param_when_needed() {
        let request = rsip::Request {
            method: rsip::Method::Invite,
            uri: rsip::Uri::try_from("sip:bob@rustpbx.com").expect("uri"),
            version: rsip::Version::V2,
            headers: vec![
                rsip::headers::Via::new(
                    "SIP/2.0/TCP proxy.rustpbx.com:5060;branch=z9hG4bK-2;received=203.0.113.9;rport=5070",
                )
                .into(),
                rsip::headers::Contact::new("<sip:alice@10.0.0.5>").into(),
                rsip::headers::From::new("Alice <sip:alice@rustpbx.com>;tag=from-tag").into(),
                rsip::headers::To::new("Bob <sip:bob@rustpbx.com>").into(),
                rsip::headers::CallId::new("callid-2").into(),
                rsip::headers::CSeq::new("1 INVITE").into(),
                rsip::headers::MaxForwards::new("70").into(),
            ]
            .into(),
            body: Vec::new(),
        };

        let (uri, contact) =
            compute_remote_target_from_request(&request).expect("computed remote target");

        assert_eq!(uri.host_with_port.host.to_string(), "203.0.113.9");
        assert_eq!(
            uri.host_with_port.port.as_ref().map(|p| p.to_string()),
            Some("5070".to_string())
        );
        assert!(
            uri.params
                .iter()
                .any(|p| matches!(p, Param::Transport(rsip::transport::Transport::Tcp)))
        );

        let contact_uri = contact.uri().expect("contact uri");
        assert_eq!(contact_uri.host_with_port.host.to_string(), "203.0.113.9");
        assert_eq!(
            contact_uri
                .host_with_port
                .port
                .as_ref()
                .map(|p| p.to_string()),
            Some("5070".to_string())
        );
        assert!(
            contact_uri
                .params
                .iter()
                .any(|p| matches!(p, Param::Transport(rsip::transport::Transport::Tcp)))
        );
    }
}
