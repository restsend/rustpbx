use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::callrecord::{CallRecord, CallRecordHangupReason, CallRecordSender};
use crate::config::{MediaProxyConfig, MediaProxyMode, ProxyConfig};
use crate::handler::call::ActiveCallType;
use crate::proxy::bridge::{MediaBridgeBuilder, MediaBridgeType};
use crate::proxy::session::{Session, SessionParty};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use rsip::headers::UntypedHeader;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::dialog::DialogState;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::DialogId;
use rsipstack::transaction::transaction::Transaction;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::select;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
    media_proxy_config: Arc<MediaProxyConfig>,
    server: SipServerRef,
    /// Sessions indexed by caller dialog ID
    pub(crate) sessions: Arc<RwLock<HashMap<DialogId, Session>>>,
    pub(crate) callee_to_sessions: Arc<RwLock<HashMap<DialogId, DialogId>>>,
    callrecord_sender: Option<CallRecordSender>,
    dialog_layer: Arc<DialogLayer>,
}

#[derive(Clone)]
pub struct CallModule {
    pub(crate) inner: Arc<CallModuleInner>,
}

impl CallModule {
    pub fn create(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = CallModule::new(config, server);
        Ok(Box::new(module))
    }

    pub fn new(config: Arc<ProxyConfig>, server: SipServerRef) -> Self {
        let sessions = Arc::new(RwLock::new(HashMap::new()));
        let callee_to_sessions = Arc::new(RwLock::new(HashMap::new()));
        let dialog_layer = Arc::new(DialogLayer::new(server.endpoint.inner.clone()));
        let inner = Arc::new(CallModuleInner {
            media_proxy_config: Arc::new(config.media_proxy.clone()),
            config,
            server: server.clone(),
            sessions,
            callee_to_sessions,
            callrecord_sender: server.callrecord_sender.clone(),
            dialog_layer,
        });
        Self { inner }
    }

    /// Detect if SDP is WebRTC format
    pub fn is_webrtc_sdp(&self, sdp: &str) -> bool {
        sdp.contains("a=ice-ufrag:")
            || sdp.contains("a=ice-pwd:")
            || sdp.contains("a=fingerprint:")
            || sdp.contains("a=setup:")
    }

    /// Check if media proxy is needed based on configuration
    pub(crate) fn should_use_media_bridge(
        &self,
        tx: &Transaction,
        _caller_iswebrtc: bool,
        _target_iswebrtc: bool,
    ) -> Result<bool> {
        let media_config = &self.inner.config.media_proxy;

        match media_config.mode {
            MediaProxyMode::None => Ok(false),
            MediaProxyMode::All => Ok(true),
            MediaProxyMode::NatOnly => {
                if let Some(content_type) = tx.original.headers.iter().find_map(|h| match h {
                    rsip::Header::ContentType(ct) => Some(ct),
                    _ => None,
                }) {
                    if content_type.value().contains("application/sdp") {
                        let body = String::from_utf8_lossy(&tx.original.body);
                        return Ok(crate::net_tool::sdp_contains_private_ip(&body).unwrap_or(false));
                    }
                }
                Ok(false)
            }
        }
    }

    /// Forward request to external proxy realm
    async fn forward_to_proxy(&self, tx: &mut Transaction, target_realm: &str) -> Result<()> {
        if !self.inner.config.enable_forwarding.unwrap_or(false) {
            return Err(anyhow!("External proxy forwarding is disabled"));
        }

        warn!(
            key = ?tx.key,
            "External proxy forwarding not implemented for realm: {}", target_realm
        );
        return Err(anyhow!("External proxy forwarding not implemented"));
    }

