use super::track_codec::TrackCodec;
use crate::{
    event::{EventSender, SessionEvent},
    media::{
        dtmf::DTMFDetector,
        jitter::JitterBuffer,
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackPacketSender},
    },
    AudioFrame, Samples, TrackId,
};
use anyhow::Result;
use async_trait::async_trait;
use rtp_rs::{RtpPacketBuilder, RtpReader, Seq};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{net::UdpSocket, select, time::interval_at, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

const RTP_MTU: usize = 1500; // Standard MTU size
const RTCP_SR_INTERVAL_MS: u64 = 5000; // 5 seconds RTCP sender report interval
const DTMF_EVENT_DURATION_MS: u64 = 160; // Default DTMF event duration (in ms)
const DTMF_EVENT_VOLUME: u8 = 10; // Default volume for DTMF events (0-63)

// Configuration for RTP track
#[derive(Clone)]
pub struct RtpTrackConfig {
    pub local_addr: SocketAddr,
    pub remote_addr: SocketAddr,
    pub payload_type: u8,
    pub ssrc: u32,
    // DTMF payload type (default is 101 as per RFC 4733)
    pub dtmf_payload_type: u8,
}

impl Default for RtpTrackConfig {
    fn default() -> Self {
        Self {
            local_addr: "0.0.0.0:0".parse().unwrap(),
            remote_addr: "127.0.0.1:5004".parse().unwrap(),
            payload_type: 0, // PCMU by default
            ssrc: rand::random::<u32>(),
            dtmf_payload_type: 101, // Default DTMF payload type
        }
    }
}

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
        let mut buf = vec![0u8; 28];

        // RTCP header (2 bytes)
        buf[0] = 0x80; // Version=2, Padding=0, Count=0
        buf[1] = 200; // PT=200 (SR)

        // Length (2 bytes) - length in 32-bit words minus 1
        // For SR, it's 6 32-bit words, so length is 5
        buf[2] = 0;
        buf[3] = 6;

        // SSRC (4 bytes)
        buf[4] = (self.ssrc >> 24) as u8;
        buf[5] = (self.ssrc >> 16) as u8;
        buf[6] = (self.ssrc >> 8) as u8;
        buf[7] = self.ssrc as u8;

        // NTP timestamp (8 bytes)
        buf[8] = (self.ntp_sec >> 24) as u8;
        buf[9] = (self.ntp_sec >> 16) as u8;
        buf[10] = (self.ntp_sec >> 8) as u8;
        buf[11] = self.ntp_sec as u8;
        buf[12] = (self.ntp_frac >> 24) as u8;
        buf[13] = (self.ntp_frac >> 16) as u8;
        buf[14] = (self.ntp_frac >> 8) as u8;
        buf[15] = self.ntp_frac as u8;

        // RTP timestamp (4 bytes)
        buf[16] = (self.rtp_timestamp >> 24) as u8;
        buf[17] = (self.rtp_timestamp >> 16) as u8;
        buf[18] = (self.rtp_timestamp >> 8) as u8;
        buf[19] = self.rtp_timestamp as u8;

        // Sender's packet count (4 bytes)
        buf[20] = (self.packet_count >> 24) as u8;
        buf[21] = (self.packet_count >> 16) as u8;
        buf[22] = (self.packet_count >> 8) as u8;
        buf[23] = self.packet_count as u8;

        // Sender's octet count (4 bytes)
        buf[24] = (self.octet_count >> 24) as u8;
        buf[25] = (self.octet_count >> 16) as u8;
        buf[26] = (self.octet_count >> 8) as u8;
        buf[27] = self.octet_count as u8;

        buf
    }
}

pub struct RtpTrack {
    track_id: TrackId,
    config: TrackConfig,
    rtp_config: RtpTrackConfig,
    processor_chain: ProcessorChain,
    rtp_socket: Option<Arc<UdpSocket>>,
    rtcp_socket: Option<Arc<UdpSocket>>,
    cancel_token: CancellationToken,
    sequence_number: AtomicU16,
    timestamp: AtomicU32,
    encoder: TrackCodec,
    // Add jitter buffer for incoming packets
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    // Track outgoing packet and byte counts for RTCP
    packet_count: AtomicU32,
    octet_count: AtomicU32,
    // DTMF detection
    dtmf_detector: Arc<Mutex<DTMFDetector>>,
}

