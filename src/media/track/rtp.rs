use super::track_codec::TrackCodec;
use crate::{
    event::{EventSender, SessionEvent},
    media::{
        codecs::CodecType,
        negotiate::select_peer_media,
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackPacketSender},
    },
    AudioFrame, Samples, TrackId,
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use rsip::HostWithPort;
use rsipstack::transport::{udp::UdpConnection, SipAddr};
use std::{
    io::Cursor,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{select, time::interval_at, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use webrtc::{
    rtcp::{
        goodbye::Goodbye,
        sender_report::SenderReport,
        source_description::{
            SdesType, SourceDescription, SourceDescriptionChunk, SourceDescriptionItem,
        },
    },
    rtp::{
        codecs::g7xx::G7xxPayloader,
        packet::Packet,
        packetizer::{new_packetizer, Packetizer},
        sequence::{new_random_sequencer, Sequencer},
    },
    sdp::{
        description::{
            common::{Address, Attribute, ConnectionInformation},
            media::{MediaName, RangedPort},
            session::{
                Origin, TimeDescription, Timing, ATTR_KEY_RTCPMUX, ATTR_KEY_SEND_ONLY,
                ATTR_KEY_SEND_RECV, ATTR_KEY_SSRC,
            },
        },
        MediaDescription, SessionDescription,
    },
    util::{Marshal, Unmarshal},
};
const RTP_MTU: usize = 1500; // UDP MTU size
const RTP_OUTBOUND_MTU: usize = 1200; // Standard MTU size
const RTCP_SR_INTERVAL_MS: u64 = 5000; // 5 seconds RTCP sender report interval
const DTMF_EVENT_DURATION_MS: u64 = 160; // Default DTMF event duration (in ms)
const DTMF_EVENT_VOLUME: u8 = 10; // Default volume for DTMF events (0-63)

struct RtpTrackState {
    timestamp: Arc<AtomicU32>,
    packet_count: Arc<AtomicU32>,
    octet_count: Arc<AtomicU32>,
    last_timestamp_update: Arc<AtomicU64>,
}

impl RtpTrackState {
    fn new() -> Self {
        Self {
            timestamp: Arc::new(AtomicU32::new(0)),
            packet_count: Arc::new(AtomicU32::new(0)),
            octet_count: Arc::new(AtomicU32::new(0)),
            last_timestamp_update: Arc::new(AtomicU64::new(crate::get_timestamp())),
        }
    }
}

pub struct RtpTrackBuilder {
    cancel_token: Option<CancellationToken>,
    track_id: TrackId,
    config: TrackConfig,
    local_addr: Option<IpAddr>,
    external_addr: Option<IpAddr>,
    stun_server: Option<String>,
    rtp_socket: Option<UdpConnection>,
    rtcp_socket: Option<UdpConnection>,
    rtcp_mux: bool,
    rtp_start_port: u16,
    rtp_end_port: u16,
    rtp_alloc_count: u32,
    enabled_codecs: Vec<CodecType>,
    ssrc_cname: String,
}

pub struct RtpTrack {
    track_id: TrackId,
    config: TrackConfig,
    cancel_token: CancellationToken,
    ssrc: u32,
    ssrc_cname: String,
    rtcp_mux: bool,
    remote_addr: Option<SipAddr>,
    remote_rtcp_addr: Option<SipAddr>,
    processor_chain: ProcessorChain,
    rtp_socket: UdpConnection,
    rtcp_socket: UdpConnection,
    encoder: TrackCodec,
    state: Arc<RtpTrackState>,
    dtmf_payload_type: u8,
    payload_type: u8,
    remote_description: Option<String>,
    sequencer: Box<dyn Sequencer + Send + Sync>,
    packetizer: Mutex<Option<Box<dyn Packetizer + Send + Sync>>>,
    enabled_codecs: Vec<CodecType>,
    sendrecv: AtomicBool,
}
impl RtpTrackBuilder {
    pub fn new(track_id: TrackId, config: TrackConfig) -> Self {
        Self {
            track_id,
            config,
            local_addr: None,
            external_addr: None,
            stun_server: None,
            cancel_token: None,
            rtp_socket: None,
            rtcp_socket: None,
            rtcp_mux: true,
            rtp_start_port: 12000,
            rtp_end_port: u16::MAX - 1,
            rtp_alloc_count: 500,
            enabled_codecs: vec![
                CodecType::G722,
                CodecType::PCMU,
                CodecType::PCMA,
                CodecType::TelephoneEvent,
            ],
            ssrc_cname: format!("rustpbx-{}", rand::random::<u32>()),
        }
    }

    pub fn with_rtp_start_port(mut self, rtp_start_port: u16) -> Self {
        self.rtp_start_port = rtp_start_port;
        self
    }
    pub fn with_rtp_end_port(mut self, rtp_end_port: u16) -> Self {
        self.rtp_end_port = rtp_end_port;
        self
    }
    pub fn with_rtp_alloc_count(mut self, rtp_alloc_count: u32) -> Self {
        self.rtp_alloc_count = rtp_alloc_count;
        self
    }
    pub fn with_local_addr(mut self, local_addr: IpAddr) -> Self {
        self.local_addr = Some(local_addr);
        self
    }

    pub fn with_external_addr(mut self, external_addr: IpAddr) -> Self {
        self.external_addr = Some(external_addr);
        self
    }

    pub fn with_stun_server(mut self, stun_server: String) -> Self {
        self.stun_server = Some(stun_server);
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn with_rtp_socket(mut self, rtp_socket: UdpConnection) -> Self {
        self.rtp_socket = Some(rtp_socket);
        self
    }
    pub fn with_rtcp_socket(mut self, rtcp_socket: UdpConnection) -> Self {
        self.rtcp_socket = Some(rtcp_socket);
        self
    }
    pub fn with_rtcp_mux(mut self, rtcp_mux: bool) -> Self {
        self.rtcp_mux = rtcp_mux;
        self
    }

    pub fn with_enabled_codecs(mut self, enabled_codecs: Vec<CodecType>) -> Self {
        self.enabled_codecs = enabled_codecs;
        self
    }
    pub fn with_session_name(mut self, session_name: String) -> Self {
        self.ssrc_cname = session_name;
        self
    }
    pub async fn build_rtp_rtcp_conn(&self) -> Result<(UdpConnection, UdpConnection)> {
        let addr = match self.local_addr {
            Some(addr) => addr,
            None => crate::net_tool::get_first_non_loopback_interface()?,
        };
        let mut rtp_conn = None;
        let mut rtcp_conn = None;

        for _ in 0..self.rtp_alloc_count {
            let port = rand::random_range::<u16, _>(self.rtp_start_port..=self.rtp_end_port);
            if port % 2 != 0 {
                continue;
            }
            if let Ok(c) =
                UdpConnection::create_connection(format!("{:?}:{}", addr, port).parse()?, None)
                    .await
            {
                if !self.rtcp_mux {
                    // if rtcp mux is not enabled, we need to create a separate RTCP socket
                    rtcp_conn = match UdpConnection::create_connection(
                        format!("{:?}:{}", addr, port + 1).parse()?,
                        None,
                    )
                    .await
                    {
                        Ok(c) => Some(c),
                        Err(_) => {
                            continue;
                        }
                    };
                } else {
                    rtcp_conn = Some(c.clone());
                }
                rtp_conn = Some(c);
                break;
            }
        }

        let mut rtp_conn = match rtp_conn {
            Some(c) => c,
            None => return Err(anyhow::anyhow!("failed to bind RTP socket")),
        };
        let mut rtcp_conn = match rtcp_conn {
            Some(c) => c,
            None => return Err(anyhow::anyhow!("failed to bind RTCP socket")),
        };

        if self.external_addr.is_none() && self.stun_server.is_some() {
            if let Some(ref server) = self.stun_server {
                match crate::net_tool::external_by_stun(
                    &mut rtp_conn,
                    &server,
                    Duration::from_secs(5),
                )
                .await
                {
                    Ok(_) => {}
                    Err(e) => info!(
                        "failed to get media external rtp addr, stunserver {} : {:?}",
                        server, e
                    ),
                }
                match crate::net_tool::external_by_stun(
                    &mut rtcp_conn,
                    &server,
                    Duration::from_secs(5),
                )
                .await
                {
                    Ok(_) => {}
                    Err(e) => info!(
                        "failed to get media external rtcp addr, stunserver {} : {:?}",
                        server, e
                    ),
                }
            }
        } else if let Some(addr) = self.external_addr {
            rtp_conn.external = Some(
                SocketAddr::new(
                    addr,
                    *rtp_conn
                        .get_addr()
                        .addr
                        .port
                        .clone()
                        .unwrap_or_default()
                        .value(),
                )
                .into(),
            );
            rtcp_conn.external = Some(
                SocketAddr::new(
                    addr,
                    *rtcp_conn
                        .get_addr()
                        .addr
                        .port
                        .clone()
                        .unwrap_or_default()
                        .value(),
                )
                .into(),
            );
        }
        Ok((rtp_conn, rtcp_conn))
    }

    pub async fn build(mut self) -> Result<RtpTrack> {
        let mut rtp_socket = self.rtp_socket.take();
        let mut rtcp_socket = self.rtcp_socket.take();

        if rtp_socket.is_none() || rtcp_socket.is_none() {
            let (rtp_conn, rtcp_conn) = self.build_rtp_rtcp_conn().await?;
            rtp_socket = Some(rtp_conn);
            rtcp_socket = Some(rtcp_conn);
        }
        let cancel_token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let processor_chain = ProcessorChain::new(self.config.samplerate);
        let ssrc = loop {
            let i = rand::random::<u32>();
            if i % 2 == 0 {
                break i;
            }
        };
        let track = RtpTrack {
            track_id: self.track_id,
            config: self.config,
            cancel_token,
            ssrc,
            rtcp_mux: self.rtcp_mux,
            remote_addr: None,
            remote_rtcp_addr: None,
            processor_chain,
            rtp_socket: rtp_socket.unwrap(),
            rtcp_socket: rtcp_socket.unwrap(),
            encoder: TrackCodec::new(),
            state: Arc::new(RtpTrackState::new()),
            dtmf_payload_type: 101,
            payload_type: 0,
            remote_description: None,
            sequencer: Box::new(new_random_sequencer()),
            packetizer: Mutex::new(None),
            enabled_codecs: self.enabled_codecs,
            sendrecv: AtomicBool::new(true),
            ssrc_cname: self.ssrc_cname,
        };
        Ok(track)
    }
}

impl RtpTrack {
    pub fn set_remote_description(&mut self, answer: &str) -> Result<()> {
        let mut reader = Cursor::new(answer);
        let sdp = SessionDescription::unmarshal(&mut reader)?;
        let peer_media = match select_peer_media(&sdp, "audio") {
            Some(peer_media) => peer_media,
            None => return Err(anyhow::anyhow!("no audio media in answer SDP")),
        };

        if peer_media.codecs.is_empty() {
            return Err(anyhow::anyhow!("no audio codecs in answer SDP"));
        }

        if peer_media.rtp_addr.is_empty() {
            return Err(anyhow::anyhow!("no rtp addr in answer SDP"));
        }

        self.remote_description.replace(answer.to_string());

        let remote_addr = SipAddr {
            addr: HostWithPort {
                host: peer_media.rtp_addr.parse()?,
                port: Some(peer_media.rtp_port.into()),
            },
            r#type: Some(rsip::transport::Transport::Udp),
        };
        let remote_rtcp_addr = SipAddr {
            addr: HostWithPort {
                host: peer_media.rtcp_addr.parse()?,
                port: Some(peer_media.rtcp_port.into()),
            },
            r#type: Some(rsip::transport::Transport::Udp),
        };

        info!(
            "set remote description peer_addr: {} rtcp_addr: {} payload_type: {}",
            remote_addr,
            remote_rtcp_addr,
            peer_media.codecs[0].payload_type()
        );

        self.payload_type = peer_media.codecs[0].payload_type();
        self.remote_addr.replace(remote_addr);
        self.remote_rtcp_addr.replace(remote_rtcp_addr);
        self.rtcp_mux = peer_media.rtcp_mux;

        let payloader = Box::<G7xxPayloader>::default();
        self.packetizer
            .lock()
            .unwrap()
            .replace(Box::new(new_packetizer(
                RTP_OUTBOUND_MTU,
                peer_media.codecs[0].payload_type(),
                self.ssrc,
                payloader,
                self.sequencer.clone(),
                peer_media.codecs[0].clock_rate(),
            )));
        Ok(())
    }

    pub fn local_description(&self) -> Result<String> {
        let socketaddr: SocketAddr = self.rtp_socket.get_addr().addr.to_owned().try_into()?;
        let mut sdp = SessionDescription::default();

        // Set session-level attributes
        sdp.version = 0;
        sdp.origin = Origin {
            username: "-".to_string(),
            session_id: 0,
            session_version: 0,
            network_type: "IN".to_string(),
            address_type: "IP4".to_string(),
            unicast_address: socketaddr.ip().to_string(),
        };
        sdp.session_name = "-".to_string();
        sdp.connection_information = Some(ConnectionInformation {
            address_type: "IP4".to_string(),
            network_type: "IN".to_string(),
            address: Some(Address {
                address: socketaddr.ip().to_string(),
                ttl: None,
                range: None,
            }),
        });
        sdp.time_descriptions.push(TimeDescription {
            timing: Timing {
                start_time: 0,
                stop_time: 0,
            },
            repeat_times: vec![],
        });

        // Add media section
        let mut media = MediaDescription::default();
        media.media_name = MediaName {
            media: "audio".to_string(),
            port: RangedPort {
                value: socketaddr.port() as isize,
                range: None,
            },
            protos: vec!["RTP".to_string(), "AVP".to_string()],
            formats: vec![],
        };

        for codec in self.enabled_codecs.iter() {
            media
                .media_name
                .formats
                .push(codec.payload_type().to_string());
            media.attributes.push(Attribute {
                key: "rtpmap".to_string(),
                value: Some(format!("{} {}", codec.payload_type(), codec.rtpmap())),
            });
        }

        // Add media-level attributes
        if self.rtcp_mux {
            media.attributes.push(Attribute {
                key: ATTR_KEY_RTCPMUX.to_string(),
                value: None,
            });
        }
        media.attributes.push(Attribute {
            key: ATTR_KEY_SSRC.to_string(),
            value: Some(if self.ssrc_cname.is_empty() {
                self.ssrc.to_string()
            } else {
                format!("{} cname:{}", self.ssrc, self.ssrc_cname)
            }),
        });
        if self.sendrecv.load(Ordering::Relaxed) {
            media.attributes.push(Attribute {
                key: ATTR_KEY_SEND_RECV.to_string(),
                value: None,
            });
        } else {
            media.attributes.push(Attribute {
                key: ATTR_KEY_SEND_ONLY.to_string(),
                value: None,
            });
        }
        sdp.media_descriptions.push(media);
        Ok(sdp.marshal())
    }

    // Send DTMF tone using RFC 4733
    pub async fn send_dtmf(&self, digit: &str, duration_ms: Option<u64>) -> Result<()> {
        let socket = &self.rtp_socket;
        let remote_addr = match self.remote_addr.as_ref() {
            Some(addr) => addr.clone(),
            None => return Err(anyhow::anyhow!("Remote address not set")),
        };
        // Map DTMF digit to event code
        let event_code = match digit {
            "0" => 0,
            "1" => 1,
            "2" => 2,
            "3" => 3,
            "4" => 4,
            "5" => 5,
            "6" => 6,
            "7" => 7,
            "8" => 8,
            "9" => 9,
            "*" => 10,
            "#" => 11,
            "A" => 12,
            "B" => 13,
            "C" => 14,
            "D" => 15,
            _ => return Err(anyhow::anyhow!("Invalid DTMF digit")),
        };

        // Use default duration if not specified
        let duration = duration_ms.unwrap_or(DTMF_EVENT_DURATION_MS);

        // Calculate number of packets to send
        // We send one packet every 20ms (default packet time)
        let num_packets = (duration as f64 / self.config.ptime.as_millis() as f64).ceil() as u32;

        // Calculate samples per packet for timestamp increments
        let samples_per_packet =
            (self.config.samplerate as f64 * self.config.ptime.as_secs_f64()) as u32;

        let now = crate::get_timestamp();
        self.state
            .last_timestamp_update
            .store(now, Ordering::Relaxed);

        // Generate RFC 4733 DTMF events
        for i in 0..num_packets {
            let is_end = i == num_packets - 1;
            let event_duration = i * (self.config.ptime.as_millis() as u32 * 8); // Duration in timestamp units

            // Create DTMF event payload
            // Format: |event(8)|E|R|Volume(6)|Duration(16)|
            let mut payload = vec![0u8; 4];
            payload[0] = event_code;
            payload[1] = DTMF_EVENT_VOLUME & 0x3F; // Volume (0-63)
            if is_end {
                payload[1] |= 0x80; // Set end bit (E)
            }

            // Duration (16 bits, network byte order)
            payload[2] = ((event_duration >> 8) & 0xFF) as u8;
            payload[3] = (event_duration & 0xFF) as u8;

            let packets = match self.packetizer.lock().unwrap().as_mut() {
                Some(p) => p.packetize(&Bytes::from_owner(payload), samples_per_packet)?,
                None => return Err(anyhow::anyhow!("Packetizer not set")),
            };
            for mut packet in packets {
                packet.header.payload_type = self.dtmf_payload_type;
                packet.header.marker = false;

                match packet.marshal() {
                    Ok(ref rtp_data) => {
                        match socket.send_raw(rtp_data, &remote_addr).await {
                            Ok(_) => {}
                            Err(e) => {
                                error!("Failed to send DTMF RTP packet: {}", e);
                            }
                        }

                        // Update counters for RTCP
                        self.state.packet_count.fetch_add(1, Ordering::Relaxed);
                        self.state
                            .octet_count
                            .fetch_add(rtp_data.len() as u32, Ordering::Relaxed);

                        // Sleep for packet time if not the last packet
                        if !is_end {
                            tokio::time::sleep(self.config.ptime).await;
                        }
                    }
                    Err(e) => {
                        error!("Failed to create DTMF RTP packet: {:?}", e);
                        continue;
                    }
                }
            }
        }

        // After sending DTMF, update the timestamp to account for the DTMF duration
        self.state
            .timestamp
            .fetch_add(samples_per_packet * num_packets, Ordering::Relaxed);

        Ok(())
    }

    async fn recv_rtp_packets(
        token: CancellationToken,
        rtp_socket: UdpConnection,
        track_id: TrackId,
        processor_chain: ProcessorChain,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let mut buf = vec![0u8; RTP_MTU];
        loop {
            if token.is_cancelled() {
                return Ok(());
            }
            let (n, _) = match rtp_socket.recv_raw(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Error receiving RTP packet: {}", e);
                    return Err(anyhow::anyhow!("Error receiving RTP packet: {}", e));
                }
            };
            if n <= 0 {
                continue;
            }

            // RTCP packet detection and filtering for rtcp-mux scenarios
            if n >= 2 {
                let version = (buf[0] >> 6) & 0x03;

                // Check if this is a STUN packet first
                // STUN packets have specific message types and magic cookie
                if n >= 8 {
                    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
                    let msg_length = u16::from_be_bytes([buf[2], buf[3]]);
                    let magic_cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

                    // STUN magic cookie is 0x2112A442
                    // STUN message types are in specific ranges (0x0001, 0x0101, etc.)
                    if magic_cookie == 0x2112A442
                        || (msg_type & 0xC000) == 0x0000 && msg_length <= (n - 20) as u16
                    {
                        debug!(
                            "Received STUN packet with message type: 0x{:04X}, length: {}, skipping RTP processing",
                            msg_type, n
                        );
                        continue;
                    }
                }

                // Check if this is an RTCP packet
                // RTCP packet structure: V(2) + P(1) + RC(5) + PT(8) + Length(16) + ...
                // For RTCP: PT is the full second byte (200-207)
                let rtcp_pt = buf[1]; // Full second byte for RTCP
                if version == 2 && rtcp_pt >= 200 && rtcp_pt <= 207 {
                    info!(
                        "Received RTCP packet with PT: {}, length: {}, skipping RTP processing",
                        rtcp_pt, n
                    );
                    continue;
                }

                // For RTP packets: V(2) + P(1) + X(1) + CC(4) + M(1) + PT(7) + ...
                // PT is only 7 bits for RTP
                let rtp_pt = buf[1] & 0x7F; // Extract payload type (7 bits) for RTP

                // Additional validation for RTP packets
                if version != 2 {
                    info!(
                        "Received packet with invalid RTP version: {}, skipping",
                        version
                    );
                    continue;
                }

                // RTP payload types should be < 128 (7 bits)
                if rtp_pt >= 128 {
                    debug!(
                        "Received packet with invalid RTP payload type: {}, might be unrecognized protocol",
                        rtp_pt
                    );
                    continue;
                }
            }

            let packet = match Packet::unmarshal(&mut &buf[0..n]) {
                Ok(packet) => packet,
                Err(e) => {
                    debug!("Error creating RTP reader: {:?}", e);
                    continue;
                }
            };

            let payload_type = packet.header.payload_type;
            let payload = packet.payload.to_vec();
            let sample_rate = match payload_type {
                9 => 16000, // G.722
                _ => 8000,
            };

            let frame = AudioFrame {
                track_id: track_id.clone(),
                samples: Samples::RTP {
                    payload_type,
                    payload,
                    sequence_number: packet.header.sequence_number.into(),
                },
                timestamp: crate::get_timestamp(),
                sample_rate,
            };

            if let Err(e) = processor_chain.process_frame(&frame) {
                error!("Failed to process frame: {}", e);
                break;
            }
            match packet_sender.send(frame) {
                Ok(_) => {}
                Err(e) => {
                    error!("Error sending audio frame: {}", e);
                    break;
                }
            }
        }
        Ok(())
    }

    // Send RTCP sender reports periodically
    async fn send_rtcp_reports(
        token: CancellationToken,
        state: Arc<RtpTrackState>,
        rtcp_socket: &UdpConnection,
        ssrc: u32,
        ssrc_cname: String,
        remote_rtcp_addr: Option<&SipAddr>,
    ) -> Result<()> {
        let mut interval = interval_at(
            Instant::now() + Duration::from_millis(RTCP_SR_INTERVAL_MS),
            Duration::from_millis(RTCP_SR_INTERVAL_MS),
        );
        loop {
            if token.is_cancelled() {
                return Ok(());
            }
            // Generate RTCP Sender Report
            let packet_count = state.packet_count.load(Ordering::Relaxed);
            let octet_count = state.octet_count.load(Ordering::Relaxed);
            let rtp_timestamp = state.timestamp.load(Ordering::Relaxed);

            let mut pkts = vec![Box::new(SenderReport {
                ssrc,
                ntp_time: Instant::now().elapsed().as_secs() as u64,
                rtp_time: rtp_timestamp,
                packet_count,
                octet_count,
                profile_extensions: Bytes::new(),
                reports: vec![],
            })
                as Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>];
            if !ssrc_cname.is_empty() {
                pkts.push(Box::new(SourceDescription {
                    chunks: vec![SourceDescriptionChunk {
                        source: ssrc,
                        items: vec![SourceDescriptionItem {
                            sdes_type: SdesType::SdesCname,
                            text: ssrc_cname.clone().into(),
                        }],
                    }],
                })
                    as Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>);
            }

            let rtcp_data = webrtc::rtcp::packet::marshal(&pkts)?;
            match remote_rtcp_addr {
                Some(ref addr) => {
                    if let Err(e) = rtcp_socket.send_raw(&rtcp_data, addr).await {
                        error!("Failed to send RTCP report: {}", e);
                    } else {
                        debug!("Sent RTCP Sender Report -> {}", addr);
                    }
                }
                None => {}
            }
            interval.tick().await;
        }
    }

    // Add a method for webrtc compatibility
    // This allows users to pass SDP strings generated by the webrtc crate
    pub fn set_remote_description_with_webrtc(&mut self, webrtc_sdp: &str) -> Result<()> {
        // For webrtc compatibility, we directly use the string representation
        self.set_remote_description(webrtc_sdp)
    }
}

#[async_trait]
impl Track for RtpTrack {
    fn id(&self) -> &TrackId {
        &self.track_id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }

    fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.insert_processor(processor);
    }

    fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.append_processor(processor);
    }

    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        let answer = self.remote_description.clone().unwrap_or("".to_string());
        Ok(answer)
    }

    async fn start(
        &self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let track_id = self.track_id.clone();
        let rtcp_addr = self.remote_rtcp_addr.clone();
        let rtcp_socket = self.rtcp_socket.clone();
        let ssrc = self.ssrc;
        let state = self.state.clone();
        let rtp_socket = self.rtp_socket.clone();
        let processor_chain = self.processor_chain.clone();
        let token = self.cancel_token.clone();
        let ssrc_cname = self.ssrc_cname.clone();
        let start_time = crate::get_timestamp();
        tokio::spawn(async move {
            select! {
                _ = token.cancelled() => {
                    info!("RTC process cancelled");
                },
                r = Self::send_rtcp_reports(token.clone(), state, &rtcp_socket, ssrc, ssrc_cname, rtcp_addr.as_ref()) => {
                    info!("RTCP sender process completed {:?}", r);
                }
                r = Self::recv_rtp_packets(token.clone(), rtp_socket, track_id.clone(), processor_chain, packet_sender) => {
                    info!("RTP processor completed {:?}", r);
                }
            };

            // send rtcp bye packet
            match rtcp_addr {
                Some(ref addr) => {
                    let pkts = vec![Box::new(Goodbye {
                        sources: vec![ssrc],
                        reason: "end of call".into(),
                    })
                        as Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>];
                    if let Ok(data) = webrtc::rtcp::packet::marshal(&pkts) {
                        if let Err(e) = rtcp_socket.send_raw(&data, addr).await {
                            error!("Failed to send RTCP goodbye packet: {}", e);
                        }
                    }
                }
                None => {}
            }

            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::get_timestamp(),
                    duration: crate::get_timestamp() - start_time,
                })
                .ok();
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        let socket = &self.rtp_socket;
        let payload = self.encoder.encode(self.payload_type, packet.clone());

        if payload.is_empty() {
            return Ok(());
        }
        let remote_addr = match self.remote_addr.as_ref() {
            Some(addr) => addr.clone(),
            None => return Err(anyhow::anyhow!("Remote address not set")),
        };

        let clock_rate = match self.payload_type {
            _ => 8000,
        };

        let now = crate::get_timestamp();
        let last_update = self.state.last_timestamp_update.load(Ordering::Relaxed);
        let skipped_packets = if last_update > 0 {
            (now - last_update) / clock_rate as u64
        } else {
            0
        };

        self.state
            .last_timestamp_update
            .store(now, Ordering::Relaxed);

        if skipped_packets > 0 {
            for _ in 0..skipped_packets {
                self.sequencer.next_sequence_number();
            }
        }

        let samples_per_packet = (clock_rate as f64 * self.config.ptime.as_secs_f64()) as u32;
        let packets = match self.packetizer.lock().unwrap().as_mut() {
            Some(p) => {
                if skipped_packets > 0 {
                    p.skip_samples((skipped_packets * samples_per_packet as u64) as u32);
                }
                p.packetize(&Bytes::from_owner(payload), samples_per_packet)?
            }
            None => return Err(anyhow::anyhow!("Packetizer not set")),
        };
        for mut packet in packets {
            packet.header.marker = false;
            match packet.marshal() {
                Ok(ref rtp_data) => match socket.send_raw(rtp_data, &remote_addr).await {
                    Ok(_) => {
                        self.state.packet_count.fetch_add(1, Ordering::Relaxed);
                        self.state
                            .octet_count
                            .fetch_add(rtp_data.len() as u32, Ordering::Relaxed);
                        self.state
                            .timestamp
                            .fetch_add(samples_per_packet, Ordering::Relaxed);
                    }
                    Err(e) => {
                        warn!("Failed to send RTP packet: {}", e);
                    }
                },
                Err(e) => {
                    error!("Failed to build RTP packet: {:?}", e);
                    return Err(anyhow::anyhow!("Failed to build RTP packet"));
                }
            }
        }
        Ok(())
    }

    /// Implementation of Track trait's set_remote_sdp method
    fn set_remote_sdp(&mut self, sdp: &str) -> Result<()> {
        self.set_remote_description(sdp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_pjsip_sdp() {
        let sdk = r#"v=0
o=- 3954304612 3954304613 IN IP4 192.168.1.202
s=pjmedia
b=AS:117
t=0 0
a=X-nat:3
m=audio 4002 RTP/AVP 9 101
c=IN IP4 192.168.1.202
b=TIAS:96000
a=rtcp:4003 IN IP4 192.168.1.202
a=sendrecv
a=rtpmap:9 G722/8000
a=ssrc:1089147397 cname:61753255553b9c6f
a=rtpmap:101 telephone-event/8000
a=fmtp:101 0-16"#;
        let mut rtp_track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build rtp track");
        rtp_track
            .set_remote_description(sdk)
            .expect("Failed to set remote description");
        assert_eq!(rtp_track.payload_type, 9);
    }
}
