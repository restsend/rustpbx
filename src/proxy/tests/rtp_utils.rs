//! RTP utilities for E2E testing
//!
//! Provides RTP packet construction, parsing, and validation for testing
//! media flow through the PBX.

use anyhow::{Result, anyhow};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

/// RTP Packet structure
#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub version: u8,      // 2 bits - should be 2
    pub padding: bool,    // 1 bit
    pub extension: bool,  // 1 bit
    pub csrc_count: u8,   // 4 bits
    pub marker: bool,     // 1 bit
    pub payload_type: u8, // 7 bits
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

impl RtpPacket {
    /// Create a new RTP packet
    pub fn new(
        payload_type: u8,
        sequence_number: u16,
        timestamp: u32,
        ssrc: u32,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            version: 2,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: false,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            payload,
        }
    }

    /// Encode RTP packet to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(12 + self.payload.len());

        // Byte 0: V(2) P(1) X(1) CC(4)
        let byte0 = (self.version << 6)
            | (if self.padding { 0x20 } else { 0 })
            | (if self.extension { 0x10 } else { 0 })
            | (self.csrc_count & 0x0F);
        data.push(byte0);

        // Byte 1: M(1) PT(7)
        let byte1 = (if self.marker { 0x80 } else { 0 }) | (self.payload_type & 0x7F);
        data.push(byte1);

        // Bytes 2-3: Sequence number
        data.extend_from_slice(&self.sequence_number.to_be_bytes());

        // Bytes 4-7: Timestamp
        data.extend_from_slice(&self.timestamp.to_be_bytes());

        // Bytes 8-11: SSRC
        data.extend_from_slice(&self.ssrc.to_be_bytes());

        // Payload
        data.extend_from_slice(&self.payload);

        data
    }

    /// Decode RTP packet from bytes
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 12 {
            return Err(anyhow!("RTP packet too short: {} bytes", data.len()));
        }

        let byte0 = data[0];
        let version = (byte0 >> 6) & 0x03;
        if version != 2 {
            return Err(anyhow!("Invalid RTP version: {}", version));
        }

        let padding = (byte0 & 0x20) != 0;
        let extension = (byte0 & 0x10) != 0;
        let csrc_count = byte0 & 0x0F;

        let byte1 = data[1];
        let marker = (byte1 & 0x80) != 0;
        let payload_type = byte1 & 0x7F;

        let sequence_number = u16::from_be_bytes([data[2], data[3]]);
        let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

        let payload_start = 12 + (csrc_count as usize * 4);
        let payload = if data.len() > payload_start {
            data[payload_start..].to_vec()
        } else {
            Vec::new()
        };

        Ok(Self {
            version,
            padding,
            extension,
            csrc_count,
            marker,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            payload,
        })
    }

    /// Create a sequence of test RTP packets
    pub fn create_sequence(
        count: usize,
        start_seq: u16,
        start_timestamp: u32,
        ssrc: u32,
        payload_type: u8,
        payload_size: usize,
        timestamp_increment: u32,
    ) -> Vec<Self> {
        (0..count)
            .map(|i| {
                let payload = vec![(i % 256) as u8; payload_size];
                Self::new(
                    payload_type,
                    start_seq.wrapping_add(i as u16),
                    start_timestamp.wrapping_add((i as u32) * timestamp_increment),
                    ssrc,
                    payload,
                )
            })
            .collect()
    }
}

/// RTP statistics for validation
#[derive(Debug, Default, Clone)]
pub struct RtpStats {
    pub packets_received: u64,
    pub bytes_received: u64,
    pub first_seq_num: Option<u16>,
    pub seq_num_gaps: Vec<(u16, u16)>, // (expected, actual) gaps
    pub payload_types: std::collections::HashSet<u8>,
    pub ssrcs: std::collections::HashSet<u32>,
    pub arrival_times: Vec<Instant>,
}

impl RtpStats {
    pub fn packet_loss_count(&self) -> u64 {
        self.seq_num_gaps
            .iter()
            .map(|(expected, actual)| (*actual as i32 - *expected as i32).max(0) as u64)
            .sum()
    }

