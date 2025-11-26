use crate::{
    call::{
        CallForwardingConfig, CallForwardingMode, DialStrategy, Dialplan, DialplanFlow,
        DialplanIvrConfig, FailureAction, Location, MediaConfig, QueueFallbackAction,
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
    proxy::{proxy_call::state::SessionActionReceiver, server::SipServerRef},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
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
    sync::{Arc, Mutex, mpsc as std_mpsc},
    time::{Duration, Instant},
};
use tokio::{
    sync::{broadcast, mpsc},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use voice_engine::{
    event::{EventReceiver, EventSender, SessionEvent, create_event_sender},
    media::{
        recorder::RecorderOption,
        stream::{MediaStream, MediaStreamBuilder},
        track::{Track, TrackConfig, file::FileTrack, rtp::RtpTrackBuilder, webrtc::WebrtcTrack},
    },
    net_tool::is_private_ip,
};
mod state;
use state::CallSessionShared;
pub use state::{
    CallSessionHandle, CallSessionSnapshot, ProxyCallEvent, ProxyCallPhase, SessionAction,
};

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
    session_shared: CallSessionShared,
    pending_hangup: Arc<Mutex<Option<PendingHangup>>>,
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
        let session_shared = CallSessionShared::new(
            session_id.clone(),
            dialplan.direction,
            dialplan.caller.as_ref().map(|c| c.to_string()),
            dialplan
                .first_target()
                .map(|location| location.aor.to_string()),
            Some(server.active_call_registry.clone()),
        );
        let pending_hangup = Arc::new(Mutex::new(None));
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
            session_shared,
            pending_hangup,
        }
    }
}

#[derive(Debug, Clone)]
struct PendingHangup {
    reason: Option<CallRecordHangupReason>,
    code: Option<u16>,
    initiator: Option<String>,
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
        #[allow(dead_code)]
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

#[derive(Clone, Default)]
struct SessionActionInbox {
    queue: Arc<Mutex<VecDeque<SessionAction>>>,
}

impl SessionActionInbox {
    fn push(&self, action: SessionAction) {
        if let Ok(mut guard) = self.queue.lock() {
            guard.push_back(action);
        } else {
            warn!("Session action inbox lock poisoned");
        }
    }

    fn drain(&self) -> Vec<SessionAction> {
        match self.queue.lock() {
            Ok(mut guard) => guard.drain(..).collect(),
            Err(_) => {
                warn!("Session action inbox lock poisoned while draining");
                Vec::new()
            }
        }
    }
}

type ActionInbox<'a> = Option<&'a SessionActionInbox>;
include!("proxy_call/session.rs");

impl ProxyCall {
    fn forwarding_config(&self) -> Option<&CallForwardingConfig> {
        self.dialplan.call_forwarding.as_ref()
    }

    #[allow(dead_code)]
    async fn handle_session_actions(
        &self,
        session: &mut CallSession,
        mut action_rx: SessionActionReceiver,
    ) -> Result<()> {
        while let Some(action) = action_rx.recv().await {
            self.apply_session_action(session, action).await?;
        }
        Ok(())
    }