    pub(crate) async fn handle_invite(&self, tx: &mut Transaction) -> Result<()> {
        let caller = tx.original.from_header()?.uri()?.to_string();
        let callee_uri = tx.original.to_header()?.uri()?;
        let callee = callee_uri.user().unwrap_or_default().to_string();
        let callee_realm = callee_uri.host().to_string();
        let callee_realm = ProxyConfig::normalize_realm(&callee_realm);
        if !self.inner.config.is_same_realm(&callee_realm) {
            info!(callee_realm, "Forwarding INVITE to external realm");
            return self.forward_to_proxy(tx, &callee_realm).await;
        }

        // Check if callee exists in locator
        let target_locations = match self
            .inner
            .server
            .locator
            .lookup(&callee, Some(callee_realm))
            .await
        {
            Ok(locations) => locations,
            Err(_) => {
                info!("User not found in locator: {}@{}", callee, callee_realm);
                tx.reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e))?;

                // Wait for ACK
                while let Some(msg) = tx.receive().await {
                    match msg {
                        rsip::message::SipMessage::Request(req) => match req.method {
                            rsip::Method::Ack => {
                                debug!("Received ACK for 404 Not Found");
                                break;
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                return Ok(());
            }
        };

        let target_location = target_locations
            .first()
            .ok_or(anyhow!("No target location found"))?;

        // Check if request body contains SDP
        let caller_offer = if !tx.original.body.is_empty() {
            String::from_utf8_lossy(&tx.original.body).to_string()
        } else {
            return Err(anyhow!("No SDP offer found in INVITE"));
        };

        // Determine if media bridge is needed
        let caller_iswebrtc = self.is_webrtc_sdp(&caller_offer);
        let should_bridge_media =
            self.should_use_media_bridge(tx, caller_iswebrtc, target_location.supports_webrtc)?;

        // Create session with cancellation token
        let session_token = self.inner.server.cancel_token.child_token();
        // Create media bridge if needed
        let mut bridge_builder = if should_bridge_media {
            let bridge_type = match (caller_iswebrtc, target_location.supports_webrtc) {
                (true, true) => MediaBridgeType::Webrtc2Webrtc,
                (true, false) => MediaBridgeType::WebRtcToRtp,
                (false, true) => MediaBridgeType::RtpToWebRtc,
                (false, false) => MediaBridgeType::Rtp2Rtp,
            };

            Some(
                MediaBridgeBuilder::new(
                    bridge_type,
                    session_token.child_token(),
                    self.inner.media_proxy_config.clone(),
                )
                .await?,
            )
        } else {
            None
        };

        // Prepare SDP for outbound call
        let outbound_sdp = if let Some(ref mut builder) = bridge_builder {
            builder.local_description().await?
        } else {
            caller_offer.clone()
        };

        // Create InviteOption for outbound call
        let invite_option = InviteOption {
            caller: tx.original.from_header()?.uri()?.clone(),
            callee: callee_uri.clone(),
            content_type: Some("application/sdp".to_string()),
            offer: Some(outbound_sdp.as_bytes().to_vec()),
            contact: tx.original.from_header()?.uri()?.clone(),
            credential: None,
            headers: None,
        };
        // Attempt outbound call
        info!(
            "Creating outbound call: {} -> {} (should_bridge_media: {})",
            caller, target_location.destination, should_bridge_media
        );

        match self
            .make_session(session_token, tx, bridge_builder, invite_option)
            .await
        {
            Ok(session) => {
                self.inner
                    .sessions
                    .write()
                    .await
                    .insert(session.dialog_id.clone(), session);
            }
            Err(e) => {
                error!("Failed to create session: {}", e);
                tx.reply(rsip::StatusCode::ServerInternalError)
                    .await
                    .map_err(|e| anyhow!(e))?;
            }
        }

        Ok(())
    }

    async fn make_session(
        &self,
        token: CancellationToken,
        tx: &mut Transaction,
        bridge_builder: Option<MediaBridgeBuilder>,
        option: InviteOption,
    ) -> Result<Session> {
        let (caller_dlg_state_sender, mut caller_dlg_state_receiver) = mpsc::unbounded_channel();
        let caller_dialog = self
            .inner
            .dialog_layer
            .get_or_create_server_invite(
                tx,
                caller_dlg_state_sender,
                None,
                Some(option.contact.clone()),
            )
            .map_err(|e| anyhow!(e))?;

        let (callee_dlg_state_sender, mut callee_dlg_state_receiver) = mpsc::unbounded_channel();
        let callees = vec![SessionParty::new(option.callee.clone())];
        let caller = SessionParty::new(option.caller.clone());

        let (callee_dialog, callee_tx) = self
            .inner
            .dialog_layer
            .create_client_invite_dialog(option, callee_dlg_state_sender)
            .map_err(|e| anyhow!(e))?;

        let session = Arc::new(Mutex::new(Some(Session::new(
            token.clone(),
            caller_dialog.id().clone(),
            callee_dialog.id().clone(),
            caller,
            callees,
        ))));

        let caller_dialog_ref = caller_dialog.clone();
        let callee_dialog_ref = callee_dialog.clone();

        let caller_dlg_loop = async move {
            while let Some(state) = caller_dlg_state_receiver.recv().await {
                match state {
                    DialogState::Terminated(dlg_id, reason) => {
                        info!(
                            "Caller dialog terminated: {:?} reason: {:?}",
                            dlg_id, reason
                        );
                        callee_dialog_ref.hangup().await.ok();
                        break;
                    }
                    _ => {}
                }
            }
        };
        let session_ref = session.clone();
        let callee_dialog_loop = async move {
            while let Some(state) = callee_dlg_state_receiver.recv().await {
                match state {
                    DialogState::Terminated(dlg_id, reason) => {
                        info!(
                            "Callee dialog terminated: {:?} reason: {:?}",
                            dlg_id, reason
                        );
                        caller_dialog_ref.bye().await.ok();
                        break;
                    }
                    DialogState::Confirmed(dlg_id) => {
                        info!("Callee dialog confirmed: {:?}", dlg_id);
                        match session_ref.lock().await.as_mut() {
                            Some(session) => {
                                session.set_status_code(200);
                                session.set_established();
                            }
                            None => {}
                        }
                    }
                    DialogState::Early(dlg_id, resp) => {
                        info!("Callee dialog early: {:?} {:?}", dlg_id, resp);
                        match session_ref.lock().await.as_mut() {
                            Some(session) => {
                                session.set_ringing();
                            }
                            None => {}
                        }
                    }
                    _ => {}
                }
            }
        };
        let event_sender = crate::event::create_event_sender();
        let mut event_receiver = event_sender.subscribe();
        let media_event_loop = async move {
            while let Ok(event) = event_receiver.recv().await {
                debug!("Media event: {:?}", event);
            }
        };
        let token_ref = token.clone();
        tokio::spawn(async move {
            select! {
                _ = token_ref.cancelled() => {
                    info!("Session token cancelled");
                }
                _ = caller_dlg_loop => {
                    info!("Caller dialog loop terminated");
                }
                _ = callee_dialog_loop => {
                    info!("Callee dialog loop terminated");
                }
                _ = media_event_loop => {
                    info!("Media event loop terminated");
                }
            }
        });

        match callee_dialog.process_invite(callee_tx).await {
            Ok((new_dlg_id, resp)) => match resp {
                Some(resp) => {
                    info!(
                        "Callee dialog confirmed: {:?} => {:?} status: {:?}",
                        callee_dialog.id(),
                        new_dlg_id,
                        resp.status_code
                    );
                    match bridge_builder {
                        Some(mut builder) => {
                            let callee_answer = String::from_utf8_lossy(&resp.body).to_string();
                            let caller_offer =
                                String::from_utf8_lossy(&tx.original.body).to_string();
                            match builder.handshake(caller_offer, callee_answer, None).await {
                                Ok(answer) => {
                                    caller_dialog
                                        .accept(None, Some(answer.as_bytes().to_vec()))
                                        .ok();
                                    let stream = builder.build(event_sender).await;
                                    tokio::spawn(async move {
                                        stream.serve().await.ok();
                                    });
                                    let mut session = match session.lock().await.take() {
                                        Some(session) => session,
                                        None => {
                                            error!("Session not found");
                                            return Err(anyhow!("Session not found"));
                                        }
                                    };
                                    session.set_established();
                                    return Ok(session);
                                }
                                Err(e) => {
                                    error!("Error during handshake: {}", e);
                                }
                            }
                        }
                        None => {
                            caller_dialog.accept(None, Some(resp.body)).ok();
                        }
                    }
                    self.inner.dialog_layer.confirm_client_dialog(callee_dialog);
                }
                None => {
                    info!("Callee dialog rejected");
                    caller_dialog.reject().ok();
                }
            },
            Err(e) => {
                error!("Error handling callee dialog: {}", e);
                caller_dialog.reject().ok();
            }
        }
        todo!()
    }

    async fn pop_session(&self, dialog_id: &DialogId) -> Option<(Session, bool)> {
        let mut sessions = self.inner.sessions.write().await;
        let mut callee_to_sessions = self.inner.callee_to_sessions.write().await;

        if let Some(session) = sessions.remove(dialog_id) {
            callee_to_sessions.remove(&session.callee_dialog_id);
            return Some((session, true));
        } else {
            if let Some(caller_id) = callee_to_sessions.remove(dialog_id) {
                if let Some(session) = sessions.remove(&caller_id) {
                    return Some((session, false));
                }
            }
        }
        None
    }
    pub(crate) async fn handle_bye(&self, tx: &mut Transaction) -> Result<()> {
        let dialog_id = DialogId::try_from(&tx.original).unwrap();
        let (session, is_from_caller) = match self.pop_session(&dialog_id).await {
            Some((session, is_from_caller)) => (session, is_from_caller),
            None => {
                return Err(anyhow!("Session not found for BYE: {}", dialog_id));
            }
        };
        info!(?dialog_id, is_from_caller, "Session found for BYE");
        match self.inner.dialog_layer.get_dialog(&dialog_id) {
            Some(mut dialog) => {
                dialog.handle(tx).await.ok();
            }
            None => {
                warn!("Dialog not found for BYE: {}", dialog_id);
            }
        }
        match self
            .inner
            .dialog_layer
            .get_dialog(&session.callee_dialog_id)
        {
            Some(dialog) => {
                dialog.hangup().await.ok();
            }
            None => {
                warn!("Dialog not found for BYE: {}", session.callee_dialog_id);
            }
        }
        let hangup_reason = if is_from_caller {
            Some(CallRecordHangupReason::ByCaller)
        } else {
            Some(CallRecordHangupReason::ByCallee)
        };
        let call_record = self.create_call_record(&session, hangup_reason);
        self.send_call_record(call_record).await;
        info!("Session terminated: {}", dialog_id);
        Ok(())
    }

    async fn handle_options(&self, tx: &mut Transaction) -> Result<()> {
        tx.reply(rsip::StatusCode::OK).await.ok();
        Ok(())
    }

    /// Create a call record from a session
    fn create_call_record(
        &self,
        session: &Session,
        hangup_reason: Option<CallRecordHangupReason>,
    ) -> CallRecord {
        CallRecord {
            call_type: ActiveCallType::Sip,
            option: None,
            call_id: session.dialog_id.to_string(),
            start_time: session.start_time,
            ring_time: session.ring_time,
            answer_time: session.answer_time,
            end_time: Utc::now(),
            caller: session.caller.get_user(),
            callee: session
                .callees
                .first()
                .map(|c| c.get_user())
                .unwrap_or_default(),
            status_code: session.status_code,
            offer: session.caller.last_sdp.clone(),
            answer: session.callees.first().and_then(|c| c.last_sdp.clone()),
            hangup_reason,
            recorder: vec![],
            extras: None,
            dump_event_file: None,
        }
    }

    /// Send call record if sender is available
    async fn send_call_record(&self, call_record: CallRecord) {
        if let Some(ref sender) = self.inner.callrecord_sender {
            if let Err(e) = sender.send(call_record) {
                error!("Failed to send call record: {}", e);
            }
        }
    }

    async fn cleanup_all_sessions(&self) -> Result<()> {
        self.inner.sessions.write().await.clear();
        self.inner.callee_to_sessions.write().await.clear();
        Ok(())
    }
}

#[async_trait]
impl ProxyModule for CallModule {
    fn name(&self) -> &str {
        "call"
    }

    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Invite,
            rsip::Method::Bye,
            rsip::Method::Info,
            rsip::Method::Ack,
            rsip::Method::Cancel,
            rsip::Method::Options,
        ]
    }