    pub fn packet_loss_rate(&self) -> f64 {
        let expected = self.packets_received + self.packet_loss_count();
        if expected == 0 {
            0.0
        } else {
            self.packet_loss_count() as f64 / expected as f64
        }
    }
}

/// RTP packet receiver for testing
pub struct RtpReceiver {
    socket: Arc<UdpSocket>,
    stats: std::sync::Arc<tokio::sync::RwLock<RtpStats>>,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl RtpReceiver {
    pub async fn bind(port: u16) -> Result<Self> {
        let addr = format!("127.0.0.1:{}", port);
        let socket = Arc::new(UdpSocket::bind(&addr).await?);
        debug!("RTP receiver bound to {}", addr);

        Ok(Self {
            socket,
            stats: std::sync::Arc::new(tokio::sync::RwLock::new(RtpStats::default())),

            cancel_token: tokio_util::sync::CancellationToken::new(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    pub fn port(&self) -> Result<u16> {
        Ok(self.socket.local_addr()?.port())
    }

    /// Start receiving packets in background
    pub fn start_receiving(&self) {
        let socket = Arc::clone(&self.socket);
        let stats = self.stats.clone();
        let cancel_token = self.cancel_token.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            let mut last_seq: Option<u16> = None;

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!("RTP receiver cancelled");
                        break;
                    }
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((len, _from)) => {
                                if let Ok(packet) = RtpPacket::decode(&buf[..len]) {
                                    let mut s = stats.write().await;
                                    s.packets_received += 1;
                                    s.bytes_received += len as u64;
                                    s.payload_types.insert(packet.payload_type);
                                    s.ssrcs.insert(packet.ssrc);
                                    s.arrival_times.push(Instant::now());

                                    if s.first_seq_num.is_none() {
                                        s.first_seq_num = Some(packet.sequence_number);
                                    }

                                    // Check for gaps
                                    if let Some(last) = last_seq {
                                        let expected = last.wrapping_add(1);
                                        if packet.sequence_number != expected {
                                            s.seq_num_gaps.push((expected, packet.sequence_number));
                                        }
                                    }
                                    last_seq = Some(packet.sequence_number);
                                }
                            }
                            Err(e) => {
                                warn!("RTP receive error: {}", e);
                            }
                        }
                    }
                }
            }
        });
    }

    pub async fn get_stats(&self) -> RtpStats {
        self.stats.read().await.clone()
    }

    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

/// RTP packet sender for testing
pub struct RtpSender {
    socket: Arc<UdpSocket>,
    stats: std::sync::Arc<tokio::sync::RwLock<RtpStats>>,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl RtpSender {
    pub async fn bind() -> Result<Self> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        Ok(Self {
            socket,
            stats: std::sync::Arc::new(tokio::sync::RwLock::new(RtpStats::default())),
            cancel_token: tokio_util::sync::CancellationToken::new(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// Send a sequence of RTP packets to target address
    pub async fn send_sequence(
        &self,
        target: SocketAddr,
        packets: Vec<RtpPacket>,
        interval_ms: u64,
    ) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));

        for packet in packets {
            interval.tick().await;

            let data = packet.encode();
            self.socket.send_to(&data, target).await?;
        }

        Ok(())
    }

    /// Send packets in background
    pub fn start_sending(&self, target: SocketAddr, packets: Vec<RtpPacket>, interval_ms: u64) {
        let socket = Arc::clone(&self.socket);
        let cancel_token = self.cancel_token.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));

            for packet in packets {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!("RTP sender cancelled");
                        break;
                    }
                    _ = interval.tick() => {
                        let data = packet.encode();
                        if let Err(e) = socket.send_to(&data, target).await {
                            warn!("RTP send error: {}", e);
                        }
                    }
                }
            }
        });
    }

    pub async fn get_stats(&self) -> RtpStats {
        self.stats.read().await.clone()
    }

    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

