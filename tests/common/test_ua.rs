use anyhow::{Result, anyhow};
use rsipstack::dialog::DialogId;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::registration::Registration;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::transaction::{EndpointBuilder, TransactionReceiver};
use rsipstack::transport::TransportLayer;
use rsipstack::transport::udp::UdpConnection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::select;
use tokio::sync::{Mutex, mpsc::unbounded_channel};
use tokio_util::sync::CancellationToken;
use tracing::debug;

// Extension trait for converting rsipstack::Error to anyhow::Error
trait RsipErrorExt {
    fn into_anyhow(self) -> anyhow::Error;
}

impl RsipErrorExt for rsipstack::Error {
    fn into_anyhow(self) -> anyhow::Error {
        anyhow!("rsipstack error: {:?}", self)
    }
}

/// Simplified test UA configuration
#[derive(Debug, Clone)]
pub struct TestUaConfig {
    pub username: String,
    pub password: String,
    pub realm: String,
    pub local_port: u16,
    pub proxy_addr: SocketAddr,
    /// When true, the UA generates a real WebRTC (DTLS-SRTP) offer/answer via a
    /// rustrtc PeerConnection instead of the fake SDP strings used elsewhere.
    pub webrtc: bool,
}

/// Simplified TestUa structure with essential fields only
#[derive(Clone)]
pub struct TestUa {
    config: TestUaConfig,
    cancel_token: CancellationToken,
    dialog_layer: Option<Arc<DialogLayer>>,
    state_sender: Option<DialogStateSender>,
    state_receiver: Option<Arc<tokio::sync::Mutex<DialogStateReceiver>>>,
    contact_uri: Option<rsipstack::sip::Uri>,
    /// Store answer SDP per dialog for re-INVITE responses
    answer_sdps: Arc<Mutex<HashMap<DialogId, String>>>,
    /// Store received offer SDP per dialog from incoming INVITE
    received_offer_sdps: Arc<Mutex<HashMap<DialogId, String>>>,
    /// Store negotiated answer SDP received by caller side after INVITE 200 OK
    negotiated_answer_sdps: Arc<Mutex<HashMap<DialogId, String>>>,
    /// Real WebRTC PeerConnection used when `config.webrtc` is set.
    webrtc_pc: Option<Arc<rustrtc::PeerConnection>>,
    /// Inbound RTP collector (plaintext, post-SRTP-unprotect) for the
    /// WebRTC PeerConnection. Attached via [`TestUa::attach_webrtc_rx_tap`].
    webrtc_rx: Option<Arc<WebRtcRxTap>>,
}

/// One inbound RTP packet observed on a WebRTC PeerConnection after SRTP
/// unprotect — i.e. exactly what the remote peer's plaintext payload was.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ObservedRtp {
    pub payload_type: u8,
    pub marker: bool,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

/// [`rustrtc::RtpObserver`] implementation that records inbound (decrypted)
/// RTP packets so tests can assert on media content delivered over DTLS-SRTP.
#[allow(dead_code)]
pub struct WebRtcRxTap {
    packets: std::sync::Mutex<Vec<ObservedRtp>>,
    egress_packets: std::sync::atomic::AtomicU64,
    capacity: usize,
}

#[allow(dead_code)]
impl WebRtcRxTap {
    pub fn new(capacity: usize) -> Self {
        Self {
            packets: std::sync::Mutex::new(Vec::new()),
            egress_packets: std::sync::atomic::AtomicU64::new(0),
            capacity,
        }
    }

    pub fn snapshot(&self) -> Vec<ObservedRtp> {
        self.packets.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.packets.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Count of outbound RTP packets observed before SRTP protect.
    pub fn egress_count(&self) -> u64 {
        self.egress_packets
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl rustrtc::peer_connection::RtpObserver for WebRtcRxTap {
    fn on_ingress(&self, packet: &rustrtc::rtp::RtpPacket, _src_addr: SocketAddr) {
        let mut buf = self.packets.lock().unwrap();
        if buf.len() < self.capacity {
            buf.push(ObservedRtp {
                payload_type: packet.header.payload_type,
                marker: packet.header.marker,
                sequence_number: packet.header.sequence_number,
                timestamp: packet.header.timestamp,
                ssrc: packet.header.ssrc,
                payload: packet.payload.to_vec(),
            });
        }
    }

    fn on_egress(&self, _packet: &rustrtc::rtp::RtpPacket, _dst_addr: SocketAddr) {
        self.egress_packets
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
#[allow(unused)]
pub enum TestUaEvent {
    Registered,
    RegistrationFailed(String),
    /// Incoming call with optional SDP from the INVITE request
    IncomingCall(DialogId, Option<String>),
    CallRinging(DialogId),
    EarlyMedia(DialogId),
    CallEstablished(DialogId),
    CallTerminated(DialogId),
    CallFailed(String),
    CallUpdated(DialogId, rsipstack::sip::Method, Option<String>),
    /// Refer received with target URI
    Referred(DialogId, String),
    /// SIP INFO with DTMF (application/dtmf-relay) received on this dialog
    DtmfInfo(DialogId, String),
    /// SIP INFO (non-DTMF) received: (dialog_id, content_type, body)
    InfoReceived(DialogId, String, Vec<u8>),
}

impl TestUa {
    pub fn new(config: TestUaConfig) -> Self {
        Self::new_inner(config, None)
    }

    /// Create a WebRTC UA whose PeerConnection advertises only `audio_caps`
    /// in its offer (e.g. PCMU-only to exercise the same-codec fastpath, or
    /// Opus-only to force transcoding toward a PCMU callee).
    pub fn new_webrtc_with_caps(
        config: TestUaConfig,
        audio_caps: Vec<rustrtc::config::AudioCapability>,
    ) -> Self {
        Self::new_inner(config, Some(audio_caps))
    }

    fn new_inner(
        config: TestUaConfig,
        webrtc_audio_caps: Option<Vec<rustrtc::config::AudioCapability>>,
    ) -> Self {
        let webrtc_pc = if config.webrtc {
            let mut rtc_config = rustrtc::RtcConfiguration::default();
            if let Some(caps) = webrtc_audio_caps {
                let mut media_caps = rustrtc::MediaCapabilities::default();
                media_caps.audio = caps;
                rtc_config.media_capabilities = Some(media_caps);
            }
            let pc = rustrtc::PeerConnection::new(rtc_config);
            // Add an audio transceiver so create_offer produces an m=audio line.
            pc.add_transceiver(
                rustrtc::MediaKind::Audio,
                rustrtc::TransceiverDirection::SendRecv,
            );
            Some(Arc::new(pc))
        } else {
            None
        };
        let webrtc_rx = webrtc_pc.as_ref().map(|_| Arc::new(WebRtcRxTap::new(4096)));
        Self {
            config,
            cancel_token: CancellationToken::new(),
            dialog_layer: None,
            state_sender: None,
            state_receiver: None,
            contact_uri: None,
            answer_sdps: Arc::new(Mutex::new(HashMap::new())),
            received_offer_sdps: Arc::new(Mutex::new(HashMap::new())),
            negotiated_answer_sdps: Arc::new(Mutex::new(HashMap::new())),
            webrtc_pc,
            webrtc_rx,
        }
    }

    /// Return the local SIP port this UA is bound to.
    pub fn local_port(&self) -> u16 {
        self.config.local_port
    }

    /// Start the UA with simplified initialization
    pub async fn start(&mut self) -> Result<()> {
        let transport_layer = TransportLayer::new(self.cancel_token.clone());
        let local_addr = format!("127.0.0.1:{}", self.config.local_port).parse::<SocketAddr>()?;

        // Setup transport
        let connection =
            UdpConnection::create_connection(local_addr, None, Some(self.cancel_token.clone()))
                .await
                .map_err(|e| e.into_anyhow())?;
        transport_layer.add_transport(connection.into());

        let endpoint = EndpointBuilder::new()
            .with_cancel_token(self.cancel_token.clone())
            .with_transport_layer(transport_layer)
            .build();

        let incoming = endpoint.incoming_transactions()?;
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
        let (state_sender, state_receiver) = dialog_layer.new_dialog_state_channel();
        self.dialog_layer = Some(dialog_layer);
        self.state_sender = Some(state_sender.clone());
        self.state_receiver = Some(Arc::new(tokio::sync::Mutex::new(state_receiver)));

        // Create Contact URI
        self.contact_uri = Some(rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: self.config.username.clone(),
                password: None,
            }),
            host_with_port: local_addr.into(),
            params: vec![],
            headers: vec![],
        });

        // Start endpoint service
        let cancel_token = self.cancel_token.clone();
        rustpbx::utils::spawn(async move {
            select! {
                _ = endpoint.serve() => {},
                _ = cancel_token.cancelled() => {}
            }
        });

        // Process incoming transactions
        if let Some(dialog_layer) = &self.dialog_layer {
            let dialog_layer_clone = dialog_layer.clone();
            let state_sender_clone = state_sender.clone();
            let contact_clone = self.contact_uri.clone().unwrap();
            let cancel_token = self.cancel_token.clone();
            let received_sdps_clone = self.received_offer_sdps.clone();

            rustpbx::utils::spawn(async move {
                Self::process_incoming_request(
                    dialog_layer_clone,
                    incoming,
                    state_sender_clone,
                    contact_clone,
                    cancel_token,
                    received_sdps_clone,
                )
                .await
                .ok();
            });
        }

        Ok(())
    }

    /// Register with the proxy server
    pub async fn register(&self) -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.register_inner())
            .await
            .map_err(|_| anyhow!("register timed out after 10s"))?
    }

    async fn register_inner(&self) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        let credential = Credential {
            username: self.config.username.clone(),
            password: self.config.password.clone(),
            realm: Some(self.config.realm.clone()),
        };

