use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::callrecord::{CallRecord, CallRecordHangupReason, CallRecordSender};
use crate::config::{MediaProxyConfig, MediaProxyMode, ProxyConfig};
use crate::event::EventSender;
use crate::handler::call::ActiveCallType;
use crate::proxy::bridge::{MediaBridgeBuilder, MediaBridgeType};
use crate::proxy::session::Session;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use rsip::headers::UntypedHeader;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::DialogId;
use rsipstack::rsip_ext::RsipHeadersExt;
use rsipstack::transaction::transaction::Transaction;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::select;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
    media_proxy_config: Arc<MediaProxyConfig>,
    server: SipServerRef,
    pub(crate) sessions: Arc<RwLock<HashMap<DialogId, Session>>>, // dialog_id -> session
    pub(crate) callee_to_sessions: Arc<RwLock<HashMap<DialogId, DialogId>>>, // callee's dialog_id -> caller's dialog_id
    callrecord_sender: Option<CallRecordSender>,
    event_sender: EventSender,
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
        let inner = Arc::new(CallModuleInner {
            media_proxy_config: Arc::new(config.media_proxy.clone()),
            config,
            server: server.clone(),
            sessions,
            callee_to_sessions: Arc::new(RwLock::new(HashMap::new())),
            callrecord_sender: server.callrecord_sender.clone(),
            event_sender: crate::event::create_event_sender(),
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

    /// Check if media proxy is needed based on nat_only configuration
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

    async fn handle_invite(&self, tx: &mut Transaction) -> Result<()> {
        let caller = tx.original.from_header()?.uri()?.to_string();
        let callee_uri = tx.original.to_header()?.uri()?;
        let callee = callee_uri.user().unwrap_or_default().to_string();
        let callee_realm = callee_uri.host().to_string();

        if !self.inner.config.is_same_realm(&callee_realm) {
            info!(callee_realm, "Forwarding INVITE to external realm");
            return self.forward_to_proxy(tx, &callee_realm).await;
        }

        // Check if request body contains SDP
        let caller_offer = if !tx.original.body.is_empty() {
            String::from_utf8_lossy(&tx.original.body).to_string()
        } else {
            return Err(anyhow!("No SDP offer found in INVITE"));
        };

        let target_locations = match self
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

        let caller_iswebrtc = self.is_webrtc_sdp(&caller_offer);

        let should_bridge_media =
            self.should_use_media_bridge(tx, caller_iswebrtc, target_location.supports_webrtc)?;

        if should_bridge_media {
            info!("Media bridge required for NAT traversal");
        }

        // Handle media bridging based on session type and proxy requirements
        let cancel_token = CancellationToken::new();

        let bridge_type = match (caller_iswebrtc, target_location.supports_webrtc) {
            (true, true) => MediaBridgeType::Webrtc2Webrtc,
            (true, false) => MediaBridgeType::WebRtcToRtp,
            (false, true) => MediaBridgeType::RtpToWebRtc,
            (false, false) => MediaBridgeType::Rtp2Rtp,
        };

        let mut bridge_builder = MediaBridgeBuilder::new(
            bridge_type,
            cancel_token.clone(),
            self.inner.media_proxy_config.clone(),
        )
        .await?;

        let mut inv_req = tx.original.clone();
        inv_req.body = match bridge_builder.local_description().await {
            Ok(body) => body.as_bytes().to_vec(),
            Err(e) => {
                error!("Failed to get local description: {}", e);
                tx.reply(rsip::StatusCode::ServerInternalError)
                    .await
                    .map_err(|e| anyhow!(e))?;
                return Ok(());
            }
        };

        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        inv_req.headers.push_front(via.into());

        if let Ok(record_route) = tx.endpoint_inner.get_record_route() {
            inv_req.headers.push_front(record_route.into());
        }

        // let key = TransactionKey::from_request(&inv_req, TransactionRole::Client)
        //     .map_err(|e| anyhow!(e))?;

        // info!(
        //     "Forwarding INVITE: {} -> {} (type: {:?})",
        //     caller, target_location.destination, bridge_type
        // );

        // let mut inv_tx = Transaction::new_client(key, inv_req, tx.endpoint_inner.clone(), None);
        // inv_tx.destination = Some(target_location.destination.clone());
        // inv_tx.send().await.map_err(|e| anyhow!(e))?;

        // loop {
        //     if inv_tx.is_terminated() {
        //         break;
        //     }

        //     select! {
        //         msg = inv_tx.receive() => {
        //             if let Some(msg) = msg {
        //                 match msg {
        //                     rsip::message::SipMessage::Response(mut resp) => {
        //                         if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
        //                             let dialog_id = match DialogId::try_from(&resp) {
        //                                 Ok(id) => id,
        //                                 Err(e) => {
        //                                     error!("Failed to create dialog ID: {}", e);
        //                                     return tx
        //                                         .reply(rsip::StatusCode::ServerInternalError)
        //                                         .await
        //                                         .map_err(|e| anyhow!(e));
        //                                 }
        //                             };

        //                             let caller_uri = tx.original.from_header()?.uri()?.clone();
        //                             if resp.body.is_empty() {
        //                                 return Err(anyhow!("No SDP answer found in INVITE"));
        //                             }
        //                             let callee_answer = String::from_utf8_lossy(&resp.body).to_string();
        //                             // Use session manager to handle the response
        //                             self.handle_invite_response(
        //                                 dialog_id.clone(),
        //                                 bridge_type,
        //                                 caller_uri,
        //                                 callee_uri.clone(),
        //                                 caller_offer.clone(),
        //                                 callee_answer,
        //                             ).await?;
        //                         }
        //                         header_pop!(resp.headers, rsip::Header::Via);
        //                         tx.respond(resp).await.map_err(|e| anyhow!(e))?;
        //                     }
        //                     _ => {}
        //                 }
        //             }
        //         }
        //         msg = tx.receive() => {
        //             if let Some(msg) = msg {
        //                 match msg {
        //                     rsip::message::SipMessage::Request(req) => match req.method {
        //                         rsip::Method::Ack => {
        //                             let mut ack_req = req.clone();
        //                             let via = tx.endpoint_inner.get_via(None, None).map_err(|e| anyhow!(e))?;
        //                             ack_req.headers.push_front(via.into());
        //                             let key = TransactionKey::from_request(&ack_req, TransactionRole::Client).map_err(|e| anyhow!(e))?;
        //                             let mut ack_tx = Transaction::new_client(key, ack_req, tx.endpoint_inner.clone(), None);
        //                             ack_tx.destination = Some(target_location.destination.clone());
        //                             ack_tx.send().await.map_err(|e| anyhow!(e))?;
        //                         }
        //                         _ => {}
        //                     },
        //                     _ => {}
        //                 }
        //             }
        //         }
        //     }
        // }
        Ok(())
    }

    pub(crate) async fn handle_bye(&self, tx: &mut Transaction) -> Result<()> {
        // let dialog_id = match DialogId::try_from(&tx.original) {
        //     Ok(id) => id,
        //     Err(e) => {
        //         error!("Failed to parse dialog ID: {}", e);
        //         return tx
        //             .reply(rsip::StatusCode::BadRequest)
        //             .await
        //             .map_err(|e| anyhow!(e));
        //     }
        // };

        // let session = {
        //     let sessions = self.inner.sessions.read().await;
        //     sessions.get(&dialog_id).cloned()
        // }
        // .ok_or(anyhow!("Media session not found for BYE: {}", dialog_id))?;

        // info!(
        //     "Session found for BYE caller: {} callees: {:?}",
        //     session.caller.aor,
        //     session
        //         .callees
        //         .iter()
        //         .map(|p| p.aor.clone())
        //         .collect::<Vec<_>>(),
        // );

        // // Stop media stream
        // if let Some(ref stream) = session.media_stream {
        //     stream.stop(Some("call_ended".to_string()), Some("sip_bye".to_string()));
        //     if let Err(e) = stream.cleanup().await {
        //         error!("Failed to cleanup media stream: {}", e);
        //     }
        // }

        // // Create and send call record
        // let hangup_reason = Some(CallRecordHangupReason::ByCaller);
        // let call_record = self.create_call_record(ms, hangup_reason);
        // self.send_call_record(call_record).await;

        // // Route BYE to the other party
        // let target_aor = {
        //     let bye_sender_uri = tx.original.from_header()?.uri()?;
        //     if session.caller.aor.user() == bye_sender_uri.user()
        //         && session.caller.aor.host() == bye_sender_uri.host()
        //     {
        //         // BYE from caller, route to first callee
        //         if let Some(first_callee) = session.callees.first() {
        //             first_callee.aor.clone()
        //         } else {
        //             return tx
        //                 .reply(rsip::StatusCode::BadRequest)
        //                 .await
        //                 .map_err(|e| anyhow!(e));
        //         }
        //     } else {
        //         // BYE from callee, route to caller
        //         session.caller.aor.clone()
        //     }
        // };

        // // Lookup target location
        // let target_user = target_aor.user().unwrap_or_default();
        // let target_realm = target_aor.host().to_string();

        // let target_locations = match self
        //     .inner
        //     .server
        //     .locator
        //     .lookup(&target_user, Some(&target_realm))
        //     .await
        // {
        //     Ok(locations) => locations,
        //     Err(_) => {
        //         return tx
        //             .reply(rsip::StatusCode::NotFound)
        //             .await
        //             .map_err(|e| anyhow!(e));
        //     }
        // };

        // // Forward BYE
        // let selected_location = self.select_location_from_multiple(&target_locations, &target_aor);
        // let mut bye_req = tx.original.clone();
        // let via = tx
        //     .endpoint_inner
        //     .get_via(None, None)
        //     .map_err(|e| anyhow!(e))?;
        // bye_req.headers.push_front(via.into());

        // let key = TransactionKey::from_request(&bye_req, TransactionRole::Client)
        //     .map_err(|e| anyhow!(e))?;
        // let mut bye_tx = Transaction::new_client(key, bye_req, tx.endpoint_inner.clone(), None);
        // bye_tx.destination = Some(selected_location.destination.clone());
        // bye_tx.send().await.map_err(|e| anyhow!(e))?;

        // while let Some(msg) = bye_tx.receive().await {
        //     match msg {
        //         rsip::message::SipMessage::Response(mut resp) => {
        //             header_pop!(resp.headers, rsip::Header::Via);
        //             tx.respond(resp).await.map_err(|e| anyhow!(e))?;
        //             break;
        //         }
        //         _ => {}
        //     }
        // }

        // self.inner.sessions.write().await.remove(&dialog_id);
        // info!("Media session terminated: {}", dialog_id);
        Ok(())
    }

    async fn handle_options(&self, tx: &mut Transaction) -> Result<()> {
        if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
            self.update_session_activity(&dialog_id).await;
        }
        tx.reply(rsip::StatusCode::OK)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    async fn handle_ack(&self, tx: &mut Transaction) -> Result<()> {
        // if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
        //     let sessions = self.inner.sessions.write().await;
        //     if sessions.contains_key(&dialog_id) {
        //         info!("ACK received for dialog: {}", dialog_id);
        //     }
        // }
        Ok(())
    }

    async fn handle_cancel(&self, tx: &mut Transaction) -> Result<()> {
        // tx.reply(rsip::StatusCode::OK)
        //     .await
        //     .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    // pub async fn handle_invite_response(
    //     &self,
    //     cancel_token: CancellationToken,
    //     dialog_id: DialogId,
    //     caller_uri: rsip::Uri,
    //     callee_uri: rsip::Uri,
    //     media_stream: MediaStream,
    // ) -> Result<()> {
    //     // Create enhanced session
    //     let caller_party = SessionParty::new(caller_uri);
    //     let callee_party = SessionParty::new(callee_uri);
    //     let mut session = Session::new(
    //         cancel_token,
    //         dialog_id.clone(),
    //         caller_party,
    //         vec![callee_party],
    //     );
    //     session.set_established();

    //     info!("Enhanced session established: {}", dialog_id);
    //     self.inner.sessions.write().await.insert(dialog_id, session);

    //     tokio::spawn(async move {
    //         if let Err(e) = media_stream.serve().await {
    //             error!("Failed to serve media stream: {}", e);
    //         }
    //     });
    //     Ok(())
    // }

    /// Create a call record from a session
    fn create_call_record(
        &self,
        session: &Session,
        hangup_reason: Option<CallRecordHangupReason>,
    ) -> CallRecord {
        CallRecord {
            call_type: ActiveCallType::Sip,
            option: None, // Set by caller if needed
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
            answer: session.callees.first().unwrap().last_sdp.clone(),
            hangup_reason,
            recorder: vec![], // No recorder for proxy sessions
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

    async fn update_session_activity(&self, dialog_id: &DialogId) {
        if let Some(caller_dialog_id) = self.inner.callee_to_sessions.read().await.get(dialog_id) {
            if let Some(session) = self.inner.sessions.write().await.get_mut(caller_dialog_id) {
                session.last_activity = std::time::Instant::now();
            }
        } else {
            if let Some(session) = self.inner.sessions.write().await.get_mut(dialog_id) {
                session.last_activity = std::time::Instant::now();
            }
        }
    }

    async fn cleanup_all_sessions(&self) -> Result<()> {
        {
            let mut sessions = self.inner.sessions.write().await;
            sessions.clear();
        }
        {
            let mut callee_to_sessions = self.inner.callee_to_sessions.write().await;
            callee_to_sessions.clear();
        }
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
        info!("Enhanced call module with WebRTC-SIP bridge started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        info!("Enhanced call module stopped, cleaning up media sessions");
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
                if let Err(e) = self.handle_ack(tx).await {
                    error!("Error handling ACK: {}", e);
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Cancel => {
                if let Err(e) = self.handle_cancel(tx).await {
                    error!("Error handling CANCEL: {}", e);
                }
                Ok(ProxyAction::Abort)
            }
            _ => Ok(ProxyAction::Continue),
        }
    }

    async fn on_transaction_end(&self, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }
}