/// Extract media endpoint from SDP
pub fn extract_media_endpoint(sdp: &str) -> Option<SocketAddr> {
    let mut connection_ip: Option<String> = None;
    let mut media_port: Option<u16> = None;

    for line in sdp.lines() {
        if line.starts_with("c=IN IP4 ") {
            connection_ip = line.strip_prefix("c=IN IP4 ").map(|s| s.to_string());
        } else if line.starts_with("m=audio ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                media_port = parts[1].parse().ok();
            }
        }
    }

    match (connection_ip, media_port) {
        (Some(ip), Some(port)) => format!("{}:{}", ip, port).parse().ok(),
        _ => None,
    }
}

/// Check if address is a proxy address
pub fn is_proxy_address(addr: &SocketAddr, proxy_ip: &str, proxy_ports: &[u16]) -> bool {
    addr.ip().to_string() == proxy_ip && proxy_ports.contains(&addr.port())
}

/// RTP validation result
#[derive(Debug, Clone)]
pub struct RtpValidationResult {
    pub is_valid: bool,
    pub errors: Vec<String>,
    pub stats: RtpStats,
}

impl RtpValidationResult {
    pub fn new(stats: RtpStats) -> Self {
        Self {
            is_valid: true,
            errors: Vec::new(),
            stats,
        }
    }

    pub fn add_error(&mut self, error: impl Into<String>) {
        self.errors.push(error.into());
        self.is_valid = false;
    }

    /// Validate that RTP stream is complete and correct
    pub fn validate_complete(&mut self, expected_packets: u64, expected_ssrc: u32) {
        // Check packet count
        if self.stats.packets_received < expected_packets {
            let msg = format!(
                "Packet loss: expected {} packets, received {}",
                expected_packets, self.stats.packets_received
            );
            self.errors.push(msg);
            self.is_valid = false;
        }

        // Check SSRC consistency
        if self.stats.ssrcs.len() != 1 {
            let msg = format!(
                "SSRC inconsistency: found {} different SSRCs",
                self.stats.ssrcs.len()
            );
            self.errors.push(msg);
            self.is_valid = false;
        } else if !self.stats.ssrcs.contains(&expected_ssrc) {
            let msg = format!(
                "SSRC mismatch: expected {}, got {:?}",
                expected_ssrc, self.stats.ssrcs
            );
            self.errors.push(msg);
            self.is_valid = false;
        }

        // Check for sequence gaps
        if !self.stats.seq_num_gaps.is_empty() {
            let msg = format!("Sequence gaps detected: {:?}", self.stats.seq_num_gaps);
            self.errors.push(msg);
            self.is_valid = false;
        }

        // Check payload type consistency
        if self.stats.payload_types.len() != 1 {
            let msg = format!(
                "Payload type inconsistency: found {} different types",
                self.stats.payload_types.len()
            );
            self.errors.push(msg);
            self.is_valid = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rtp_packet_encode_decode() {
        let packet = RtpPacket::new(
            0, // PCMU
            12345,
            987654321,
            0x12345678,
            vec![0xAB, 0xCD, 0xEF],
        );

        let encoded = packet.encode();
        let decoded = RtpPacket::decode(&encoded).unwrap();

        assert_eq!(decoded.version, 2);
        assert_eq!(decoded.payload_type, 0);
        assert_eq!(decoded.sequence_number, 12345);
        assert_eq!(decoded.timestamp, 987654321);
        assert_eq!(decoded.ssrc, 0x12345678);
        assert_eq!(decoded.payload, vec![0xAB, 0xCD, 0xEF]);
    }

    #[test]
    fn test_extract_media_endpoint() {
        let sdp = "v=0\r\n\
            o=- 123456 123456 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.100\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 0\r\n";

        let endpoint = extract_media_endpoint(sdp);
        assert_eq!(endpoint, Some("192.168.1.100:10000".parse().unwrap()));
    }

    #[test]
    fn test_create_sequence() {
        let packets = RtpPacket::create_sequence(
            10, 1000, 50000, 0xABCDEF01, 0, // PCMU
            160, 160, // 20ms at 8kHz
        );

        assert_eq!(packets.len(), 10);
        assert_eq!(packets[0].sequence_number, 1000);
        assert_eq!(packets[9].sequence_number, 1009);
        assert_eq!(packets[0].timestamp, 50000);
        assert_eq!(packets[9].timestamp, 50000 + 9 * 160);
    }
}
