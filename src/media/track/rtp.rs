use super::track_codec::TrackCodec;
use crate::{
    event::{EventSender, SessionEvent},
    media::{
        negotiate::select_peer_media,
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackPacketSender},
    },
    AudioFrame, Samples, TrackId,
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use rsip::HostWithPort;
use rsipstack::transport::{udp::UdpConnection, SipAddr};
//use rtp_rs::{RtpPacketBuilder, RtpReader, Seq};
use std::{
    io::Cursor,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{select, time::interval_at, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use webrtc::{sdp::SessionDescription, util::Marshal, util::Unmarshal};
const RTP_MTU: usize = 1500; // Standard MTU size
const RTCP_SR_INTERVAL_MS: u64 = 5000; // 5 seconds RTCP sender report interval
const DTMF_EVENT_DURATION_MS: u64 = 160; // Default DTMF event duration (in ms)
const DTMF_EVENT_VOLUME: u8 = 10; // Default volume for DTMF events (0-63)

// RTCP Sender Report packet structure
// https://datatracker.ietf.org/doc/html/rfc3550#section-6.4.1
struct RtcpSenderReport {
    ssrc: u32,
    ntp_sec: u32,
    ntp_frac: u32,
    rtp_timestamp: u32,
    packet_count: u32,
    octet_count: u32,
}

impl RtcpSenderReport {
    fn new(ssrc: u32, rtp_timestamp: u32, packet_count: u32, octet_count: u32) -> Self {
        // Get current time
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();

        // Convert to NTP format
        let ntp_sec = now.as_secs() as u32 + 2208988800u32; // Seconds since 1900-01-01
        let ntp_frac = ((now.subsec_nanos() as u64 * 0x100000000u64) / 1_000_000_000u64) as u32;

        Self {
            ssrc,
            ntp_sec,
            ntp_frac,
            rtp_timestamp,
            packet_count,
            octet_count,
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        // Create a buffer with enough capacity for the RTCP SR packet
        let mut buf = BytesMut::with_capacity(28);

        // RTCP header (4 bytes)
        // First byte: V=2, P=0, RC=0
        buf.put_u8(0x80); // Version=2, Padding=0, Count=0
        buf.put_u8(200); // PT=200 (SR)
        buf.put_u16(6); // Length (6 32-bit words minus 1) = 6 words - 1 = 5, in network byte order

        // SSRC of sender (4 bytes)
        buf.put_u32(self.ssrc);

        // NTP timestamp (8 bytes)
        buf.put_u32(self.ntp_sec);
        buf.put_u32(self.ntp_frac);

        // RTP timestamp (4 bytes)
        buf.put_u32(self.rtp_timestamp);

        // Sender's packet count (4 bytes)
        buf.put_u32(self.packet_count);

        // Sender's octet count (4 bytes)
        buf.put_u32(self.octet_count);

        // Convert to Vec<u8> and return
        buf.to_vec()
    }
}

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
    local_addr: Option<SocketAddr>,
    external_addr: Option<SocketAddr>,
    stun_server: Option<String>,
    rtp_socket: Option<UdpConnection>,
    rtcp_socket: Option<UdpConnection>,
    rtp_start_port: u16,
}

pub struct RtpTrack {
    track_id: TrackId,
    config: TrackConfig,
    cancel_token: CancellationToken,
    ssrc: u32,
    remote_addr: Mutex<Option<SipAddr>>,
    remote_rtcp_addr: Mutex<Option<SipAddr>>,
    processor_chain: ProcessorChain,
    rtp_socket: UdpConnection,
    rtcp_socket: UdpConnection,
    sequence_number: AtomicU16,
    encoder: TrackCodec,
    state: Arc<RtpTrackState>,
    dtmf_payload_type: u8,
    payload_type: AtomicU8,
    remote_description: Mutex<Option<String>>,
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
            rtp_start_port: 12000,
        }
    }

    pub fn with_rtp_start_port(mut self, rtp_start_port: u16) -> Self {
        self.rtp_start_port = rtp_start_port;
        self
    }

    pub fn with_local_addr(mut self, local_addr: SocketAddr) -> Self {
        self.local_addr = Some(local_addr);
        self
    }

    pub fn with_external_addr(mut self, external_addr: SocketAddr) -> Self {
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

    pub async fn build_rtp_rtcp_conn(&self) -> Result<(UdpConnection, UdpConnection)> {
        let addr = crate::net_tool::get_first_non_loopback_interface()?;
        let mut rtp_conn = None;
        let mut rtcp_conn = None;

        for p in 0..100 {
            let port = self.rtp_start_port + p * 2;
            if let Ok(c) =
                UdpConnection::create_connection(format!("{:?}:{}", addr, port).parse()?, None)
                    .await
            {
                rtp_conn = Some(c);
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
                break;
            } else {
                info!("failed to bind RTP socket on port: {}", port);
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
        let track = RtpTrack {
            track_id: self.track_id,
            config: self.config,
            cancel_token,
            ssrc: rand::random::<u32>(),
            remote_addr: Mutex::new(None),
            remote_rtcp_addr: Mutex::new(None),
            processor_chain,
            rtp_socket: rtp_socket.unwrap(),
            rtcp_socket: rtcp_socket.unwrap(),
            sequence_number: AtomicU16::new(0),
            encoder: TrackCodec::new(),
            state: Arc::new(RtpTrackState::new()),
            dtmf_payload_type: 101,
            payload_type: AtomicU8::new(9),
            remote_description: Mutex::new(None),
        };
        Ok(track)
    }
}

impl RtpTrack {
    pub fn set_remote_description(&self, answer: &str) -> Result<()> {
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

        self.remote_description
            .lock()
            .unwrap()
            .replace(answer.to_string());

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
            "set remote description peer_addr: {} payload_type: {}",
            remote_addr,
            peer_media.codecs[0].payload_type()
        );

        self.payload_type
            .store(peer_media.codecs[0].payload_type(), Ordering::Relaxed);

        self.remote_addr.lock().unwrap().replace(remote_addr);
        self.remote_rtcp_addr
            .lock()
            .unwrap()
            .replace(remote_rtcp_addr);

        Ok(())
    }

    pub fn local_description(&self) -> Result<String> {
        let socketaddr: SocketAddr = self.rtp_socket.get_addr().addr.to_owned().try_into()?;
        let ssrc = self.ssrc;
        let sdp = format!(
            "v=0\r\n\
o=- 0 0 IN IP4 {}\r\n\
s=rustpbx\r\n\
c=IN IP4 {}\r\n\
t=0 0\r\n\
m=audio {} RTP/AVP 9 0 101\r\n\
a=rtpmap:9 G722/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n\
a=ssrc:{}\r\n\
a=sendrecv\r\n",
            socketaddr.ip(),
            socketaddr.ip(),
            socketaddr.port(),
            ssrc,
        );
        Ok(sdp)
    }

    // Send DTMF tone using RFC 4733
    pub async fn send_dtmf(&self, digit: &str, duration_ms: Option<u64>) -> Result<()> {
        let socket = &self.rtp_socket;
        let remote_addr = match self.remote_addr.lock().unwrap().as_ref() {
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

        // Get the current timestamp before we start sending DTMF
        let current_ts = self.state.timestamp.load(Ordering::Relaxed);
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

            // Get next sequence number
            let seq = self.sequence_number.fetch_add(1, Ordering::Relaxed);

            // Use the same timestamp for all DTMF packets of the same event
            // This is per RFC 4733 requirements
            let ts = current_ts;

            let payload_len = payload.len();
            let packet = webrtc::rtp::packet::Packet {
                header: webrtc::rtp::header::Header {
                    payload_type: self.dtmf_payload_type,
                    ssrc: self.ssrc,
                    sequence_number: seq.into(),
                    timestamp: ts,
                    ..Default::default()
                },
                payload: Bytes::from(payload),
                ..Default::default()
            };

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
                        .fetch_add(payload_len as u32, Ordering::Relaxed);

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

        // After sending DTMF, update the timestamp to account for the DTMF duration
        self.state
            .timestamp
            .fetch_add(samples_per_packet * num_packets, Ordering::Relaxed);

        Ok(())
    }

    async fn recv_rtp_packets(
        rtp_socket: UdpConnection,
        track_id: TrackId,
        processor_chain: ProcessorChain,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let mut buf = vec![0u8; RTP_MTU];
        loop {
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

            let packet = match webrtc::rtp::packet::Packet::unmarshal(&mut &buf[0..n]) {
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
        state: Arc<RtpTrackState>,
        rtcp_socket: UdpConnection,
        ssrc: u32,
        remote_rtcp_addr: Option<SipAddr>,
    ) -> Result<()> {
        let mut interval = interval_at(
            Instant::now() + Duration::from_millis(RTCP_SR_INTERVAL_MS),
            Duration::from_millis(RTCP_SR_INTERVAL_MS),
        );

        loop {
            // Generate RTCP Sender Report
            let packet_count = state.packet_count.load(Ordering::Relaxed);
            let octet_count = state.octet_count.load(Ordering::Relaxed);
            let rtp_timestamp = state.timestamp.load(Ordering::Relaxed);

            let sr = RtcpSenderReport::new(ssrc, rtp_timestamp, packet_count, octet_count);
            let rtcp_data = sr.to_bytes();

            match remote_rtcp_addr {
                Some(ref addr) => {
                    if let Err(e) = rtcp_socket.send_raw(&rtcp_data, addr).await {
                        error!("Failed to send RTCP report: {}", e);
                    } else {
                        debug!("Sent RTCP Sender Report");
                    }
                }
                None => {}
            }
            interval.tick().await;
        }
    }

    // Add a method for webrtc compatibility
    // This allows users to pass SDP strings generated by the webrtc crate
    pub fn set_remote_description_with_webrtc(&self, webrtc_sdp: &str) -> Result<()> {
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
        let answer = self
            .remote_description
            .lock()
            .unwrap()
            .clone()
            .unwrap_or("".to_string());
        Ok(answer)
    }

    async fn start(
        &self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let track_id = self.track_id.clone();
        let rtcp_addr = self.remote_rtcp_addr.lock().unwrap().clone();
        let rtcp_socket = self.rtcp_socket.clone();
        let ssrc = self.ssrc;
        let state = self.state.clone();
        let rtp_socket = self.rtp_socket.clone();
        let processor_chain = self.processor_chain.clone();
        let token = self.cancel_token.clone();
        tokio::spawn(async move {
            event_sender
                .send(SessionEvent::TrackStart {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                })
                .ok();

            select! {
                _ = token.cancelled() => {},
                r = Self::send_rtcp_reports(state, rtcp_socket, ssrc, rtcp_addr) => {
                    info!("RTCP sender process completed {:?}", r);
                }
                r = Self::recv_rtp_packets(rtp_socket, track_id.clone(), processor_chain, packet_sender) => {
                    info!("RTP processor completed {:?}", r);
                }
            };

            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::get_timestamp(),
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
        let payload = self
            .encoder
            .encode(self.payload_type.load(Ordering::Relaxed), packet.clone());

        if payload.is_empty() {
            return Ok(());
        }
        let remote_addr = match self.remote_addr.lock().unwrap().as_ref() {
            Some(addr) => addr.clone(),
            None => return Err(anyhow::anyhow!("Remote address not set")),
        };

        let seq = self.sequence_number.fetch_add(1, Ordering::Relaxed);
        let samples_per_packet =
            (self.config.codec.clock_rate() as f64 * self.config.ptime.as_secs_f64()) as u32;
        let now = crate::get_timestamp();
        let last_update = self.state.last_timestamp_update.load(Ordering::Relaxed);
        let elapsed_ms = now.saturating_sub(last_update);

        let timestamp_increment = if elapsed_ms > self.config.ptime.as_millis() as u64 {
            let silence_periods = elapsed_ms as f64 / self.config.ptime.as_millis() as f64;
            (silence_periods * samples_per_packet as f64) as u32
        } else {
            samples_per_packet
        };

        let ts = self
            .state
            .timestamp
            .fetch_add(timestamp_increment, Ordering::Relaxed);

        self.state
            .last_timestamp_update
            .store(now, Ordering::Relaxed);

        let payload_len = payload.len();
        // Create RTP packet
        let packet = webrtc::rtp::packet::Packet {
            header: webrtc::rtp::header::Header {
                payload_type: self.payload_type.load(Ordering::Relaxed),
                ssrc: self.ssrc,
                sequence_number: seq.into(),
                timestamp: ts,
                ..Default::default()
            },
            payload: Bytes::from(payload),
            ..Default::default()
        };
        match packet.marshal() {
            Ok(ref rtp_data) => {
                match socket.send_raw(rtp_data, &remote_addr).await {
                    Ok(_) => {
                        // Update packet and octet counts for RTCP
                        self.state.packet_count.fetch_add(1, Ordering::Relaxed);
                        self.state
                            .octet_count
                            .fetch_add(payload_len as u32, Ordering::Relaxed);
                    }
                    Err(e) => {
                        warn!("Failed to send RTP packet: {}", e);
                    }
                }
            }
            Err(e) => {
                error!("Failed to build RTP packet: {:?}", e);
                return Err(anyhow::anyhow!("Failed to build RTP packet"));
            }
        }
        Ok(())
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
        let rtp_track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build rtp track");
        rtp_track
            .set_remote_description(sdk)
            .expect("Failed to set remote description");
        assert_eq!(rtp_track.payload_type.load(Ordering::Relaxed), 9);
    }
}
