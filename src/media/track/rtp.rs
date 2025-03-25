use crate::media::{
    jitter::JitterBuffer,
    processor::{AudioFrame, AudioPayload, Processor},
    stream::EventSender,
    track::{Track, TrackConfig, TrackId, TrackPacketSender},
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::StreamExt;
use parking_lot::Mutex;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, sync::mpsc};
use tokio_stream::wrappers::IntervalStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::TrackPacketReceiver;

// Simple RTP packet implementation based on RFC 3550
const RTP_VERSION: u8 = 2;

#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
    pub serialized: Vec<u8>, // Cached serialized form
}

impl RtpPacket {
    pub fn new(
        payload_type: u8,
        sequence_number: u16,
        timestamp: u32,
        ssrc: u32,
        payload: &[u8],
    ) -> Self {
        let mut packet = Self {
            version: RTP_VERSION,
            padding: false,
            extension: false,
            marker: false,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            payload: payload.to_vec(),
            serialized: Vec::new(),
        };

        // Serialize the packet
        packet.serialize();
        packet
    }

    fn serialize(&mut self) {
        let mut buffer = Vec::with_capacity(12 + self.payload.len());

        // First byte: [V V P X C C C C]
        let first_byte =
            (self.version << 6) | ((self.padding as u8) << 5) | ((self.extension as u8) << 4);
        buffer.push(first_byte);

        // Second byte: [M P P P P P P P]
        let second_byte = ((self.marker as u8) << 7) | (self.payload_type & 0x7F);
        buffer.push(second_byte);

        // Sequence number (16 bits)
        buffer.extend_from_slice(&self.sequence_number.to_be_bytes());

        // Timestamp (32 bits)
        buffer.extend_from_slice(&self.timestamp.to_be_bytes());

        // SSRC (32 bits)
        buffer.extend_from_slice(&self.ssrc.to_be_bytes());

        // Payload
        buffer.extend_from_slice(&self.payload);

        self.serialized = buffer;
    }

    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 12 {
            return Err(anyhow!("RTP packet too short"));
        }

        let version = data[0] >> 6;
        if version != RTP_VERSION {
            return Err(anyhow!("Unsupported RTP version"));
        }

        let padding = (data[0] & 0x20) != 0;
        let extension = (data[0] & 0x10) != 0;
        let marker = (data[1] & 0x80) != 0;
        let payload_type = data[1] & 0x7F;

        let sequence_number = u16::from_be_bytes([data[2], data[3]]);
        let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

        let payload = data[12..].to_vec();

        Ok(Self {
            version,
            padding,
            extension,
            marker,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            payload,
            serialized: data.to_vec(),
        })
    }
}

/// RTP session configuration
#[derive(Debug, Clone)]
pub struct RtpSessionConfig {
    pub local_addr: SocketAddr,
    pub remote_addr: Option<SocketAddr>,
    pub ssrc: u32,
    pub payload_type: u8,
    pub buffer_size: usize,
}

impl Default for RtpSessionConfig {
    fn default() -> Self {
        Self {
            local_addr: "0.0.0.0:0".parse().unwrap(), // Any local port
            remote_addr: None,
            ssrc: rand::random::<u32>(),
            payload_type: 0,   // PCMU by default
            buffer_size: 1500, // Standard MTU size
        }
    }
}

/// RTP session state manager
#[derive(Clone)]
pub struct RtpSession {
    socket: Arc<UdpSocket>,
    config: RtpSessionConfig,
    sequence_number: u16,
    timestamp: u32,
    payload_type_map: HashMap<u8, String>,
    started: bool,
}

impl RtpSession {
    pub async fn new(config: RtpSessionConfig) -> Result<Self> {
        // Bind to the local address
        let socket = UdpSocket::bind(&config.local_addr).await?;

        // Initialize common RTP payload types
        let mut payload_type_map = HashMap::new();
        payload_type_map.insert(0, "PCMU".to_string());
        payload_type_map.insert(8, "PCMA".to_string());
        payload_type_map.insert(9, "G722".to_string());

        Ok(Self {
            socket: Arc::new(socket),
            config,
            sequence_number: rand::random::<u16>(),
            timestamp: rand::random::<u32>(),
            payload_type_map,
            started: false,
        })
    }