impl RtpTrack {
    pub fn new(id: TrackId) -> Self {
        let config = TrackConfig::default().with_sample_rate(8000);
        Self {
            track_id: id,
            config: config.clone(),
            rtp_config: RtpTrackConfig::default(),
            processor_chain: ProcessorChain::new(config.sample_rate),
            rtp_socket: None,
            rtcp_socket: None,
            cancel_token: CancellationToken::new(),
            sequence_number: AtomicU16::new(0),
            timestamp: AtomicU32::new(0),
            encoder: TrackCodec::new(),
            jitter_buffer: Arc::new(Mutex::new(JitterBuffer::with_max_size(100))),
            packet_count: AtomicU32::new(0),
            octet_count: AtomicU32::new(0),
            dtmf_detector: Arc::new(Mutex::new(DTMFDetector::new())),
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config.clone();
        self
    }

    pub fn with_rtp_config(mut self, rtp_config: RtpTrackConfig) -> Self {
        self.rtp_config = rtp_config;
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);
        self.processor_chain = ProcessorChain::new(sample_rate);
        self
    }

    pub fn with_socket(mut self, rtp_socket: Arc<UdpSocket>, rtcp_socket: Arc<UdpSocket>) -> Self {
        self.rtp_socket = Some(rtp_socket);
        self.rtcp_socket = Some(rtcp_socket);
        self
    }

    pub async fn setup_rtp_socket(&mut self) -> Result<()> {
        let socket = UdpSocket::bind(self.rtp_config.local_addr).await?;
        socket.connect(self.rtp_config.remote_addr).await?;

        info!(
            "rtptrack: RTP socket created - local: {:?}, remote: {:?}",
            self.rtp_config.local_addr, self.rtp_config.remote_addr
        );

        self.rtp_socket = Some(Arc::new(socket));
        Ok(())
    }

    pub async fn setup_rtcp_socket(&mut self) -> Result<()> {
        // RTCP port is traditionally RTP port + 1
        let local_rtcp_addr = SocketAddr::new(
            self.rtp_config.local_addr.ip(),
            self.rtp_config.local_addr.port() + 1,
        );
        let remote_rtcp_addr = SocketAddr::new(
            self.rtp_config.remote_addr.ip(),
            self.rtp_config.remote_addr.port() + 1,
        );

        let socket = UdpSocket::bind(local_rtcp_addr).await?;
        socket.connect(remote_rtcp_addr).await?;

        info!(
            "rtptrack: RTCP socket created - local: {:?}, remote: {:?}",
            local_rtcp_addr, remote_rtcp_addr
        );

        self.rtcp_socket = Some(Arc::new(socket));
        Ok(())
    }

    // Variant of setup_rtcp_socket that allows binding to a specific address
    pub async fn setup_rtcp_socket_with_addr(
        &mut self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
    ) -> Result<()> {
        let socket = UdpSocket::bind(local_addr).await?;
        socket.connect(remote_addr).await?;

        info!(
            "rtptrack: RTCP socket created - local: {:?}, remote: {:?}",
            local_addr, remote_addr
        );

        self.rtcp_socket = Some(Arc::new(socket));
        Ok(())
    }

    // Send DTMF tone using RFC 4733
    pub async fn send_dtmf(&self, digit: &str, duration_ms: Option<u64>) -> Result<()> {
        if self.rtp_socket.is_none() {
            return Err(anyhow::anyhow!("rtptrack: Socket not initialized"));
        }

        let socket = self.rtp_socket.as_ref().unwrap();

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

            // Use the current timestamp
            let ts = self.timestamp.load(Ordering::Relaxed);

            // Create RTP packet with DTMF payload
            let result = RtpPacketBuilder::new()
                .payload_type(self.rtp_config.dtmf_payload_type)
                .ssrc(self.rtp_config.ssrc)
                .sequence(Seq::from(seq))
                .timestamp(ts)
                .payload(&payload)
                .build();

            if let Ok(rtp_data) = result {
                socket.send(&rtp_data).await?;

                // Update counters for RTCP
                self.packet_count.fetch_add(1, Ordering::Relaxed);
                self.octet_count
                    .fetch_add(payload.len() as u32, Ordering::Relaxed);

                // Sleep for packet time if not the last packet
                if !is_end {
                    tokio::time::sleep(self.config.ptime).await;
                }
            } else {
                return Err(anyhow::anyhow!("Failed to build DTMF RTP packet"));
            }
        }

