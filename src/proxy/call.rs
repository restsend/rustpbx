use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::callrecord::{CallRecord, CallRecordHangupReason, CallRecordSender};
use crate::config::{MediaProxyConfig, MediaProxyMode, ProxyConfig, RouteResult};
use crate::handler::call::ActiveCallType;
use crate::proxy::bridge::{MediaBridgeBuilder, MediaBridgeType};
use crate::proxy::locator::Location;
use crate::proxy::server::TransactionCookie;
use crate::proxy::session::{Session, SessionParty};
use crate::proxy::user::SipUser;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use rsip::headers::UntypedHeader;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::dialog::DialogState;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::DialogId;
use rsipstack::rsip_ext::RsipResponseExt;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::SipAddr;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
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
    pub(crate) fn shoud_nat_traversal(
        &self,
        tx: &Transaction,
        caller_iswebrtc: bool,
        target_iswebrtc: bool,
    ) -> Result<bool> {
        let media_config = &self.inner.config.media_proxy;
        match media_config.mode {
            MediaProxyMode::None => Ok(false),
            MediaProxyMode::All => Ok(true),
            _ => {
                // Auto mode
                if matches!(media_config.mode, MediaProxyMode::Auto) {
                    match (caller_iswebrtc, target_iswebrtc) {
                        (true, true) => return Ok(false),
                        (true, false) => return Ok(true),
                        (false, true) => return Ok(true),
                        _ => {}
                    }
                }
                // NAT mode
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

    pub(crate) async fn handle_invite(
        &self,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<()> {
        let caller = cookie.get_user().ok_or(anyhow!("No user in cookie"))?;
        let callee_uri = tx.original.to_header()?.uri()?;
        let callee = callee_uri.user().unwrap_or_default().to_string();
        let callee_realm = callee_uri.host().to_string();

        let target_locations = if !self.inner.server.is_same_realm(&callee_realm).await {
            info!(callee_realm, "Forwarding INVITE to external realm");
            vec![Location {
                aor: callee_uri.clone(),
                destination: SipAddr::try_from(&callee_uri).map_err(|e| anyhow!(e))?,
                supports_webrtc: false,
                expires: 0,
                last_modified: Instant::now(),
            }]
        } else {
            // Check if callee exists in locator
            match self
                .inner
                .server
                .locator
                .lookup(&callee, Some(&callee_realm))
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
            self.shoud_nat_traversal(tx, caller_iswebrtc, target_location.supports_webrtc)?;
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

        let caller_contact = match caller.build_contact_from_invite(&*tx) {
            Some(contact) => contact,
            None => {
                return Err(anyhow!("Failed to build caller contact"));
            }
        };
        // Create InviteOption for outbound call
        let mut invite_option = InviteOption {
            caller: caller_contact.uri.clone(),
            callee: target_location.aor.clone().into(),
            destination: Some(target_location.destination.clone()),
            content_type: Some("application/sdp".to_string()),
            offer: Some(outbound_sdp.as_bytes().to_vec()),
            contact: caller_contact.uri.clone(),
            credential: None,
            headers: None,
        };
        let route_result = self
            .inner
            .config
            .route_invite(
                invite_option,
                &tx.original,
                &self.inner.server.routing_state,
            )
            .await?;
        match route_result {
            RouteResult::Forward(option) => {
                invite_option = option;
            }
            RouteResult::Abort(code, reason) => {
                info!("Route abort: {} {}", code, reason);
                let headers = vec![];
                tx.reply_with(code.into(), headers, None)
                    .await
                    .map_err(|e| anyhow!(e))?;
                return Ok(());
            }
        }
        // Attempt outbound call
        info!(
            "Creating outbound call: {} -> {} via {:?} (should_bridge_media: {}) sdp:\n {:?}",
            invite_option.caller,
            invite_option.callee,
            invite_option.destination,
            should_bridge_media,
            outbound_sdp
        );
        let (caller_dlg_state_sender, mut caller_dlg_state_receiver) = mpsc::unbounded_channel();
        let caller_dialog = self
            .inner
            .dialog_layer
            .get_or_create_server_invite(
                tx,
                caller_dlg_state_sender,
                None,
                Some(invite_option.contact.clone()),
            )
            .map_err(|e| anyhow!(e))?;

        let (callee_dlg_state_sender, mut callee_dlg_state_receiver) = mpsc::unbounded_channel();

        let caller = SessionParty::new(invite_option.caller.clone());
        let callees = vec![SessionParty::new(invite_option.callee.clone())];

        let (callee_dialog, callee_tx) = self
            .inner
            .dialog_layer
            .create_client_invite_dialog(invite_option, callee_dlg_state_sender)
            .map_err(|e| anyhow!(e))?;

        let session = Arc::new(Mutex::new(Some(Session::new(
            session_token.clone(),
            caller_dialog.id().clone(),
            callee_dialog.id().clone(),
            caller,
            callees,
        ))));

        let callee_dialog_ref = callee_dialog.clone();

        let caller_dlg_loop = async move {
            while let Some(state) = caller_dlg_state_receiver.recv().await {
                match state {
                    DialogState::Terminated(dlg_id, reason) => {
                        info!(
                            "Caller dialog terminated: {:?} reason: {:?}",
                            dlg_id, reason
                        );
                        tokio::spawn(async move {
                            callee_dialog_ref.hangup().await.ok();
                        });
                        break;
                    }
                    _ => {}
                }
            }
        };
        let session_ref = session.clone();
        let caller_dialog_ref = caller_dialog.clone();
        let callee_dialog_loop = async move {
            while let Some(state) = callee_dlg_state_receiver.recv().await {
                match state {
                    DialogState::Terminated(dlg_id, reason) => {
                        info!(
                            "Callee dialog terminated: {:?} reason: {:?}",
                            dlg_id, reason
                        );
                        tokio::spawn(async move {
                            caller_dialog_ref.bye().await.ok();
                        });
                        break;
                    }
                    DialogState::Confirmed(dlg_id) => {
                        info!("Callee dialog confirmed: {:?}", dlg_id);
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
        let mut caller_dialog_ref = caller_dialog.clone();
        let caller_offer = String::from_utf8_lossy(&tx.original.body).to_string();
        let dialog_layer = self.inner.dialog_layer.clone();
        let make_callee_dialog_loop = async move {
            match callee_dialog.process_invite(callee_tx).await {
                Ok((new_dlg_id, resp)) => match resp {
                    Some(resp) => {
                        info!(
                            "Callee dialog confirmed: {} status: {:?}",
                            new_dlg_id.to_string(),
                            resp.status_code
                        );
                        dialog_layer.confirm_client_dialog(callee_dialog);

                        let answer_headers = resp
                            .content_type()
                            .map(|ct| vec![ct.into()])
                            .unwrap_or_default();

                        match bridge_builder {
                            Some(mut builder) => {
                                let callee_answer = String::from_utf8_lossy(&resp.body).to_string();
                                match builder.handshake(caller_offer, callee_answer, None).await {
                                    Ok(answer) => {
                                        match caller_dialog.accept(
                                            Some(answer_headers),
                                            Some(answer.as_bytes().to_vec()),
                                        ) {
                                            Ok(_) => {
                                                info!("Handshake successful");
                                            }
                                            Err(e) => {
                                                error!("Error accepting callee dialog: {}", e);
                                            }
                                        }
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
                                        session.set_status_code(200);
                                        session.set_established(new_dlg_id);
                                        return Ok(Some(session));
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
            };
            Ok(None)
        };
        let token_ref = session_token.clone();
        let sessions_ref = self.inner.sessions.clone();
        let callee_to_sessions_ref = self.inner.callee_to_sessions.clone();
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
            _ = async {
                match make_callee_dialog_loop.await {
                    Ok(Some(session)) => {
                        info!("Session created caller:{:?} callee: {:?}", session.dialog_id, session.callee_dialog_id);
                        callee_to_sessions_ref.write().await.insert(session.callee_dialog_id.clone(), session.dialog_id.clone());
                        sessions_ref.write().await.insert(session.dialog_id.clone(), session);
                        token_ref.cancelled().await;
                    }
                    Ok(None) => {
                        token_ref.cancelled().await;
                        info!("Callee dialog loop terminated");
                    }
                    Err(e) => {
                        error!("Error making callee dialog: {}", e);
                    }
                }
            } => {
                info!("Callee dialog loop terminated");
            }
            }
            info!("Session loop terminated");
        });
        select! {
            _ = session_token.cancelled() => {
                info!("Session token cancelled");
            }
            _ = caller_dialog_ref.handle(tx) => {
                info!("Caller dialog loop terminated");
            }
        };
        Ok(())
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
        debug!("handle_bye: {}", tx.original.to_string());
        let dialog_id = DialogId::try_from(&tx.original).unwrap();
        let (session, is_from_caller) = match self.pop_session(&dialog_id).await {
            Some((session, is_from_caller)) => (session, is_from_caller),
            None => {
                // // Sesssion not found, but it may be a callee dialog
                // // so we need to hang up the callee dialog
                // match self.inner.dialog_layer.get_dialog(&dialog_id) {
                //     Some(dialog) => {
                //         let mut bye_req = tx.original.clone();
                //         let via = tx
                //             .endpoint_inner
                //             .get_via(None, None)
                //             .map_err(|e| anyhow!(e))?;
                //         bye_req.headers.push_front(via.into());

                //         let key = TransactionKey::from_request(&bye_req, TransactionRole::Client)
                //             .expect("client_transaction");

                //         let mut bye_tx =
                //             Transaction::new_client(key, bye_req, tx.endpoint_inner.clone(), None);
                //         match dialog.remote_contact() {
                //             Some(ref contact) => {
                //                 bye_tx.destination = contact.try_into().ok();
                //             }
                //             None => {
                //                 warn!("No remote contact found for BYE: {}", dialog_id);
                //                 return Err(anyhow!(
                //                     "No remote contact found for BYE: {}",
                //                     dialog_id
                //                 ));
                //             }
                //         }
                //         info!(
                //             "UAC/BYE Forwarding \n{} \nto {:?}",
                //             tx.original.to_string(),
                //             bye_tx.destination
                //         );
                //         bye_tx.send().await.ok();
                //         while let Some(msg) = bye_tx.receive().await {
                //             match msg {
                //                 rsip::message::SipMessage::Response(mut resp) => {
                //                     header_pop!(resp.headers, rsip::Header::Via);
                //                     info!("UAC/BYE Forwarding response: {}", resp.to_string());
                //                     tx.respond(resp).await.ok();
                //                     break;
                //                 }
                //                 _ => {
                //                     error!("UAC/BYE Received request: {}", msg.to_string());
                //                 }
                //             }
                //         }
                //     }
                //     None => {
                //         warn!("Dialog not found for BYE: {}", dialog_id);
                //         return Err(anyhow!("Dialog not found for BYE: {}", dialog_id));
                //     }
                // }
                return Ok(());
            }
        };
        info!(?dialog_id, is_from_caller, "Session found for BYE");
        match self.inner.dialog_layer.get_dialog(&dialog_id) {
            Some(mut dialog) => {
                debug!("Hanging up caller dialog: {:?}", dialog.id());
                dialog.handle(tx).await.ok();
            }
            None => {
                warn!("Dialog not found for BYE: {}", dialog_id);
                return Ok(());
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
        debug!("Call module with Dialog-based B2BUA started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        debug!("Call module stopped, cleaning up sessions");
        self.cleanup_all_sessions().await?;
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        match cookie.get_user() {
            None => {
                cookie.set_user(SipUser::try_from(&*tx)?);
            }
            _ => {}
        }
        debug!("on_transaction_begin: {}", tx.original.to_string());
        match tx.original.method {
            rsip::Method::Invite => {
                if let Err(e) = self.handle_invite(tx, cookie).await {
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
            rsip::Method::Ack => {
                let dialog_id = DialogId::try_from(&tx.original).map_err(|e| anyhow!(e))?;
                let mut dialog = match self.inner.dialog_layer.get_dialog(&dialog_id) {
                    Some(dialog) => dialog,
                    None => {
                        warn!("Dialog not found for ACK or CANCEL: {}", dialog_id);
                        return Ok(ProxyAction::Abort);
                    }
                };
                match dialog.handle(tx).await {
                    Ok(_) => {}
                    Err(e) => {
                        error!("Error handling ACK or CANCEL: {}", e);
                    }
                };
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Cancel => {
                info!("Received CANCEL : {:?}", tx.original);
                Ok(ProxyAction::Abort)
            }
            _ => Ok(ProxyAction::Continue),
        }
    }

    async fn on_transaction_end(&self, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }
}