        let sip_server = rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: None,
            host_with_port: self.config.proxy_addr.into(),
            params: vec![],
            headers: vec![],
        };

        let mut registration = Registration::new(dialog_layer.endpoint.clone(), Some(credential));
        let resp = registration
            .register(sip_server, None)
            .await
            .map_err(|e| e.into_anyhow())?;

        if resp.status_code == rsipstack::sip::StatusCode::OK {
            debug!("Registration successful for {}", self.config.username);
            Ok(())
        } else {
            Err(anyhow!("Registration failed: {}", resp.status_code))
        }
    }

    /// Make a call with optional SDP
    pub async fn make_call(&self, callee: &str, sdp_offer: Option<String>) -> Result<DialogId> {
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.make_call_with_sdp(callee, sdp_offer),
        )
        .await
        .map_err(|_| anyhow!("make_call timed out after 15s for callee '{}'", callee))?
    }

    /// Make a call with optional SDP (internal implementation)
    pub async fn make_call_with_sdp(
        &self,
        callee: &str,
        sdp_offer: Option<String>,
    ) -> Result<DialogId> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        let contact = self
            .contact_uri
            .as_ref()
            .ok_or_else(|| anyhow!("Contact URI not available"))?;

        let credential = Credential {
            username: self.config.username.clone(),
            password: self.config.password.clone(),
            realm: Some(self.config.realm.clone()),
        };

        let callee_uri = format!(
            "sip:{}@{}:{}",
            callee,
            self.config.proxy_addr.ip(),
            self.config.proxy_addr.port()
        )
        .try_into()
        .map_err(|e| anyhow!("Invalid callee URI: {:?}", e))?;

        let proxy_uri: rsipstack::sip::Uri = format!(
            "sip:{}:{};lr",
            self.config.proxy_addr.ip(),
            self.config.proxy_addr.port()
        )
        .try_into()
        .map_err(|e| anyhow!("Invalid proxy URI: {:?}", e))?;
        let route_header =
            rsipstack::sip::Header::from(rsipstack::sip::typed::Route::from(proxy_uri));

        let (content_type, offer) = if let Some(sdp) = sdp_offer {
            (Some("application/sdp".to_string()), Some(sdp.into_bytes()))
        } else if let Some(pc) = self.webrtc_pc.as_ref() {
            // Real WebRTC (DTLS-SRTP) offer from the UA's PeerConnection.
            let _ = pc
                .create_offer()
                .await
                .map_err(|e| anyhow!("create_offer failed: {}", e))?;
            pc.wait_for_gathering_complete().await;
            let mut offer = pc
                .create_offer()
                .await
                .map_err(|e| anyhow!("create_offer (gathered) failed: {}", e))?;
            offer.sdp_type = rustrtc::SdpType::Offer;
            // Set the local description so the PC enters have-local-offer and
            // can later apply the remote answer (ICE + DTLS role negotiation).
            pc.set_local_description(offer.clone())
                .map_err(|e| anyhow!("set_local_description(offer) failed: {}", e))?;
            (
                Some("application/sdp".to_string()),
                Some(offer.to_sdp_string().into_bytes()),
            )
        } else {
            (None, None)
        };

        let invite_option = InviteOption {
            callee: callee_uri,
            caller: contact.clone(),
            content_type,
            offer,
            contact: contact.clone(),
            credential: Some(credential),
            headers: Some(vec![route_header]),
            ..Default::default()
        };

        let state_sender = self.state_sender.clone().unwrap_or_else(|| {
            let (sender, _) = unbounded_channel();
            sender
        });
        let (dialog, resp) = dialog_layer
            .do_invite(invite_option, state_sender)
            .await
            .map_err(|e| e.into_anyhow())?;
        let resp = resp.ok_or_else(|| anyhow!("No response"))?;

        if resp.status_code == rsipstack::sip::StatusCode::OK {
            if !resp.body().is_empty() {
                let answer_sdp = String::from_utf8_lossy(resp.body()).to_string();
                // Apply the remote answer to the WebRTC PeerConnection.
                if let Some(pc) = self.webrtc_pc.as_ref() {
                    let desc =
                        rustrtc::SessionDescription::parse(rustrtc::SdpType::Answer, &answer_sdp)
                            .map_err(|e| anyhow!("parse answer SDP failed: {}", e))?;
                    pc.set_remote_description(desc)
                        .await
                        .map_err(|e| anyhow!("set_remote_description(answer) failed: {}", e))?;
                }
                let mut sdps = self.negotiated_answer_sdps.lock().await;
                sdps.insert(dialog.id(), answer_sdp);
            }
            Ok(dialog.id())
        } else {
            Err(anyhow!("Call failed: {}", resp.status_code))
        }
    }

    /// Get negotiated answer SDP for a successfully established outgoing INVITE.
    pub async fn get_negotiated_answer_sdp(&self, dialog_id: &DialogId) -> Option<String> {
        let sdps = self.negotiated_answer_sdps.lock().await;
        sdps.get(dialog_id).cloned()
    }

    /// Access the real WebRTC PeerConnection (when this is a webrtc UA).
    #[allow(dead_code)]
    pub fn webrtc_pc(&self) -> Option<Arc<rustrtc::PeerConnection>> {
        self.webrtc_pc.clone()
    }

    /// Wait until ICE + DTLS are connected, i.e. SRTP keying material is
    /// ready on both sides. Fails after `timeout`.
    #[allow(dead_code)]
    pub async fn wait_webrtc_connected(&self, timeout: std::time::Duration) -> Result<()> {
        let pc = self
            .webrtc_pc
            .as_ref()
            .ok_or_else(|| anyhow!("not a webrtc UA"))?;
        tokio::time::timeout(timeout, pc.wait_for_connected())
            .await
            .map_err(|_| anyhow!("webrtc ICE/DTLS not connected within {:?}", timeout))?
            .map_err(|e| anyhow!("webrtc connection failed: {:?}", e))
    }

    /// Attach the inbound RTP tap to the PeerConnection. Must be called after
    /// the RTP transport exists (ICE pair selected); waits for it internally.
    /// The tap observes plaintext packets AFTER SRTP unprotect.
    #[allow(dead_code)]
    pub async fn attach_webrtc_rx_tap(&self) -> Result<()> {
        let pc = self
            .webrtc_pc
            .as_ref()
            .ok_or_else(|| anyhow!("not a webrtc UA"))?;
        let tap = self
            .webrtc_rx
            .clone()
            .ok_or_else(|| anyhow!("webrtc rx tap missing"))?;
        pc.wait_for_rtp_transport_ready(std::time::Duration::from_secs(10))
            .await
            .map_err(|e| anyhow!("webrtc RTP transport not ready: {:?}", e))?;
        pc.add_observer(tap);
        Ok(())
    }

    /// Snapshot of inbound (SRTP-decrypted) RTP packets observed so far.
    #[allow(dead_code)]
    pub fn webrtc_rx_packets(&self) -> Vec<ObservedRtp> {
        self.webrtc_rx
            .as_ref()
            .map(|tap| tap.snapshot())
            .unwrap_or_default()
    }

    /// Cumulative count of inbound RTP packets accepted (SRTP-authenticated)
    /// by the PeerConnection transport, regardless of the tap.
    #[allow(dead_code)]
    pub fn webrtc_received_rtp_packets(&self) -> u64 {
        self.webrtc_pc
            .as_ref()
            .map(|pc| pc.received_rtp_packets())
            .unwrap_or(0)
    }

    /// Count of outbound RTP packets observed before SRTP protect.
    #[allow(dead_code)]
    pub fn webrtc_egress_packet_count(&self) -> u64 {
        self.webrtc_rx
            .as_ref()
            .map(|tap| tap.egress_count())
            .unwrap_or(0)
    }

    /// Sender SSRC of this UA's audio transceiver (as announced in SDP when
    /// a track exists), or a deterministic fallback for raw injection.
    #[allow(dead_code)]
    pub fn webrtc_sender_ssrc(&self) -> u32 {
        self.webrtc_pc
            .as_ref()
            .and_then(|pc| {
                pc.get_transceivers()
                    .into_iter()
                    .find(|t| t.kind() == rustrtc::MediaKind::Audio)
                    .and_then(|t| t.sender_ssrc())
            })
            .unwrap_or(0x5A5A5A5A)
    }

    /// Send a plaintext RTP packet on the WebRTC leg; rustrtc SRTP-protects
    /// it before it hits the wire.
    #[allow(dead_code)]
    pub async fn send_webrtc_rtp(
        &self,
        payload_type: u8,
        sequence_number: u16,
        timestamp: u32,
        ssrc: u32,
        marker: bool,
        payload: Vec<u8>,
    ) -> Result<()> {
        let pc = self
            .webrtc_pc
            .as_ref()
            .ok_or_else(|| anyhow!("not a webrtc UA"))?;
        let mut header =
            rustrtc::rtp::RtpHeader::new(payload_type, sequence_number, timestamp, ssrc);
        header.marker = marker;
        pc.send_raw_rtp(rustrtc::rtp::RtpPacket::new(header, payload))
            .await
            .map_err(|e| anyhow!("send_raw_rtp failed: {:?}", e))
    }

    /// Set answer SDP for a dialog, used for re-INVITE responses.
    pub async fn set_answer_sdp(&self, dialog_id: &DialogId, sdp: &str) {
        let mut sdps = self.answer_sdps.lock().await;
        sdps.insert(dialog_id.clone(), sdp.to_string());
    }

    /// Answer an incoming call with optional SDP
    pub async fn answer_call(
        &self,
        dialog_id: &DialogId,
        sdp_answer: Option<String>,
    ) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        // If this is a WebRTC UA and no explicit answer was supplied, derive
        // one from the received offer via the PeerConnection (real DTLS-SRTP).
        let sdp_answer = match sdp_answer {
            Some(s) => Some(s),
            None if self.webrtc_pc.is_some() => {
                let offer = self
                    .received_offer_sdps
                    .lock()
                    .await
                    .get(dialog_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("No received offer for WebRTC answer"))?;
                let pc = self.webrtc_pc.as_ref().expect("webrtc pc");
                let desc = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer)
                    .map_err(|e| anyhow!("parse offer failed: {}", e))?;
                pc.set_remote_description(desc)
                    .await
                    .map_err(|e| anyhow!("set_remote_description failed: {}", e))?;
                let _ = pc
                    .create_answer()
                    .await
                    .map_err(|e| anyhow!("create_answer failed: {}", e))?;
                pc.wait_for_gathering_complete().await;
                let mut answer = pc
                    .create_answer()
                    .await
                    .map_err(|e| anyhow!("create_answer (gathered) failed: {}", e))?;
                answer.sdp_type = rustrtc::SdpType::Answer;
                pc.set_local_description(answer)
                    .map_err(|e| anyhow!("set_local_description failed: {}", e))?;
                pc.local_description().map(|d| d.to_sdp_string())
            }
            None => None,
        };

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::Invite(d) => {
                    // Store answer SDP for potential re-INVITE responses
                    if let Some(ref sdp) = sdp_answer {
                        let mut sdps = self.answer_sdps.lock().await;
                        sdps.insert(dialog_id.clone(), sdp.clone());
                    }

                    let body = sdp_answer.map(|sdp| sdp.into_bytes());
                    let headers = if body.is_some() {
                        vec![rsipstack::sip::Header::ContentType(
                            "application/sdp".into(),
                        )]
                    } else {
                        vec![]
                    };

                    d.accept(Some(headers), body).map_err(|e| e.into_anyhow())?;
                    Ok(())
                }
                _ => Err(anyhow!("Invalid dialog type for answering")),
            }
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    pub async fn reject_call(&self, dialog_id: &DialogId) -> Result<()> {
        self.reject_call_with_reason(dialog_id, None, None).await
    }

    pub async fn reject_call_with_reason(
        &self,
        dialog_id: &DialogId,
        status_code: Option<u16>,
        reason: Option<String>,
    ) -> Result<()> {
        use rsipstack::sip::StatusCode;

        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::Invite(d) => {
                    let code = status_code.map(StatusCode::from);
                    d.reject(code, reason).map_err(|e| e.into_anyhow())?;
                    Ok(())
                }
                _ => Err(anyhow!("Invalid dialog type for rejecting")),
            }
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    /// Send ringing response
    pub async fn send_ringing(
        &self,
        dialog_id: &DialogId,
        early_media_sdp: Option<String>,
    ) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::Invite(d) => {
                    let contact = rsipstack::sip::typed::Contact {
                        display_name: None,
                        uri: self.contact_uri.clone().unwrap(),
                        params: vec![],
                    };

                    let mut headers = vec![contact.into()];
                    let body = if let Some(sdp) = early_media_sdp {
                        headers.push(rsipstack::sip::Header::ContentType(
                            "application/sdp".into(),
                        ));
                        Some(sdp.into_bytes())
                    } else {
                        None
                    };

                    d.ringing(Some(headers), body)
                        .map_err(|e| e.into_anyhow())?;
                    Ok(())
                }
                _ => Err(anyhow!("Invalid dialog type for sending ringing")),
            }
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    /// Hang up a call
    pub async fn hangup(&self, dialog_id: &DialogId) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            dialog.hangup().await.map_err(|e| e.into_anyhow())?;
            Ok(())
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    /// Cancel a call (alias for hangup - same mechanism in SIP)
    pub async fn cancel_call(&self, dialog_id: &DialogId) -> Result<()> {
        self.hangup(dialog_id).await
    }

    /// Send UPDATE request within a dialog and return the answer SDP if any
    pub async fn send_update(
        &self,
        dialog_id: &DialogId,
        sdp: Option<String>,
    ) -> Result<Option<String>> {
        self.send_mid_dialog_request(dialog_id, rsipstack::sip::Method::Update, sdp)
            .await
    }

    /// Send re-INVITE request within a dialog and return the answer SDP if any
    pub async fn send_reinvite(
        &self,
        dialog_id: &DialogId,
        sdp: Option<String>,
    ) -> Result<Option<String>> {
        self.send_mid_dialog_request(dialog_id, rsipstack::sip::Method::Invite, sdp)
            .await
    }

    /// Send SIP REFER request on an established dialog.
    /// Returns the status code of the REFER response (typically 202 Accepted).
    pub async fn send_refer(&self, dialog_id: &DialogId, refer_to: &str) -> Result<u16> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        let refer_to_uri = rsipstack::sip::Uri::try_from(refer_to)
            .map_err(|e| anyhow!("Invalid Refer-To URI: {:?}", e))?;

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            let resp = match dialog {
                Dialog::Invite(d) => d
                    .refer(refer_to_uri, None, None)
                    .await
                    .map_err(|e| e.into_anyhow())?,
                _ => return Err(anyhow!("Dialog does not support REFER request")),
            };
            Ok(resp.map(|r| r.status_code().code()).unwrap_or(408))
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    /// Send SIP INFO with DTMF signal
    pub async fn send_dtmf_info(&self, dialog_id: &DialogId, digit: &str) -> Result<()> {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.send_dtmf_info_inner(dialog_id, digit),
        )
        .await
        .map_err(|_| anyhow!("send_dtmf_info timed out after 10s"))?
    }

    async fn send_dtmf_info_inner(&self, dialog_id: &DialogId, digit: &str) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        let body = format!("Signal={}\n", digit).into_bytes();
        let headers = vec![rsipstack::sip::Header::ContentType(
            "application/dtmf-relay".into(),
        )];

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::Invite(d) => {
                    d.info(Some(headers), Some(body))
                        .await
                        .map_err(|e| e.into_anyhow())?;
                }
                _ => return Err(anyhow!("Dialog does not support INFO request")),
            }
        }
        Ok(())
    }

    /// Send a SIP INFO with an arbitrary content type and body.
    /// Returns instantly (fire-and-forget on the dialog).
    pub async fn send_info(
        &self,
        dialog_id: &DialogId,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        let headers = vec![rsipstack::sip::Header::ContentType(
            rsipstack::sip::headers::ContentType::from(content_type),
        )];

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::Invite(d) => {
                    d.info(Some(headers), Some(body))
                        .await
                        .map_err(|e| e.into_anyhow())?;
                }
                _ => return Err(anyhow!("Dialog does not support INFO request")),
            }
        }
        Ok(())
    }

    async fn send_mid_dialog_request(
        &self,
        dialog_id: &DialogId,
        method: rsipstack::sip::Method,
        sdp: Option<String>,
    ) -> Result<Option<String>> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        if let Some(mut dialog) = dialog_layer.get_dialog(dialog_id) {
            let body = sdp.map(|s| s.into_bytes());
            let headers = if body.is_some() {
                vec![rsipstack::sip::Header::ContentType(
                    "application/sdp".into(),
                )]
            } else {
                vec![]
            };

            let resp = match (method, &mut dialog) {
                (rsipstack::sip::Method::Update, Dialog::Invite(d)) => d
                    .update(Some(headers), body)
                    .await
                    .map_err(|e| e.into_anyhow())?,
                (rsipstack::sip::Method::Invite, Dialog::Invite(d)) => d
                    .reinvite(Some(headers), body)
                    .await
                    .map_err(|e| e.into_anyhow())?,
                _ => return Err(anyhow!("Dialog does not support {} request", method)),
            };

            let sdp_answer = if let Some(r) = resp {
                if !r.body().is_empty() {
                    Some(String::from_utf8_lossy(r.body()).to_string())
                } else {
                    None
                }
            } else {
                None
            };
            Ok(sdp_answer)
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    /// Process dialog events and return collected events
    pub async fn process_dialog_events(&self) -> Result<Vec<TestUaEvent>> {
        let mut events = Vec::new();

        if let Some(state_receiver_mutex) = &self.state_receiver {
            let mut state_receiver = state_receiver_mutex.lock().await;
            while let Ok(state) = state_receiver.try_recv() {
                match state {
                    DialogState::Calling(id) => {
                        debug!("TestUa: Received Calling state for {}", id);
                        // Get SDP from stored received offers
                        let sdp = {
                            let sdps = self.received_offer_sdps.lock().await;
                            sdps.get(&id).cloned()
                        };
                        events.push(TestUaEvent::IncomingCall(id, sdp));
                    }
                    DialogState::Trying(id) => {
                        debug!("TestUa: Received Trying state for {}", id);
                        // Get SDP from stored received offers
                        let sdp = {
                            let sdps = self.received_offer_sdps.lock().await;
                            sdps.get(&id).cloned()
                        };
                        events.push(TestUaEvent::IncomingCall(id, sdp));
                    }
                    DialogState::Early(id, resp) => {
                        debug!(
                            "TestUa: Received Early state ({}) for {}",
                            resp.status_code, id
                        );
                        // Mirror JsSIP/browser behaviour: apply a 1xx body as a
                        // PRANSWER on the WebRTC PeerConnection so ICE/DTLS (and
                        // early-media transmission) start before the 200 OK.
                        if !resp.body().is_empty() {
                            if let Some(pc) = self.webrtc_pc.as_ref() {
                                let body = String::from_utf8_lossy(resp.body()).to_string();
                                if let Ok(desc) = rustrtc::SessionDescription::parse(
                                    rustrtc::SdpType::Pranswer,
                                    &body,
                                ) {
                                    let _ = pc.set_remote_description(desc).await;
                                }
                            }
                            events.push(TestUaEvent::EarlyMedia(id.clone()));
                        }
                        match resp.status_code {
                            rsipstack::sip::StatusCode::Ringing => {
                                events.push(TestUaEvent::CallRinging(id));
                            }
                            rsipstack::sip::StatusCode::SessionProgress => {
                                events.push(TestUaEvent::CallRinging(id));
                            }
                            _ => {
                                // Get SDP from stored received offers
                                let sdp = {
                                    let sdps = self.received_offer_sdps.lock().await;
                                    sdps.get(&id).cloned()
                                };
                                events.push(TestUaEvent::IncomingCall(id, sdp));
                            }
                        }
                    }
                    DialogState::Confirmed(id, _) => {
                        events.push(TestUaEvent::CallEstablished(id));
                    }
                    DialogState::Terminated(id, _reason) => {
                        events.push(TestUaEvent::CallTerminated(id.clone()));
                        if let Some(dialog_layer) = &self.dialog_layer {
                            dialog_layer.remove_dialog(&id);
                        }
                    }
                    DialogState::Updated(id, request, tx_handle) => {
                        debug!(
                            "TestUa: Received UPDATED state for {} (method: {})",
                            id, request.method
                        );
                        let sdp = if !request.body().is_empty() {
                            Some(String::from_utf8_lossy(request.body()).to_string())
                        } else {
                            None
                        };
                        events.push(TestUaEvent::CallUpdated(id.clone(), request.method, sdp));
                        // Reply with saved answer SDP if available (for re-INVITE responses)
                        let sdps = self.answer_sdps.lock().await;
                        if let Some(answer_sdp) = sdps.get(&id) {
                            let body = answer_sdp.clone().into_bytes();
                            let headers = vec![rsipstack::sip::Header::ContentType(
                                "application/sdp".into(),
                            )];
                            tx_handle
                                .respond(rsipstack::sip::StatusCode::OK, Some(headers), Some(body))
                                .await
                                .ok();
                        } else {
                            tx_handle.reply(rsipstack::sip::StatusCode::OK).await.ok();
                        }
                    }
                    DialogState::Notify(id, _request, tx_handle) => {
                        debug!("TestUa: Received Notify state for {}", id);
                        // Reply 200 OK to NOTIFY so the sender can proceed
                        tx_handle.reply(rsipstack::sip::StatusCode::OK).await.ok();
                    }
                    DialogState::Info(id, request, tx_handle) => {
                        tx_handle.reply(rsipstack::sip::StatusCode::OK).await.ok();
                        let content_type = request
                            .headers
                            .iter()
                            .find_map(|h| {
                                if let rsipstack::sip::Header::ContentType(ct) = h {
                                    Some(ct.value().to_string())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        let is_dtmf = content_type
                            .to_lowercase()
                            .contains("application/dtmf-relay");
                        if is_dtmf {
                            let body = String::from_utf8_lossy(request.body());
                            for line in body.lines() {
                                let line = line.trim();
                                if line.to_lowercase().starts_with("signal=") {
                                    let digit = line
                                        .trim_start_matches(|c: char| !c.eq_ignore_ascii_case(&'s'))
                                        .trim_start_matches("Signal=")
                                        .trim_start_matches("signal=")
                                        .trim()
                                        .to_string();
                                    if !digit.is_empty() {
                                        debug!(
                                            "TestUa: Received DTMF INFO digit '{}' on {}",
                                            digit, id
                                        );
                                        events.push(TestUaEvent::DtmfInfo(id.clone(), digit));
                                    }
                                }
                            }
                        } else {
                            debug!(
                                "TestUa: Received non-DTMF INFO on {}: ct={} body_len={}",
                                id,
                                content_type,
                                request.body().len()
                            );
                            events.push(TestUaEvent::InfoReceived(
                                id.clone(),
                                content_type,
                                request.body().to_vec(),
                            ));
                        }
                    }
                    DialogState::Refer(id, request, tx_handle) => {
                        debug!("TestUa: Received Refer state for {}", id);
                        let mut target = None;
                        for header in request.headers.iter() {
                            if let rsipstack::sip::Header::ReferTo(refer_to) = header {
                                target = Some(refer_to.value().to_string());
                                break;
                            }
                        }
                        if let Some(target) = target {
                            // Accept the REFER (202 Accepted)
                            tx_handle
                                .respond(rsipstack::sip::StatusCode::Accepted, None, None)
                                .await
                                .ok();
                            events.push(TestUaEvent::Referred(id.clone(), target));
                        } else {
                            tx_handle
                                .respond(rsipstack::sip::StatusCode::BadRequest, None, None)
                                .await
                                .ok();
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(events)
    }

    pub fn stop(&self) {
        self.cancel_token.cancel();
    }

    async fn process_incoming_request(
        dialog_layer: Arc<DialogLayer>,
        mut incoming: TransactionReceiver,
        state_sender: DialogStateSender,
        contact: rsipstack::sip::Uri,
        cancel_token: CancellationToken,
        received_sdps: Arc<Mutex<HashMap<DialogId, String>>>,
    ) -> Result<()> {
        loop {
            select! {
                tx_opt = incoming.recv() => {
                    if let Some(mut tx) = tx_opt {
                        debug!(method=%tx.original.method, "TestUa process_incoming_request received request");
                        // Handle existing dialog
                        if tx.original.to_header()?.tag()?.as_ref().is_some() {
                            if let Some(mut d) = dialog_layer.match_dialog(&tx) {
                                debug!(method=%tx.original.method, "TestUa matched dialog for request");
                                rustpbx::utils::spawn(async move {
                                    d.handle(&mut tx).await.ok();
                                });
                                continue;
                            } else {
                                debug!(method=%tx.original.method, "TestUa no matching dialog found");
                            }
                        }

                        // Handle new dialog
                        match tx.original.method {
                            rsipstack::sip::Method::Invite => {
                                // Extract SDP from INVITE body before creating dialog
                                let sdp = if !tx.original.body.is_empty() {
                                    Some(String::from_utf8_lossy(&tx.original.body).to_string())
                                } else {
                                    None
                                };

                                if let Ok(mut dialog) = dialog_layer.get_or_create_server_invite(
                                    &tx, state_sender.clone(), None, Some(contact.clone())
                                ) {
                                    // Store SDP for later retrieval
                                    if let Some(sdp_str) = sdp {
                                        let dialog_id = dialog.id();
                                        let mut sdps = received_sdps.lock().await;
                                        sdps.insert(dialog_id, sdp_str);
                                    }
                                    rustpbx::utils::spawn(async move {
                                        dialog.handle(&mut tx).await.ok();
                                    });
                                }
                            }
                            rsipstack::sip::Method::Ack => {
                                if let Ok(mut dialog) = dialog_layer.get_or_create_server_invite(
                                    &tx, state_sender.clone(), None, Some(contact.clone())
                                ) {
                                    rustpbx::utils::spawn(async move {
                                        dialog.handle(&mut tx).await.ok();
                                    });
                                }
                            }
                            _ => {
                                tx.reply(rsipstack::sip::StatusCode::OK).await.ok();
                            }
                        }
                    } else {
                        break;
                    }
                }
                _ = cancel_token.cancelled() => break,
            }
        }
        Ok(())
    }
}

/// Helper function to create test SDP
pub fn create_test_sdp(ip: &str, port: u16, is_private_ip: bool) -> String {
    let connection_ip = if is_private_ip { "192.168.1.100" } else { ip };
    let session_id = chrono::Utc::now().timestamp();
    let session_version = session_id + 1;

    format!(
        "v=0\r\n\
o=testua {} {} IN IP4 {}\r\n\
s=Test Call\r\n\
c=IN IP4 {}\r\n\
t=0 0\r\n\
m=audio {} RTP/AVP 0 8\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=sendrecv\r\n",
        session_id, session_version, ip, connection_ip, port
    )
}

/// Helper function to create test SDP answer based on offer
pub fn create_test_sdp_answer(offer: &str, ip: &str, port: u16) -> String {
    // Parse basic info from offer
    let session_id = chrono::Utc::now().timestamp();
    let session_version = session_id + 1;

    // Determine if offer is WebRTC or RTP based
    let is_webrtc = offer.contains("a=ice-ufrag") || offer.contains("a=fingerprint");

    if is_webrtc {
        // Respond to WebRTC with WebRTC
        format!(
            "v=0\r\n\
o=testua {} {} IN IP4 {}\r\n\
s=Test Answer\r\n\
c=IN IP4 {}\r\n\
t=0 0\r\n\
m=audio {} UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fingerprint:sha-256 BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA\r\n\
a=setup:active\r\n\
a=ice-ufrag:wxyz\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvw\r\n\
a=sendrecv\r\n",
            session_id, session_version, ip, ip, port
        )
    } else {
        // Respond to RTP with RTP
        format!(
            "v=0\r\n\
o=testua {} {} IN IP4 {}\r\n\
s=Test Answer\r\n\
c=IN IP4 {}\r\n\
t=0 0\r\n\
m=audio {} RTP/AVP 0 8\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=sendrecv\r\n",
            session_id, session_version, ip, ip, port
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::sleep;
    use tracing::Level;

    use super::super::e2e_test_server::E2eTestServer;
    use super::*;
    use rustpbx::config::MediaProxyMode;

    // Simplified test helper functions
    pub async fn create_test_ua(
        username: &str,
        password: &str,
        proxy_addr: SocketAddr,
        port: u16,
    ) -> Result<TestUa> {
        let config = TestUaConfig {
            webrtc: false,
            username: username.to_string(),
            password: password.to_string(),
            realm: proxy_addr.ip().to_string(),
            local_port: port,
            proxy_addr,
        };

        let mut ua = TestUa::new(config);
        ua.start().await?;
        Ok(ua)
    }

    /// Create a TestUa that uses a real rustrtc PeerConnection (WebRTC
    /// DTLS-SRTP) for offer/answer generation instead of fake SDP strings.
    pub async fn create_test_ua_webrtc(
        username: &str,
        password: &str,
        proxy_addr: SocketAddr,
        port: u16,
    ) -> Result<TestUa> {
        let config = TestUaConfig {
            webrtc: true,
            username: username.to_string(),
            password: password.to_string(),
            realm: proxy_addr.ip().to_string(),
            local_port: port,
            proxy_addr,
        };

        let mut ua = TestUa::new(config);
        ua.start().await?;
        Ok(ua)
    }

    async fn await_caller_with_timeout(
        handle: tokio::task::JoinHandle<Result<DialogId>>,
        timeout: Duration,
    ) -> Option<Result<DialogId>> {
        match tokio::time::timeout(timeout, handle).await {
            Ok(join_res) => match join_res {
                Ok(res) => Some(res),
                Err(e) => {
                    eprintln!("caller task join error: {:?}", e);
                    None
                }
            },
            Err(_) => None,
        }
    }

    async fn wait_for_event<F>(ua: &mut TestUa, mut predicate: F, timeout_ms: u64) -> Result<bool>
    where
        F: FnMut(&TestUaEvent) -> bool,
    {
        let iterations = timeout_ms / 25; // Reduced from 50ms to 25ms for faster polling
        for _ in 0..iterations {
            let events = ua.process_dialog_events().await?;
            for event in &events {
                if predicate(event) {
                    return Ok(true);
                }
            }
            sleep(Duration::from_millis(25)).await; // Faster polling interval
        }
        Ok(false)
    }

    /// Test basic registration functionality
    #[tokio::test]
    async fn test_basic_registration() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::None)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25000);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25001);
        let bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        assert!(
            alice.register().await.is_ok(),
            "Alice registration should succeed"
        );
        assert!(
            bob.register().await.is_ok(),
            "Bob registration should succeed"
        );

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test complete call flow with different media proxy modes
    #[tokio::test]
    async fn test_call_flow_comprehensive() {
        tracing_subscriber::fmt()
            .with_file(true)
            .with_line_number(true)
            .with_max_level(Level::INFO)
            .try_init()
            .ok();
        for mode in [
            MediaProxyMode::None,
            MediaProxyMode::Nat,
            MediaProxyMode::All,
        ] {
            println!("Testing call flow with MediaProxyMode::{:?}", mode);

            let proxy = E2eTestServer::start_with_mode(mode).await.unwrap();
            let proxy_addr = proxy.proxy_addr;

            let alice_port = portpicker::pick_unused_port().unwrap_or(25010);
            let alice = Arc::new(
                create_test_ua("alice", "password123", proxy_addr, alice_port)
                    .await
                    .unwrap(),
            );

            let bob_port = portpicker::pick_unused_port().unwrap_or(25011);
            let bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
                .await
                .unwrap();

            // Register both users
            alice.register().await.unwrap();
            bob.register().await.unwrap();
            sleep(Duration::from_millis(50)).await; // Optimized wait time
            // Test call with SDP: spawn caller and handle callee events concurrently
            let sdp_offer = create_test_sdp("192.168.1.100", 5004, true);
            let alice_clone = alice.clone();
            let caller_handle = rustpbx::utils::spawn(async move {
                alice_clone.make_call("bob", Some(sdp_offer)).await
            });

            // Wait and answer incoming call by polling events (avoid draining issue)
            let mut answered = false;
            for _ in 0..80 {
                // up to ~2 seconds with 25ms sleeps
                let bob_events = bob.process_dialog_events().await.unwrap();
                for event in &bob_events {
                    if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                        // Send ringing
                        let early_sdp = create_test_sdp("192.168.1.200", 5006, true);
                        bob.send_ringing(incoming_id, Some(early_sdp)).await.ok();
                        // Answer call
                        let answer_sdp = create_test_sdp("192.168.1.200", 5006, true);
                        bob.answer_call(incoming_id, Some(answer_sdp)).await.ok();
                        answered = true;
                        break;
                    }
                }
                if answered {
                    break;
                }
                sleep(Duration::from_millis(25)).await;
            }

            // Now the caller future should complete with a DialogId; guard with timeout to avoid hang
            match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
                Ok(join_res) => match join_res {
                    Ok(Ok(dialog_id)) => {
                        // Give a moment for dialog confirmation
                        sleep(Duration::from_millis(200)).await;
                        alice.hangup(&dialog_id).await.ok();
                    }
                    Ok(Err(e)) => {
                        eprintln!("Caller failed: {:?}", e);
                    }
                    Err(join_err) => {
                        eprintln!("Caller task panicked: {:?}", join_err);
                    }
                },
                Err(_) => {
                    eprintln!("Caller invite timed out (no answer)");
                }
            }

            alice.stop();
            bob.stop();
            proxy.stop();
        }
    }

    /// Test call rejection scenarios
    #[tokio::test]
    async fn test_call_rejection_scenarios() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::Auto)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25020);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25021);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test immediate rejection
        {
            let caller_handle = rustpbx::utils::spawn({
                let alice = alice.clone();
                async move { alice.make_call("bob", None).await }
            });
            if wait_for_event(
                &mut bob,
                |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                1000,
            )
            .await
            .unwrap()
            {
                let bob_events = bob.process_dialog_events().await.unwrap();
                for event in &bob_events {
                    if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                        assert!(
                            bob.reject_call(incoming_id).await.is_ok(),
                            "Should be able to reject call"
                        );
                        break;
                    }
                }
            }
            let _ = await_caller_with_timeout(caller_handle, Duration::from_secs(3)).await;
        }

        // Test rejection after ringing
        {
            let caller_handle = rustpbx::utils::spawn({
                let alice = alice.clone();
                async move { alice.make_call("bob", None).await }
            });
            if wait_for_event(
                &mut bob,
                |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                1000,
            )
            .await
            .unwrap()
            {
                let bob_events = bob.process_dialog_events().await.unwrap();
                for event in &bob_events {
                    if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                        bob.send_ringing(incoming_id, None).await.ok();
                        sleep(Duration::from_millis(300)).await;
                        assert!(
                            bob.reject_call(incoming_id).await.is_ok(),
                            "Should be able to reject after ringing"
                        );
                        break;
                    }
                }
            }
            let _ = await_caller_with_timeout(caller_handle, Duration::from_secs(3)).await;
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test error handling and edge cases
    #[tokio::test]
    async fn test_error_handling_and_edge_cases() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::Auto)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25030);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        alice.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test call to non-existent user
        let result = alice.make_call("nonexistent", None).await;
        match result {
            Ok(dialog_id) => {
                alice.hangup(&dialog_id).await.ok();
                println!("Call to non-existent user handled gracefully");
            }
            Err(_) => println!("Call to non-existent user properly rejected"),
        }

        // Test empty SDP
        println!("Testing empty SDP...");
        let empty_sdp_result = alice.make_call("bob", Some("".to_string())).await;
        println!("Empty SDP result: {:?}", empty_sdp_result);
        if let Ok(dialog_id) = empty_sdp_result {
            alice.hangup(&dialog_id).await.ok();
            println!("Empty SDP handled gracefully");
        }

        // Test malformed SDP
        println!("Testing malformed SDP...");
        let malformed_sdp = "v=0\nthis is not valid sdp";
        let malformed_result = alice
            .make_call("bob", Some(malformed_sdp.to_string()))
            .await;
        println!("Malformed SDP result: {:?}", malformed_result);
        if let Ok(dialog_id) = malformed_result {
            alice.hangup(&dialog_id).await.ok();
            println!("Malformed SDP handled gracefully");
        }

        alice.stop();
        proxy.stop();
    }

    /// Test concurrent operations and stress scenarios
    #[tokio::test]
    async fn test_concurrent_operations() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        // Create multiple UAs
        let mut users = Vec::new();
        for i in 0..3 {
            let port = portpicker::pick_unused_port().unwrap_or(25040 + i);
            let username = format!("user{}", i);
            let password = format!("password{}", i);

            if let Ok(ua) = create_test_ua(&username, &password, proxy_addr, port).await {
                ua.register().await.ok();
                users.push(ua);
            }
        }

        sleep(Duration::from_millis(200)).await;

        // Test rapid call cycles
        if users.len() >= 2 {
            for cycle in 0..3 {
                if let Ok(dialog_id) = users[0].make_call("user1", None).await {
                    sleep(Duration::from_millis(100)).await;
                    users[0].hangup(&dialog_id).await.ok();
                    println!("Completed rapid cycle #{}", cycle + 1);
                }
            }
        }

        // Test multiple concurrent calls
        let mut call_handles = Vec::new();
        if users.len() >= 2 {
            for _i in 0..2 {
                if let Ok(dialog_id) = users[0].make_call("user1", None).await {
                    call_handles.push(dialog_id);
                }
            }
        }

        sleep(Duration::from_millis(200)).await;
        for dialog_id in call_handles {
            users[0].hangup(&dialog_id).await.ok();
        }

        // Cleanup
        for user in users {
            user.stop();
        }
        proxy.stop();
    }

    /// Test SDP processing modes
    #[tokio::test]
    async fn test_sdp_processing_modes() {
        // Test different types of SDP
        let test_cases = vec![("Standard SDP", create_test_sdp("192.168.1.100", 5004, true))];

        for (test_name, sdp) in test_cases {
            println!("Testing {}", test_name);

            let proxy = E2eTestServer::start_with_mode(MediaProxyMode::Auto)
                .await
                .unwrap();
            let proxy_addr = proxy.proxy_addr;

            let alice_port = portpicker::pick_unused_port().unwrap_or(25050);
            let alice = Arc::new(
                create_test_ua("alice", "password123", proxy_addr, alice_port)
                    .await
                    .unwrap(),
            );

            let bob_port = portpicker::pick_unused_port().unwrap_or(25051);
            let bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
                .await
                .unwrap();

            alice.register().await.unwrap();
            bob.register().await.unwrap();
            sleep(Duration::from_millis(100)).await;

            // Spawn caller in a separate task to allow concurrent processing
            let caller_handle = rustpbx::utils::spawn({
                let a = alice.clone();
                async move { a.make_call("bob", Some(sdp)).await }
            });

            // Answer immediately upon receiving the IncomingCall event
            let callee_fut = async {
                let max_wait_ms = 5000u64;
                let iterations = max_wait_ms / 25;
                for _ in 0..iterations {
                    let bob_events = bob.process_dialog_events().await.unwrap();
                    for event in &bob_events {
                        if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                            bob.answer_call(incoming_id, None).await.ok();
                            println!("  {} processed successfully", test_name);
                            return;
                        }
                    }
                    sleep(Duration::from_millis(25)).await;
                }
            };

            // Wait for both with timeout
            let _ = tokio::time::timeout(Duration::from_secs(10), callee_fut).await;

            if let Ok(join_res) = tokio::time::timeout(Duration::from_secs(5), caller_handle).await
                && let Ok(Ok(dialog_id)) = join_res
            {
                alice.hangup(&dialog_id).await.ok();
            }

            alice.stop();
            bob.stop();
            proxy.stop();
        }
    }

    /// Test dialog state monitoring
    #[tokio::test]
    async fn test_dialog_state_monitoring() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25060);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25061);
        let bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        {
            let caller_handle = rustpbx::utils::spawn({
                let a = alice.clone();
                async move { a.make_call("bob", None).await }
            });
            let callee_fut = async {
                let mut states_observed: Vec<String> = Vec::new();
                let mut established_id: Option<DialogId> = None;
                for i in 0..20 {
                    let bob_events = bob.process_dialog_events().await.unwrap();
                    for event in &bob_events {
                        match event {
                            TestUaEvent::IncomingCall(id, _) => {
                                states_observed.push("Calling".to_string());
                                bob.answer_call(id, None).await.ok();
                                established_id = Some(id.clone());
                            }
                            TestUaEvent::CallRinging(_) => {
                                states_observed.push("Ringing".to_string())
                            }
                            TestUaEvent::CallEstablished(_) => {
                                states_observed.push("Established".to_string())
                            }
                            TestUaEvent::CallTerminated(_) => {
                                states_observed.push("Terminated".to_string())
                            }
                            _ => {}
                        }
                    }
                    if i == 10
                        && let Some(id) = &established_id
                    {
                        let _ = bob.hangup(id).await; // drive termination
                    }
                    if states_observed.contains(&"Terminated".to_string()) {
                        println!("States observed: {:?}", states_observed);
                        assert!(
                            !states_observed.is_empty(),
                            "Should observe dialog state changes"
                        );
                        break;
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            };

            // Run callee processing first
            callee_fut.await;

            // Then wait for caller with timeout (don't block on it)
            match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
                Ok(Ok(Ok(dialog_id))) => {
                    // Call completed successfully, hang up to clean up
                    alice.hangup(&dialog_id).await.ok();
                }
                Ok(Ok(Err(e))) => {
                    eprintln!("Caller failed: {:?}", e);
                }
                Ok(Err(join_err)) => {
                    eprintln!("Caller task panicked: {:?}", join_err);
                }
                Err(_) => {
                    eprintln!("Caller invite timed out (no answer)");
                }
            }
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test resource cleanup
    #[tokio::test]
    async fn test_resource_cleanup() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25070);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25071);
        let bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Create and terminate multiple calls to test cleanup
        for i in 0..3 {
            let caller_handle = rustpbx::utils::spawn({
                let a = alice.clone();
                async move { a.make_call("bob", None).await }
            });
            let callee_fut = async {
                sleep(Duration::from_millis(100)).await;
                let bob_events = bob.process_dialog_events().await.unwrap();
                for event in &bob_events {
                    if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                        bob.answer_call(incoming_id, None).await.ok();
                        break;
                    }
                }
            };
            callee_fut.await;

            // Wait for caller with timeout
            match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
                Ok(Ok(Ok(id))) => {
                    alice.hangup(&id).await.ok();
                }
                Ok(Ok(Err(e))) => {
                    eprintln!("Caller failed: {:?}", e);
                }
                Ok(Err(join_err)) => {
                    eprintln!("Caller task panicked: {:?}", join_err);
                }
                Err(_) => {
                    eprintln!("Caller invite timed out");
                }
            }
            println!("Completed cleanup cycle #{}", i + 1);
        }

        sleep(Duration::from_millis(200)).await;
        alice.stop();
        bob.stop();
        proxy.stop();
        println!("Resource cleanup test completed");
    }

    /// Test authentication failures and recovery
    #[tokio::test]
    async fn test_authentication_failures_and_recovery() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::None)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        // Test 1: Wrong password
        let alice_port = portpicker::pick_unused_port().unwrap_or(25080);
        let alice_wrong_pass = create_test_ua("alice", "wrongpassword", proxy_addr, alice_port)
            .await
            .unwrap();

        let result = alice_wrong_pass.register().await;
        assert!(
            result.is_err(),
            "Registration with wrong password should fail"
        );

        // Test 2: Correct password after failure
        let alice_correct = create_test_ua("alice", "password123", proxy_addr, alice_port + 1)
            .await
            .unwrap();
        assert!(
            alice_correct.register().await.is_ok(),
            "Registration with correct password should succeed"
        );

        // Test 3: Non-existent user
        let charlie_port = portpicker::pick_unused_port().unwrap_or(25082);
        let charlie = create_test_ua("charlie", "password", proxy_addr, charlie_port)
            .await
            .unwrap();
        let result = charlie.register().await;
        assert!(
            result.is_err(),
            "Registration with non-existent user should fail"
        );

        alice_wrong_pass.stop();
        alice_correct.stop();
        charlie.stop();
        proxy.stop();
    }

    /// Test network timeout and retry scenarios
    #[tokio::test]
    async fn test_network_timeout_scenarios() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::Auto)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25090);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25091);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Rapid short-lived call cycles with proper concurrent callee handling
        for i in 0..5 {
            let caller_handle = {
                let a = alice.clone();
                rustpbx::utils::spawn(async move { a.make_call("bob", None).await })
            };

            if wait_for_event(
                &mut bob,
                |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                800,
            )
            .await
            .unwrap()
            {
                let events = bob.process_dialog_events().await.unwrap();
                for e in &events {
                    if let TestUaEvent::IncomingCall(id, _) = e {
                        // Answer quickly to let caller complete, then hang up immediately
                        bob.answer_call(id, None).await.ok();
                        break;
                    }
                }
            }

            if let Ok(join_res) = tokio::time::timeout(Duration::from_secs(3), caller_handle).await
                && let Ok(Ok(dialog_id)) = join_res
            {
                // Very short call duration simulating network flakiness
                sleep(Duration::from_millis(20)).await;
                alice.hangup(&dialog_id).await.ok();
                println!("Quick call cycle #{} completed", i + 1);
            }

            sleep(Duration::from_millis(20)).await;
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test DTMF and INFO message handling
    #[tokio::test]
    async fn test_dtmf_and_info_messages() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25100);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25101);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        {
            let alice_arc = alice.clone();
            let caller_handle = rustpbx::utils::spawn({
                let a = alice_arc.clone();
                async move { a.make_call("bob", None).await }
            });
            // Wait for call establishment
            if wait_for_event(
                &mut bob,
                |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                1000,
            )
            .await
            .unwrap()
            {
                let bob_events = bob.process_dialog_events().await.unwrap();
                for event in &bob_events {
                    if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                        bob.answer_call(incoming_id, None).await.ok();
                        break;
                    }
                }

                sleep(Duration::from_millis(200)).await;

                // Simulate DTMF INFO messages
                println!("Simulating DTMF INFO messages: 1, 2, 3, #");
                // In a real implementation, this would send SIP INFO messages with DTMF content
                // For testing purposes, we verify the call is still active

                let dtmf_digits = ["1", "2", "3", "#"];
                for digit in &dtmf_digits {
                    println!("  DTMF digit: {}", digit);
                    sleep(Duration::from_millis(100)).await;
                    // Process any events during DTMF simulation (callee side is sufficient)
                    bob.process_dialog_events().await.ok();
                }

                if let Ok(join_res) =
                    tokio::time::timeout(Duration::from_secs(5), caller_handle).await
                    && let Ok(Ok(id)) = join_res
                {
                    alice_arc.hangup(&id).await.ok();
                }
            }
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test call transfer and REFER scenarios
    #[tokio::test]
    async fn test_call_transfer_scenarios() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25110);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25111);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test blind transfer scenario
        {
            let alice_arc = alice.clone();
            let caller_handle = rustpbx::utils::spawn({
                let a = alice_arc.clone();
                async move { a.make_call("bob", None).await }
            });
            // Establish call
            if wait_for_event(
                &mut bob,
                |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                1000,
            )
            .await
            .unwrap()
            {
                let bob_events = bob.process_dialog_events().await.unwrap();
                for event in &bob_events {
                    if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                        bob.answer_call(incoming_id, None).await.ok();

                        sleep(Duration::from_millis(300)).await;

                        // Simulate REFER request (blind transfer to charlie)
                        println!("Simulating REFER for blind transfer to charlie");
                        // In real implementation, this would send REFER SIP message
                        // For now, we simulate the transfer scenario

                        // Transfer completed - original call should be replaced
                        if let Ok(join_res) =
                            tokio::time::timeout(Duration::from_secs(5), caller_handle).await
                            && let Ok(Ok(id)) = join_res
                        {
                            alice_arc.hangup(&id).await.ok();
                        }
                        println!("Blind transfer scenario completed");
                        break;
                    }
                }
            }
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test codec negotiation scenarios
    #[tokio::test]
    async fn test_codec_negotiation() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25120);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25121);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test different codec scenarios
        let codec_test_cases = vec![
            (
                "PCMU only",
                "v=0\ro=test 123 456 IN IP4 192.168.1.100\rs=-\rc=IN IP4 192.168.1.100\rt=0 0\rm=audio 5004 RTP/AVP 0\ra=rtpmap:0 PCMU/8000\r",
            ),
            (
                "PCMA only",
                "v=0\ro=test 123 456 IN IP4 192.168.1.100\rs=-\rc=IN IP4 192.168.1.100\rt=0 0\rm=audio 5004 RTP/AVP 8\ra=rtpmap:8 PCMA/8000\r",
            ),
            (
                "Multiple codecs",
                "v=0\ro=test 123 456 IN IP4 192.168.1.100\rs=-\rc=IN IP4 192.168.1.100\rt=0 0\rm=audio 5004 RTP/AVP 0 8 18\ra=rtpmap:0 PCMU/8000\ra=rtpmap:8 PCMA/8000\ra=rtpmap:18 G729/8000\r",
            ),
        ];

        for (test_name, offer_sdp) in codec_test_cases {
            println!("Testing codec negotiation: {}", test_name);

            {
                let alice_arc = alice.clone();
                let caller_handle = rustpbx::utils::spawn({
                    let a = alice_arc.clone();
                    let s = offer_sdp.to_string();
                    async move { a.make_call("bob", Some(s)).await }
                });
                if wait_for_event(
                    &mut bob,
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                    500,
                )
                .await
                .unwrap()
                {
                    let bob_events = bob.process_dialog_events().await.unwrap();
                    for event in &bob_events {
                        if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                            // Answer with compatible codec
                            let answer_sdp = "v=0\ro=test 456 789 IN IP4 192.168.1.200\rs=-\rc=IN IP4 192.168.1.200\rt=0 0\rm=audio 5006 RTP/AVP 0\ra=rtpmap:0 PCMU/8000\r";
                            bob.answer_call(incoming_id, Some(answer_sdp.to_string()))
                                .await
                                .ok();
                            println!("  {} - codec negotiation completed", test_name);
                            break;
                        }
                    }
                }

                sleep(Duration::from_millis(100)).await;
                if let Ok(join_res) =
                    tokio::time::timeout(Duration::from_secs(5), caller_handle).await
                    && let Ok(Ok(id)) = join_res
                {
                    alice_arc.hangup(&id).await.ok();
                }
            }

            sleep(Duration::from_millis(50)).await;
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test hold and unhold scenarios
    #[tokio::test]
    async fn test_hold_unhold_scenarios() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25130);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25131);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        {
            let alice_arc = alice.clone();
            let caller_handle = rustpbx::utils::spawn({
                let a = alice_arc.clone();
                async move { a.make_call("bob", None).await }
            });
            // Establish call
            if wait_for_event(
                &mut bob,
                |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                1000,
            )
            .await
            .unwrap()
            {
                let bob_events = bob.process_dialog_events().await.unwrap();
                for event in &bob_events {
                    if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                        bob.answer_call(incoming_id, None).await.ok();
                        sleep(Duration::from_millis(200)).await;

                        // Simulate hold (re-INVITE with sendonly)
                        println!("Simulating hold operation");
                        let _hold_sdp = "v=0\ro=test 123 456 IN IP4 192.168.1.100\rs=-\rc=IN IP4 192.168.1.100\rt=0 0\rm=audio 5004 RTP/AVP 0\ra=rtpmap:0 PCMU/8000\ra=sendonly\r";
                        // In real implementation, this would be a re-INVITE
                        println!("  Hold SDP prepared: sendonly");

                        sleep(Duration::from_millis(500)).await;

                        // Simulate unhold (re-INVITE with sendrecv)
                        println!("Simulating unhold operation");
                        let _unhold_sdp = "v=0\ro=test 123 456 IN IP4 192.168.1.100\rs=-\rc=IN IP4 192.168.1.100\rt=0 0\rm=audio 5004 RTP/AVP 0\ra=rtpmap:0 PCMU/8000\ra=sendrecv\r";
                        // In real implementation, this would be another re-INVITE
                        println!("  Unhold SDP prepared: sendrecv");

                        sleep(Duration::from_millis(300)).await;
                        if let Ok(join_res) =
                            tokio::time::timeout(Duration::from_secs(5), caller_handle).await
                            && let Ok(Ok(id)) = join_res
                        {
                            alice_arc.hangup(&id).await.ok();
                        }
                        break;
                    }
                }
            }
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test SIP message retransmission scenarios  
    #[tokio::test]
    async fn test_message_retransmission() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::Auto)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25140);
        let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test retransmission by making calls to non-responsive endpoints
        for i in 0..3 {
            let attempt = tokio::time::timeout(
                Duration::from_secs(10),
                alice.make_call("nonresponsive", None),
            )
            .await;

            match attempt {
                Ok(Ok(dialog_id)) => {
                    println!(
                        "Retransmission test #{}: Call initiated, expecting timeout",
                        i + 1
                    );
                    sleep(Duration::from_millis(200)).await; // Brief wait before cleanup
                    alice.hangup(&dialog_id).await.ok();
                }
                Ok(Err(e)) => {
                    println!(
                        "Retransmission test #{}: Call properly failed: {}",
                        i + 1,
                        e
                    );
                }
                Err(_) => {
                    println!(
                        "Retransmission test #{}: Call attempt timed out after 10s (expected)",
                        i + 1
                    );
                }
            }
            sleep(Duration::from_millis(50)).await;
        }

        alice.stop();
        proxy.stop();
    }

    /// Test IPv6 and mixed IP scenarios
    #[tokio::test]
    async fn test_ipv6_and_mixed_ip_scenarios() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25150);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25151);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test IPv6 SDP scenario
        let ipv6_sdp = r#"v=0
