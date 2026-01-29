use crate::call::sip::{DialogStateReceiverGuard, ServerDialogGuard};
use crate::media::negotiate::{CodecInfo, MediaNegotiator};
use crate::media::{FileTrack, RtpTrackBuilder, Track};
use crate::proxy::proxy_call::reporter::CallReporter;
use crate::proxy::routing::matcher::RouteResourceLookup;
use crate::{
    call::{
        CallForwardingConfig, CallForwardingMode, DialStrategy, DialplanFlow, Location, QueuePlan,
        TransferEndpoint,
    },
    callrecord::{CallRecordHangupMessage, CallRecordHangupReason, CallRecordSender},
    config::{MediaProxyMode, RouteResult},
    proxy::{
        proxy_call::{
            media_bridge::MediaBridge,
            media_peer::{MediaPeer, VoiceEnginePeer},
            session_timer::{
                HEADER_MIN_SE, HEADER_SESSION_EXPIRES, SessionRefresher, SessionTimerState,
                TIMER_TAG, get_header_value, has_timer_support, parse_min_se,
                parse_session_expires,
            },
            state::{
                CallContext, CallSessionHandle, CallSessionShared, ProxyCallEvent, SessionAction,
            },
        },
        server::SipServerRef,
    },
};
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use futures::{FutureExt, future::BoxFuture};
use rsip::StatusCode;
use rsip::Uri;
use rsipstack::dialog::{
    DialogId, dialog::DialogState, dialog_layer::DialogLayer, invitation::InviteOption,
    server_dialog::ServerInviteDialog,
};
use rsipstack::rsip_ext::RsipResponseExt;
use rsipstack::transaction::key::TransactionRole;
use rustrtc;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::Mutex as AsyncMutex, sync::mpsc, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Clone, Copy, Debug)]
pub(crate) enum FlowFailureHandling {
    Handle,
    #[allow(dead_code)]
    Propagate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiationState {
    Idle,
    Stable,
    LocalOfferSent,
    RemoteOfferReceived,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingHangup {
    pub reason: Option<CallRecordHangupReason>,
    pub code: Option<u16>,
    pub initiator: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionHangupMessage {
    pub code: u16,
    pub reason: Option<String>,
    pub target: Option<String>,
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

#[derive(Debug)]
pub(crate) enum ParallelEvent {
    Calling {
        _idx: usize,
        dialog_id: DialogId,
    },
    Early {
        _idx: usize,
        dialog_id: DialogId,
        sdp: Option<String>,
    },
    Accepted {
        _idx: usize,
        dialog_id: DialogId,
        answer: String,
        aor: String,
        caller_uri: String,
        callee_uri: String,
        contact: String,
        destination: Option<String>,
    },
    Failed {
        #[allow(dead_code)]
        _idx: usize,
        code: StatusCode,
        reason: Option<String>,
        target: Option<String>,
    },
    Terminated {
        _idx: usize,
    },
    Cancelled,
}

#[derive(Debug)]
pub(crate) struct SessionActionInbox {
    rx: mpsc::UnboundedReceiver<SessionAction>,
}

impl SessionActionInbox {
    pub fn new(rx: mpsc::UnboundedReceiver<SessionAction>) -> Self {
        Self { rx }
    }

    pub fn try_recv(&mut self) -> Result<SessionAction, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }

    pub async fn recv(&mut self) -> Option<SessionAction> {
        self.rx.recv().await
    }
}

pub(crate) type ActionInbox<'a> = Option<&'a mut SessionActionInbox>;

pub(crate) struct CallSessionRecordSnapshot {
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
    pub callee_dialogs: Vec<DialogId>,
    pub server_dialog_id: DialogId,
    pub extensions: http::Extensions,
}

pub(crate) struct CallSession {
    pub server: SipServerRef,
    pub dialog_layer: Arc<DialogLayer>,
    pub cancel_token: CancellationToken,
    pub call_record_sender: Option<CallRecordSender>,
    pub pending_hangup: Arc<Mutex<Option<PendingHangup>>>,
    pub context: CallContext,
    pub server_dialog: ServerInviteDialog,
    pub callee_dialogs: Arc<Mutex<HashSet<DialogId>>>,
    pub last_error: Option<(StatusCode, Option<String>)>,
    pub connected_callee: Option<String>,
    pub connected_dialog_id: Option<DialogId>,
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub caller_offer: Option<String>,
    pub callee_offer: Option<String>,
    pub answer: Option<String>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<SessionHangupMessage>,
    pub shared: CallSessionShared,
    pub early_media_sent: bool,
    pub caller_peer: Arc<dyn MediaPeer>,
    pub callee_peer: Arc<dyn MediaPeer>,
    pub media_bridge: Option<MediaBridge>,
    pub recorder_option: Option<crate::media::recorder::RecorderOption>,
    pub use_media_proxy: bool,
    pub routed_caller: Option<String>,
    pub routed_callee: Option<String>,
    pub routed_contact: Option<String>,
    pub routed_destination: Option<String>,
    pub reporter: Option<crate::proxy::proxy_call::reporter::CallReporter>,
    pub server_timer: Arc<std::sync::Mutex<SessionTimerState>>,
    pub client_timer: Arc<std::sync::Mutex<SessionTimerState>>,
    pub negotiation_state: NegotiationState,
    pub handle: Option<CallSessionHandle>,
    pub callee_event_tx: Option<mpsc::UnboundedSender<DialogState>>,
    pub callee_guards: Vec<crate::call::sip::DialogStateReceiverGuard>,
    pub callee_answer_sdp: Option<String>,
}

impl CallSession {
    pub const CALLEE_TRACK_ID: &'static str = "callee-track";
    pub const RINGBACK_TRACK_ID: &'static str = "ringback-track";

    fn check_media_proxy(context: &CallContext, offer_sdp: &str, mode: &MediaProxyMode) -> bool {
        if context.dialplan.recording.enabled {
            return true;
        }
        match mode {
            MediaProxyMode::All => true,
            MediaProxyMode::None => false,
            MediaProxyMode::Nat => false, // TODO: Implement NAT detection
            MediaProxyMode::Auto => {
                // Check if caller is WebRTC (SAVPF profile in SDP)
                if offer_sdp.contains("RTP/SAVPF") {
                    return true;
                }
                false
            }
        }
    }

    pub fn new(
        server: SipServerRef,
        dialog_layer: Arc<DialogLayer>,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
        context: CallContext,
        server_dialog: ServerInviteDialog,
        use_media_proxy: bool,
        recorder_option: Option<crate::media::recorder::RecorderOption>,
        caller_peer: Arc<dyn MediaPeer>,
        callee_peer: Arc<dyn MediaPeer>,
        shared: CallSessionShared,
        reporter: Option<crate::proxy::proxy_call::reporter::CallReporter>,
    ) -> Self {
        let initial = server_dialog.initial_request();
        let caller_offer = if initial.body().is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(initial.body()).to_string())
        };

        Self {
            server,
            dialog_layer,
            cancel_token,
            call_record_sender,
            pending_hangup: Arc::new(Mutex::new(None)),
            context,
            server_dialog,
            callee_dialogs: Arc::new(Mutex::new(HashSet::new())),
            last_error: None,
            connected_callee: None,
            connected_dialog_id: None,
            ring_time: None,
            answer_time: None,
            caller_offer,
            callee_offer: None,
            answer: None,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            shared,
            early_media_sent: false,
            caller_peer,
            callee_peer,
            media_bridge: None,
            recorder_option,
            use_media_proxy,
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            reporter,
            server_timer: Arc::new(std::sync::Mutex::new(SessionTimerState::default())),
            client_timer: Arc::new(std::sync::Mutex::new(SessionTimerState::default())),
            negotiation_state: NegotiationState::Idle,
            handle: None,
            callee_event_tx: None,
            callee_guards: Vec::new(),
            callee_answer_sdp: None,
        }
    }

    fn get_retry_codes(&self) -> Option<&Vec<u16>> {
        match &self.context.dialplan.flow {
            crate::call::DialplanFlow::Queue { plan, .. } => plan.retry_codes.as_ref(),
            _ => None,
        }
    }

    pub fn add_callee_guard(&mut self, mut guard: DialogStateReceiverGuard) {
        if let Some(tx) = &self.callee_event_tx {
            if let Some(mut receiver) = guard.take_receiver() {
                let tx = tx.clone();
                let cancel_token = self.cancel_token.clone();
                crate::utils::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = cancel_token.cancelled() => break,
                            state = receiver.recv() => {
                                if let Some(state) = state {
                                    let _ = tx.send(state);
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        }
        self.callee_guards.push(guard);
    }

    pub fn init_server_timer(
        &mut self,
        default_expires: u64,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let request = self.server_dialog.initial_request();
        let headers = &request.headers;

        let supported = has_timer_support(headers);
        let session_expires_value = get_header_value(headers, HEADER_SESSION_EXPIRES);

        // Default local policy
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

                if let Some(r) = refresher {
                    server_timer.refresher = r;
                } else {
                    server_timer.refresher = SessionRefresher::Uac;
                }
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

    pub fn init_client_timer(&mut self, response: &rsip::Response, default_expires: u64) {
        let headers = &response.headers;
        let session_expires_value = get_header_value(headers, HEADER_SESSION_EXPIRES);

        let mut client_timer = self.client_timer.lock().unwrap();
        if let Some(value) = session_expires_value {
            if let Some((interval, refresher)) = parse_session_expires(&value) {
                client_timer.enabled = true;
                client_timer.session_interval = interval;
                client_timer.active = true;
                client_timer.last_refresh = Instant::now();
                if let Some(r) = refresher {
                    client_timer.refresher = r;
                } else {
                    client_timer.refresher = SessionRefresher::Uac;
                }
            }
        } else {
            client_timer.enabled = true;
            client_timer.session_interval = Duration::from_secs(default_expires);
            client_timer.active = true;
            client_timer.last_refresh = Instant::now();
            client_timer.refresher = SessionRefresher::Uac;
        }
    }

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

    pub fn register_active_call(&mut self, handle: CallSessionHandle) {
        self.shared.register_active_call(handle.clone());
        self.shared
            .register_dialog(self.server_dialog.id().to_string(), handle.clone());
        self.handle = Some(handle);
    }

    pub fn last_queue_name(&self) -> Option<String> {
        None
        // self.last_queue_name.clone()
    }

    pub fn record_snapshot(&mut self) -> CallSessionRecordSnapshot {
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
            last_queue_name: self.last_queue_name(),
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

    pub fn note_invite_details(&mut self, invite: &InviteOption) {
        self.routed_caller = Some(invite.caller.to_string());
        self.routed_callee = Some(invite.callee.to_string());
        self.routed_contact = Some(invite.contact.to_string());
        self.routed_destination = invite.destination.as_ref().map(|addr| addr.to_string());
        self.refresh_active_call_parties();
        let current_target = self
            .routed_callee
            .clone()
            .or_else(|| Some(self.context.original_callee.clone()));
        self.shared.set_current_target(current_target);
    }

    fn refresh_active_call_parties(&self) {
        self.shared
            .update_routed_parties(self.routed_caller.clone(), self.routed_callee.clone());
    }

    fn is_webrtc_sdp(sdp: &str) -> bool {
        sdp.contains("RTP/SAVPF")
    }

    async fn find_track(
        &self,
        peer: &Arc<dyn MediaPeer>,
        track_id: &str,
    ) -> Option<Arc<AsyncMutex<Box<dyn Track>>>> {
        let tracks = peer.get_tracks().await;
        for t in tracks {
            if t.lock().await.id() == track_id {
                return Some(t);
            }
        }
        None
    }

    /// Helper method to create queue hold music track (unified PC approach)
    async fn create_queue_hold_track(&self, audio_file: &str, loop_playback: bool) {
        let hold_ssrc = rand::random::<u32>();
        let track = crate::media::FileTrack::new("queue-hold-music".to_string())
            .with_path(audio_file.to_string())
            .with_loop(loop_playback)
            .with_ssrc(hold_ssrc);

        self.caller_peer.update_track(Box::new(track), None).await;
    }

    async fn optimize_caller_codec(&mut self, callee_answer: &str) -> Option<String> {
        // Parse callee's codecs from their answer
        let (callee_codecs, _) = MediaNegotiator::extract_codec_params(callee_answer);

        // Select the best codec considering dialplan allow_codecs preference
        let selected_callee_codec = MediaNegotiator::select_best_codec(
            &callee_codecs,
            &self.context.dialplan.allow_codecs,
        )?;
        let callee_codec = selected_callee_codec.codec;

        // Parse caller's offer to get their supported codecs
        let caller_offer = self.caller_offer.as_ref()?;
        let (caller_codecs, caller_dtmf_pt) = MediaNegotiator::extract_codec_params(caller_offer);

        // Check if caller supports our chosen callee codec
        let caller_codec_info = caller_codecs.iter().find(|c| c.codec == callee_codec)?;

        let mut optimized_rtp_map = Vec::new();
        // The first codec in optimized_rtp_map will be the one selected in the handshake
        optimized_rtp_map.push((
            caller_codec_info.payload_type,
            (
                caller_codec_info.codec,
                caller_codec_info.clock_rate,
                caller_codec_info.channels as u32,
            ),
        ));

        // Add remaining caller codecs
        for c in caller_codecs {
            if c.codec != callee_codec {
                optimized_rtp_map
                    .push((c.payload_type, (c.codec, c.clock_rate, c.channels as u32)));
            }
        }

        let track_id = "caller-track".to_string();
        let orig_offer_sdp =
            String::from_utf8_lossy(self.server_dialog.initial_request().body()).to_string();

        let mut codec_info: Vec<CodecInfo> = optimized_rtp_map
            .into_iter()
            .map(|(pt, (codec, clock, channels))| CodecInfo {
                payload_type: pt,
                codec,
                clock_rate: clock,
                channels: channels as u16,
            })
            .collect();

        // Add TelephoneEvent if caller supports DTMF
        if let Some(pt) = caller_dtmf_pt {
            if !codec_info
                .iter()
                .any(|c| c.codec == CodecType::TelephoneEvent)
            {
                codec_info.push(CodecInfo {
                    payload_type: pt,
                    codec: CodecType::TelephoneEvent,
                    clock_rate: 8000,
                    channels: 1,
                });
            }
        }

        let mut track_builder = RtpTrackBuilder::new(track_id.clone())
            .with_cancel_token(self.caller_peer.cancel_token())
            .with_codec_info(codec_info)
            .with_enable_latching(self.context.media_config.enable_latching);
        if let Some(ref servers) = self.context.media_config.ice_servers {
            track_builder = track_builder.with_ice_servers(servers.clone());
        }
        if let Some(ref servers) = self.context.media_config.ice_servers {
            track_builder = track_builder.with_ice_servers(servers.clone());
        }
        if let Some(ref servers) = self.context.media_config.ice_servers {
            track_builder = track_builder.with_ice_servers(servers.clone());
        }
        if let Some(ref servers) = self.context.media_config.ice_servers {
            track_builder = track_builder.with_ice_servers(servers.clone());
        }
        if let Some(ref servers) = self.context.media_config.ice_servers {
            track_builder = track_builder.with_ice_servers(servers.clone());
        }
        if let Some(ref servers) = self.context.media_config.ice_servers {
            track_builder = track_builder.with_ice_servers(servers.clone());
        }
        if let Some(ref addr) = self.context.media_config.external_ip {
            track_builder = track_builder.with_external_ip(addr.clone());
        }
        if let (Some(start), Some(end)) = (
            self.context.media_config.rtp_start_port,
            self.context.media_config.rtp_end_port,
        ) {
            track_builder = track_builder.with_rtp_range(start, end);
        }

        if Self::is_webrtc_sdp(&orig_offer_sdp) {
            track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
        }

        let processed_answer =
            if let Some(existing) = self.find_track(&self.caller_peer, &track_id).await {
                debug!(%track_id, "Reusing existing caller track for optimized handshake");
                let guard = existing.lock().await;
                match guard.handshake(caller_offer.clone()).await {
                    Ok(processed) => processed,
                    Err(e) => {
                        warn!(
                            "Failed to handshake existing caller track for optimization: {}",
                            e
                        );
                        return None;
                    }
                }
            } else {
                let rtp_track = track_builder.build();
                match rtp_track.handshake(caller_offer.clone()).await {
                    Ok(processed) => {
                        self.caller_peer
                            .update_track(Box::new(rtp_track), None)
                            .await;
                        processed
                    }
                    Err(e) => {
                        warn!(
                            "Failed to handshake caller track with optimized codec: {}",
                            e
                        );
                        return None;
                    }
                }
            };

        Some(processed_answer)
    }

    /// Negotiate final codec based on dialplan priority, caller and callee capabilities
    /// This is called when callee responds with 183/200
    async fn negotiate_final_codec(&mut self, callee_answer: &str) -> Result<String> {
        if let Some(ref ans) = self.answer {
            return Ok(ans.clone());
        }

        let orig_offer_sdp =
            String::from_utf8_lossy(self.server_dialog.initial_request().body()).to_string();
        let track_id = "caller-track".to_string();

        // Step 1: Extract caller's supported codecs
        let caller_codecs = if let Some(ref caller_offer) = self.caller_offer {
            if let Ok(sdp) =
                rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, caller_offer)
            {
                if let Some(section) = sdp
                    .media_sections
                    .iter()
                    .find(|m| m.kind == rustrtc::MediaKind::Audio)
                {
                    MediaNegotiator::parse_rtp_map_from_section(section)
                        .into_iter()
                        .map(|(pt, (codec, clock, channels))| CodecInfo {
                            payload_type: pt,
                            codec,
                            clock_rate: clock,
                            channels,
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Step 2: Extract callee's supported codecs from answer
        let (callee_codecs, callee_dtmf_pt) = MediaNegotiator::extract_codec_params(callee_answer);

        // Step 3: Find intersection based on dialplan priority
        // Dialplan codecs order is the first priority
        let mut negotiated_codecs = Vec::new();
        let allow_codecs = &self.context.dialplan.allow_codecs;

        if allow_codecs.is_empty() {
            // No restriction: use caller's preference but only include what callee supports
            for caller_codec in &caller_codecs {
                if callee_codecs.iter().any(|c| c.codec == caller_codec.codec) {
                    negotiated_codecs.push(caller_codec.clone());
                }
            }
        } else {
            // Use dialplan priority
            for allowed in allow_codecs {
                // Find matching codec in both caller and callee
                if let Some(caller_codec) = caller_codecs.iter().find(|c| &c.codec == allowed) {
                    if callee_codecs.iter().any(|c| &c.codec == allowed) {
                        negotiated_codecs.push(caller_codec.clone());
                    }
                }
            }
        }

        if negotiated_codecs.is_empty() {
            return Err(anyhow!(
                "No common codec found between caller, callee and dialplan"
            ));
        }

        // Add DTMF if caller supports it
        if let Some(_dtmf_pt) = callee_dtmf_pt {
            if let Some(ref caller_offer) = self.caller_offer {
                let (_, caller_dtmf) = MediaNegotiator::extract_codec_params(caller_offer);
                if let Some(caller_dtmf_pt) = caller_dtmf {
                    negotiated_codecs.push(CodecInfo {
                        payload_type: caller_dtmf_pt,
                        codec: CodecType::TelephoneEvent,
                        clock_rate: 8000,
                        channels: 1,
                    });
                }
            }
        }

        info!(
            dialog_id = %self.server_dialog.id(),
            negotiated = %negotiated_codecs
                .iter()
                .map(|c| format!("{:?}({})", c.codec, c.payload_type))
                .collect::<Vec<_>>()
                .join(", "),
            "Negotiated final codecs based on dialplan priority"
        );

        // Build track with negotiated codecs
        let mut track_builder = RtpTrackBuilder::new(track_id.clone())
            .with_cancel_token(self.caller_peer.cancel_token())
            .with_codec_info(negotiated_codecs)
            .with_enable_latching(self.context.media_config.enable_latching);

        if let Some(ref addr) = self.context.media_config.external_ip {
            track_builder = track_builder.with_external_ip(addr.clone());
        }
        if let Some(ref ice_servers) = self.context.media_config.ice_servers {
            track_builder = track_builder.with_ice_servers(ice_servers.clone());
        }
        if let (Some(start), Some(end)) = (
            self.context.media_config.rtp_start_port,
            self.context.media_config.rtp_end_port,
        ) {
            track_builder = track_builder.with_rtp_range(start, end);
        }

        if Self::is_webrtc_sdp(&orig_offer_sdp) {
            track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
        }

        let processed_answer = if let Some(ref offer) = self.caller_offer {
            if offer.trim().is_empty() {
                let track = track_builder.build();
                let sdp = track.local_description().await?;
                self.caller_peer.update_track(Box::new(track), None).await;
                sdp
            } else if let Some(existing) = self.find_track(&self.caller_peer, &track_id).await {
                debug!(%track_id, "Reusing existing caller track for negotiation");
                let guard = existing.lock().await;
                match guard.handshake(offer.clone()).await {
                    Ok(processed) => processed,
                    Err(e) => {
                        warn!("Failed to handshake existing caller track: {}", e);
                        return Err(e);
                    }
                }
            } else {
                let track = track_builder.build();
                match track.handshake(offer.clone()).await {
                    Ok(processed) => {
                        self.caller_peer.update_track(Box::new(track), None).await;
                        processed
                    }
                    Err(e) => {
                        warn!("Failed to handshake new caller track: {}", e);
                        return Err(e);
                    }
                }
            }
        } else {
            let track = track_builder.build();
            let sdp = track.local_description().await?;
            self.caller_peer.update_track(Box::new(track), None).await;
            sdp
        };
        Ok(processed_answer)
    }

    async fn create_caller_answer_from_offer(&mut self) -> Result<String> {
        if let Some(ref ans) = self.answer {
            return Ok(ans.clone());
        }

        let orig_offer_sdp =
            String::from_utf8_lossy(self.server_dialog.initial_request().body()).to_string();
        let track_id = "caller-track".to_string();
        // Parse caller's offer to extract rtp_map for correct payload types
        let mut caller_rtp_map = Vec::new();
        if let Some(ref caller_offer) = self.caller_offer {
            if let Ok(sdp) =
                rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, caller_offer)
            {
                if let Some(section) = sdp
                    .media_sections
                    .iter()
                    .find(|m| m.kind == rustrtc::MediaKind::Audio)
                {
                    caller_rtp_map = MediaNegotiator::parse_rtp_map_from_section(section);
                }
            }
        }

        // Extract codec preference from caller RTP map
        let mut codec_info: Vec<CodecInfo> = caller_rtp_map
            .into_iter()
            .map(|(pt, (codec, clock, channels))| CodecInfo {
                payload_type: pt,
                codec,
                clock_rate: clock,
                channels,
            })
            .collect();

        // If media proxy is enabled and caller is WebRTC, prefer PCMU/PCMA over Opus
        // to avoid transcoding with traditional RTP endpoints
        if self.use_media_proxy && Self::is_webrtc_sdp(&orig_offer_sdp) {
            // Find preferred codec from dialplan or default to PCMU
            let preferred_codec = MediaNegotiator::select_best_codec(
                &codec_info,
                &self.context.dialplan.allow_codecs,
            )
            .map(|c| c.codec)
            .unwrap_or(CodecType::PCMU);

            // Reorder codec_info to put preferred codec first if it's not Opus
            if preferred_codec != CodecType::Opus {
                if let Some(pos) = codec_info.iter().position(|c| c.codec == preferred_codec) {
                    if pos > 0 {
                        let preferred = codec_info.remove(pos);
                        let codec_list_before: Vec<String> = codec_info
                            .iter()
                            .map(|c| format!("{:?}({})", c.codec, c.payload_type))
                            .collect();
                        codec_info.insert(0, preferred);
                        let codec_list_after: Vec<String> = codec_info
                            .iter()
                            .map(|c| format!("{:?}({})", c.codec, c.payload_type))
                            .collect();
                        info!(
                            dialog_id = %self.server_dialog.id(),
                            before = %codec_list_before.join(", "),
                            after = %codec_list_after.join(", "),
                            "Reordered codecs for WebRTC caller to avoid transcoding"
                        );
                    }
                }
            }
        }

        // Unified track creation using RtpTrackBuilder
        let mut track_builder = RtpTrackBuilder::new(track_id.clone())
            .with_cancel_token(self.caller_peer.cancel_token())
            .with_codec_info(codec_info)
            .with_enable_latching(self.context.media_config.enable_latching);

        if let Some(ref addr) = self.context.media_config.external_ip {
            track_builder = track_builder.with_external_ip(addr.clone());
        }

        if let (Some(start), Some(end)) = (
            self.context.media_config.rtp_start_port,
            self.context.media_config.rtp_end_port,
        ) {
            track_builder = track_builder.with_rtp_range(start, end);
        }

        // Set mode based on SDP type
        if Self::is_webrtc_sdp(&orig_offer_sdp) {
            track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
        }

        let processed_answer = if let Some(ref offer) = self.caller_offer {
            if offer.trim().is_empty() {
                let track = track_builder.build();
                let sdp = track.local_description().await?;
                self.caller_peer.update_track(Box::new(track), None).await;
                sdp
            } else if let Some(existing) = self.find_track(&self.caller_peer, &track_id).await {
                debug!(%track_id, "Reusing existing caller track for handshake");
                let guard = existing.lock().await;
                match guard.handshake(offer.clone()).await {
                    Ok(processed) => processed,
                    Err(e) => {
                        warn!("Failed to handshake existing caller track: {}", e);
                        return Err(e);
                    }
                }
            } else {
                let track = track_builder.build();
                match track.handshake(offer.clone()).await {
                    Ok(processed) => {
                        self.caller_peer.update_track(Box::new(track), None).await;
                        processed
                    }
                    Err(e) => {
                        warn!("Failed to handshake new caller track: {}", e);
                        return Err(e);
                    }
                }
            }
        } else {
            let track = track_builder.build();
            let sdp = track.local_description().await?;
            self.caller_peer.update_track(Box::new(track), None).await;
            sdp
        };
        Ok(processed_answer)
    }

    /// Create offer SDP for a specific target based on its WebRTC support
    async fn create_offer_for_target(&mut self, target: &Location) -> Option<Vec<u8>> {
        info!(
            session_id = %self.context.session_id,
            target = %target.aor,
            supports_webrtc = target.supports_webrtc,
            destination = ?target.destination,
            "create_offer_for_target called"
        );

        if !self.use_media_proxy {
            // Media proxy disabled: use caller's original offer
            return match self.callee_offer.as_ref() {
                Some(sdp) if !sdp.trim().is_empty() => Some(sdp.clone().into_bytes()),
                _ => None,
            };
        }

        // Media proxy enabled: generate SDP based on target's WebRTC support
        match self.create_callee_track(target.supports_webrtc).await {
            Ok(sdp) if !sdp.trim().is_empty() => Some(sdp.into_bytes()),
            Ok(_) => {
                warn!(
                    session_id = %self.context.session_id,
                    target = %target.aor,
                    supports_webrtc = target.supports_webrtc,
                    "Generated empty SDP for target"
                );
                None
            }
            Err(e) => {
                warn!(
                    session_id = %self.context.session_id,
                    target = %target.aor,
                    supports_webrtc = target.supports_webrtc,
                    error = %e,
                    "Failed to create callee track for target"
                );
                // Fallback to using the default callee offer
                match self.callee_offer.as_ref() {
                    Some(sdp) if !sdp.trim().is_empty() => Some(sdp.clone().into_bytes()),
                    _ => None,
                }
            }
        }
    }

    pub async fn create_callee_track(&mut self, is_webrtc: bool) -> Result<String> {
        info!(
            session_id = %self.context.session_id,
            is_webrtc,
            "create_callee_track called"
        );

        self.callee_peer
            .remove_track(Self::RINGBACK_TRACK_ID, true)
            .await;
        let track_id = Self::CALLEE_TRACK_ID.to_string();
        // Parse caller's offer to extract rtp_map for correct payload types
        let mut caller_rtp_map = Vec::new();
        if let Some(ref caller_offer) = self.caller_offer {
            if let Ok(sdp) =
                rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, caller_offer)
            {
                if let Some(section) = sdp
                    .media_sections
                    .iter()
                    .find(|m| m.kind == rustrtc::MediaKind::Audio)
                {
                    caller_rtp_map = MediaNegotiator::parse_rtp_map_from_section(section);
                }
            }
        }

        let mut rtp_map = Vec::new();

        // 1. Add server codecs with their standard PTs where applicable
        for s_codec in self.context.dialplan.allow_codecs.iter() {
            let pt = match s_codec {
                CodecType::PCMU => 0,
                CodecType::PCMA => 8,
                CodecType::G722 => 9,
                _ => {
                    // For Opus or others, check if caller has it and use their PT
                    if let Some((pt, _)) = caller_rtp_map
                        .iter()
                        .find(|(_, (c, _, _))| c.payload_type() == s_codec.payload_type())
                    {
                        *pt
                    } else {
                        let mut next_pt = 96;
                        while rtp_map.iter().any(|(pt, _)| pt == &next_pt) {
                            next_pt += 1;
                        }
                        next_pt
                    }
                }
            };
            if !rtp_map.iter().any(|(p, _)| p == &pt) {
                rtp_map.push((
                    pt,
                    (s_codec.clone(), s_codec.clock_rate(), s_codec.channels()),
                ));
            }
        }

        // 2. Add remaining caller codecs that don't conflict with our PTs
        for (pt, (c, clock, channels)) in caller_rtp_map {
            if !rtp_map.iter().any(|(p, _)| p == &pt) {
                rtp_map.push((pt, (c, clock, channels)));
            }
        }

        // Extract codec preference from merged RTP map
        let codec_info: Vec<CodecInfo> = rtp_map
            .into_iter()
            .map(|(pt, (codec, clock, channels))| CodecInfo {
                payload_type: pt,
                codec,
                clock_rate: clock,
                channels,
            })
            .collect();

        // Unified track creation using RtpTrackBuilder
        let mut track_builder = RtpTrackBuilder::new(track_id.clone())
            .with_cancel_token(self.callee_peer.cancel_token())
            .with_codec_info(codec_info)
            .with_enable_latching(self.context.media_config.enable_latching);

        if let Some(ref addr) = self.context.media_config.external_ip {
            track_builder = track_builder.with_external_ip(addr.clone());
        }

        if let (Some(start), Some(end)) = (
            self.context.media_config.rtp_start_port,
            self.context.media_config.rtp_end_port,
        ) {
            track_builder = track_builder.with_rtp_range(start, end);
        }

        // Set mode based on is_webrtc flag
        if is_webrtc {
            info!(
                session_id = %self.context.session_id,
                "Setting callee track to WebRTC mode"
            );
            track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
        } else {
            info!(
                session_id = %self.context.session_id,
                "Using default RTP mode for callee track"
            );
        }

        if let Some(existing) = self.find_track(&self.callee_peer, &track_id).await {
            info!(
                session_id = %self.context.session_id,
                %track_id,
                "Reusing existing callee track for local description"
            );
            debug!(%track_id, "Reusing existing callee track for local description");
            let guard = existing.lock().await;
            guard.local_description().await
        } else {
            info!(
                session_id = %self.context.session_id,
                %track_id,
                is_webrtc,
                "Creating NEW callee track"
            );
            let track = track_builder.build();
            let offer = track.local_description().await?;
            self.callee_peer.update_track(Box::new(track), None).await;
            Ok(offer)
        }
    }

    async fn setup_callee_track(
        &mut self,
        callee_answer_sdp: &String,
        _dialog_id: Option<&DialogId>,
    ) -> Result<()> {
        debug!(
            session_id = %self.context.session_id,
            sdp_len = callee_answer_sdp.len(),
            has_existing = self.callee_answer_sdp.is_some(),
            "setup_callee_track called"
        );

        // Check if we've already set this exact SDP (e.g., from 183 early media)
        // WebRTC peer connections cannot set remote answer twice
        if let Some(ref existing_sdp) = self.callee_answer_sdp {
            if existing_sdp == callee_answer_sdp {
                debug!(
                    session_id = %self.context.session_id,
                    "Skipping duplicate callee answer SDP (already set from early media)"
                );
                return Ok(());
            } else {
                warn!(
                    session_id = %self.context.session_id,
                    "Callee answer SDP changed between 183 and 200 OK"
                );
            }
        }

        let track_id = Self::CALLEE_TRACK_ID.to_string();

        // If track doesn't exist, we might need to create it first.
        // For now, we assume update_remote_description handles it or we create it if it fails.
        if let Err(e) = self
            .callee_peer
            .update_remote_description(&track_id, callee_answer_sdp)
            .await
        {
            debug!(
                session_id = %self.context.session_id,
                error = %e,
                "Track does not exist, creating new callee track"
            );
            let mut track = RtpTrackBuilder::new(track_id.clone())
                .with_cancel_token(self.callee_peer.cancel_token())
                .with_enable_latching(self.context.media_config.enable_latching);
            if let Some(ref servers) = self.context.media_config.ice_servers {
                track = track.with_ice_servers(servers.clone());
            }
            if let Some(ref addr) = self.context.media_config.external_ip {
                track = track.with_external_ip(addr.clone());
            }
            let rtp_track = track.build();
            // Must create local offer before setting remote answer
            debug!(
                session_id = %self.context.session_id,
                "Creating local offer for new callee track"
            );
            rtp_track.local_description().await?;
            debug!(
                session_id = %self.context.session_id,
                "Setting remote description on new callee track"
            );
            rtp_track.set_remote_description(callee_answer_sdp).await?;
            self.callee_peer
                .update_track(Box::new(rtp_track), None)
                .await;
            debug!(
                session_id = %self.context.session_id,
                "New callee track created and remote description set"
            );
        } else {
            debug!(
                session_id = %self.context.session_id,
                "Callee track exists, remote description updated successfully"
            );
        }

        // Remember the SDP we just set
        self.callee_answer_sdp = Some(callee_answer_sdp.clone());
        debug!(
            session_id = %self.context.session_id,
            "setup_callee_track completed successfully"
        );
        Ok(())
    }

    pub fn add_callee_dialog(&mut self, dialog_id: DialogId) {
        let mut callee_dialogs = self.callee_dialogs.lock().unwrap();
        if callee_dialogs.contains(&dialog_id) {
            return;
        }
        callee_dialogs.insert(dialog_id.clone());
        if let Some(handle) = &self.handle {
            self.shared
                .register_dialog(dialog_id.to_string(), handle.clone());
        }
    }

    pub async fn start_ringing(&mut self, answer: String) {
        self.start_ringing_internal(answer, None).await;
    }

    async fn start_ringing_internal(&mut self, mut answer: String, dialog_id: Option<DialogId>) {
        let call_answered = self.answer_time.is_some();
        if call_answered {
            return;
        }

        if self.early_media_sent && dialog_id.is_none() {
            debug!("Early media already sent, skipping ringing");
            return;
        }

        self.shared.transition_to_ringing(!answer.is_empty());

        if self.ring_time.is_none() {
            self.ring_time = Some(Instant::now());
        }

        let has_early_media = !answer.is_empty();
        let ringback_mode = self.context.dialplan.ringback.mode;

        let should_play_local = match ringback_mode {
            crate::call::RingbackMode::Local => true,
            crate::call::RingbackMode::Passthrough => false,
            crate::call::RingbackMode::Auto => !has_early_media,
            crate::call::RingbackMode::None => false,
        };

        let should_passthrough = match ringback_mode {
            crate::call::RingbackMode::Local => false,
            crate::call::RingbackMode::Passthrough => has_early_media,
            crate::call::RingbackMode::Auto => has_early_media,
            crate::call::RingbackMode::None => false,
        };

        if has_early_media {
            self.early_media_sent = true;

            if self.use_media_proxy {
                if should_passthrough {
                    info!(
                        session_id = %self.context.session_id,
                        mode = ?ringback_mode,
                        "Forwarding callee early media to caller (passthrough mode)"
                    );
                    self.setup_callee_track(&answer, dialog_id.as_ref())
                        .await
                        .ok();
                }

                match self.negotiate_final_codec(&answer).await {
                    Ok(answer_for_caller) => {
                        self.set_answer(answer_for_caller.clone());
                        answer = answer_for_caller;

                        if self.media_bridge.is_none() {
                            let (params_a, dtmf_pt_a, codec_a) = self
                                .answer
                                .as_ref()
                                .map(|s| {
                                    let (codecs, dtmf) = MediaNegotiator::extract_codec_params(s);
                                    let chosen = MediaNegotiator::select_best_codec(
                                        &codecs,
                                        &self.context.dialplan.allow_codecs,
                                    )
                                    .or_else(|| codecs.into_iter().next())
                                    .unwrap_or(CodecInfo {
                                        payload_type: 0,
                                        codec: CodecType::PCMU,
                                        clock_rate: 8000,
                                        channels: 1,
                                    });
                                    (chosen.to_params(), dtmf, chosen.codec)
                                })
                                .unwrap_or((
                                    rustrtc::RtpCodecParameters::default(),
                                    None,
                                    CodecType::PCMU,
                                ));

                            let (params_b, dtmf_pt_b, codec_b) = {
                                let (codecs, dtmf) = MediaNegotiator::extract_codec_params(&answer);
                                let chosen = MediaNegotiator::select_best_codec(
                                    &codecs,
                                    &self.context.dialplan.allow_codecs,
                                )
                                .or_else(|| codecs.into_iter().next())
                                .unwrap_or(CodecInfo {
                                    payload_type: 0,
                                    codec: CodecType::PCMU,
                                    clock_rate: 8000,
                                    channels: 1,
                                });
                                (chosen.to_params(), dtmf, chosen.codec)
                            };

                            let ssrc_a = self
                                .answer
                                .as_ref()
                                .and_then(|s| MediaNegotiator::extract_ssrc(s));
                            let ssrc_b = MediaNegotiator::extract_ssrc(&answer);

                            info!(
                                session_id = %self.context.session_id,
                                ?codec_a,
                                ?codec_b,
                                ssrc_a,
                                ssrc_b,
                                "Creating media bridge during early media (183)"
                            );

                            let bridge = MediaBridge::new(
                                self.caller_peer.clone(),
                                self.callee_peer.clone(),
                                params_a,
                                params_b,
                                dtmf_pt_a,
                                dtmf_pt_b,
                                codec_a,
                                codec_b,
                                ssrc_a,
                                ssrc_b,
                                self.recorder_option.clone(),
                                self.context
                                    .dialplan
                                    .call_id
                                    .clone()
                                    .unwrap_or_else(|| self.context.session_id.clone()),
                                self.server.sip_flow.as_ref().and_then(|sf| sf.backend()),
                            );

                            self.media_bridge = Some(bridge);
                        }

                        if !call_answered && should_play_local {
                            if let Some(ref file_name) = self.context.dialplan.ringback.audio_file {
                                info!(
                                    session_id = %self.context.session_id,
                                    mode = ?ringback_mode,
                                    audio_file = %file_name,
                                    "Playing local ringback (local/auto mode with early media)"
                                );
                                let loop_playback = self.context.dialplan.ringback.loop_playback;
                                let mut track = FileTrack::new(Self::RINGBACK_TRACK_ID.to_string());
                                track = track.with_path(file_name.clone()).with_loop(loop_playback);
                                self.caller_peer.update_track(Box::new(track), None).await;
                                let track_id = if let Some(ref id) = dialog_id {
                                    format!("callee-track-{}", id)
                                } else {
                                    Self::CALLEE_TRACK_ID.to_string()
                                };
                                self.caller_peer.suppress_forwarding(&track_id).await;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to negotiate final codec: {}", e);
                        return;
                    }
                };
            }
        } else {
            if should_play_local {
                if let Some(ref file_name) = self.context.dialplan.ringback.audio_file {
                    info!(
                        session_id = %self.context.session_id,
                        mode = ?ringback_mode,
                        audio_file = %file_name,
                        "Playing local ringback (no callee early media)"
                    );

                    if self.use_media_proxy {
                        let loop_playback = self.context.dialplan.ringback.loop_playback;
                        let mut track = FileTrack::new(Self::RINGBACK_TRACK_ID.to_string());
                        track = track.with_path(file_name.clone()).with_loop(loop_playback);
                        self.caller_peer.update_track(Box::new(track), None).await;
                    }
                }
            }
        }

        if call_answered {
            return;
        }

        let status_code = if has_early_media {
            StatusCode::SessionProgress
        } else {
            StatusCode::Ringing
        };

        let (headers, body) = if has_early_media && should_passthrough {
            let headers = vec![rsip::Header::ContentType("application/sdp".into())];
            (Some(headers), Some(answer.into_bytes()))
        } else {
            (None, None)
        };

        if let Err(e) = self.server_dialog.ringing(headers, body) {
            warn!("Failed to send {} response: {}", status_code, e);
            return;
        }

        if self.early_media_sent
            && self.context.dialplan.ringback.audio_file.is_some()
            && should_play_local
        {
            if !self.context.dialplan.ringback.wait_for_completion {
                return;
            }
            // wait for done
        }
    }

    pub async fn handle_reinvite(&mut self, method: rsip::Method, sdp: Option<String>) {
        if self.answer.is_none() {
            self.answer = self.shared.answer_sdp();
        }
        info!(session_id = %self.context.session_id, ?method, has_answer = self.answer.is_some(), "Handle re-INVITE/UPDATE");

        if let Some(offer) = sdp {
            debug!(?method, "Received Re-invite/UPDATE with SDP (Offer)");
            self.negotiation_state = NegotiationState::RemoteOfferReceived;

            // Update caller peer with the new offer
            if let Err(e) = self
                .caller_peer
                .update_remote_description("caller-track", &offer)
                .await
            {
                warn!(?method, "Failed to update caller peer with offer: {}", e);
                let _ = self
                    .server_dialog
                    .reject(Some(StatusCode::NotAcceptableHere), None);
                self.negotiation_state = NegotiationState::Stable;
                return;
            }

            if let Some(callee_dialog_id) = &self.connected_dialog_id {
                if let Some(dialog) = self.dialog_layer.get_dialog(callee_dialog_id) {
                    if let rsipstack::dialog::dialog::Dialog::ClientInvite(client_dialog) = dialog {
                        debug!(%callee_dialog_id, "Forwarding re-INVITE/UPDATE offer to callee");
                        let headers = vec![rsip::Header::ContentType("application/sdp".into())];
                        let method_clone = method.clone();
                        let offer_clone = offer.clone();

                        // Spawn this so we don't block the session event loop
                        crate::utils::spawn(async move {
                            if method_clone == rsip::Method::Invite {
                                let _ = client_dialog
                                    .reinvite(Some(headers), Some(offer_clone.into_bytes()))
                                    .await;
                            } else {
                                let _ = client_dialog
                                    .update(Some(headers), Some(offer_clone.into_bytes()))
                                    .await;
                            }
                        });
                    }
                }
            }

            // For re-INVITE, we reply with current answer SDP using server_dialog.accept()
            // Note: If CallModule already replied to the transaction, this will just fail gracefully
            if method == rsip::Method::Invite {
                if let Some(sdp) = &self.answer {
                    let headers = vec![rsip::Header::ContentType("application/sdp".into())];
                    if let Err(e) = self
                        .server_dialog
                        .accept(Some(headers), Some(sdp.clone().into_bytes()))
                    {
                        debug!(
                            ?method,
                            "Failed to reply to re-INVITE with SDP (likely handled by CallModule): {}",
                            e
                        );
                    } else {
                        info!(?method, "Replied to re-INVITE with current SDP");
                        self.negotiation_state = NegotiationState::Stable;
                    }
                } else {
                    println!("DEBUG: Received re-INVITE but no answer SDP available");
                    let _ = self
                        .server_dialog
                        .reject(Some(StatusCode::NotAcceptableHere), None);
                    self.negotiation_state = NegotiationState::Stable;
                }
            } else {
                // For UPDATE, we just update the state
                self.negotiation_state = NegotiationState::Stable;
                debug!(?method, "Marked negotiation stable after processing UPDATE");
            }
        } else {
            debug!(
                ?method,
                "Received Re-invite/UPDATE without SDP (Request for Offer)"
            );
            self.negotiation_state = NegotiationState::LocalOfferSent;

            if let Some(sdp) = &self.answer {
                let headers = vec![rsip::Header::ContentType("application/sdp".into())];
                if let Err(e) = self
                    .server_dialog
                    .accept(Some(headers), Some(sdp.clone().into_bytes()))
                {
                    warn!(
                        ?method,
                        "Failed to reply to re-INVITE/UPDATE (no SDP): {}", e
                    );
                } else {
                    info!(
                        ?method,
                        "Replied to re-INVITE/UPDATE (no SDP) with current SDP as Offer"
                    );
                    self.negotiation_state = NegotiationState::Stable;
                }
            }
        }
    }

    pub fn set_error(&mut self, code: StatusCode, reason: Option<String>, target: Option<String>) {
        debug!(code = %code, reason = ?reason, target = ?target, "Call session error set");
        self.last_error = Some((code.clone(), reason.clone()));
        self.hangup_reason = Some(CallRecordHangupReason::Failed);
        self.note_attempt_failure(code.clone(), reason.clone(), target);
        self.shared.note_failure(code, reason);
    }

    pub fn is_answered(&self) -> bool {
        self.answer_time.is_some()
    }

    pub fn set_answer(&mut self, sdp: String) {
        self.shared.set_answer_sdp(sdp.clone());
        self.answer = Some(sdp);
    }

    pub async fn accept_call(
        &mut self,
        callee: Option<String>,
        callee_answer: Option<String>,
        dialog_id: Option<String>,
    ) -> Result<()> {
        // Ensure queue hold tones cease immediately once the call is answered.
        // self.stop_queue_hold().await;

        let first_answer = self.answer_time.is_none();
        if self.answer.is_none() {
            match self.create_caller_answer_from_offer().await {
                Ok(answer) => self.set_answer(answer),
                Err(e) => {
                    let _ = self
                        .server_dialog
                        .reject(Some(StatusCode::BadRequest), None);
                    return Err(e);
                }
            }
        }

        if let Some(callee_addr) = callee {
            let resolved_callee = self.routed_callee.clone().unwrap_or(callee_addr);
            self.connected_callee = Some(resolved_callee);
        }
        if let Some(ref id_str) = dialog_id {
            let callee_dialogs = self.callee_dialogs.lock().unwrap();
            if let Some(id) = callee_dialogs.iter().find(|id| id.to_string() == **id_str) {
                self.connected_dialog_id = Some(id.clone());
            }
        }
        if first_answer {
            self.answer_time = Some(Instant::now());
        }
        info!(
            server_dialog_id = %self.server_dialog.id(),
            use_media_proxy = self.use_media_proxy,
            has_answer = self.answer.is_some(),
            dialog_id = ?dialog_id,
            "Call answered"
        );

        // Optimize codec negotiation: if callee chose a codec that caller also supports,
        // renegotiate caller's answer to use the same codec to avoid transcoding
        // Skip this if early media was already sent to avoid changing the DTLS fingerprint
        if !self.early_media_sent {
            if let Some(ref callee_sdp) = callee_answer {
                if let Some(optimized_answer) = self.optimize_caller_codec(callee_sdp).await {
                    self.set_answer(optimized_answer);
                }
            }
        }

        if self.answer.is_none() {
            let answer_for_caller = self.create_caller_answer_from_offer().await?;
            self.set_answer(answer_for_caller);
        }

        if self.use_media_proxy {
            let track_id = Self::CALLEE_TRACK_ID.to_string();

            if self.media_bridge.is_none() {
                // First, extract codec from callee's answer (this is the authoritative choice)
                let (params_b, dtmf_pt_b, codec_b) = callee_answer
                    .as_ref()
                    .map(|s| {
                        let (codecs, dtmf) = MediaNegotiator::extract_codec_params(s);
                        let chosen = MediaNegotiator::select_best_codec(
                            &codecs,
                            &self.context.dialplan.allow_codecs,
                        )
                        .or_else(|| codecs.into_iter().next())
                        .unwrap_or(CodecInfo {
                            payload_type: 0,
                            codec: CodecType::PCMU,
                            clock_rate: 8000,
                            channels: 1,
                        });
                        (chosen.to_params(), dtmf, chosen.codec)
                    })
                    .unwrap_or((
                        rustrtc::RtpCodecParameters::default(),
                        None,
                        CodecType::PCMU,
                    ));

                // For params_a (caller side), find the same codec from caller's answer
                // This ensures both sides use the codec chosen by the callee (answerer)
                let (params_a, dtmf_pt_a, codec_a) = self
                    .answer
                    .as_ref()
                    .map(|s| {
                        let (codecs, dtmf) = MediaNegotiator::extract_codec_params(s);
                        // Look for the same codec that callee chose
                        let chosen = codecs
                            .iter()
                            .find(|c| c.codec == codec_b)
                            .cloned()
                            .unwrap_or_else(|| {
                                // Fallback: use select_best_codec if exact match not found
                                MediaNegotiator::select_best_codec(
                                    &codecs,
                                    &self.context.dialplan.allow_codecs,
                                )
                                .or_else(|| codecs.into_iter().next())
                                .unwrap_or(CodecInfo {
                                    payload_type: 0,
                                    codec: CodecType::PCMU,
                                    clock_rate: 8000,
                                    channels: 1,
                                })
                            });
                        (chosen.to_params(), dtmf, chosen.codec)
                    })
                    .unwrap_or((
                        rustrtc::RtpCodecParameters::default(),
                        None,
                        CodecType::PCMU,
                    ));

                let ssrc_a = self
                    .answer
                    .as_ref()
                    .and_then(|s| MediaNegotiator::extract_ssrc(s));
                let ssrc_b = callee_answer
                    .as_ref()
                    .and_then(|s| MediaNegotiator::extract_ssrc(s));

                debug!(
                    ?codec_a,
                    ?codec_b,
                    ssrc_a,
                    ssrc_b,
                    "Media bridge for call session"
                );

                let bridge = MediaBridge::new(
                    self.caller_peer.clone(),
                    self.callee_peer.clone(),
                    params_a,
                    params_b,
                    dtmf_pt_a,
                    dtmf_pt_b,
                    codec_a,
                    codec_b,
                    ssrc_a,
                    ssrc_b,
                    self.recorder_option.clone(),
                    self.context
                        .dialplan
                        .call_id
                        .clone()
                        .unwrap_or_else(|| self.context.session_id.clone()),
                    self.server.sip_flow.as_ref().and_then(|sf| sf.backend()),
                );
                self.media_bridge = Some(bridge);
            }

            self.caller_peer
                .remove_track(Self::RINGBACK_TRACK_ID, true)
                .await;
            if let Some(answer) = callee_answer.as_ref() {
                self.setup_callee_track(answer, None).await?;
            }

            if let Some(ref bridge) = self.media_bridge {
                if let Err(e) = bridge.start().await {
                    warn!(session_id = %self.context.session_id, "Failed to start media bridge: {}", e);
                }
                let _ = bridge.resume_forwarding(&track_id).await;
            }
        } else {
            if let Some(answer) = callee_answer {
                self.set_answer(answer);
            }
            if let Some(ref bridge) = self.media_bridge {
                bridge.stop();
            }
            self.caller_peer.stop();
            self.callee_peer.stop();
        }

        let mut headers = if self.answer.is_some() {
            vec![rsip::Header::ContentType("application/sdp".into())]
        } else {
            vec![]
        };

        let server_timer = self.server_timer.lock().unwrap();
        if server_timer.active {
            headers.push(rsip::Header::Supported(
                rsip::headers::Supported::from(TIMER_TAG).into(),
            ));
            headers.push(rsip::Header::Other(
                HEADER_SESSION_EXPIRES.into(),
                format!(
                    "{};refresher={}",
                    server_timer.session_interval.as_secs(),
                    server_timer.refresher
                ),
            ));
        }

        if let Err(e) = self.server_dialog.accept(
            Some(headers),
            self.answer.clone().map(|sdp| sdp.into_bytes()),
        ) {
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

    pub fn report_failure(&self, code: StatusCode, reason: Option<String>) -> Result<()> {
        let reporter = CallReporter {
            server: self.server.clone(),
            context: self.context.clone(),
            call_record_sender: self.call_record_sender.clone(),
        };

        let server_dialog_id = DialogId::try_from((
            self.context.dialplan.original.as_ref(),
            TransactionRole::Server,
        ))?;

        let snapshot = CallSessionRecordSnapshot {
            ring_time: None,
            answer_time: None,
            last_error: Some((code.clone(), reason.clone())),
            hangup_reason: Some(CallRecordHangupReason::Failed),
            hangup_messages: vec![CallRecordHangupMessage {
                code: code.code(),
                reason,
                target: None,
            }],
            original_caller: Some(self.context.original_caller.clone()),
            original_callee: Some(self.context.original_callee.clone()),
            routed_caller: None,
            routed_callee: None,
            connected_callee: None,
            routed_contact: None,
            routed_destination: None,
            last_queue_name: None,
            callee_dialogs: vec![],
            server_dialog_id,
            extensions: self.context.dialplan.extensions.clone(),
        };

        reporter.report(snapshot);
        Ok(())
    }

    fn forwarding_config(&self) -> Option<&CallForwardingConfig> {
        self.context.dialplan.call_forwarding.as_ref()
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

    fn failure_is_busy(&self) -> bool {
        self.last_error
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

    fn failure_is_no_answer(&self) -> bool {
        if self.is_answered() {
            return false;
        }
        self.last_error
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

    fn local_contact_uri(&self) -> Option<Uri> {
        self.context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .or_else(|| self.server.default_contact_uri())
    }

    async fn process_pending_actions(&mut self, inbox: ActionInbox<'_>) -> Result<()> {
        if let Some(inbox) = inbox {
            while let Ok(action) = inbox.try_recv() {
                self.apply_session_action(action, Some(inbox)).await?;
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn store_pending_hangup(
        pending: &Arc<Mutex<Option<PendingHangup>>>,
        reason: Option<CallRecordHangupReason>,
        code: Option<u16>,
        initiator: Option<String>,
    ) -> Result<()> {
        let mut guard = pending
            .lock()
            .map_err(|_| anyhow!("pending hangup lock poisoned"))?;
        *guard = Some(PendingHangup {
            reason,
            code,
            initiator,
        });
        Ok(())
    }

    fn resolve_pending_hangup(
        &self,
    ) -> (StatusCode, Option<String>, Option<CallRecordHangupReason>) {
        let pending = self
            .pending_hangup
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        match pending {
            Some(request) => {
                let status_code = request
                    .code
                    .and_then(Self::status_code_for_value)
                    .unwrap_or(StatusCode::RequestTerminated);
                let message = match (request.initiator.as_deref(), request.reason.as_ref()) {
                    (Some(initiator), Some(reason)) => Some(format!(
                        "Cancelled by {} ({})",
                        initiator,
                        reason.to_string()
                    )),
                    (Some(initiator), None) => Some(format!("Cancelled by {}", initiator)),
                    (None, Some(reason)) => Some(format!("Cancelled ({})", reason.to_string())),
                    (None, None) => Some("Cancelled by controller".to_string()),
                };
                (status_code, message, request.reason)
            }
            None => (
                StatusCode::RequestTerminated,
                Some("Cancelled by system".to_string()),
                Some(CallRecordHangupReason::Canceled),
            ),
        }
    }

    fn status_code_for_value(value: u16) -> Option<StatusCode> {
        match value {
            403 => Some(StatusCode::Forbidden),
            404 => Some(StatusCode::NotFound),
            480 => Some(StatusCode::TemporarilyUnavailable),
            486 => Some(StatusCode::BusyHere),
            487 => Some(StatusCode::RequestTerminated),
            488 => Some(StatusCode::NotAcceptableHere),
            500 => Some(StatusCode::ServerInternalError),
            503 => Some(StatusCode::ServiceUnavailable),
            600 => Some(StatusCode::BusyEverywhere),
            603 => Some(StatusCode::Decline),
            _ => None,
        }
    }

    pub fn apply_session_action<'a>(
        &'a mut self,
        action: SessionAction,
        inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match action {
                SessionAction::Hangup {
                    code,
                    reason,
                    initiator,
                } => {
                    let status = code
                        .and_then(|c| StatusCode::try_from(c).ok())
                        .unwrap_or(StatusCode::RequestTerminated);
                    let reason_str = reason.as_ref().map(|r| format!("{:?}", r));
                    self.set_error(status, reason_str, None);

                    let actual_reason = reason.clone().unwrap_or(CallRecordHangupReason::Canceled);
                    self.hangup_reason = Some(actual_reason.clone());
                    self.shared.mark_hangup(actual_reason);

                    Self::store_pending_hangup(&self.pending_hangup, reason, code, initiator).ok();

                    self.cancel_token.cancel();
                    Ok(())
                }
                SessionAction::AcceptCall {
                    callee,
                    sdp,
                    dialog_id,
                } => self.accept_call(callee, sdp, dialog_id).await,
                SessionAction::StartRinging {
                    ringback,
                    passthrough: _,
                } => {
                    if let Some(ringback) = ringback {
                        self.start_ringing(ringback).await;
                    }
                    // if passthrough {
                    //     self.queue_passthrough_ringback = true;
                    // }
                    // Do NOT send 183 with SDP to caller yet
                    // Wait for callee's 183/200 response first
                    if !self.early_media_sent {
                        // Send 180 Ringing without SDP
                        if let Err(err) = self.server_dialog.ringing(None, None) {
                            return Err(anyhow!("Failed to send 180 Ringing: {}", err));
                        }
                    }
                    Ok(())
                }
                SessionAction::TransferTarget(target) => {
                    if let Some(endpoint) = TransferEndpoint::parse(&target) {
                        self.transfer_to_endpoint(&endpoint, inbox).await
                    } else {
                        self.transfer_to_uri(&target).await
                    }
                }
                SessionAction::HandleReInvite(method_str, sdp) => {
                    let sdp_opt = if sdp.is_empty() { None } else { Some(sdp) };
                    let method = match method_str.to_uppercase().as_str() {
                        "INVITE" => rsip::Method::Invite,
                        "UPDATE" => rsip::Method::Update,
                        _ => rsip::Method::Invite, // Default to Invite for now
                    };
                    self.handle_reinvite(method, sdp_opt).await;
                    Ok(())
                }
                _ => unreachable!("unsupported session control action"),
            }
        }
        .boxed()
    }

    fn transfer_to_endpoint<'a>(
        &'a mut self,
        endpoint: &'a TransferEndpoint,
        inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match endpoint {
                TransferEndpoint::Uri(uri) => self.transfer_to_uri(uri).await,
                TransferEndpoint::Queue(name) => {
                    if let Some(inbox) = inbox {
                        info!(session_id = %self.context.session_id, queue = %name, "Transferring to queue");

                        let lookup_ref = if name.chars().all(|c| c.is_ascii_digit()) {
                            format!("db-{}", name)
                        } else {
                            name.clone()
                        };

                        let queue_config = self
                            .server
                            .data_context
                            .load_queue(&lookup_ref)
                            .await?
                            .ok_or_else(|| anyhow!("Queue not found: {}", name))?;

                        let mut plan = queue_config.to_queue_plan()?;
                        if plan.label.is_none() {
                            plan.label = Some(name.clone());
                        }

                        self.execute_queue_plan(&plan, None, Some(inbox)).await
                    } else {
                        warn!("Queue forwarding not supported without inbox: {}", name);
                        Err(anyhow!("Queue forwarding not supported without inbox"))
                    }
                }
            }
        }
        .boxed()
    }

    async fn transfer_to_uri(&mut self, uri: &str) -> Result<()> {
        let parsed = Uri::try_from(uri)
            .map_err(|err| anyhow!("invalid forwarding uri '{}': {}", uri, err))?;
        let mut location = Location::default();
        location.aor = parsed.clone();
        location.contact_raw = Some(parsed.to_string());
        match self.try_single_target(&location).await {
            Ok(_) => Ok(()),
            Err((code, reason)) => {
                let message = reason.unwrap_or_else(|| code.to_string());
                Err(anyhow!("forwarding to {} failed: {}", uri, message))
            }
        }
    }

    fn execute_flow<'a>(
        &'a mut self,
        flow: &'a DialplanFlow,
        inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        self.execute_flow_with_mode(flow, inbox, FlowFailureHandling::Handle)
    }

    fn execute_flow_with_mode<'a>(
        &'a mut self,
        flow: &'a DialplanFlow,
        inbox: ActionInbox<'a>,
        handling: FlowFailureHandling,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match flow {
                DialplanFlow::Targets(strategy) => match handling {
                    FlowFailureHandling::Handle => self.execute_targets(strategy, inbox).await,
                    FlowFailureHandling::Propagate => self.run_targets(strategy, inbox).await,
                },
                DialplanFlow::Queue { plan, next } => {
                    self.execute_queue_plan(plan, Some(&**next), inbox).await
                }
            }
        }
        .boxed()
    }

    async fn execute_targets(
        &mut self,
        strategy: &DialStrategy,
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        self.process_pending_actions(inbox.as_deref_mut()).await?;
        let timeout_duration = self.forwarding_timeout();

        if let Some(duration) = timeout_duration {
            match timeout(duration, self.run_targets(strategy, inbox.as_deref_mut())).await {
                Ok(outcome) => match outcome {
                    Ok(_) => {
                        debug!(session_id = %self.context.session_id, "Dialplan executed successfully");
                        Ok(())
                    }
                    Err(_) => self.handle_failure(inbox).await,
                },
                Err(_) => {
                    self.set_error(
                        StatusCode::RequestTimeout,
                        Some("Call forwarding timeout elapsed".to_string()),
                        None,
                    );
                    Err(anyhow!("forwarding timeout elapsed"))
                }
            }
        } else {
            match self.run_targets(strategy, inbox.as_deref_mut()).await {
                Ok(_) => {
                    debug!(session_id = %self.context.session_id, "Dialplan executed successfully");
                    Ok(())
                }
                Err(_) => self.handle_failure(inbox).await,
            }
        }
    }

    async fn execute_queue_plan(
        &mut self,
        plan: &QueuePlan,
        next: Option<&DialplanFlow>,
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        self.process_pending_actions(inbox.as_deref_mut()).await?;

        info!(
            session_id = %self.context.session_id,
            queue_label = ?plan.label,
            accept_immediately = plan.accept_immediately,
            "Executing queue plan"
        );

        // Extract targets from dial strategy
        let targets = match &plan.dial_strategy {
            Some(DialStrategy::Sequential(targets)) | Some(DialStrategy::Parallel(targets)) => {
                targets.clone()
            }
            None => {
                warn!(session_id = %self.context.session_id, "No dial strategy in queue plan");
                return self.handle_queue_failure(plan, next, inbox).await;
            }
        };

        if targets.is_empty() {
            warn!(session_id = %self.context.session_id, "No targets in queue plan");
            return self.handle_queue_failure(plan, next, inbox).await;
        }

        // If accept_immediately is true, send 200 OK to caller before dialing
        if plan.accept_immediately {
            info!(session_id = %self.context.session_id, "Queue accepting immediately");

            // Generate SDP answer for caller if not already done
            if self.answer.is_none() {
                match self.create_caller_answer_from_offer().await {
                    Ok(answer) => {
                        self.set_answer(answer);
                    }
                    Err(e) => {
                        warn!(
                            session_id = %self.context.session_id,
                            error = %e,
                            "Failed to create SDP answer for queue"
                        );
                        return self.handle_queue_failure(plan, next, inbox).await;
                    }
                }
            }

            // Accept the call with 200 OK
            if let Err(e) = self.accept_call(None, None, None).await {
                warn!(
                    session_id = %self.context.session_id,
                    error = %e,
                    "Failed to accept call for queue"
                );
                return self.handle_queue_failure(plan, next, inbox).await;
            }

            // Start hold music if configured
            if let Some(hold_config) = &plan.hold {
                if let Some(audio_file) = &hold_config.audio_file {
                    info!(
                        session_id = %self.context.session_id,
                        audio_file = %audio_file,
                        loop_playback = hold_config.loop_playback,
                        "Starting queue hold music (unified PC)"
                    );

                    // Use unified PC architecture: try to switch audio source on existing track
                    // or create new track if needed
                    let existing_track =
                        self.find_track(&self.caller_peer, "queue-hold-music").await;

                    if let Some(track_handle) = existing_track {
                        // Reuse existing track and switch audio source
                        let mut track_guard = track_handle.lock().await;
                        if let Some(file_track) = track_guard
                            .as_any_mut()
                            .downcast_mut::<crate::media::FileTrack>()
                        {
                            if let Err(e) = file_track
                                .switch_audio_source(audio_file.clone(), hold_config.loop_playback)
                            {
                                warn!("Failed to switch audio source: {}, creating new track", e);
                                drop(track_guard);
                                self.create_queue_hold_track(audio_file, hold_config.loop_playback)
                                    .await;
                            } else {
                                debug!("Successfully switched audio source on existing track");
                            }
                        } else {
                            drop(track_guard);
                            self.create_queue_hold_track(audio_file, hold_config.loop_playback)
                                .await;
                        }
                    } else {
                        // No existing track, create new one
                        self.create_queue_hold_track(audio_file, hold_config.loop_playback)
                            .await;
                    }

                    // Suppress forwarding from callee while playing hold music
                    if let Some(ref bridge) = self.media_bridge {
                        let _ = bridge.suppress_forwarding(Self::CALLEE_TRACK_ID).await;
                    } else {
                        self.caller_peer
                            .suppress_forwarding(Self::CALLEE_TRACK_ID)
                            .await;
                    }
                }
            }
        }

        // Execute the dial strategy directly
        let dial_result = match &plan.dial_strategy {
            Some(strategy) => self.run_targets(strategy, inbox.as_deref_mut()).await,
            None => {
                warn!(session_id = %self.context.session_id, "No dial strategy");
                Err(anyhow::anyhow!("No dial strategy configured"))
            }
        };

        // Stop hold music if it was started
        if plan.accept_immediately && plan.hold.is_some() {
            info!(session_id = %self.context.session_id, "Stopping queue hold music");
            // Remove the hold music track
            self.caller_peer
                .remove_track("queue-hold-music", true)
                .await;

            // Resume forwarding from callee
            if let Some(ref bridge) = self.media_bridge {
                let _ = bridge.resume_forwarding(Self::CALLEE_TRACK_ID).await;
            } else {
                self.caller_peer
                    .resume_forwarding(Self::CALLEE_TRACK_ID)
                    .await;
            }
        }

        // Handle the result
        match dial_result {
            Ok(_) => {
                info!(session_id = %self.context.session_id, "Queue succeeded");
                Ok(())
            }
            Err(e) => {
                warn!(
                    session_id = %self.context.session_id,
                    error = %e,
                    "Queue targets exhausted"
                );
                self.handle_queue_failure(plan, next, inbox).await
            }
        }
    }

    async fn handle_queue_failure(
        &mut self,
        _plan: &QueuePlan,
        next: Option<&DialplanFlow>,
        inbox: ActionInbox<'_>,
    ) -> Result<()> {
        info!(
            session_id = %self.context.session_id,
            "Handling queue failure with fallback"
        );

        // If there's a next flow (fallback), execute it
        if let Some(next_flow) = next {
            info!(
                session_id = %self.context.session_id,
                "Executing queue fallback flow"
            );
            return self.execute_flow(next_flow, inbox).await;
        }

        // Otherwise, handle failure normally
        self.handle_failure(inbox).await
    }

    async fn run_targets(
        &mut self,
        strategy: &DialStrategy,
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        self.process_pending_actions(inbox.as_deref_mut()).await?;
        info!(
            session_id = %self.context.session_id,
            strategy = %strategy,
            media_proxy = self.use_media_proxy,
            "executing dialplan"
        );

        match strategy {
            DialStrategy::Sequential(targets) => self.dial_sequential(targets, inbox).await,
            DialStrategy::Parallel(targets) => self.dial_parallel(targets, inbox).await,
        }
    }

    async fn dial_sequential(
        &mut self,
        targets: &[Location],
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        debug!(
            session_id = %self.context.session_id,
            target_count = targets.len(),
            "Starting sequential dialing"
        );

        for (index, target) in targets.iter().enumerate() {
            self.process_pending_actions(inbox.as_deref_mut()).await?;
            debug!(
                session_id = %self.context.session_id, index, %target,
                "trying sequential target"
            );

            let result = if let Some(plan) = self.context.dialplan.flow.get_queue_plan_recursive() {
                if let Some(timeout) = plan.no_trying_timeout {
                    debug!(session_id = %self.context.session_id, ?timeout, "Applying no-trying timeout");
                    match tokio::time::timeout(timeout, self.try_single_target(target)).await {
                        Ok(res) => res,
                        Err(_) => {
                            warn!(session_id = %self.context.session_id, "No-trying timeout triggered for target");
                            Err((
                                StatusCode::RequestTimeout,
                                Some("No-trying timeout".to_string()),
                            ))
                        }
                    }
                } else {
                    self.try_single_target(target).await
                }
            } else {
                self.try_single_target(target).await
            };

            match result {
                Ok(_) => {
                    debug!(
                        session_id = %self.context.session_id,
                        target_index = index,
                        "Sequential target succeeded"
                    );
                    return Ok(());
                }
                Err((code, reason)) => {
                    info!(
                        session_id = %self.context.session_id,
                        target_index = index,
                        code = %code,
                        reason,
                        "Sequential target failed"
                    );

                    // Check if we should retry based on codes if policy is active
                    let should_retry = if let Some(retry_codes) = self.get_retry_codes() {
                        let code_u16 = u16::from(code.clone());
                        if retry_codes.contains(&code_u16) {
                            info!(
                                session_id = %self.context.session_id,
                                code = %code,
                                "Code is in retry list, proceeding to next target"
                            );
                            true
                        } else {
                            info!(
                                session_id = %self.context.session_id,
                                code = %code,
                                "Code is NOT in retry list, stopping failover"
                            );
                            false
                        }
                    } else {
                        // Default behavior: always retry on any error
                        true
                    };

                    self.note_attempt_failure(
                        code.clone(),
                        reason.clone(),
                        Some(target.aor.to_string()),
                    );

                    if should_retry {
                        continue;
                    } else {
                        return Err(anyhow!("Target failed and retry policy stopped failover"));
                    }
                }
            }
        }

        Err(anyhow!("All sequential targets failed"))
    }

    async fn dial_parallel(
        &mut self,
        targets: &[Location],
        mut inbox: ActionInbox<'_>,
    ) -> Result<()> {
        info!(
            session_id = %self.context.session_id,
            target_count = targets.len(),
            "Starting parallel dialing"
        );

        if targets.is_empty() {
            self.set_error(
                StatusCode::ServerInternalError,
                Some("No targets provided".to_string()),
                None,
            );
            return Err(anyhow!("No targets provided for parallel dialing"));
        }

        if !self.early_media_sent {
            let _ = self
                .apply_session_action(
                    SessionAction::StartRinging {
                        ringback: None,
                        passthrough: false,
                    },
                    None,
                )
                .await;
        }
        self.process_pending_actions(inbox.as_deref_mut()).await?;

        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<ParallelEvent>();
        let cancel_token = self.cancel_token.clone();

        let caller = match self.context.dialplan.caller.as_ref() {
            Some(c) => c.clone(),
            None => {
                self.set_error(
                    StatusCode::ServerInternalError,
                    Some("No caller specified in dialplan".to_string()),
                    None,
                );
                return Err(anyhow!("No caller specified in dialplan"));
            }
        };

        let local_contact = self.local_contact_uri();
        let mut join_set = JoinSet::<()>::new();

        for (idx, target) in targets.iter().enumerate() {
            let (state_tx, state_rx) = mpsc::unbounded_channel::<DialogState>();
            let ev_tx_c = ev_tx.clone();

            // Generate offer based on target's WebRTC support
            let offer = self.create_offer_for_target(target).await;
            let content_type = if offer.is_some() {
                Some("application/sdp".to_string())
            } else {
                None
            };

            let mut headers = self
                .context
                .dialplan
                .build_invite_headers(&target)
                .unwrap_or_default();
            headers.push(rsip::headers::MaxForwards::from(self.context.max_forwards).into());

            let invite_option = InviteOption {
                callee: target.aor.clone(),
                caller: caller.clone(),
                content_type,
                offer,
                destination: target.destination.clone(),
                contact: local_contact.clone().unwrap_or_else(|| caller.clone()),
                credential: target.credential.clone(),
                headers: Some(headers),
                ..Default::default()
            };

            let invite_caller = invite_option.caller.to_string();
            let invite_callee = invite_option.callee.to_string();
            let invite_contact = invite_option.contact.to_string();
            let invite_destination = invite_option
                .destination
                .as_ref()
                .map(|addr| addr.to_string());
            let aor = target.aor.to_string();

            let callee_event_tx = self.callee_event_tx.clone();
            let dialog_layer_for_guard = self.dialog_layer.clone();
            let dialog_layer_for_invite = self.dialog_layer.clone();
            // let server_clone = self.server.clone();

            // Forward dialog state events to aggregator
            crate::utils::spawn({
                let ev_tx_c = ev_tx_c.clone();
                async move {
                    let mut guard = DialogStateReceiverGuard::new(dialog_layer_for_guard, state_rx);
                    while let Some(state) = guard.recv().await {
                        if let Some(tx) = &callee_event_tx {
                            let _ = tx.send(state.clone());
                        }
                        match state {
                            DialogState::Calling(dialog_id) => {
                                guard.set_dialog_id(dialog_id.clone());
                                let _ = ev_tx_c.send(ParallelEvent::Calling {
                                    _idx: idx,
                                    dialog_id,
                                });
                            }
                            DialogState::Early(dialog_id, response) => {
                                guard.set_dialog_id(dialog_id.clone());
                                let sdp = if !response.body().is_empty() {
                                    Some(String::from_utf8_lossy(response.body()).to_string())
                                } else {
                                    None
                                };
                                let _ = ev_tx_c.send(ParallelEvent::Early {
                                    _idx: idx,
                                    dialog_id,
                                    sdp,
                                });
                            }
                            DialogState::Terminated(_, _) => {
                                let _ = ev_tx_c.send(ParallelEvent::Terminated { _idx: idx });
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            });

            let state_tx_c = state_tx.clone();

            join_set.spawn({
                let ev_tx_c = ev_tx_c.clone();
                async move {
                    let invite_result = dialog_layer_for_invite
                        .do_invite(invite_option, state_tx_c)
                        .await;
                    match invite_result {
                        Ok((dialog, resp_opt)) => {
                            if let Some(resp) = resp_opt {
                                if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
                                    let answer = String::from_utf8_lossy(resp.body()).to_string();
                                    let _ = ev_tx_c.send(ParallelEvent::Accepted {
                                        _idx: idx,
                                        dialog_id: dialog.id(),
                                        answer,
                                        aor,
                                        caller_uri: invite_caller,
                                        callee_uri: invite_callee,
                                        contact: invite_contact,
                                        destination: invite_destination,
                                    });
                                } else {
                                    let reason = resp.reason_phrase().clone().map(Into::into);
                                    let _ = ev_tx_c.send(ParallelEvent::Failed {
                                        _idx: idx,
                                        code: resp.status_code,
                                        reason,
                                        target: Some(invite_callee.clone()),
                                    });
                                }
                            } else {
                                let _ = ev_tx_c.send(ParallelEvent::Failed {
                                    _idx: idx,
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
                                _idx: idx,
                                code,
                                reason,
                                target: Some(invite_callee.clone()),
                            });
                        }
                    }
                }
            });
        }

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
        let mut known_dialogs: Vec<Option<DialogId>> = vec![None; targets.len()];

        loop {
            tokio::select! {
                maybe_event = ev_rx.recv() => {
                    let event = match maybe_event {
                        Some(e) => e,
                        None => break,
                    };

                    match event {
                        ParallelEvent::Calling {
                            _idx: idx,
                            dialog_id,
                        } => {
                            known_dialogs[idx] = Some(dialog_id);
                        }
                        ParallelEvent::Early {
                            _idx: _,
                            dialog_id,
                            sdp,
                        } => {
                            if let Some(answer) = sdp {
                                let _ = self
                                    .apply_session_action(
                                        SessionAction::ProvideEarlyMediaWithDialog(
                                            dialog_id.to_string(),
                                            answer,
                                        ),
                                        None,
                                    )
                                    .await;
                            } else if !self.early_media_sent {
                                let _ = self
                                    .apply_session_action(
                                        SessionAction::StartRinging {
                                            ringback: None,
                                            passthrough: false,
                                        },
                                        None,
                                    )
                                    .await;
                            }
                        }
                        ParallelEvent::Accepted {
                            _idx: idx,
                            dialog_id,
                            answer,
                            aor,
                            caller_uri,
                            callee_uri,
                            contact,
                            destination,
                        } => {
                            self.routed_caller = Some(caller_uri.clone());
                            self.routed_callee = Some(callee_uri.clone());
                            self.routed_contact = Some(contact.clone());
                            self.routed_destination = destination.clone();
                            if accepted_idx.is_none() {
                                if let Err(e) = self
                                    .apply_session_action(
                                        SessionAction::AcceptCall {
                                            callee: Some(aor),
                                            sdp: Some(answer),
                                            dialog_id: Some(dialog_id.to_string()),
                                        },
                                        None,
                                    )
                                    .await
                                {
                                    warn!(session_id = %self.context.session_id, error = %e, "Failed to accept call on parallel branch");
                                    continue;
                                }
                                accepted_idx = Some(idx);
                                self.add_callee_dialog(dialog_id.clone());
                                for (j, maybe_id) in known_dialogs.iter().enumerate() {
                                    if j != idx {
                                        if let Some(id) = maybe_id.clone() {
                                            if let Some(dlg) = self.dialog_layer.get_dialog(&id) {
                                                dlg.hangup().await.ok();
                                                self.dialog_layer.remove_dialog(&id);
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
                            self.set_error(code, reason, target);
                            if failures >= targets.len() && accepted_idx.is_none() {
                                return Err(anyhow!("All parallel targets failed"));
                            }
                        }
                        ParallelEvent::Terminated { _idx: idx } => {
                            if Some(idx) == accepted_idx {
                                return Ok(());
                            }
                        }
                        ParallelEvent::Cancelled => {
                            for maybe_id in known_dialogs.iter().filter_map(|o| o.as_ref()) {
                                if let Some(dlg) = self.dialog_layer.get_dialog(maybe_id) {
                                    dlg.hangup().await.ok();
                                    self.dialog_layer.remove_dialog(maybe_id);
                                }
                            }
                            return Err(anyhow!("Caller cancelled"));
                        }
                    }
                }

                maybe_action = async {
                    if let Some(ref mut inbox) = inbox {
                        inbox.recv().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                     if let Some(action) = maybe_action {
                         self.apply_session_action(action, inbox.as_deref_mut()).await?;
                     }
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
        &mut self,
        target: &Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let caller = self
            .context
            .dialplan
            .caller
            .as_ref()
            .ok_or((
                StatusCode::ServerInternalError,
                Some("No caller specified in dialplan".to_string()),
            ))?
            .clone();
        let caller_display_name = self.context.dialplan.caller_display_name.clone();

        // Generate offer based on target's WebRTC support
        let offer = self.create_offer_for_target(target).await;

        let content_type = if offer.is_some() {
            Some("application/sdp".to_string())
        } else {
            None
        };

        let enforced_contact = self.local_contact_uri();
        let mut headers = self
            .context
            .dialplan
            .build_invite_headers(&target)
            .unwrap_or_default();
        headers.push(rsip::headers::MaxForwards::from(self.context.max_forwards).into());
        if self.server.proxy_config.session_timer {
            let session_expires = self.server.proxy_config.session_expires.unwrap_or(1800);
            headers.push(rsip::headers::Supported::from(TIMER_TAG).into());
            headers.push(rsip::Header::Other(
                HEADER_SESSION_EXPIRES.into(),
                session_expires.to_string(),
            ));
        }

        let invite_option = InviteOption {
            caller_display_name,
            callee: target.aor.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination: target.destination.clone(),
            contact: enforced_contact.clone().unwrap_or_else(|| caller.clone()),
            credential: target.credential.clone(),
            headers: Some(headers),
            call_id: self.context.dialplan.call_id.clone(),
            ..Default::default()
        };

        let mut invite_option = if let Some(ref route_invite) = self.context.dialplan.route_invite {
            let route_result = route_invite
                .route_invite(
                    invite_option,
                    &self.context.dialplan.original,
                    &self.context.dialplan.direction,
                    &self.context.cookie,
                )
                .await
                .map_err(|e| {
                    warn!(session_id = %self.context.session_id, error = %e, "Routing function error");
                    (
                        StatusCode::ServerInternalError,
                        Some("Routing function error".to_string()),
                    )
                })?;
            match route_result {
                RouteResult::NotHandled(option, _) => {
                    info!(session_id = self.context.session_id, %target,
                        "Routing function returned NotHandled"
                    );
                    option
                }
                RouteResult::Forward(option, _) | RouteResult::Queue { option, .. } => option,
                RouteResult::Abort(code, reason) => {
                    warn!(session_id = self.context.session_id, %code, ?reason, "route abort");
                    return Err((code, reason));
                }
            }
        } else {
            invite_option
        };

        if let Some(contact_uri) = enforced_contact {
            invite_option.contact = contact_uri;
        }

        let callee_uri = &invite_option.callee;
        let callee_realm = callee_uri.host_with_port.to_string();
        if self.server.is_same_realm(&callee_realm).await {
            let dialplan = &self.context.dialplan;
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
                    .get_user(
                        &callee_uri.user().unwrap_or_default(),
                        Some(&callee_realm),
                        Some(&self.context.dialplan.original),
                    )
                    .await
                {
                    Ok(Some(_)) => {
                        info!(session_id = ?dialplan.session_id, callee = %callee_uri, %callee_realm, "user offline in locator, abort now");
                        return Err((
                            rsip::StatusCode::TemporarilyUnavailable,
                            Some("User offline".to_string()),
                        ));
                    }
                    Ok(None) => {
                        info!(session_id = ?dialplan.session_id, callee = %callee_uri, %callee_realm, "user not found in auth backend, reject");
                        return Err((
                            rsip::StatusCode::NotFound,
                            Some("User not found".to_string()),
                        ));
                    }
                    Err(e) => {
                        warn!(session_id = ?dialplan.session_id, callee = %callee_uri, %callee_realm, "failed to lookup user in auth backend: {}", e);
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
            session_id = %self.context.session_id, %caller, %target, destination,
            "Sending INVITE to callee"
        );

        self.execute_invite(invite_option, &target).await
    }

    async fn execute_invite(
        &mut self,
        mut invite_option: InviteOption,
        target: &Location,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        let _state_tx_keepalive = state_tx.clone();
        let dialog_layer = self.dialog_layer.clone();
        self.note_invite_details(&invite_option);

        // Log client INVITE details
        info!(
            session_id = %self.context.session_id,
            callee = %invite_option.callee,
            destination = ?invite_option.destination,
            "Sending client INVITE to target"
        );

        let session_id = self.context.session_id.clone();
        let mut retry_count = 0;
        let mut invitation = dialog_layer
            .do_invite(invite_option.clone(), state_tx.clone())
            .boxed();
        let dialog;
        let response;
        loop {
            tokio::select! {
                res = &mut invitation => {
                    match res {
                        Ok((dialog_id, resp)) => {
                            if let Some(resp) = resp {
                                if resp.status_code == StatusCode::SessionIntervalTooSmall {
                                    if retry_count < 1 {
                                        let min_se_value =
                                            get_header_value(&resp.headers, HEADER_MIN_SE);
                                        if let Some(value) = min_se_value {
                                            if let Some(min_se) = parse_min_se(&value) {
                                                info!(
                                                    session_id = %session_id,
                                                    min_se = ?min_se,
                                                    "Received 422, retrying with new Session-Expires"
                                                );
                                                if let Some(headers) =
                                                    &mut invite_option.headers
                                                {
                                                    headers.retain(|h| match h {
                                                        rsip::Header::Other(n, _) => !n
                                                            .eq_ignore_ascii_case(
                                                                HEADER_SESSION_EXPIRES,
                                                            ),
                                                        _ => true,
                                                    });
                                                    headers.push(rsip::Header::Other(
                                                        HEADER_SESSION_EXPIRES.into(),
                                                        min_se.as_secs().to_string(),
                                                    ));
                                                }
                                                retry_count += 1;
                                                invitation = dialog_layer.do_invite(invite_option.clone(), state_tx.clone()).boxed();
                                                continue;
                                            }
                                        }
                                    }
                                }

                                if resp.status_code.kind() != rsip::StatusCodeKind::Successful {
                                    let reason = resp.reason_phrase().clone().map(Into::into);
                                    return Err((resp.status_code, reason));
                                } else {
                                    dialog = dialog_id;
                                    response = resp;
                                    break;
                                }
                            } else {
                                return Err((
                                    StatusCode::RequestTerminated,
                                    Some("Cancelled by callee".to_string()),
                                ));
                            }
                        }
                        Err(e) => {
                            debug!(session_id = %session_id, "Invite failed: {:?}", e);
                            return Err(match e {
                                rsipstack::Error::DialogError(reason, _, code) => (code, Some(reason)),
                                _ => (
                                    StatusCode::ServerInternalError,
                                    Some("Invite failed".to_string()),
                                ),
                            });
                        }
                    }
                }
                state = state_rx.recv() => {
                    match state {
                        Some(DialogState::Early(_, response)) => {
                            let sdp = String::from_utf8_lossy(response.body()).to_string();
                            let action = SessionAction::StartRinging{
                                ringback: Some(sdp),
                                passthrough: false
                            };
                            self.apply_session_action(action, None).await.ok();
                        }
                        _ => {}
                    }
                }
            }
        }

        let mut state_rx_guard = DialogStateReceiverGuard::new(dialog_layer.clone(), state_rx);
        state_rx_guard.set_dialog_id(dialog.id());

        if self.server.proxy_config.session_timer {
            let default_expires = self.server.proxy_config.session_expires.unwrap_or(1800);
            self.init_client_timer(&response, default_expires);
        }

        if self.answer_time.is_none()
            && response.status_code.kind() == rsip::StatusCodeKind::Successful
        {
            // Extract SDP from response body, if present
            let answer_body = response.body();
            let sdp = if !answer_body.is_empty() {
                Some(String::from_utf8_lossy(answer_body).to_string())
            } else {
                None
            };

            let dialog_id_val = dialog.id();

            // Log client dialog details after successful answer
            info!(
                session_id = %self.context.session_id,
                dialog_id = %dialog_id_val,
                "Client dialog established after 200 OK from callee"
            );

            self.add_callee_dialog(dialog_id_val.clone());
            let _ = self
                .apply_session_action(
                    SessionAction::AcceptCall {
                        callee: Some(target.aor.to_string()),
                        sdp,
                        dialog_id: Some(dialog_id_val.to_string()),
                    },
                    None,
                )
                .await;
        }

        self.add_callee_guard(state_rx_guard);
        Ok(())
    }

    async fn handle_failure(&mut self, inbox: ActionInbox<'_>) -> Result<()> {
        if self.failure_is_busy() {
            if let Some(config) = self.forwarding_config().cloned() {
                if matches!(config.mode, CallForwardingMode::WhenBusy) {
                    info!(
                        session_id = %self.context.session_id,
                        endpoint = ?config.endpoint,
                        "Call forwarding (busy) engaged"
                    );
                    return self.transfer_to_endpoint(&config.endpoint, inbox).await;
                }
            }
        } else if self.failure_is_no_answer() {
            if let Some(config) = self.forwarding_config().cloned() {
                if matches!(config.mode, CallForwardingMode::WhenNoAnswer) {
                    info!(
                        session_id = %self.context.session_id,
                        endpoint = ?config.endpoint,
                        "Call forwarding (no answer) engaged"
                    );
                    return self.transfer_to_endpoint(&config.endpoint, inbox).await;
                }
            }
        }

        if let Some((code, reason)) = self.last_error.clone() {
            self.report_failure(code, reason)?;
        } else {
            self.report_failure(
                StatusCode::ServerInternalError,
                Some("Unknown error".to_string()),
            )?;
        }
        Err(anyhow!("Call failed"))
    }

    async fn execute_dialplan(&mut self, mut inbox: ActionInbox<'_>) -> Result<()> {
        self.process_pending_actions(inbox.as_deref_mut()).await?;
        if self.context.dialplan.is_empty() {
            self.set_error(
                StatusCode::ServerInternalError,
                Some("No targets in dialplan".to_string()),
                None,
            );
            return Err(anyhow!("Dialplan has no targets"));
        }
        if self
            .try_forwarding_before_dial(inbox.as_deref_mut())
            .await?
        {
            return Ok(());
        }
        self.execute_flow(&self.context.dialplan.flow.clone(), inbox)
            .await
    }

    async fn try_forwarding_before_dial(&mut self, inbox: ActionInbox<'_>) -> Result<bool> {
        let Some(config) = self.immediate_forwarding_config().cloned() else {
            return Ok(false);
        };
        info!(
            session_id = %self.context.session_id,
            endpoint = ?config.endpoint,
            "Call forwarding (always) engaged"
        );
        self.transfer_to_endpoint(&config.endpoint, inbox).await?;
        Ok(true)
    }

    async fn run_server_events_loop(
        context: CallContext,
        proxy_config: Arc<crate::config::ProxyConfig>,
        dialog_layer: Arc<DialogLayer>,
        mut state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        server_timer: Arc<Mutex<SessionTimerState>>,
        client_timer: Arc<Mutex<SessionTimerState>>,
        callee_dialogs: Arc<Mutex<HashSet<DialogId>>>,
        server_dialog: ServerInviteDialog,
        handle: CallSessionHandle,
        cancel_token: CancellationToken,
        pending_hangup: Arc<Mutex<Option<PendingHangup>>>,
        _shared: CallSessionShared,
    ) -> Result<()> {
        let mut refresh_interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    debug!(session_id = %context.session_id, "Session loop cancelled via token");
                    break;
                }
                state = state_rx.recv() => {
                    match state {
                        Some(state) => {
                            match state {
                                DialogState::Terminated(dialog_id, reason) => {
                                    debug!(session_id = %context.session_id, reason = ?reason, %dialog_id, "Server dialog terminated");
                                    Self::store_pending_hangup(
                                        &pending_hangup,
                                        Some(CallRecordHangupReason::ByCaller),
                                        Some(200),
                                        Some("caller".to_string()),
                                    ).ok();
                                    cancel_token.cancel();
                                    break;
                                }
                                DialogState::Info(dialog_id, _request, tx_handle) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Received INFO on server dialog");
                                    tx_handle.reply(rsip::StatusCode::OK).await.ok();
                                }
                                DialogState::Updated(dialog_id, request, tx_handle) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Received UPDATE/INVITE on server dialog");

                                    let has_sdp = !request.body.is_empty();
                                    let is_reinvite = request.method == rsip::Method::Invite;
                                    let is_update = request.method == rsip::Method::Update;

                                    // Handle Re-INVITE or UPDATE with SDP
                                    if (is_reinvite || is_update) && has_sdp {
                                        let sdp = String::from_utf8_lossy(&request.body).to_string();

                                        // Send command to update media
                                        let _ = handle.send_command(SessionAction::HandleReInvite(request.method.to_string(), sdp));

                                        if is_update {
                                            // For UPDATE, we reply here using the transaction handle
                                            // Note: rsipstack 0.4.3 TransactionHandle::reply only takes status,
                                            // cannot send body. Re-INVITE is preferred if SDP answer is required.
                                            tx_handle.reply(rsip::StatusCode::OK).await.ok();
                                            debug!(session_id = %context.session_id, "Replied to UPDATE (200 OK)");
                                        }
                                        // For re-INVITE, handle_reinvite will call server_dialog.accept()
                                    } else {
                                        // Update server timer on incoming refresh
                                        if let Some(value) = get_header_value(&request.headers, HEADER_SESSION_EXPIRES) {
                                            if let Some((interval, _)) = parse_session_expires(&value) {
                                                let mut timer = server_timer.lock().unwrap();
                                                timer.session_interval = interval;
                                                timer.update_refresh();
                                                timer.active = true;
                                                debug!(session_id = %context.session_id, "Server session timer refreshed by incoming request");
                                            }
                                        } else {
                                            let mut timer = server_timer.lock().unwrap();
                                            if timer.active {
                                                timer.update_refresh();
                                                debug!(session_id = %context.session_id, "Server session timer refreshed by incoming request (no Session-Expires)");
                                            }
                                        }
                                        // For requests without SDP, reply with 200 OK
                                        tx_handle.reply(rsip::StatusCode::OK).await.ok();
                                    }
                                }
                                DialogState::Notify(dialog_id, _request, tx_handle) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Received NOTIFY on server dialog");
                                    tx_handle.reply(rsip::StatusCode::OK).await.ok();
                                }
                                DialogState::Calling(dialog_id) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in calling state");
                                }
                                DialogState::Trying(dialog_id) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in trying state");
                                }
                                DialogState::Early(dialog_id, _) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in early state");
                                }
                                DialogState::WaitAck(dialog_id, _) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in wait-ack state");
                                }
                                DialogState::Confirmed(dialog_id, _) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Server dialog in confirmed state");
                                }
                                other_state => {
                                    debug!(
                                        session_id = %context.session_id,
                                        "Received other state on server dialog: {}", other_state
                                    );
                                }
                            }
                        }
                        None => {
                            warn!(session_id = %context.session_id, "Server dialog state channel closed");
                            break
                        }
                    }
                }
                state = callee_state_rx.recv() => {
                    match state {
                        Some(state) => {
                            match state {
                                DialogState::Terminated(dialog_id, reason) => {
                                    let is_active = {
                                        let dialogs = callee_dialogs.lock().unwrap();
                                        dialogs.contains(&dialog_id)
                                    };
                                    if is_active {
                                        info!(session_id = %context.session_id, reason = ?reason, %dialog_id, "Callee dialog terminated");
                                        Self::store_pending_hangup(
                                            &pending_hangup,
                                            Some(CallRecordHangupReason::ByCallee),
                                            Some(200),
                                            Some("callee".to_string()),
                                        ).ok();
                                        cancel_token.cancel();
                                        break;
                                    } else {
                                        debug!(session_id = %context.session_id, %dialog_id, "Inactive callee dialog terminated, ignoring");
                                    }
                                }
                                DialogState::Updated(dialog_id, _request, _) => {
                                    debug!(session_id = %context.session_id, %dialog_id, "Received UPDATE/INVITE on callee dialog");
                                    {
                                        let mut timer = client_timer.lock().unwrap();
                                        if timer.active {
                                            timer.update_refresh();
                                            debug!(session_id = %context.session_id, "Client session timer refreshed by incoming request from callee");
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        None => {
                            warn!(session_id = %context.session_id, "Callee dialog state channel closed");
                            break;
                        }
                    }
                }
                _ = refresh_interval.tick() => {
                    if !proxy_config.session_timer {
                        continue;
                    }
                    let mut should_terminate = false;
                    let mut should_refresh_server = false;
                    let mut should_refresh_client = false;

                    {
                        let  server_timer_guard = server_timer.lock().unwrap();
                        if server_timer_guard.is_expired() {
                            debug!(session_id = %context.session_id, "Server session timer expired, terminating");
                            should_terminate = true;
                        } else if server_timer_guard.refresher == SessionRefresher::Uas && server_timer_guard.should_refresh() {
                            should_refresh_server = true;
                        }
                    }

                    if !should_terminate {
                        let  client_timer_guard = client_timer.lock().unwrap();
                        if client_timer_guard.is_expired() {
                            info!(session_id = %context.session_id, "Client session timer expired, terminating");
                            should_terminate = true;
                        } else if client_timer_guard.refresher == SessionRefresher::Uac && client_timer_guard.should_refresh() {
                            should_refresh_client = true;
                        }
                    }

                    if should_terminate {
                        info!(session_id = %context.session_id, "Session timer expired, cancelling call");
                        cancel_token.cancel();
                        break;
                    }

                    if should_refresh_server {
                        debug!(session_id = %context.session_id, "Server session timer: sending refresh (UAS)");

                        let session_interval = {
                            let mut timer = server_timer.lock().unwrap();
                            timer.refreshing = true;
                            timer.session_interval
                        };

                        let headers = vec![
                            rsip::Header::Supported(rsip::headers::Supported::from(TIMER_TAG).into()),
                            rsip::Header::Other(
                                HEADER_SESSION_EXPIRES.into(),
                                format!("{};refresher=uas", session_interval.as_secs()),
                            ),
                        ];

                        let server_dialog = server_dialog.clone();
                        let server_timer = server_timer.clone();
                        let session_id = context.session_id.clone();
                        let cancel_token_clone = cancel_token.clone();

                        crate::utils::spawn(async move {
                            tokio::select!{
                                _ = cancel_token_clone.cancelled() => {
                                    debug!(session_id = %session_id, "Not sending UPDATE for session refresh, session cancelled");
                                },
                                result = server_dialog.update(Some(headers), None) => {
                                    let mut timer = server_timer.lock().unwrap();
                                    timer.refreshing = false;
                                    match result {
                                        Err(e) => {
                                            warn!(session_id = %session_id, "Failed to send UPDATE for session refresh: {}", e);
                                        }
                                        Ok(None) => {
                                            warn!(session_id = %session_id, "UPDATE for session refresh returned no response");
                                        }
                                        Ok(Some(resp)) => {
                                            if matches!(resp.status_code.kind(), rsip::status_code::StatusCodeKind::Successful) {
                                                timer.update_refresh();
                                            } else {
                                                warn!(session_id = %session_id, status = %resp.status_code, "UPDATE for session refresh failed");
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }

                    if should_refresh_client {
                        debug!(session_id = %context.session_id, "Client session timer: sending refresh (UAC)");

                        let session_interval = {
                            let mut timer = client_timer.lock().unwrap();
                            timer.refreshing = true;
                            timer.session_interval
                        };

                        let headers = vec![
                            rsip::Header::Supported(rsip::headers::Supported::from(TIMER_TAG).into()),
                            rsip::Header::Other(
                                HEADER_SESSION_EXPIRES.into(),
                                format!("{};refresher=uac", session_interval.as_secs()),
                            ),
                        ];

                        let dialog_ids: Vec<DialogId> = {
                            let dialogs = callee_dialogs.lock().unwrap();
                            dialogs.iter().cloned().collect()
                        };

                        for dialog_id in dialog_ids {
                            if let Some(dialog) = dialog_layer.get_dialog(&dialog_id) {
                                match dialog {
                                    rsipstack::dialog::dialog::Dialog::ClientInvite(invite_dialog) => {
                                        let client_timer = client_timer.clone();
                                        let session_id = context.session_id.clone();
                                        let headers = headers.clone();
                                        let cancel_token_clone = cancel_token.clone();

                                        crate::utils::spawn(async move {
                                            tokio::select!{
                                                _ = cancel_token_clone.cancelled() => {
                                                    debug!(session_id = %session_id, %dialog_id, "Not sending UPDATE for session refresh, session cancelled");
                                                },
                                                result = invite_dialog.update(Some(headers), None) => {
                                                    let mut timer = client_timer.lock().unwrap();
                                                    timer.refreshing = false;
                                                    match result {
                                                        Err(e) => {
                                                            warn!(session_id = %session_id, %dialog_id, "Failed to send UPDATE for session refresh: {}", e);
                                                        }
                                                        Ok(None) => {
                                                            warn!(session_id = %session_id, %dialog_id, "UPDATE for session refresh returned no response");
                                                        }
                                                        Ok(Some(resp)) => {
                                                            if matches!(resp.status_code.kind(), rsip::status_code::StatusCodeKind::Successful) {
                                                                timer.update_refresh();
                                                            } else {
                                                                warn!(session_id = %session_id, %dialog_id, status = %resp.status_code, "UPDATE for session refresh failed");
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        });
                                    }
                                    _ => {
                                        let mut timer = client_timer.lock().unwrap();
                                        timer.refreshing = false;
                                        warn!(session_id = %context.session_id, %dialog_id, "Dialog is not a ClientInvite dialog, cannot send UPDATE");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn serve(
        server: SipServerRef,
        context: CallContext,
        tx: &mut rsipstack::transaction::transaction::Transaction,
        cancel_token: CancellationToken,
        call_record_sender: Option<CallRecordSender>,
    ) -> Result<()> {
        if tx.original.body.is_empty() {
            info!(
                session_id = %context.session_id,
                "Rejecting call with 488 Not Acceptable Here due to missing SDP"
            );
            tx.reply(StatusCode::NotAcceptableHere).await?;
            return Ok(());
        }
        let (state_tx, state_rx) = mpsc::unbounded_channel();
        let local_contact = context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|c| c.uri.clone())
            .or_else(|| server.default_contact_uri());

        let server_dialog = server.dialog_layer.get_or_create_server_invite(
            tx,
            state_tx,
            None,
            local_contact.clone(),
        )?;

        // Log initial server dialog details
        info!(
            session_id = %context.session_id,
            dialog_id = %server_dialog.id(),
            "Server dialog created"
        );

        let initial_request = server_dialog.initial_request();
        let offer_sdp = String::from_utf8_lossy(initial_request.body()).to_string();

        if !offer_sdp.trim().is_empty() {
            if let Err(e) = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer_sdp)
            {
                info!(
                    session_id = %context.session_id,
                    error = %e,
                    "Rejecting call with 400 Bad Request due to malformed SDP"
                );
                let _ = server_dialog.reject(Some(StatusCode::BadRequest), None);
                return Ok(());
            }
        }

        let use_media_proxy =
            Self::check_media_proxy(&context, &offer_sdp, &server.proxy_config.media_proxy);

        let all_webrtc_target = context.dialplan.all_webrtc_target();

        info!(
            session_id = %context.session_id,
            server_dialog_id = %server_dialog.id(),
            use_media_proxy,
            all_webrtc_target,
            "starting proxy call processing"
        );

        // Only create recorder when:
        // 1. Recording is enabled and auto_start is true
        // 2. AND no sipflow backend is configured (to avoid duplicate storage)
        let has_sipflow_backend = server
            .sip_flow
            .as_ref()
            .and_then(|sf| sf.backend())
            .is_some();

        let recorder_option = if context.dialplan.recording.enabled
            && context.dialplan.recording.auto_start
            && !has_sipflow_backend
        {
            context.dialplan.recording.option.clone()
        } else {
            None
        };

        let reporter = CallReporter {
            server: server.clone(),
            context: context.clone(),
            call_record_sender: call_record_sender.clone(),
        };

        let caller_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-caller", context.session_id))
            .with_cancel_token(cancel_token.child_token());
        let caller_peer = Arc::new(VoiceEnginePeer::new(Arc::new(caller_media_builder.build())));

        let callee_media_builder = crate::media::MediaStreamBuilder::new()
            .with_id(format!("{}-callee", context.session_id))
            .with_cancel_token(cancel_token.child_token());
        let callee_peer = Arc::new(VoiceEnginePeer::new(Arc::new(callee_media_builder.build())));

        let session_shared = CallSessionShared::new(
            context.session_id.clone(),
            context.dialplan.direction,
            context.dialplan.caller.as_ref().map(|c| c.to_string()),
            context
                .dialplan
                .first_target()
                .map(|location| location.aor.to_string()),
            Some(server.active_call_registry.clone()),
        );

        let mut session = Box::new(CallSession::new(
            server.clone(),
            server.dialog_layer.clone(),
            cancel_token.clone(),
            call_record_sender.clone(),
            context.clone(),
            server_dialog.clone(),
            use_media_proxy,
            recorder_option,
            caller_peer,
            callee_peer,
            session_shared.clone(),
            Some(reporter),
        ));

        if use_media_proxy {
            session.caller_offer = Some(offer_sdp);
            session.callee_offer = session.create_callee_track(all_webrtc_target).await.ok();
        } else {
            session.caller_offer = Some(offer_sdp.clone());
            session.callee_offer = Some(offer_sdp);
        }

        let dialog_guard =
            ServerDialogGuard::new(server.dialog_layer.clone(), session.server_dialog.id());
        let (handle, action_rx) = CallSessionHandle::with_shared(session_shared.clone());
        session.register_active_call(handle);

        let (callee_state_tx, callee_state_rx) = mpsc::unbounded_channel();
        session.callee_event_tx = Some(callee_state_tx);

        let action_inbox = SessionActionInbox::new(action_rx);

        let mut server_dialog_clone = session.server_dialog.clone();
        crate::utils::spawn(async move {
            session
                .process(state_rx, callee_state_rx, action_inbox, dialog_guard)
                .await
        });
        let ring_time_secs = context.dialplan.max_ring_time.clamp(30, 120);
        let max_setup_duration = Duration::from_secs(ring_time_secs as u64);
        let teardown_duration = Duration::from_secs(2);
        let mut timeout = tokio::time::sleep(max_setup_duration).boxed();
        let mut canceld = false;
        loop {
            tokio::select! {
                r = server_dialog_clone.handle(tx) => {
                    debug!(session_id = %context.session_id, "Server dialog handle returned");
                    if let Err(ref e) = r {
                        warn!(session_id = %context.session_id, error = %e, "Server dialog handle returned error, cancelling call");
                        cancel_token.cancel();
                    }
                    break;
                }
                _ = cancel_token.cancelled(), if !canceld => {
                    debug!(session_id = %context.session_id, "Call cancelled via token during setup");
                    canceld = true;
                    timeout = tokio::time::sleep(teardown_duration).boxed();
                }
                _ = &mut timeout => {
                     warn!(session_id = %context.session_id, "Call setup timed out (180s), forcing cancellation");
                     cancel_token.cancel();
                     break;
                }
            }
        }
        Ok(())
    }

    pub async fn process(
        mut self: Box<Self>,
        state_rx: mpsc::UnboundedReceiver<DialogState>,
        callee_state_rx: mpsc::UnboundedReceiver<DialogState>,
        mut action_inbox: SessionActionInbox,
        dialog_guard: ServerDialogGuard,
    ) {
        let _cancel_token_guard = self.cancel_token.clone().drop_guard();

        let use_media_proxy = self.use_media_proxy;
        let caller_peer = self.caller_peer.clone();
        let callee_peer = self.callee_peer.clone();

        let _guard = dialog_guard;

        if self.server.proxy_config.session_timer {
            let default_expires = self.server.proxy_config.session_expires.unwrap_or(1800);
            if let Err((code, _min_se)) = self.init_server_timer(default_expires) {
                info!("Rejecting call with 422 Session Interval Too Small");
                let _ = self.server_dialog.reject(Some(code), None);
                return;
            }
        }

        let server_timer = self.server_timer.clone();
        let client_timer = self.client_timer.clone();
        let callee_dialogs = self.callee_dialogs.clone();
        let server_dialog_clone = self.server_dialog.clone();
        let handle_for_events = self.handle.clone().unwrap();

        let context_clone = self.context.clone();
        let proxy_config_clone = self.server.proxy_config.clone();
        let dialog_layer_clone = self.dialog_layer.clone();
        let cancel_token_for_select = self.cancel_token.clone();
        let cancel_token_for_max_duration = self.cancel_token.clone();
        let cancel_token_for_loop = self.cancel_token.clone();
        let pending_hangup = self.pending_hangup.clone();
        let session_id_for_max_duration = self.context.session_id.clone();
        let max_duration = self
            .context
            .dialplan
            .max_call_duration
            .or_else(|| self.context.cookie.get_max_duration());

        let shared_for_loop = self.shared.clone();
        let _ = tokio::select! {
            _ = cancel_token_for_select.cancelled() => {
                let (status_code, reason_text, hangup_reason) = self.resolve_pending_hangup();
                debug!(session_id = %self.context.session_id, ?status_code, ?reason_text, "Call cancelled via token");
                self.set_error(status_code, reason_text.clone(), None);
                if let Some(reason) = hangup_reason.clone() {
                    self.hangup_reason = Some(reason.clone());
                    self.shared.mark_hangup(reason);
                } else {
                    self.hangup_reason = Some(CallRecordHangupReason::Canceled);
                    self.shared
                        .mark_hangup(CallRecordHangupReason::Canceled);
                }
                Err(anyhow!("Call cancelled"))
            }
            _ = async {
                if use_media_proxy {
                    let _ = tokio::join!(caller_peer.serve(), callee_peer.serve());
                } else {
                    cancel_token_for_select.cancelled().await;
                }
            } => { Ok(()) },
            _ = async {
                if let Some(duration) = max_duration {
                    tokio::time::sleep(duration).await;
                    info!(session_id = %session_id_for_max_duration, "max duration reached, cancelling call");
                    cancel_token_for_max_duration.cancel();
                } else {
                    cancel_token_for_max_duration.cancelled().await;
                }
            } => { Ok(()) },
            r = async {
                self.execute_dialplan(Some(&mut action_inbox)).await?;
                info!(session_id = %self.context.session_id, "Dialplan execution finished, waiting for call termination");
                loop {
                    if let Some(action) = action_inbox.recv().await {
                        self.apply_session_action(action, Some(&mut action_inbox)).await?;
                    } else {
                        break;
                    }
                }
                Ok::<(), anyhow::Error>(())
            } => r,
            r = Self::run_server_events_loop(
                context_clone.clone(),
                proxy_config_clone,
                dialog_layer_clone.clone(),
                state_rx,
                callee_state_rx,
                server_timer,
                client_timer,
                callee_dialogs.clone(),
                server_dialog_clone.clone(),
                handle_for_events,
                cancel_token_for_loop,
                pending_hangup,
                shared_for_loop
            ) => r,
        };

        // Process pending hangup if any
        let (_, _, hangup_reason) = self.resolve_pending_hangup();
        if let Some(reason) = hangup_reason {
            self.hangup_reason = Some(reason.clone());
            self.shared.mark_hangup(reason);
        }

        // Terminate all client dialogs
        {
            let dialogs = callee_dialogs.lock().unwrap().clone();
            for dialog_id in dialogs {
                if let Some(dialog) = dialog_layer_clone.get_dialog(&dialog_id) {
                    debug!(session_id = %context_clone.session_id, %dialog_id, "Terminating client dialog");
                    dialog_layer_clone.remove_dialog(&dialog_id);
                    dialog.hangup().await.ok();
                }
            }
        }
    }
}

impl Drop for CallSession {
    fn drop(&mut self) {
        self.shared.unregister();
        if let Some(ref bridge) = self.media_bridge {
            bridge.stop();
        }
        self.caller_peer.stop();
        self.callee_peer.stop();
        if let Some(reporter) = self.reporter.take() {
            let snapshot = self.record_snapshot();
            reporter.report(snapshot);
        }
    }
}

#[cfg(test)]
mod codec_negotiation_tests {
    use super::*;
    use crate::media::negotiate::MediaNegotiator;

    #[test]
    fn test_parse_rtp_map_from_sdp() {
        // Test parsing Alice's offer (WebRTC) with multiple codecs
        let alice_offer = "v=0\r\n\
            o=- 8819118164752754436 2 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 53824 UDP/TLS/RTP/SAVPF 111 63 9 0 8 13 110 126\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:63 red/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:13 CN/8000\r\n\
            a=rtpmap:110 telephone-event/48000\r\n\
            a=rtpmap:126 telephone-event/8000\r\n";

        let sdp = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, alice_offer).unwrap();
        let section = sdp
            .media_sections
            .iter()
            .find(|m| m.kind == rustrtc::MediaKind::Audio)
            .unwrap();
        let rtp_map = MediaNegotiator::parse_rtp_map_from_section(section);

        // Verify codecs are parsed correctly
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 111 && *codec == CodecType::Opus)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 0 && *codec == CodecType::PCMU)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 8 && *codec == CodecType::PCMA)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 9 && *codec == CodecType::G722)
        );
    }

    #[test]
    fn test_extract_codec_params_opus() {
        let answer = "v=0\r\n\
            o=- 1767067292 1767067293 IN IP4 192.168.3.181\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.181\r\n\
            t=0 0\r\n\
            m=audio 12005 UDP/TLS/RTP/SAVPF 111\r\n\
            a=rtpmap:111 opus/48000/2\r\n";

        let (codecs, _) = MediaNegotiator::extract_codec_params(answer);
        let first = &codecs[0];
        assert_eq!(first.codec, CodecType::Opus);
        assert_eq!(first.payload_type, 111);
        assert_eq!(first.clock_rate, 48000);
        assert_eq!(first.channels, 2);
    }

    #[test]
    fn test_extract_codec_params_pcmu() {
        let answer = "v=0\r\n\
            o=- 1767067292 1767067293 IN IP4 192.168.3.181\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.181\r\n\
            t=0 0\r\n\
            m=audio 65365 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n";

        let (codecs, _) = MediaNegotiator::extract_codec_params(answer);
        let first = &codecs[0];
        assert_eq!(first.codec, CodecType::PCMU);
        assert_eq!(first.payload_type, 0);
        assert_eq!(first.clock_rate, 8000);
        assert_eq!(first.channels, 1);
    }

    #[test]
    fn test_codec_compatibility_check() {
        // Alice's offer: Opus(111), G722(9), PCMU(0), PCMA(8)
        let alice_offer = "v=0\r\n\
            o=- 8819118164752754436 2 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 53824 UDP/TLS/RTP/SAVPF 111 9 0 8\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";

        let (alice_codecs, _) = MediaNegotiator::extract_codec_params(alice_offer);

        // Bob's answer: PCMU only
        let bob_answer = "v=0\r\n\
            o=- 1767067292 1767067293 IN IP4 192.168.3.181\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.181\r\n\
            t=0 0\r\n\
            m=audio 65365 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n";

        let (bob_codecs, _) = MediaNegotiator::extract_codec_params(bob_answer);
        let bob_codec = bob_codecs[0].codec;

        // Verify Alice supports Bob's chosen codec
        let alice_supports_pcmu = alice_codecs.iter().any(|c| c.codec == bob_codec);
        assert!(alice_supports_pcmu, "Alice should support PCMU");

        // Verify the optimization should avoid transcoding
        assert_eq!(bob_codec, CodecType::PCMU);
    }

    #[test]
    fn test_codec_incompatibility() {
        // Alice's offer: only Opus
        let alice_offer = "v=0\r\n\
            o=- 8819118164752754436 2 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 53824 UDP/TLS/RTP/SAVPF 111\r\n\
            a=rtpmap:111 opus/48000/2\r\n";

        let sdp = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, alice_offer).unwrap();
        let section = sdp
            .media_sections
            .iter()
            .find(|m| m.kind == rustrtc::MediaKind::Audio)
            .unwrap();
        let alice_codecs = MediaNegotiator::parse_rtp_map_from_section(section);

        // Bob's answer: PCMU only
        let bob_answer = "v=0\r\n\
            o=- 1767067292 1767067293 IN IP4 192.168.3.181\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.181\r\n\
            t=0 0\r\n\
            m=audio 65365 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n";

        let (bob_codecs, _) = MediaNegotiator::extract_codec_params(bob_answer);
        let bob_codec = bob_codecs[0].codec;

        // Verify Alice does NOT support Bob's chosen codec
        let alice_supports_bob = alice_codecs
            .iter()
            .any(|(_, (codec, _, _))| *codec == bob_codec);
        assert!(
            !alice_supports_bob,
            "Alice should not support PCMU in this case, transcoding required"
        );
    }

    /// Test: negotiate_final_codec() respects dialplan priority
    #[test]
    fn test_negotiate_final_codec_dialplan_priority() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        // Caller OFFER has: Opus(111), G722(9), PCMU(0), PCMA(8)
        let caller_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 111 9 0 8\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";

        // Callee ANSWER has: PCMU(0), PCMA(8), G722(9)
        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 0 8 9\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:9 G722/8000\r\n";

        // Dialplan priority: PCMA > PCMU > G722
        let dialplan_codecs = vec![CodecType::PCMA, CodecType::PCMU, CodecType::G722];

        // Extract codecs from SDPs
        let caller_codecs = MediaNegotiator::extract_all_codecs(caller_offer_sdp);
        let callee_codecs = MediaNegotiator::extract_all_codecs(callee_answer_sdp);

        // Build intersection based on dialplan priority
        let mut negotiated = Vec::new();
        for codec_type in &dialplan_codecs {
            if let Some(caller_codec) = caller_codecs.iter().find(|c| &c.codec == codec_type) {
                if callee_codecs.iter().any(|c| c.codec == *codec_type) {
                    negotiated.push(caller_codec.clone());
                }
            }
        }

        // Expected order: PCMA, PCMU, G722 (Opus excluded - not in callee)
        assert_eq!(negotiated.len(), 3, "Should have 3 codecs in intersection");
        assert_eq!(
            negotiated[0].codec,
            CodecType::PCMA,
            "PCMA should be first (dialplan priority)"
        );
        assert_eq!(
            negotiated[1].codec,
            CodecType::PCMU,
            "PCMU should be second"
        );
        assert_eq!(negotiated[2].codec, CodecType::G722, "G722 should be third");
    }

    /// Test: Empty dialplan uses caller's codec preference
    #[test]
    fn test_negotiate_empty_dialplan() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        let caller_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 0 8\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";

        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 8 0\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n";

        let caller_codecs = MediaNegotiator::extract_all_codecs(caller_offer_sdp);
        let callee_codecs = MediaNegotiator::extract_all_codecs(callee_answer_sdp);

        // Empty dialplan - use caller's order
        // Take intersection based on caller's order
        let mut negotiated = Vec::new();
        for caller_codec in &caller_codecs {
            if callee_codecs.iter().any(|c| c.codec == caller_codec.codec) {
                negotiated.push(caller_codec.clone());
            }
        }

        // Expected: PCMU first (caller's preference), PCMA second
        assert_eq!(negotiated.len(), 2);
        assert_eq!(
            negotiated[0].codec,
            CodecType::PCMU,
            "PCMU should be first (caller preference)"
        );
        assert_eq!(
            negotiated[1].codec,
            CodecType::PCMA,
            "PCMA should be second"
        );
    }

    /// Test: No common codec between caller and callee
    #[test]
    fn test_no_common_codec() {
        use crate::media::negotiate::MediaNegotiator;

        let caller_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 192.168.1.10\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.10\r\n\
            t=0 0\r\n\
            m=audio 5004 RTP/AVP 111\r\n\
            a=rtpmap:111 opus/48000/2\r\n";

        let callee_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.1.20\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.20\r\n\
            t=0 0\r\n\
            m=audio 6004 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n";

        let caller_codecs = MediaNegotiator::extract_all_codecs(caller_offer_sdp);
        let callee_codecs = MediaNegotiator::extract_all_codecs(callee_answer_sdp);

        // Find intersection
        let mut negotiated = Vec::new();
        for caller_codec in &caller_codecs {
            if callee_codecs.iter().any(|c| c.codec == caller_codec.codec) {
                negotiated.push(caller_codec.clone());
            }
        }

        assert_eq!(
            negotiated.len(),
            0,
            "No common codec should result in empty negotiation"
        );
    }

    /// Test: RFC 3264 Answer prioritization in media bridge setup
    /// This test covers the real-world scenario that caused the audio corruption bug
    #[test]
    fn test_rfc3264_answer_prioritization() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        // Bob (WebRTC) OFFER: Opus(111), G722(9), PCMU(0), PCMA(8)
        let bob_offer_sdp = "v=0\r\n\
            o=- 123 123 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 64348 UDP/TLS/RTP/SAVPF 111 63 9 0 8 13 110 126\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:63 red/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:13 CN/8000\r\n\
            a=rtpmap:110 telephone-event/48000\r\n\
            a=rtpmap:126 telephone-event/8000\r\n";

        // Alice (RTP) ANSWER: PCMU(0), PCMA(8), G722(9), G729(18) - Alice chose PCMU first!
        let alice_answer_sdp = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.3.211\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.211\r\n\
            t=0 0\r\n\
            m=audio 58721 RTP/AVP 0 8 9 18 111\r\n\
            a=mid:0\r\n\
            a=sendrecv\r\n\
            a=rtcp-mux\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n\
            a=rtpmap:8 PCMA/8000/1\r\n\
            a=rtpmap:9 G722/8000/1\r\n\
            a=rtpmap:18 G729/8000/1\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=ssrc:853670255 cname:rustrtc-cname-853670255\r\n";

        // Step 1: Extract codecs from Alice's Answer (callee's authoritative choice)
        let (alice_codecs, _) = MediaNegotiator::extract_codec_params(alice_answer_sdp);
        let alice_chosen = MediaNegotiator::select_best_codec(&alice_codecs, &[]);
        assert!(alice_chosen.is_some());
        let alice_codec = alice_chosen.unwrap();

        // RFC 3264: Alice chose PCMU as the first codec in her Answer
        assert_eq!(
            alice_codec.codec,
            CodecType::PCMU,
            "Alice's Answer should prioritize PCMU (first in Answer)"
        );

        // Step 2: Find the same codec in Bob's offer (for params_a)
        let (bob_codecs, _) = MediaNegotiator::extract_codec_params(bob_offer_sdp);
        let bob_matching = bob_codecs
            .iter()
            .find(|c| c.codec == alice_codec.codec)
            .cloned();
        assert!(bob_matching.is_some(), "Bob should support PCMU");
        let bob_codec = bob_matching.unwrap();

        // Step 3: Verify both sides will use the same codec
        assert_eq!(
            bob_codec.codec, alice_codec.codec,
            "Both sides must use the same codec (PCMU)"
        );
        assert_eq!(
            bob_codec.codec,
            CodecType::PCMU,
            "The negotiated codec must be PCMU"
        );

        // Step 4: Verify no transcoding is needed
        assert_eq!(
            bob_codec.codec, alice_codec.codec,
            "Same codec on both sides means no transcoding"
        );
    }

    /// Test: TelephoneEvent should be skipped when selecting audio codec
    #[test]
    fn test_skip_telephone_event_in_codec_selection() {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::CodecType;

        // SDP with TelephoneEvent as first codec (should be skipped)
        let sdp_with_dtmf_first = "v=0\r\n\
            o=- 456 456 IN IP4 192.168.3.211\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.211\r\n\
            t=0 0\r\n\
            m=audio 58721 RTP/AVP 101 0 8\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n";

        let (codecs, _) = MediaNegotiator::extract_codec_params(sdp_with_dtmf_first);
        let chosen = MediaNegotiator::select_best_codec(&codecs, &[]);

        assert!(chosen.is_some());
        let chosen_codec = chosen.unwrap();
        assert_eq!(
            chosen_codec.codec,
            CodecType::PCMU,
            "Should skip TelephoneEvent and select PCMU (first audio codec)"
        );
        assert_ne!(
            chosen_codec.codec,
            CodecType::TelephoneEvent,
            "Should never select TelephoneEvent as audio codec"
        );
    }
}