    pub fn get_local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    pub fn set_remote_addr(&mut self, addr: SocketAddr) {
        self.config.remote_addr = Some(addr);
    }

    pub fn get_remote_addr(&self) -> Option<SocketAddr> {
        self.config.remote_addr
    }

    pub fn set_payload_type(&mut self, payload_type: u8) {
        self.config.payload_type = payload_type;
    }

    pub fn get_payload_type(&self) -> u8 {
        self.config.payload_type
    }

    pub fn set_ssrc(&mut self, ssrc: u32) {
        self.config.ssrc = ssrc;
    }

    pub fn get_ssrc(&self) -> u32 {
        self.config.ssrc
    }

    pub async fn send_rtp(&mut self, payload: &[u8], timestamp: Option<u32>) -> Result<()> {
        if let Some(remote_addr) = self.config.remote_addr {
            // Create RTP packet
            let timestamp_val = timestamp.unwrap_or_else(|| {
                let ts = self.timestamp;
                self.timestamp = self.timestamp.wrapping_add(160); // Increment by 160 samples (20ms at 8kHz)
                ts
            });

            let packet = RtpPacket::new(
                self.config.payload_type,
                self.sequence_number,
                timestamp_val,
                self.config.ssrc,
                payload,
            );

            // Increment sequence number
            self.sequence_number = self.sequence_number.wrapping_add(1);

            // Send the packet
            self.socket
                .send_to(&packet.serialized, &remote_addr)
                .await?;

            Ok(())
        } else {
            Err(anyhow!("Remote address not set"))
        }
    }

    pub async fn receive_rtp(&self) -> Result<(RtpPacket, SocketAddr)> {
        let mut buf = vec![0u8; self.config.buffer_size];
        let (size, addr) = self.socket.recv_from(&mut buf).await?;
        buf.truncate(size);

        let packet = RtpPacket::parse(&buf)?;
        Ok((packet, addr))
    }

    pub fn update_remote_from_packet(&mut self, addr: SocketAddr) {
        if self.config.remote_addr.is_none() {
            debug!("Setting remote address to {}", addr);
            self.config.remote_addr = Some(addr);
        }
    }

    pub fn start(&mut self) {
        self.started = true;
    }

    pub fn is_started(&self) -> bool {
        self.started
    }
}

/// RtpTrack handles RTP packet processing using a jitter buffer
/// It can be used for SIP/RTP media streams
pub struct RtpTrack {
    id: TrackId,
    config: TrackConfig,
    processors: Vec<Box<dyn Processor>>,
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    receiver: Arc<Mutex<Option<TrackPacketReceiver>>>,
    packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
    cancel_token: CancellationToken,
    rtp_session: Arc<Mutex<Option<RtpSession>>>,
    session_config: RtpSessionConfig,
}

impl RtpTrack {
    pub fn new(id: TrackId) -> Self {
        let config = TrackConfig::default().with_sample_rate(8000); // Default to 8kHz for RTP

        Self {
            id,
            config: config.clone(),
            processors: Vec::new(),
            jitter_buffer: Arc::new(Mutex::new(JitterBuffer::new(config.sample_rate))),
            receiver: Arc::new(Mutex::new(None)),
            packet_sender: Arc::new(Mutex::new(None)),
            cancel_token: CancellationToken::new(),
            rtp_session: Arc::new(Mutex::new(None)),
            session_config: RtpSessionConfig::default(),
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config.clone();

        // Update jitter buffer with new sample rate
        {
            let mut jitter_buffer = self.jitter_buffer.lock();
            *jitter_buffer = JitterBuffer::new(config.sample_rate);
        }

        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);

        // Update jitter buffer with new sample rate
        {
            let mut jitter_buffer = self.jitter_buffer.lock();
            *jitter_buffer = JitterBuffer::new(sample_rate);
        }

        self
    }