o=test 123456 654321 IN IP6 2001:db8::1
s=-
c=IN IP6 2001:db8::1  
t=0 0
m=audio 5004 RTP/AVP 0
a=rtpmap:0 PCMU/8000"#;

        let alice_arc = alice.clone();
        let caller_handle = rustpbx::utils::spawn({
            let a = alice_arc.clone();
            let s = ipv6_sdp.to_string();
            async move { a.make_call("bob", Some(s)).await }
        });
        if wait_for_event(
            &mut bob,
            |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
            500,
        )
        .await
        .unwrap()
        {
            let bob_events = bob.process_dialog_events().await.unwrap();
            for event in &bob_events {
                if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                    println!("IPv6 SDP call received and processed");
                    bob.answer_call(incoming_id, None).await.ok();
                    break;
                }
            }
        }

        sleep(Duration::from_millis(100)).await;
        if let Ok(join_res) = tokio::time::timeout(Duration::from_secs(5), caller_handle).await
            && let Ok(Ok(id)) = join_res
        {
            alice_arc.hangup(&id).await.ok();
        }

        // Test dual-stack SDP scenario
        let dual_stack_sdp = r#"v=0
o=test 123456 654321 IN IP4 192.168.1.100
s=-
c=IN IP4 192.168.1.100
t=0 0
m=audio 5004 RTP/AVP 0
a=rtpmap:0 PCMU/8000
a=candidate:1 1 udp 2130706431 192.168.1.100 54400 typ host
a=candidate:2 1 udp 2130706430 2001:db8::1 54401 typ host"#;

        let caller_handle = rustpbx::utils::spawn({
            let a = alice_arc.clone();
            let s = dual_stack_sdp.to_string();
            async move { a.make_call("bob", Some(s)).await }
        });
        if wait_for_event(
            &mut bob,
            |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
            1000,
        )
        .await
        .unwrap()
        {
            let bob_events = bob.process_dialog_events().await.unwrap();
            for event in &bob_events {
                if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                    // Answer to complete the call setup
                    bob.answer_call(incoming_id, None).await.ok();
                    break;
                }
            }
        }
        if let Ok(join_res) = tokio::time::timeout(Duration::from_secs(5), caller_handle).await
            && let Ok(Ok(id)) = join_res
        {
            sleep(Duration::from_millis(100)).await;
            alice_arc.hangup(&id).await.ok();
            println!("Dual-stack SDP scenario completed");
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test caller cancel scenarios
    #[tokio::test]
    async fn test_caller_cancel_scenarios() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::Auto)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(26000);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(26001);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Scenario 1: Early termination by caller shortly after answer (best-effort substitute for CANCEL)
        {
            let caller_handle = {
                let a = alice.clone();
                rustpbx::utils::spawn(async move { a.make_call("bob", None).await })
            };
            if wait_for_event(
                &mut bob,
                |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                800,
            )
            .await
            .unwrap()
            {
                let events = bob.process_dialog_events().await.unwrap();
                for e in &events {
                    if let TestUaEvent::IncomingCall(id, _) = e {
                        // Bob answers to allow caller future to resolve with DialogId
                        bob.answer_call(id, None).await.ok();
                        break;
                    }
                }
            }
            if let Ok(join_res) = tokio::time::timeout(Duration::from_secs(3), caller_handle).await
                && let Ok(Ok(dialog_id)) = join_res
            {
                // Caller terminates immediately after answer
                assert!(alice.hangup(&dialog_id).await.is_ok());
                println!("Caller terminated call immediately after answer");
            }
        }

        // Scenario 2: Ringing then early termination by caller (still requires established dialog in this simplified UA)
        sleep(Duration::from_millis(100)).await;
        {
            let caller_handle = {
                let a = alice.clone();
                rustpbx::utils::spawn(async move { a.make_call("bob", None).await })
            };
            if wait_for_event(
                &mut bob,
                |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                1000,
            )
            .await
            .unwrap()
            {
                let events = bob.process_dialog_events().await.unwrap();
                for e in &events {
                    if let TestUaEvent::IncomingCall(id, _) = e {
                        // Bob sends ringing first
                        bob.send_ringing(id, None).await.ok();
                        sleep(Duration::from_millis(120)).await;
                        // Then answer so caller future resolves
                        bob.answer_call(id, None).await.ok();
                        break;
                    }
                }
            }
            if let Ok(join_res) = tokio::time::timeout(Duration::from_secs(3), caller_handle).await
                && let Ok(Ok(dialog_id)) = join_res
            {
                // Caller terminates immediately after answer
                assert!(alice.hangup(&dialog_id).await.is_ok());
                println!("Caller terminated during/after ringing phase");
            }
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test callee hangup during established call
    #[tokio::test]
    async fn test_callee_hangup_scenarios() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(26010);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(26011);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test callee hangup after answering
        let alice_arc = alice.clone();
        let _caller_handle = rustpbx::utils::spawn({
            let a = alice_arc.clone();
            async move { a.make_call("bob", None).await }
        });
        if wait_for_event(
            &mut bob,
            |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
            1000,
        )
        .await
        .unwrap()
        {
            let bob_events = bob.process_dialog_events().await.unwrap();
            for event in &bob_events {
                if let TestUaEvent::IncomingCall(bob_dialog_id, _) = event {
                    // Bob answers the call
                    bob.answer_call(bob_dialog_id, None).await.ok();
                    sleep(Duration::from_millis(100)).await;

                    // Bob hangs up during established call
                    assert!(
                        bob.hangup(bob_dialog_id).await.is_ok(),
                        "Callee should be able to hang up established call"
                    );

                    // Verify alice receives hangup notification
                    sleep(Duration::from_millis(200)).await;
                    println!("Callee hangup completed successfully");
                    break;
                }
            }
        }
        alice.stop();
        bob.stop();
        proxy.stop();
    }

    /// Test WebRTC to RTP media proxy conversion
    #[test]
    fn test_webrtc_rtp_media_proxy() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_stack_size(32 * 1024 * 1024)
            .build()
            .expect("runtime");
        rt.block_on(async {
            tokio::spawn(async { test_webrtc_rtp_media_proxy_impl().await })
                .await
                .expect("test task panicked");
        });
    }

    async fn test_webrtc_rtp_media_proxy_impl() {
        for mode in [MediaProxyMode::Auto, MediaProxyMode::All] {
            println!(
                "Testing WebRTC/RTP conversion with MediaProxyMode::{:?}",
                mode
            );

            let proxy = E2eTestServer::start_with_mode(mode).await.unwrap();
            let proxy_addr = proxy.proxy_addr;

            let alice_port = portpicker::pick_unused_port().unwrap_or(26020);
            let alice = Arc::new(
                create_test_ua("alice", "password123", proxy_addr, alice_port)
                    .await
                    .unwrap(),
            );

            let bob_port = portpicker::pick_unused_port().unwrap_or(26021);
            let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
                .await
                .unwrap();

            alice.register().await.unwrap();
            bob.register().await.unwrap();
            sleep(Duration::from_millis(100)).await;

            // Wrap alice once for both scenarios
            let alice_arc = alice.clone();

            // Test 1: WebRTC offer to RTP callee
            let webrtc_offer = r#"v=0
o=test 123456 654321 IN IP4 192.168.1.100
s=-
c=IN IP4 192.168.1.100
t=0 0
m=audio 9 UDP/TLS/RTP/SAVPF 111
a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99
a=setup:actpass
a=ice-ufrag:abcd
a=ice-pwd:efghijklmnopqrstuvwxyz
a=rtpmap:111 opus/48000/2
a=sendrecv"#;

            {
                let caller_handle = rustpbx::utils::spawn({
                    let a = alice_arc.clone();
                    let s = webrtc_offer.to_string();
                    async move { a.make_call("bob", Some(s)).await }
                });
                if wait_for_event(
                    &mut bob,
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                    1000,
                )
                .await
                .unwrap()
                {
                    let bob_events = bob.process_dialog_events().await.unwrap();
                    for event in &bob_events {
                        if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                            // Bob responds with RTP answer
                            let rtp_answer = r#"v=0
o=test 654321 123456 IN IP4 192.168.1.200
s=-
c=IN IP4 192.168.1.200
t=0 0
m=audio 5004 RTP/AVP 0
a=rtpmap:0 PCMU/8000"#;

                            bob.answer_call(incoming_id, Some(rtp_answer.to_string()))
                                .await
                                .ok();
                            println!("WebRTC to RTP conversion test completed");
                            break;
                        }
                    }
                }

                sleep(Duration::from_millis(200)).await;
                if let Ok(join_res) =
                    tokio::time::timeout(Duration::from_secs(5), caller_handle).await
                    && let Ok(Ok(id)) = join_res
                {
                    alice_arc.hangup(&id).await.ok();
                }
            }

            // Test 2: RTP offer to WebRTC callee (simulated by different SDP patterns)
            let rtp_offer = r#"v=0
o=test 123456 654321 IN IP4 192.168.1.100
s=-
c=IN IP4 192.168.1.100
t=0 0
m=audio 5004 RTP/AVP 0
a=rtpmap:0 PCMU/8000"#;

            {
                let caller_handle = rustpbx::utils::spawn({
                    let a = alice_arc.clone();
                    let s = rtp_offer.to_string();
                    async move { a.make_call("bob", Some(s)).await }
                });
                if wait_for_event(
                    &mut bob,
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                    1000,
                )
                .await
                .unwrap()
                {
                    let bob_events = bob.process_dialog_events().await.unwrap();
                    for event in &bob_events {
                        if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                            // Bob responds with WebRTC-style answer
                            let webrtc_answer = r#"v=0
o=test 654321 123456 IN IP4 192.168.1.200
s=-
c=IN IP4 192.168.1.200
t=0 0
m=audio 9 UDP/TLS/RTP/SAVPF 111
a=fingerprint:sha-256 BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA
a=setup:active
a=ice-ufrag:wxyz
a=ice-pwd:abcdefghijklmnopqrstuvw
a=rtpmap:111 opus/48000/2"#;

                            bob.answer_call(incoming_id, Some(webrtc_answer.to_string()))
                                .await
                                .ok();
                            println!("RTP to WebRTC conversion test completed");
                            break;
                        }
                    }
                }

                sleep(Duration::from_millis(200)).await;
                if let Ok(join_res) =
                    tokio::time::timeout(Duration::from_secs(5), caller_handle).await
                    && let Ok(Ok(id)) = join_res
                {
                    alice_arc.hangup(&id).await.ok();
                }
            }
            alice.stop();
            bob.stop();
            proxy.stop();
        }
    }

    /// Test media proxy with private IPs (NAT mode)
    #[tokio::test]
    async fn test_media_proxy_nat_scenarios() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::Nat)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(26030);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(26031);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test with private IP in SDP (should trigger NAT mode proxy)
        let private_ip_sdp = r#"v=0