    async fn process_pending_actions(
        &self,
        session: &mut CallSession,
        inbox: Option<&SessionActionInbox>,
    ) -> Result<()> {
        if let Some(inbox) = inbox {
            for action in inbox.drain() {
                self.apply_session_action(session, action).await?;
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
        if self.dialplan.has_queue_hold_audio() {
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
            self.cancel_token.clone(),
            self.session_id.clone(),
            server_dialog.clone(),
            use_media_proxy,
            self.event_sender.clone(),
            self.dialplan.media.clone(),
            recorder_option,
            self.session_shared.clone(),
        );
        if use_media_proxy {
            session.caller_offer = Some(offer_sdp);
            session.callee_offer = session.create_callee_track(all_webrtc_target).await.ok();
        } else {
            session.caller_offer = Some(offer_sdp.clone());
            session.callee_offer = Some(offer_sdp);
        }
        let media_stream = session.media_stream.clone();
        let dialog_guard = ServerDialogGuard::new(self.dialog_layer.clone(), server_dialog.id());
        let (handle, mut action_rx) = CallSessionHandle::with_shared(self.session_shared.clone());
        session.register_active_call(handle);
        let action_inbox = SessionActionInbox::default();
        let inbox_forward = action_inbox.clone();
        tokio::spawn(async move {
            while let Some(action) = action_rx.recv().await {
                inbox_forward.push(action);
            }
        });

        let (_, result) = tokio::join!(server_dialog.handle(tx), async {
            let result = tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    let (status_code, reason_text, hangup_reason) = self.resolve_pending_hangup();
                    session.set_error(status_code, reason_text.clone(), None);
                    if let Some(reason) = hangup_reason.clone() {
                        session.hangup_reason = Some(reason.clone());
                        self.session_shared.mark_hangup(reason);
                    } else {
                        session.hangup_reason = Some(CallRecordHangupReason::Canceled);
                        self.session_shared
                            .mark_hangup(CallRecordHangupReason::Canceled);
                    }
                    Err(anyhow!("Call cancelled"))
                }
                _ = media_stream.serve() => {Ok(())},
                r = self.execute_dialplan(&mut session, Some(&action_inbox)) => r,
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

    async fn execute_dialplan(
        &self,
        session: &mut CallSession,
        inbox: ActionInbox<'_>,
    ) -> Result<()> {
        self.process_pending_actions(session, inbox).await?;
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
        self.execute_flow(session, &self.dialplan.flow, inbox).await
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

    fn apply_session_action<'a>(
        &'a self,
        session: &'a mut CallSession,
        action: SessionAction,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match action {
                SessionAction::AcceptCall { callee, sdp } => session.accept_call(callee, sdp).await,
                SessionAction::EnterQueue { name } => self.transfer_to_queue(session, &name).await,
                SessionAction::ExitQueue => {
                    session.stop_queue_hold().await;
                    session.set_queue_name(None);
                    Ok(())
                }
                SessionAction::EnterIvr { reference } => {
                    self.transfer_to_ivr(session, &reference).await
                }
                SessionAction::TransferTarget(target) => {
                    self.transfer_to_uri(session, target.trim()).await
                }
                SessionAction::ProvideEarlyMedia(sdp) => {
                    session.start_ringing(sdp, self).await;
                    Ok(())
                }
                SessionAction::StartRinging {
                    ringback,
                    passthrough,
                } => {
                    if passthrough {
                        session
                            .start_ringing(ringback.unwrap_or_default(), self)
                            .await;
                        return Ok(());
                    }
                    if let Some(audio) = ringback {
                        session
                            .play_ringtone(&audio, Some(self.event_sender.subscribe()), true)
                            .await
                    } else {
                        session.start_ringing(String::new(), self).await;
                        Ok(())
                    }
                }
                SessionAction::PlayPrompt {
                    audio_file,
                    send_progress,
                    await_completion,
                } => {
                    let event_rx = if await_completion {
                        Some(self.event_sender.subscribe())
                    } else {
                        None
                    };
                    session
                        .play_ringtone(&audio_file, event_rx, send_progress)
                        .await
                }
                SessionAction::SetQueueName(_)
                | SessionAction::SetQueueRingbackPassthrough(_)
                | SessionAction::StartQueueHold(_)
                | SessionAction::StopQueueHold => {
                    self.handle_session_control(session, action).await
                }
                SessionAction::Hangup {
                    reason,
                    code,
                    initiator,
                } => {
                    Self::store_pending_hangup(&self.pending_hangup, reason, code, initiator)?;
                    self.cancel_token.cancel();
                    Ok(())
                }
            }
        }
        .boxed()
    }

    async fn handle_session_control(
        &self,
        session: &mut CallSession,
        action: SessionAction,
    ) -> Result<()> {
        match action {
            SessionAction::SetQueueName(name) => {
                session.set_queue_name(name);
                Ok(())
            }
            SessionAction::SetQueueRingbackPassthrough(enabled) => {
                session.set_queue_ringback_passthrough(enabled);
                Ok(())
            }
            SessionAction::StartQueueHold(config) => session.start_queue_hold(config).await,
            SessionAction::StopQueueHold => {
                session.stop_queue_hold().await;
                Ok(())
            }
            _ => unreachable!("unsupported session control action"),
        }
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

    fn execute_flow<'a>(
        &'a self,
        session: &'a mut CallSession,
        flow: &'a DialplanFlow,
        inbox: ActionInbox<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        self.execute_flow_with_mode(session, flow, inbox, FlowFailureHandling::Handle)
    }

    fn execute_flow_with_mode<'a>(
        &'a self,
        session: &'a mut CallSession,
        flow: &'a DialplanFlow,
        inbox: ActionInbox<'a>,
        handling: FlowFailureHandling,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            match flow {
                DialplanFlow::Targets(strategy) => match handling {
                    FlowFailureHandling::Handle => {
                        self.execute_targets(session, strategy, inbox).await
                    }
                    FlowFailureHandling::Propagate => {
                        self.run_targets(session, strategy, inbox).await
                    }
                },
                DialplanFlow::Queue { plan, next } => {
                    let propagate_failure = matches!(handling, FlowFailureHandling::Handle);
                    self.execute_queue_plan(session, plan, next, 0, propagate_failure, inbox)
                        .await
                }
                DialplanFlow::Ivr(config) => self.run_ivr(session, config, None).await,
            }
        }
        .boxed()
    }

    async fn execute_targets(
        &self,
        session: &mut CallSession,
        strategy: &DialStrategy,
        inbox: ActionInbox<'_>,
    ) -> Result<()> {
        self.process_pending_actions(session, inbox).await?;
        let run_future = self.run_targets(session, strategy, inbox);
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

    async fn run_targets(
        &self,
        session: &mut CallSession,
        strategy: &DialStrategy,
        inbox: ActionInbox<'_>,
    ) -> Result<()> {
        self.process_pending_actions(session, inbox).await?;
        info!(
            session_id = %self.session_id,
            strategy = %strategy,
            media_proxy = session.use_media_proxy,
            "executing dialplan"
        );

        match strategy {
            DialStrategy::Sequential(targets) => {
                self.dial_sequential(session, targets, inbox).await
            }
            DialStrategy::Parallel(targets) => self.dial_parallel(session, targets, inbox).await,
        }
    }

    async fn dial_sequential(
        &self,
        session: &mut CallSession,
        targets: &[Location],
        inbox: ActionInbox<'_>,
    ) -> Result<()> {
        info!(
            session_id = %self.session_id,
            target_count = targets.len(),
            "Starting sequential dialing"
        );

        for (index, target) in targets.iter().enumerate() {
            self.process_pending_actions(session, inbox).await?;
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

    async fn dial_parallel(
        &self,
        session: &mut CallSession,
        targets: &[Location],
        inbox: ActionInbox<'_>,
    ) -> Result<()> {
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
            let _ = self
                .apply_session_action(
                    session,
                    SessionAction::StartRinging {
                        ringback: None,
                        passthrough: false,
                    },
                )
                .await;
        }
        self.process_pending_actions(session, inbox).await?;

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
            self.process_pending_actions(session, inbox).await?;
            match event {
                ParallelEvent::Calling { idx, dialog_id } => {
                    known_dialogs[idx] = Some(dialog_id);
                }
                ParallelEvent::Early { sdp } => {
                    if let Some(answer) = sdp {
                        let _ = self
                            .apply_session_action(session, SessionAction::ProvideEarlyMedia(answer))
                            .await;
                    } else if !session.early_media_sent {
                        let _ = self
                            .apply_session_action(
                                session,
                                SessionAction::StartRinging {
                                    ringback: None,
                                    passthrough: false,
                                },
                            )
                            .await;
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
                        if let Err(e) = self
                            .apply_session_action(
                                session,
                                SessionAction::AcceptCall {
                                    callee: Some(aor),
                                    sdp: Some(answer),
                                },
                            )
                            .await
                        {
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
        info!(
            session_id = %self.session_id,
            target = %target,
            "INVITE process completed"
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
                let already_answered = session.answer_time.is_some();
                if let Err(err) = session
                    .play_ringtone(
                        audio_file,
                        Some(self.event_sender.subscribe()),
                        !already_answered,
                    )
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
        let snapshot = session.record_snapshot();

        let ring_time = snapshot.ring_time.map(|rt| {
            start_time
                + chrono::Duration::from_std(rt.duration_since(self.start_time)).unwrap_or_default()
        });

        let answer_time = snapshot.answer_time.map(|at| {
            start_time
                + chrono::Duration::from_std(at.duration_since(self.start_time)).unwrap_or_default()
        });

        let status_code = snapshot
            .last_error
            .as_ref()
            .map(|(code, _)| u16::from(code.clone()))
            .unwrap_or(200);

        let hangup_reason = snapshot.hangup_reason.clone().or_else(|| {
            if snapshot.last_error.is_some() {
                Some(CallRecordHangupReason::Failed)
            } else if snapshot.answer_time.is_some() {
                Some(CallRecordHangupReason::BySystem)
            } else {
                Some(CallRecordHangupReason::Failed)
            }
        });

        let original_caller = snapshot
            .original_caller
            .clone()
            .or_else(|| self.dialplan.caller.as_ref().map(|c| c.to_string()))
            .unwrap_or_default();

        let original_callee = snapshot
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

        let caller = snapshot
            .routed_caller
            .clone()
            .unwrap_or_else(|| original_caller.clone());

        let callee = snapshot
            .routed_callee
            .clone()
            .or_else(|| snapshot.connected_callee.clone())
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
        if let Some((code, reason)) = snapshot.last_error.as_ref() {
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

        if let Some(queue) = snapshot.last_queue_name.clone() {
            extras_map.insert("last_queue".to_string(), Value::String(queue));
        }

        if let Some(trace) = snapshot.ivr_trace.clone() {
            let mut ivr_payload = JsonMap::new();
            if let Some(reference) = trace.reference.clone() {
                ivr_payload.insert("reference".to_string(), Value::String(reference));
            }
            if let Some(plan_id) = trace.plan_id.clone() {
                ivr_payload.insert("plan_id".to_string(), Value::String(plan_id));
            }
            if let Some(exit) = trace.exit.clone() {
                ivr_payload.insert("exit".to_string(), Value::String(exit));
            }
            if let Some(detail) = trace.detail.clone() {
                ivr_payload.insert("detail".to_string(), Value::String(detail));
            }
            if !ivr_payload.is_empty() {
                extras_map.insert("ivr".to_string(), Value::Object(ivr_payload));
            }
        }

        let mut hangup_messages = snapshot.hangup_messages.clone();
        if hangup_messages.is_empty() {
            if let Some((code, reason)) = snapshot.last_error.as_ref() {
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
        if let Some(contact) = snapshot.routed_contact.as_ref() {
            rewrite_payload.insert("contact".to_string(), Value::String(contact.clone()));
        }
        if let Some(destination) = snapshot.routed_destination.as_ref() {
            rewrite_payload.insert(
                "destination".to_string(),
                Value::String(destination.clone()),
            );
        }
        extras_map.insert("rewrite".to_string(), Value::Object(rewrite_payload));

        let mut sip_flows_map: HashMap<String, Vec<SipMessageItem>> = HashMap::new();
        let server_dialog_id = snapshot.server_dialog_id.clone();
        let mut call_ids: HashSet<String> = HashSet::new();
        call_ids.insert(server_dialog_id.call_id.clone());

        for dialog_id in &snapshot.callee_dialogs {
            call_ids.insert(dialog_id.call_id.clone());
        }

        let mut sip_leg_roles: HashMap<String, String> = HashMap::new();
        sip_leg_roles.insert(server_dialog_id.call_id.clone(), "primary".to_string());
        for dialog_id in &snapshot.callee_dialogs {
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
            offer: snapshot.caller_offer.clone(),
            answer: snapshot.answer.clone(),
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
        persist_args.queue = snapshot.last_queue_name.clone();
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

#[derive(Clone, Copy)]
enum FlowFailureHandling {
    Handle,
    Propagate,
}

include!("proxy_call/queue.rs");
include!("proxy_call/ivr.rs");

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
                        is_answered = session.is_answered(),
                        %dialog_id,
                        ?reason,
                        "Established call terminated by callee"
                    );
                    session.add_callee_dialog(dialog_id);
                    session.callee_terminated(reason).await;
                    if session.is_answered() {
                        let _ = proxy_call
                            .apply_session_action(
                                session,
                                SessionAction::Hangup {
                                    reason: Some(CallRecordHangupReason::ByCallee),
                                    code: None,
                                    initiator: Some("callee".to_string()),
                                },
                            )
                            .await;
                    }
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
                    let _ = proxy_call
                        .apply_session_action(session, SessionAction::ProvideEarlyMedia(answer))
                        .await;
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
                    if let Err(e) = proxy_call
                        .apply_session_action(
                            session,
                            SessionAction::AcceptCall {
                                callee: Some(target.aor.to_string()),
                                sdp: Some(answer),
                            },
                        )
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