    async fn on_start(&mut self) -> Result<()> {
        info!("Call module with Dialog-based B2BUA started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        info!("Call module stopped, cleaning up sessions");
        self.cleanup_all_sessions().await?;
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        match tx.original.method {
            rsip::Method::Invite => {
                if let Err(e) = self.handle_invite(tx).await {
                    error!("Error handling INVITE: {}", e);
                    if tx.last_response.is_none() {
                        tx.reply_with(
                            rsip::StatusCode::ServerInternalError,
                            vec![rsip::Header::ErrorInfo(e.to_string().into())],
                            None,
                        )
                        .await
                        .map_err(|e| anyhow!(e))?;
                    }
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Bye => {
                if let Err(e) = self.handle_bye(tx).await {
                    error!("Error handling BYE: {}", e);
                    if tx.last_response.is_none() {
                        tx.reply(rsip::StatusCode::ServerInternalError)
                            .await
                            .map_err(|e| anyhow!(e))?;
                    }
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Options => {
                if let Err(e) = self.handle_options(tx).await {
                    error!("Error handling OPTIONS: {}", e);
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Ack | rsip::Method::Cancel => {
                warn!("Received ACK or CANCEL for non-INVITE transaction");
                Ok(ProxyAction::Abort)
            }
            _ => Ok(ProxyAction::Continue),
        }
    }

    async fn on_transaction_end(&self, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }
}