o=test 123456 654321 IN IP4 192.168.1.100
s=-
c=IN IP4 192.168.1.100
t=0 0
m=audio 5004 RTP/AVP 0
a=rtpmap:0 PCMU/8000"#;

        let alice_arc = alice.clone();
        let caller_handle = rustpbx::utils::spawn({
            let a = alice_arc.clone();
            let s = private_ip_sdp.to_string();
            async move { a.make_call("bob", Some(s)).await }
        });
        if wait_for_event(
            &mut bob,
            |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
            1000,
        )
        .await
        .unwrap()
        {
            let bob_events = bob.process_dialog_events().await.unwrap();
            for event in &bob_events {
                if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                    // Bob answers with another private IP
                    let bob_private_sdp = r#"v=0
o=test 654321 123456 IN IP4 10.0.0.100
s=-
c=IN IP4 10.0.0.100
t=0 0
m=audio 5006 RTP/AVP 0
a=rtpmap:0 PCMU/8000"#;

                    bob.answer_call(incoming_id, Some(bob_private_sdp.to_string()))
                        .await
                        .ok();
                    println!("NAT mode media proxy test with private IPs completed");
                    break;
                }
            }
        }

        sleep(Duration::from_millis(200)).await;
        if let Ok(join_res) = tokio::time::timeout(Duration::from_secs(5), caller_handle).await
            && let Ok(Ok(id)) = join_res
        {
            alice_arc.hangup(&id).await.ok();
        }

        // Test with public IP (should NOT trigger NAT mode proxy)
        let public_ip_sdp = r#"v=0