    pub fn with_rtp_session_config(mut self, session_config: RtpSessionConfig) -> Self {
        self.session_config = session_config;
        self
    }

    pub async fn get_local_addr(&self) -> Result<Option<SocketAddr>> {
        if let Some(session) = self.rtp_session.lock().as_ref() {
            Ok(Some(session.get_local_addr()?))
        } else {
            Ok(None)
        }
    }

    pub fn set_remote_addr(&self, addr: SocketAddr) -> Result<()> {
        if let Some(ref mut session) = *self.rtp_session.lock() {
            session.set_remote_addr(addr);
            Ok(())
        } else {
            Err(anyhow!("RTP session not initialized"))
        }
    }

    async fn start_rtp_session(&self) -> Result<()> {
        // Initialize RTP session
        let session = RtpSession::new(self.session_config.clone()).await?;
        let local_addr = session.get_local_addr()?;
        info!("RTP session started on {}", local_addr);

        {
            let mut session_guard = self.rtp_session.lock();
            *session_guard = Some(session);
        }

        Ok(())
    }

    async fn rtp_receiver_task(&self, token: CancellationToken) -> Result<()> {
        // Clone required data for the task
        let session_ref = self.rtp_session.clone();
        let jitter_buffer = self.jitter_buffer.clone();
        let track_id = self.id.clone();
        let sample_rate = self.config.sample_rate;

        tokio::spawn(async move {
            debug!("Starting RTP receiver task");

            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        debug!("RTP receiver task cancelled");
                        break;
                    }
                    result = async {
                        // Get a cloned socket from the session if available
                        let socket_and_buffer_size = {
                            let session_opt = session_ref.lock();
                            if let Some(ref session) = *session_opt {
                                Some((Arc::clone(&session.socket), session.config.buffer_size))
                            } else {
                                None
                            }
                        };

                        if let Some((socket, buffer_size)) = socket_and_buffer_size {
                            // Now we have the socket without holding the mutex
                            let mut buf = vec![0u8; buffer_size];
                            let (size, addr) = socket.recv_from(&mut buf).await?;
                            buf.truncate(size);

                            let packet = RtpPacket::parse(&buf)?;
                            return Ok((packet, addr));
                        }
                        Err(anyhow!("RTP session not available"))
                    } => {
                        match result {
                            Ok((packet, addr)) => {
                                // Update remote address if needed
                                {
                                    let mut session_opt = session_ref.lock();
                                    if let Some(ref mut session) = *session_opt {
                                        session.update_remote_from_packet(addr);
                                    }
                                }

                                // Extract packet data
                                let payload_type = packet.payload_type;
                                let timestamp = packet.timestamp;
                                let payload = packet.payload;

                                debug!(
                                    "Received RTP packet: pt={}, ts={}, size={}",
                                    payload_type, timestamp, payload.len()
                                );

                                // Create an AudioFrame
                                let frame = AudioFrame {
                                    track_id: track_id.clone(),
                                    timestamp,
                                    samples: AudioPayload::RTP(payload_type, payload),
                                    sample_rate: sample_rate as u16,
                                };

                                // Add to jitter buffer
                                let mut jitter = jitter_buffer.lock();
                                jitter.push(frame);
                            }
                            Err(e) => {
                                debug!("Error receiving RTP packet: {}", e);
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    async fn rtp_sender_task(&self, token: CancellationToken) -> Result<()> {
        // No need to clone session_ref since we're not using it in this task

        tokio::spawn(async move {
            debug!("Starting RTP sender task");

            // Wait a bit for RTP session to initialize
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            loop {
                if token.is_cancelled() {
                    debug!("RTP sender task cancelled");
                    break;
                }

                // Sleep to avoid busy waiting
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        });

        Ok(())
    }

    async fn start_jitter_processing(
        &self,
        token: CancellationToken,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Create a task to process frames from the jitter buffer
        let jitter_buffer = self.jitter_buffer.clone();
        let sample_rate = self.config.sample_rate;
        let ptime_ms = self.config.ptime.as_millis() as u32;

        // Use ptime from config for the processing interval
        let interval = tokio::time::interval(self.config.ptime);
        let mut interval_stream = IntervalStream::new(interval);

        tokio::spawn(async move {
            debug!(
                "Started RTP jitter buffer processing with ptime: {}ms",
                ptime_ms
            );

            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        debug!("RTP jitter processing task cancelled");
                        break;
                    }
                    _ = interval_stream.next() => {
                        // Get frames from jitter buffer
                        let frames = {
                            let mut jitter = jitter_buffer.lock();
                            jitter.pull_frames(ptime_ms, sample_rate)
                        };

                        if frames.is_empty() {
                            continue;
                        }

                        for frame in frames {
                            // Forward the frame to any connected tracks
                            let _ = packet_sender.send(frame);
                        }
                    }
                }
            }
        });

        Ok(())
    }

    // Send RTP packet using the RTP session
    pub async fn send_rtp_packet(
        &self,
        payload_type: u8,
        payload: &[u8],
        timestamp: Option<u32>,
    ) -> Result<()> {
        // Don't need to create a session_clone, just check if session exists
        {
            let session_opt = self.rtp_session.lock();
            if let Some(ref session) = *session_opt {
                // Set payload type if different from the current one
                if payload_type != session.get_payload_type() {
                    // We can't modify the session here as we would need &mut
                    // But we'll update it below right before sending
                }
            } else {
                return Err(anyhow!("RTP session not initialized"));
            }
        }

        // Get the remote address
        let remote_addr = {
            let session_guard = self.rtp_session.lock();
            if let Some(ref session) = *session_guard {
                session.get_remote_addr()
            } else {
                None
            }
        };

        if let Some(addr) = remote_addr {
            // Now prepare the packet data without holding the mutex
            let packet_data = {
                let mut session_guard = self.rtp_session.lock();
                if let Some(ref mut session) = *session_guard {
                    // Now we can update the payload type
                    if payload_type != session.get_payload_type() {
                        session.set_payload_type(payload_type);
                    }

                    let ts = timestamp.unwrap_or_else(|| {
                        let ts = session.timestamp;
                        session.timestamp = session.timestamp.wrapping_add(160);
                        ts
                    });

                    let packet = RtpPacket::new(
                        session.get_payload_type(),
                        session.sequence_number,
                        ts,
                        session.config.ssrc,
                        payload,
                    );

                    // Increment sequence number
                    session.sequence_number = session.sequence_number.wrapping_add(1);

                    packet.serialized.clone()
                } else {
                    return Err(anyhow!("RTP session not available"));
                }
            };

            // Now we can safely send without holding the mutex
            let socket = {
                let session_guard = self.rtp_session.lock();
                if let Some(ref session) = *session_guard {
                    Arc::clone(&session.socket)
                } else {
                    return Err(anyhow!("RTP session not available"));
                }
            };

            // Send the packet
            socket.send_to(&packet_data, &addr).await?;

            Ok(())
        } else {
            Err(anyhow!("Remote address not set"))
        }
    }
}

#[async_trait]
impl Track for RtpTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }

    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>) {
        self.processors.extend(processors);
    }

