use super::mediaproxy::{MediaProxy, MediaProxyConfig, SipNatBridge, WebRtcToRtpBridge};
use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::callrecord::{CallRecord, CallRecordHangupReason, CallRecordSender};
use crate::config::{MediaProxyMode, ProxyConfig};
use crate::handler::call::ActiveCallType;
use crate::proxy::session::{MediaBridgeType, MediaSession, Session, SessionParty, SessionType};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use rsip::headers::UntypedHeader;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::DialogId;
use rsipstack::header_pop;
use rsipstack::rsip_ext::RsipHeadersExt;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::transaction::Transaction;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::select;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Information about active media bridges
#[derive(Clone)]
enum BridgeInfo {
    WebRtcToRtp(WebRtcToRtpBridge),
    SipNat(SipNatBridge),
}

#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
    server: SipServerRef,
    pub(crate) sessions: Arc<RwLock<HashMap<DialogId, MediaSession>>>,
    pub(crate) media_proxy: Option<MediaProxy>,
    callrecord_sender: Option<CallRecordSender>,
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

        // Create MediaProxy if media proxy is enabled
        let media_proxy = if config.media_proxy.mode != MediaProxyMode::None {
            let local_ip = crate::net_tool::get_first_non_loopback_interface()
                .unwrap_or_else(|_| "127.0.0.1".parse().unwrap());

            let media_config = MediaProxyConfig {
                local_ip,
                external_ip: None, // Could be configured from config in the future
                rtp_port_range: (20000, 30000), // Could be configured from config
                rtcp_mux: true,
                stun_server: None, // Could be configured from config
            };
            Some(MediaProxy::new(media_config))
        } else {
            None
        };

        let inner = Arc::new(CallModuleInner {
            config,
            server: server.clone(),
            sessions,
            media_proxy,
            callrecord_sender: server.callrecord_sender.clone(),
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
    pub(crate) fn should_use_media_proxy(&self, tx: &Transaction) -> Result<bool> {
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
        let sdp_offer = if !tx.original.body.is_empty() {
            Some(String::from_utf8_lossy(&tx.original.body).to_string())
        } else {
            None
        };

        // Detect if this is WebRTC to SIP call
        let session_type = match &sdp_offer {
            Some(sdp) if self.is_webrtc_sdp(sdp) => SessionType::WebRtcToSip,
            Some(_) => SessionType::SipToSip,
            None => SessionType::SipToSip,
        };

        info!("Detected session type: {:?}", session_type);

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

        let target_location = self.select_location_from_multiple(&target_locations, &callee_uri);

        let should_proxy_media = self.should_use_media_proxy(tx)?;
        if should_proxy_media {
            info!("Media proxy required for NAT traversal");
        }

        // Handle media bridging based on session type and proxy requirements
        let session_id = format!("{}_{}", caller, callee);
        let cancel_token = CancellationToken::new();

        let (processed_body, bridge_info) = match (&session_type, should_proxy_media) {
            (SessionType::WebRtcToSip, _) => {
                if let Some(ref webrtc_sdp) = sdp_offer {
                    info!("Processing WebRTC to SIP conversion using MediaProxy");

                    if let Some(ref media_proxy) = self.inner.media_proxy {
                        let bridge = media_proxy
                            .create_webrtc_to_rtp_bridge(
                                session_id.clone(),
                                webrtc_sdp,
                                cancel_token.clone(),
                            )
                            .await?;

                        // Generate SIP compatible SDP
                        let sip_sdp = bridge.generate_sip_sdp_answer(webrtc_sdp)?;

                        // Start the bridge
                        bridge.start().await?;

                        (
                            Some(sip_sdp.as_bytes().to_vec()),
                            Some(BridgeInfo::WebRtcToRtp(bridge)),
                        )
                    } else {
                        warn!("MediaProxy not available for WebRTC to SIP conversion");
                        (Some(tx.original.body.clone()), None)
                    }
                } else {
                    (Some(tx.original.body.clone()), None)
                }
            }
            (SessionType::SipToSip, true) => {
                if let Some(ref offer_sdp) = sdp_offer {
                    info!("Processing SIP to SIP NAT conversion using MediaProxy");

                    if let Some(ref media_proxy) = self.inner.media_proxy {
                        let bridge = media_proxy
                            .create_sip_nat_bridge(
                                session_id.clone(),
                                offer_sdp,
                                cancel_token.clone(),
                            )
                            .await?;

                        // Set caller's remote SDP (original INVITE SDP)
                        if let Err(e) = bridge.set_caller_remote_sdp(offer_sdp).await {
                            error!("Failed to set caller remote SDP for SIP NAT bridge: {}", e);
                        } else {
                            info!("Successfully set caller remote SDP for SIP NAT bridge");
                        }

                        // Generate modified SDP for callee
                        let callee_sdp = bridge.generate_callee_invite_sdp(offer_sdp)?;

                        // Start the bridge
                        bridge.start().await?;

                        (
                            Some(callee_sdp.as_bytes().to_vec()),
                            Some(BridgeInfo::SipNat(bridge)),
                        )
                    } else {
                        warn!("MediaProxy not available for SIP NAT conversion");
                        (Some(tx.original.body.clone()), None)
                    }
                } else {
                    (Some(tx.original.body.clone()), None)
                }
            }
            _ => {
                // No media proxy needed for this call
                (Some(tx.original.body.clone()), None)
            }
        };

        let mut inv_req = tx.original.clone();

        // Update request body with processed SDP
        if let Some(body) = processed_body {
            inv_req.body = body;
        }

        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        inv_req.headers.push_front(via.into());

        if let Ok(record_route) = tx.endpoint_inner.get_record_route() {
            inv_req.headers.push_front(record_route.into());
        }

        let key = TransactionKey::from_request(&inv_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;
        info!(
            "Forwarding INVITE: {} -> {} (type: {:?})",
            caller, target_location.destination, session_type
        );

        let mut inv_tx = Transaction::new_client(key, inv_req, tx.endpoint_inner.clone(), None);
        inv_tx.destination = Some(target_location.destination.clone());
        inv_tx.send().await.map_err(|e| anyhow!(e))?;

        loop {
            if inv_tx.is_terminated() {
                break;
            }

            select! {
                msg = inv_tx.receive() => {
                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Response(mut resp) => {
                                if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
                                    let dialog_id = match DialogId::try_from(&resp) {
                                        Ok(id) => id,
                                        Err(e) => {
                                            error!("Failed to create dialog ID: {}", e);
                                            return tx
                                                .reply(rsip::StatusCode::ServerInternalError)
                                                .await
                                                .map_err(|e| anyhow!(e));
                                        }
                                    };

                                    let caller_uri = tx.original.from_header()?.uri()?.clone();
                                    let sip_answer = if resp.body.is_empty() {
                                        None
                                    } else {
                                        Some(String::from_utf8_lossy(&resp.body).to_string())
                                    };

                                    // Set remote SDP for bridge if we have bridge info and SIP answer
                                    if let (Some(ref bridge), Some(ref answer)) = (&bridge_info, &sip_answer) {
                                        match bridge {
                                            BridgeInfo::WebRtcToRtp(webrtc_bridge) => {
                                                if let Err(e) = webrtc_bridge.set_sip_answer_sdp(answer).await {
                                                    error!("Failed to set SIP answer SDP for WebRTC bridge: {}", e);
                                                } else {
                                                    info!("Successfully set SIP answer SDP for WebRTC bridge");
                                                }
                                            }
                                            BridgeInfo::SipNat(nat_bridge) => {
                                                // Set callee's answer SDP for SIP NAT bridge
                                                if let Err(e) = nat_bridge.set_callee_answer_sdp(answer).await {
                                                    error!("Failed to set callee answer SDP for SIP NAT bridge: {}", e);
                                                } else {
                                                    info!("Successfully set callee answer SDP for SIP NAT bridge");
                                                }
                                            }
                                        }
                                    }

                                    // Use session manager to handle the response
                                    self.handle_invite_response(
                                        dialog_id.clone(),
                                        session_type.clone(),
                                        caller_uri,
                                        callee_uri.clone(),
                                        sdp_offer.clone(),
                                        sip_answer,
                                        bridge_info.clone(),
                                    ).await?;
                                }
                                header_pop!(resp.headers, rsip::Header::Via);
                                tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                            }
                            _ => {}
                        }
                    }
                }
                msg = tx.receive() => {
                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Request(req) => match req.method {
                                rsip::Method::Ack => {
                                    let mut ack_req = req.clone();
                                    let via = tx.endpoint_inner.get_via(None, None).map_err(|e| anyhow!(e))?;
                                    ack_req.headers.push_front(via.into());
                                    let key = TransactionKey::from_request(&ack_req, TransactionRole::Client).map_err(|e| anyhow!(e))?;
                                    let mut ack_tx = Transaction::new_client(key, ack_req, tx.endpoint_inner.clone(), None);
                                    ack_tx.destination = Some(target_location.destination.clone());
                                    ack_tx.send().await.map_err(|e| anyhow!(e))?;
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn handle_bye(&self, tx: &mut Transaction) -> Result<()> {
        let dialog_id = match DialogId::try_from(&tx.original) {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to parse dialog ID: {}", e);
                return tx
                    .reply(rsip::StatusCode::BadRequest)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        let media_session = {
            let sessions = self.inner.sessions.read().await;
            sessions.get(&dialog_id).cloned()
        };

        if let Some(ref ms) = media_session {
            info!(
                "Media session found for BYE caller: {} callees: {:?} type: {:?}",
                ms.session.caller.aor,
                ms.session
                    .callees
                    .iter()
                    .map(|p| p.aor.clone())
                    .collect::<Vec<_>>(),
                ms.session_type
            );

            // Stop media stream
            if let Some(ref stream) = ms.media_stream {
                stream.stop(Some("call_ended".to_string()), Some("sip_bye".to_string()));
                if let Err(e) = stream.cleanup().await {
                    error!("Failed to cleanup media stream: {}", e);
                }
            }

            // Create and send call record
            let hangup_reason = Some(CallRecordHangupReason::ByCaller);
            let call_record = self.create_call_record(ms, hangup_reason);
            self.send_call_record(call_record).await;
        }

        // Route BYE to the other party
        let target_aor = match media_session {
            Some(ref ms) => {
                let bye_sender_uri = tx.original.from_header()?.uri()?;
                if ms.session.caller.aor.user() == bye_sender_uri.user()
                    && ms.session.caller.aor.host() == bye_sender_uri.host()
                {
                    // BYE from caller, route to first callee
                    if let Some(first_callee) = ms.session.callees.first() {
                        first_callee.aor.clone()
                    } else {
                        return tx
                            .reply(rsip::StatusCode::BadRequest)
                            .await
                            .map_err(|e| anyhow!(e));
                    }
                } else {
                    // BYE from callee, route to caller
                    ms.session.caller.aor.clone()
                }
            }
            None => {
                info!("Media session not found for BYE: {}", dialog_id);
                return tx
                    .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        // Lookup target location
        let target_user = target_aor.user().unwrap_or_default();
        let target_realm = target_aor.host().to_string();

        let target_locations = match self
            .inner
            .server
            .locator
            .lookup(&target_user, Some(&target_realm))
            .await
        {
            Ok(locations) => locations,
            Err(_) => {
                return tx
                    .reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        // Forward BYE
        let selected_location = self.select_location_from_multiple(&target_locations, &target_aor);
        let mut bye_req = tx.original.clone();
        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        bye_req.headers.push_front(via.into());

        let key = TransactionKey::from_request(&bye_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;
        let mut bye_tx = Transaction::new_client(key, bye_req, tx.endpoint_inner.clone(), None);
        bye_tx.destination = Some(selected_location.destination.clone());
        bye_tx.send().await.map_err(|e| anyhow!(e))?;

        while let Some(msg) = bye_tx.receive().await {
            match msg {
                rsip::message::SipMessage::Response(mut resp) => {
                    header_pop!(resp.headers, rsip::Header::Via);
                    tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                    break;
                }
                _ => {}
            }
        }

        self.inner.sessions.write().await.remove(&dialog_id);
        info!("Media session terminated: {}", dialog_id);
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
        if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
            let sessions = self.inner.sessions.write().await;
            if sessions.contains_key(&dialog_id) {
                info!("ACK received for dialog: {}", dialog_id);
            }
        }
        Ok(())
    }

    async fn handle_cancel(&self, tx: &mut Transaction) -> Result<()> {
        tx.reply(rsip::StatusCode::OK)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    /// Select a location from multiple locations using a load balancing strategy
    pub(crate) fn select_location_from_multiple<'a>(
        &self,
        locations: &'a [super::locator::Location],
        _aor: &rsip::Uri,
    ) -> &'a super::locator::Location {
        // For now, just select the first location
        &locations[0]
    }

    async fn handle_invite_response(
        &self,
        dialog_id: DialogId,
        session_type: SessionType,
        caller_uri: rsip::Uri,
        callee_uri: rsip::Uri,
        sdp_offer: Option<String>,
        sip_answer: Option<String>,
        bridge_info: Option<BridgeInfo>,
    ) -> Result<()> {
        // Create enhanced session
        let caller_party = SessionParty::new(caller_uri);
        let callee_party = SessionParty::new(callee_uri);
        let mut session = Session::new(dialog_id.clone(), caller_party, vec![callee_party]);
        session.set_established();

        // Set media bridge type
        match session_type {
            SessionType::WebRtcToSip => {
                session.set_media_bridge_type(MediaBridgeType::WebRtcToSip);
            }
            SessionType::SipToWebRtc => {
                session.set_media_bridge_type(MediaBridgeType::SipToWebRtc);
            }
            _ => {
                session.set_media_bridge_type(MediaBridgeType::None);
            }
        }

        // Extract media stream from bridge info
        let media_stream = match &bridge_info {
            Some(BridgeInfo::WebRtcToRtp(bridge)) => Some(bridge.media_stream.clone()),
            Some(BridgeInfo::SipNat(bridge)) => Some(bridge.media_stream.clone()),
            None => None,
        };

        let enhanced_session = MediaSession {
            session,
            media_stream: media_stream.clone(),
            session_type: session_type.clone(),
            webrtc_sdp: sdp_offer,
            sip_sdp: sip_answer,
        };

        // Start media stream service (bridge should already be started)
        if let Some(ref stream) = enhanced_session.media_stream {
            let stream_clone = stream.clone();
            tokio::spawn(async move {
                if let Err(e) = stream_clone.serve().await {
                    error!("Media stream error: {}", e);
                }
            });
        }

        self.inner
            .sessions
            .write()
            .await
            .insert(dialog_id.clone(), enhanced_session);
        info!(
            "Enhanced session established: {} (type: {:?})",
            dialog_id, session_type
        );
        Ok(())
    }

    /// Create a call record from a session
    fn create_call_record(
        &self,
        media_session: &MediaSession,
        hangup_reason: Option<CallRecordHangupReason>,
    ) -> CallRecord {
        let session = &media_session.session;

        // Determine call type based on session type
        let call_type = match media_session.session_type {
            SessionType::WebRtcToSip | SessionType::WebRtcToWebRtc => ActiveCallType::Webrtc,
            SessionType::SipToSip | SessionType::SipToWebRtc => ActiveCallType::Sip,
        };

        CallRecord {
            call_type,
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
            offer: media_session.webrtc_sdp.clone(),
            answer: media_session.sip_sdp.clone(),
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
        if let Some(session) = self.inner.sessions.write().await.get_mut(dialog_id) {
            session.session.last_activity = std::time::Instant::now();
        }
    }

    async fn cleanup_all_sessions(&self) -> Result<()> {
        let sessions = self.inner.sessions.read().await;
        for (dialog_id, media_session) in sessions.iter() {
            if let Some(ref stream) = media_session.media_stream {
                stream.stop(
                    Some("proxy_shutdown".to_string()),
                    Some("system".to_string()),
                );
                if let Err(e) = stream.cleanup().await {
                    error!("Failed to cleanup media stream for {}: {}", dialog_id, e);
                }
            }
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