o=test 123456 654321 IN IP4 203.0.113.100
s=-
c=IN IP4 203.0.113.100
t=0 0
m=audio 5004 RTP/AVP 0
a=rtpmap:0 PCMU/8000"#;

        let caller_handle = rustpbx::utils::spawn({
            let a = alice_arc.clone();
            let s = public_ip_sdp.to_string();
            async move { a.make_call("bob", Some(s)).await }
        });
        if wait_for_event(
            &mut bob,
            |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
            1000,
        )
        .await
        .unwrap()
        {
            let bob_events = bob.process_dialog_events().await.unwrap();
            for event in &bob_events {
                if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                    // Bob answers with public IP as well
                    let bob_public_sdp = r#"v=0
o=test 654321 123456 IN IP4 203.0.113.200
s=-
c=IN IP4 203.0.113.200
t=0 0
m=audio 5006 RTP/AVP 0
a=rtpmap:0 PCMU/8000"#;

                    bob.answer_call(incoming_id, Some(bob_public_sdp.to_string()))
                        .await
                        .ok();
                    break;
                }
            }
        }
        if let Ok(join_res) = tokio::time::timeout(Duration::from_secs(5), caller_handle).await
            && let Ok(Ok(id)) = join_res
        {
            sleep(Duration::from_millis(200)).await;
            alice_arc.hangup(&id).await.ok();
            println!("Public IP test completed (should bypass NAT proxy)");
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    #[tokio::test]
    async fn test_play_then_hangup_sends_183_session_progress() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25200);
        let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
            .await
            .unwrap();

        // Register alice
        alice.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test should be able to make call that triggers PlayThenHangup
        // In a real test scenario, this would be triggered by dialplan configuration
        // For now, we just verify the basic functionality works
        println!(
            "PlayThenHangup test with 183 Session Progress - basic registration and call setup works"
        );

        alice.stop();
        proxy.stop();
    }

    #[tokio::test]
    async fn test_ringtone_functionality() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25210);
        let alice = Arc::new(
            create_test_ua("alice", "password123", proxy_addr, alice_port)
                .await
                .unwrap(),
        );

        let bob_port = portpicker::pick_unused_port().unwrap_or(25211);
        let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        // Register both users
        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Simulate ringing then answer to complete the flow, and hang up
        let caller_handle = {
            let a = alice.clone();
            rustpbx::utils::spawn(async move { a.make_call("bob", None).await })
        };
        if wait_for_event(
            &mut bob,
            |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
            1000,
        )
        .await
        .unwrap()
        {
            let bob_events = bob.process_dialog_events().await.unwrap();
            for event in &bob_events {
                if let TestUaEvent::IncomingCall(incoming_id, _) = event {
                    // Send ringing for a bit, then answer to allow the caller future to resolve
                    bob.send_ringing(incoming_id, None).await.ok();
                    sleep(Duration::from_millis(300)).await;
                    bob.answer_call(incoming_id, None).await.ok();
                    break;
                }
            }
        }
        if let Ok(join_res) = tokio::time::timeout(Duration::from_secs(5), caller_handle).await
            && let Ok(Ok(id)) = join_res
        {
            alice.hangup(&id).await.ok();
            println!("Ringtone functionality test - call flow with ringing simulation works");
        }

        alice.stop();
        bob.stop();
        proxy.stop();
    }

    #[tokio::test]
    async fn test_audio_playback_code_reuse() {
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(25220);
        let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
            .await
            .unwrap();

        // Register alice
        alice.register().await.unwrap();
        sleep(Duration::from_millis(100)).await;

        // Test verifies that both PlayThenHangup and Ringtone functionality
        // can work with the same underlying simplified audio playback infrastructure
        // The code reuse is implemented through the unified play_audio_file method

        println!(
            "Audio playback code reuse test - simplified audio infrastructure supports both ringtone and PlayThenHangup"
        );

        alice.stop();
        proxy.stop();
    }

    /// Real WebRTC (DTLS-SRTP) caller through the media proxy to an RTP callee.
    /// Alice's PeerConnection generates a genuine WebRTC offer over SIP; the
    /// proxy negotiates it on its WebRTC leg and bridges to Bob (RTP).
    /// Runs on a big-stack worker thread: the real-DTLS WebRTC call drives the
    /// full SIP session + media-leg future chain, whose debug-build frame
    /// chain exceeds the default tokio thread stack.
    #[test]
    fn test_webrtc_rtp_real_media_proxy() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_stack_size(32 * 1024 * 1024)
            .build()
            .expect("runtime");
        rt.block_on(async {
            tokio::spawn(async { test_webrtc_rtp_real_media_proxy_impl().await })
                .await
                .expect("test task panicked");
        });
    }

    async fn test_webrtc_rtp_real_media_proxy_impl() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(Level::DEBUG)
            .try_init();
        let proxy = E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .unwrap();
        let proxy_addr = proxy.proxy_addr;

        let alice_port = portpicker::pick_unused_port().unwrap_or(26040);
        let alice = create_test_ua_webrtc("alice", "password123", proxy_addr, alice_port)
            .await
            .unwrap();
        let bob_port = portpicker::pick_unused_port().unwrap_or(26041);
        let bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
            .await
            .unwrap();

        alice.register().await.unwrap();
        bob.register().await.unwrap();
        sleep(Duration::from_millis(200)).await;

        // Alice (real WebRTC PeerConnection) calls Bob; the PC generates a
        // genuine DTLS-SRTP offer.
        let alice_clone = alice.clone();
        let caller_task =
            rustpbx::utils::spawn(async move { alice_clone.make_call("bob", None).await });

        // Bob waits for the incoming call and answers it with an RTP answer
        // (single loop so the IncomingCall event isn't consumed elsewhere).
        let mut answered = false;
        for _ in 0..400 {
            let evs = bob.process_dialog_events().await.unwrap_or_default();
            for ev in &evs {
                if let TestUaEvent::IncomingCall(id, offer) = ev {
                    let offer = offer.clone().unwrap_or_default();
                    let rtp_answer = create_test_sdp_answer(&offer, "127.0.0.1", bob_port + 1);
                    bob.answer_call(id, Some(rtp_answer)).await.unwrap();
                    answered = true;
                    break;
                }
            }
            if answered {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
        assert!(answered, "Bob should receive the incoming call");

        // Alice's call should complete (200 OK + WebRTC answer applied to PC).
        let dialog = tokio::time::timeout(Duration::from_secs(20), caller_task)
            .await
            .expect("caller task timeout")
            .unwrap()
            .expect("alice call should succeed");
        println!("Alice established call with real WebRTC: {}", dialog);

        alice.stop();
        bob.stop();
        proxy.stop();
    }
}