    fn processors(&self) -> Vec<&dyn Processor> {
        self.processors
            .iter()
            .map(|p| p.as_ref() as &dyn Processor)
            .collect()
    }

    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Save packet sender for later use
        {
            let mut sender_guard = self.packet_sender.lock();
            *sender_guard = Some(packet_sender.clone());
        }

        // Create a channel for receiving packets
        let (_receiver_sender, receiver) = mpsc::unbounded_channel();

        // Store the receiver in self
        {
            let mut receiver_guard = self.receiver.lock();
            *receiver_guard = Some(receiver);
        }

        // Initialize RTP session
        self.start_rtp_session().await?;

        // Start RTP receiver task
        self.rtp_receiver_task(token.child_token()).await?;

        // Start RTP sender task
        self.rtp_sender_task(token.child_token()).await?;

        // Signal that the track is ready
        let _ = event_sender.send(crate::media::stream::MediaStreamEvent::TrackStart(
            self.id.clone(),
        ));

        // Start jitter buffer processing
        self.start_jitter_processing(token.clone(), packet_sender)
            .await?;

        // Clone token for the task
        let token_clone = token.clone();
        let event_sender_clone = event_sender.clone();
        let track_id = self.id.clone();

        // Start a task to watch for cancellation
        tokio::spawn(async move {
            token_clone.cancelled().await;
            let _ = event_sender_clone
                .send(crate::media::stream::MediaStreamEvent::TrackStop(track_id));
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Cancel all processing
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        let mut frame = packet.clone();

        // Process the frame with all processors
        for processor in &self.processors {
            let _ = processor.process_frame(&mut frame);
        }

        // For RTP packets, try to send them directly via RTP session if possible
        if let AudioPayload::RTP(payload_type, ref payload) = frame.samples {
            // Create a block scope to ensure mutex guard is dropped before await
            let can_send_rtp = {
                let session_opt = self.rtp_session.lock();
                if let Some(ref session) = *session_opt {
                    session.is_started() && session.get_remote_addr().is_some()
                } else {
                    false
                }
            };

            if can_send_rtp {
                // Try to send the packet via RTP
                let timestamp = frame.timestamp;
                return self
                    .send_rtp_packet(payload_type, payload, Some(timestamp))
                    .await;
            }
        }

        // Add processed frame to jitter buffer for any packet format
        let mut jitter_buffer = self.jitter_buffer.lock();
        jitter_buffer.push(frame);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::processor::{AudioFrame, AudioPayload};
    use anyhow::Result;
    use std::sync::{Arc, Mutex};
    use tokio::sync::broadcast;
    use tokio::time::Duration;
    use tokio_util::sync::CancellationToken;

    // Simple test processor that counts frames
    struct CountingProcessor {
        count: Arc<Mutex<usize>>,
    }

    impl CountingProcessor {
        fn new() -> (Self, Arc<Mutex<usize>>) {
            let count = Arc::new(Mutex::new(0));
            (
                Self {
                    count: count.clone(),
                },
                count,
            )
        }
    }

    impl Processor for CountingProcessor {
        fn process_frame(&self, _frame: &mut AudioFrame) -> Result<()> {
            let mut count = self.count.lock().unwrap();
            *count += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_rtp_track_pcm() -> Result<()> {
        // Create an RTP track
        let track_id = "test_rtp_track".to_string();
        let mut rtp_track = RtpTrack::new(track_id.clone());

        // Create a processor
        let (processor, count) = CountingProcessor::new();
        rtp_track.with_processors(vec![Box::new(processor)]);

        // Create channels
        let (event_sender, _) = broadcast::channel(16);
        let (packet_sender, mut packet_receiver) = mpsc::unbounded_channel();

        // Start the track
        let token = CancellationToken::new();
        rtp_track
            .start(token.clone(), event_sender, packet_sender)
            .await?;

        // Create a PCM packet
        let pcm_data: Vec<i16> = (0..160)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let pcm_packet = AudioFrame {
            track_id: track_id.clone(),
            timestamp: 1000,
            samples: AudioPayload::PCM(pcm_data),
            sample_rate: 8000,
        };

        // Send the packet to the track
        rtp_track.send_packet(&pcm_packet).await?;

        // Wait for the packet to be processed (it should be stored in the jitter buffer)
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Check if processor was called
        {
            let processor_count = *count.lock().unwrap();
            assert!(
                processor_count > 0,
                "Processor should have been called at least once"
            );
        }

        // Verify the packet was received
        let received =
            tokio::time::timeout(Duration::from_millis(100), packet_receiver.recv()).await;
        assert!(received.is_ok(), "Should have received a packet");

        // Stop the track
        rtp_track.stop().await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_rtp_track_rtp() -> Result<()> {
        // Create an RTP track
        let track_id = "test_rtp_track".to_string();
        let rtp_track = RtpTrack::new(track_id.clone());

        // Create channels
        let (event_sender, _) = broadcast::channel(16);
        let (packet_sender, mut packet_receiver) = mpsc::unbounded_channel();

        // Start the track
        let token = CancellationToken::new();
        rtp_track
            .start(token.clone(), event_sender, packet_sender)
            .await?;

        // Create an RTP packet (simplified)
        let rtp_data = vec![0u8; 172]; // Typical RTP packet size for PCMU audio
        let rtp_packet = AudioFrame {
            track_id: track_id.clone(),
            timestamp: 1000,
            samples: AudioPayload::RTP(0, rtp_data), // 0 is PCMU
            sample_rate: 8000,
        };

        // Send the packet to the track
        rtp_track.send_packet(&rtp_packet).await?;

        // Wait for processing to occur
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Verify the packet was received
        let received =
            tokio::time::timeout(Duration::from_millis(100), packet_receiver.recv()).await;
        assert!(received.is_ok(), "Should have received a packet");

        // Stop the track
        rtp_track.stop().await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_rtp_packet() -> Result<()> {
        // Create a sample RTP packet
        let payload = vec![1, 2, 3, 4, 5];
        let packet = RtpPacket::new(
            0, // PCMU
            12345, 67890, 123456789, &payload,
        );

        // Verify the packet fields
        assert_eq!(packet.version, 2);
        assert_eq!(packet.padding, false);
        assert_eq!(packet.extension, false);
        assert_eq!(packet.marker, false);
        assert_eq!(packet.payload_type, 0);
        assert_eq!(packet.sequence_number, 12345);
        assert_eq!(packet.timestamp, 67890);
        assert_eq!(packet.ssrc, 123456789);
        assert_eq!(packet.payload, payload);

        // Serialized packet should be at least 12 bytes (header) + payload length
        assert_eq!(packet.serialized.len(), 12 + payload.len());

        // Try parsing the serialized data
        let parsed = RtpPacket::parse(&packet.serialized)?;

        // Check that the parsed packet matches the original
        assert_eq!(parsed.version, packet.version);
        assert_eq!(parsed.padding, packet.padding);
        assert_eq!(parsed.extension, packet.extension);
        assert_eq!(parsed.marker, packet.marker);
        assert_eq!(parsed.payload_type, packet.payload_type);
        assert_eq!(parsed.sequence_number, packet.sequence_number);
        assert_eq!(parsed.timestamp, packet.timestamp);
        assert_eq!(parsed.ssrc, packet.ssrc);
        assert_eq!(parsed.payload, packet.payload);

        Ok(())
    }

    #[tokio::test]
    async fn test_rtp_session() -> Result<()> {
        // Create an RTP session configuration
        let config = RtpSessionConfig {
            local_addr: "127.0.0.1:0".parse().unwrap(), // Any port on localhost
            remote_addr: None,
            ssrc: 12345,
            payload_type: 0, // PCMU
            buffer_size: 1500,
        };

        // Create the session
        let mut session = RtpSession::new(config).await?;
        let local_addr = session.get_local_addr()?;

        // Set remote addr to the local addr for testing (loopback)
        session.set_remote_addr(local_addr);

        // Send a packet
        let payload = vec![0u8; 160]; // 20ms of PCMU audio
        session.send_rtp(&payload, Some(1000)).await?;

        // Session should be created and packet sent without errors
        assert!(session.get_remote_addr().is_some());

        Ok(())
    }
}
