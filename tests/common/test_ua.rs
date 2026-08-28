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
    /// Send a 180 Ringing provisional response for an incoming call —
    /// simulates a real phone ringing before the agent answers. The session
    /// turns this into `call_ringing` / `queue_agent_offered` RWI events.
    pub async fn ring_call(&self, dialog_id: &DialogId) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;
        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::Invite(d) => {
                    d.ringing(None, None).map_err(|e| e.into_anyhow())?;
                    Ok(())
                }
                _ => Err(anyhow!("Invalid dialog type for ringing")),
            }
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    /// Answer an incoming call with a 200 OK (optionally carrying an SDP answer).
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