        Ok(())
    }

    // Process incoming RTP packets, extract DTMF, and put them in jitter buffer
    async fn process_rtp_packets(
        self: Arc<Self>,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let rtp_socket = self
            .rtp_socket
            .clone()
            .ok_or_else(|| anyhow::anyhow!("rtptrack: RTP socket not initialized"))?;

        let track_id = self.track_id.clone();
        let config = self.config.clone();
        let jitter_buffer = self.jitter_buffer.clone();
        let dtmf_detector = self.dtmf_detector.clone();

        // Jitter buffer processing interval
        let mut jb_interval = interval_at(
            Instant::now() + Duration::from_millis(20),
            Duration::from_millis(20),
        );

        let mut buf = vec![0u8; RTP_MTU];

        loop {
            select! {
                _ = token.cancelled() => {
                    break;
                }
                _ = jb_interval.tick() => {
                    // Process frames from jitter buffer
                    let frames = {
                        let mut jb = jitter_buffer.lock().expect("Failed to lock jitter buffer");
                        jb.pull_frames(20) // Pull frames for 20ms
                    };

                    // Send frames to packet sender
                    for frame in frames {
                        if let Err(e) = packet_sender.send(frame) {
                            error!("rtptrack: Failed to send frame from jitter buffer: {}", e);
                        }
                    }
                }
                result = rtp_socket.recv(&mut buf) => {
                    match result {
                        Ok(n) => {
                            if n <= 0 {
                                continue;
                            }

                            if let Ok(reader) = RtpReader::new(&buf[0..n]) {
                                let payload_type = reader.payload_type();
                                let payload = reader.payload().to_vec();
                                let timestamp = reader.timestamp();

                                // Check for DTMF events (RFC 4733)
                                if payload_type == self.rtp_config.dtmf_payload_type {
                                    let mut detector = dtmf_detector.lock().expect("Failed to lock DTMF detector");
                                    if let Some(digit) = detector.detect_rtp(&track_id, payload_type, &payload) {
                                        // Clone digit before moving
                                        let digit_clone = digit.clone();
                                        // Send DTMF event
                                        event_sender
                                            .send(SessionEvent::DTMF {
                                                track_id: track_id.clone(),
                                                timestamp: crate::get_timestamp(),
                                                digit,
                                            })
                                            .ok();

                                        debug!("rtptrack: Detected DTMF {}", digit_clone);
                                    }
                                } else {
                                    // Regular RTP audio packet
                                    let frame = AudioFrame {
                                        track_id: track_id.clone(),
                                        samples: Samples::RTP {
                                            payload_type,
                                            payload,
                                            sequence_number: reader.sequence_number().into(),
                                        },
                                        timestamp: timestamp as u64,
                                        sample_rate: config.sample_rate,
                                    };

                                    // Add to jitter buffer
                                    {
                                        let mut jb = jitter_buffer.lock().expect("Failed to lock jitter buffer");
                                        jb.push(frame);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("rtptrack: Error receiving RTP packet: {}", e);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // Send RTCP sender reports periodically
    async fn send_rtcp_reports(self: Arc<Self>, token: CancellationToken) -> Result<()> {
        let rtcp_socket = match self.rtcp_socket.clone() {
            Some(socket) => socket,
            None => return Ok(()),
        };

        let mut interval = interval_at(
            Instant::now() + Duration::from_millis(RTCP_SR_INTERVAL_MS),
            Duration::from_millis(RTCP_SR_INTERVAL_MS),
        );

        loop {
            select! {
                _ = token.cancelled() => {
                    break;
                }
                _ = interval.tick() => {
                    // Generate RTCP Sender Report
                    let packet_count = self.packet_count.load(Ordering::Relaxed);
                    let octet_count = self.octet_count.load(Ordering::Relaxed);
                    let rtp_timestamp = self.timestamp.load(Ordering::Relaxed);

                    let sr = RtcpSenderReport::new(
                        self.rtp_config.ssrc,
                        rtp_timestamp,
                        packet_count,
                        octet_count
                    );

                    // Send RTCP packet
                    let rtcp_data = sr.to_bytes();
                    if let Err(e) = rtcp_socket.send(&rtcp_data).await {
                        error!("rtptrack: Failed to send RTCP report: {}", e);
                    } else {
                        debug!("rtptrack: Sent RTCP Sender Report");
                    }
                }
            }
        }

        Ok(())
    }
}

impl Clone for RtpTrack {
    fn clone(&self) -> Self {
        Self {
            track_id: self.track_id.clone(),
            config: self.config.clone(),
            rtp_config: self.rtp_config.clone(),
            processor_chain: self.processor_chain.clone(),
            rtp_socket: self.rtp_socket.clone(),
            rtcp_socket: self.rtcp_socket.clone(),
            cancel_token: self.cancel_token.clone(),
            sequence_number: AtomicU16::new(self.sequence_number.load(Ordering::Relaxed)),
            timestamp: AtomicU32::new(self.timestamp.load(Ordering::Relaxed)),
            encoder: self.encoder.clone(),
            jitter_buffer: self.jitter_buffer.clone(),
            packet_count: AtomicU32::new(self.packet_count.load(Ordering::Relaxed)),
            octet_count: AtomicU32::new(self.octet_count.load(Ordering::Relaxed)),
            dtmf_detector: self.dtmf_detector.clone(),
        }
    }
}

#[async_trait]
impl Track for RtpTrack {
    fn id(&self) -> &TrackId {
        &self.track_id
    }

    fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.insert_processor(processor);
    }

    fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.append_processor(processor);
    }

    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let rtcp_token_clone = token.clone();
        let this = Arc::new(self.clone());
        let rtcp_sender_task = this.clone().send_rtcp_reports(rtcp_token_clone);

        let rtp_token_clone = token.clone();
        let rtp_processor_task =
            this.clone()
                .process_rtp_packets(rtp_token_clone, event_sender.clone(), packet_sender);

        let track_id = self.track_id.clone();
        tokio::spawn(async move {
            event_sender
                .send(SessionEvent::TrackStart {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                })
                .ok();

            select! {
                _ = token.cancelled() => {},
                r = rtcp_sender_task => {
                    info!("RTCP sender process completed {:?}", r);
                }
                r = rtp_processor_task => {
                    info!("RTP processor completed {:?}", r);
                }
            };

            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id: track_id,
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
        if self.rtp_socket.is_none() {
            return Err(anyhow::anyhow!("rtptrack: Socket not initialized"));
        }

        let socket = self.rtp_socket.as_ref().unwrap();

        let payload = self
            .encoder
            .encode(self.rtp_config.payload_type, packet.clone());
        if payload.is_empty() {
            return Ok(());
        }

        // Update RTP sequence number and timestamp
        let seq = self.sequence_number.fetch_add(1, Ordering::Relaxed);
        let samples_per_packet =
            (self.config.sample_rate as f32 * self.config.ptime.as_secs_f32()) as u32;
        let ts = self
            .timestamp
            .fetch_add(samples_per_packet, Ordering::Relaxed);

        // Create RTP packet using rtp-rs
        let result = RtpPacketBuilder::new()
            .payload_type(self.rtp_config.payload_type)
            .ssrc(self.rtp_config.ssrc)
            .sequence(Seq::from(seq))
            .timestamp(ts)
            .payload(&payload)
            .build();

        if let Ok(rtp_data) = result {
            socket.send(&rtp_data).await?;

            // Update packet and octet counts for RTCP
            self.packet_count.fetch_add(1, Ordering::Relaxed);
            self.octet_count
                .fetch_add(payload.len() as u32, Ordering::Relaxed);
        } else {
            return Err(anyhow::anyhow!("Failed to build RTP packet"));
        }
        Ok(())
    }
}
