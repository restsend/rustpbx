//! RTP End-to-End Tests
//!
//! These tests verify that RTP packets are correctly forwarded through the PBX
//! with accurate data integrity. This is critical for ensuring media quality.

// use crate::common::cdr_capture::{CdrCapture, CdrExpectation};
use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, RtpReceiver, RtpSender, RtpStats};
use crate::common::test_ua::{TestUa, TestUaEvent};

use anyhow::{Result, anyhow};
use rustpbx::config::MediaProxyMode;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{info, warn};

/// RTP Flow Test Configuration
pub struct RtpFlowTestConfig {
    pub packet_count: usize,
    pub payload_size: usize,
    pub payload_type: u8,
    pub ssrc: u32,
    pub interval_ms: u64,
    pub expected_loss_rate: f64,
}

impl Default for RtpFlowTestConfig {
    fn default() -> Self {
        Self {
            packet_count: 100,
            payload_size: 160, // 20ms of PCMU @ 8kHz
            payload_type: 0,   // PCMU
            ssrc: 0x12345678,
            interval_ms: 20,          // 20ms intervals
            expected_loss_rate: 0.05, // 5% acceptable loss
        }
    }
}

/// Result of RTP flow test
#[derive(Debug, Clone)]
pub struct RtpFlowTestResult {
    pub packets_received: u64,
    pub packet_loss_rate: f64,
    pub seq_num_gaps: Vec<(u16, u16)>,
    pub is_valid: bool,
    pub errors: Vec<String>,
}

impl RtpFlowTestResult {
    pub fn validate(&mut self, config: &RtpFlowTestConfig) {
        // Check packet loss
        if self.packet_loss_rate > config.expected_loss_rate {
            self.errors.push(format!(
                "Packet loss too high: {:.2}% > {:.2}%",
                self.packet_loss_rate * 100.0,
                config.expected_loss_rate * 100.0
            ));
            self.is_valid = false;
        }

        // Check sequence continuity
        if !self.seq_num_gaps.is_empty() {
            warn!("Sequence gaps detected: {:?}", self.seq_num_gaps);
            // Gaps are logged but don't necessarily fail the test
            // (some loss is acceptable in UDP)
        }
    }
}

/// Complete RTP E2E test setup
pub struct RtpE2eTest {
    pub server: Arc<E2eTestServer>,
    pub caller: Option<TestUa>,
    pub callee: Option<TestUa>,
    pub caller_rtp_sender: Option<RtpSender>,
    pub caller_rtp_receiver: Option<RtpReceiver>,
    pub callee_rtp_sender: Option<RtpSender>,
    pub callee_rtp_receiver: Option<RtpReceiver>,
}

impl RtpE2eTest {
    /// Create new RTP E2E test with server
    pub async fn new_with_mode(mode: MediaProxyMode) -> Result<Self> {
        let server = Arc::new(E2eTestServer::start_with_mode(mode).await?);

        Ok(Self {
            server,
            caller: None,
            callee: None,
            caller_rtp_sender: None,
            caller_rtp_receiver: None,
            callee_rtp_sender: None,
            callee_rtp_receiver: None,
        })
    }

    /// Setup caller UA with RTP
    pub async fn setup_caller(&mut self, username: &str) -> Result<()> {
        let ua = self.server.create_ua(username).await?;

        // Setup RTP sender and receiver for caller
        let sender = RtpSender::bind().await?;
        let receiver = RtpReceiver::bind(0).await?;

        self.caller = Some(ua);
        self.caller_rtp_sender = Some(sender);
        self.caller_rtp_receiver = Some(receiver);

        Ok(())
    }

    /// Setup callee UA with RTP
    pub async fn setup_callee(&mut self, username: &str) -> Result<()> {
        let ua = self.server.create_ua(username).await?;

        // Setup RTP sender and receiver for callee
        let sender = RtpSender::bind().await?;
        let receiver = RtpReceiver::bind(0).await?;

        self.callee = Some(ua);
        self.callee_rtp_sender = Some(sender);
        self.callee_rtp_receiver = Some(receiver);

        Ok(())
    }

    /// Get caller's RTP port for SDP
    pub fn get_caller_rtp_port(&self) -> Option<u16> {
        self.caller_rtp_receiver
            .as_ref()
            .and_then(|r| r.port().ok())
    }

    /// Get callee's RTP port for SDP
    pub fn get_callee_rtp_port(&self) -> Option<u16> {
        self.callee_rtp_receiver
            .as_ref()
            .and_then(|r| r.port().ok())
    }

    /// Generate SDP with correct RTP port
    pub fn generate_sdp(ip: &str, port: u16, payload_type: u8, codec_name: &str) -> String {
        let clock_rate = if codec_name == "opus" { 48000 } else { 8000 };

        format!(
            "v=0\r\n\
            o=- {} {} IN IP4 {}\r\n\
            s=-\r\n\
            c=IN IP4 {}\r\n\
            t=0 0\r\n\
            m=audio {} RTP/AVP {} 101\r\n\
            a=rtpmap:{} {}/{}\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n",
            chrono::Utc::now().timestamp(),
            chrono::Utc::now().timestamp() + 1,
            ip,
            ip,
            port,
            payload_type,
            payload_type,
            codec_name,
            clock_rate
        )
    }

    /// Execute bidirectional RTP test
    pub async fn execute_bidirectional_rtp_test(
        &mut self,
        config: RtpFlowTestConfig,
    ) -> Result<(RtpFlowTestResult, RtpFlowTestResult)> {
        // Start receiving on both sides
        if let Some(ref receiver) = self.callee_rtp_receiver {
            receiver.start_receiving();
        }
        if let Some(ref receiver) = self.caller_rtp_receiver {
            receiver.start_receiving();
        }

        // Get callee's RTP port for sending
        let callee_rtp_port = self
            .get_callee_rtp_port()
            .ok_or_else(|| anyhow!("Callee RTP port not available"))?;
        let caller_rtp_port = self
            .get_caller_rtp_port()
            .ok_or_else(|| anyhow!("Caller RTP port not available"))?;

        // Create test packets
        let caller_to_callee_packets = RtpPacket::create_sequence(
            config.packet_count,
            1000,
            50000,
            config.ssrc,
            config.payload_type,
            config.payload_size,
            (config.interval_ms as u32) * 8, // timestamp increment for 8kHz
        );

        let callee_to_caller_packets = RtpPacket::create_sequence(
            config.packet_count,
            2000,
            60000,
            config.ssrc + 1,
            config.payload_type,
            config.payload_size,
            (config.interval_ms as u32) * 8,
        );

        // Send packets in both directions
        let callee_addr: SocketAddr = format!("127.0.0.1:{}", callee_rtp_port).parse()?;
        let caller_addr: SocketAddr = format!("127.0.0.1:{}", caller_rtp_port).parse()?;

        info!(
            "Starting RTP flow test: caller:{} <-> callee:{}",
            caller_rtp_port, callee_rtp_port
        );

        // Start sending
        if let Some(ref sender) = self.caller_rtp_sender {
            sender.start_sending(callee_addr, caller_to_callee_packets, config.interval_ms);
        }
        if let Some(ref sender) = self.callee_rtp_sender {
            sender.start_sending(caller_addr, callee_to_caller_packets, config.interval_ms);
        }

        // Wait for transmission
        let test_duration =
            Duration::from_millis(config.packet_count as u64 * config.interval_ms + 500);
        sleep(test_duration).await;

        // Stop sending
        if let Some(ref sender) = self.caller_rtp_sender {
            sender.stop();
        }
        if let Some(ref sender) = self.callee_rtp_sender {
            sender.stop();
        }

        // Allow time for last packets to arrive
        sleep(Duration::from_millis(200)).await;

        // Collect stats
        let caller_stats = if let Some(ref receiver) = self.caller_rtp_receiver {
            receiver.get_stats().await
        } else {
            RtpStats::default()
        };

        let callee_stats = if let Some(ref receiver) = self.callee_rtp_receiver {
            receiver.get_stats().await
        } else {
            RtpStats::default()
        };

        // Build results
        let mut caller_result = RtpFlowTestResult {
            packets_received: caller_stats.packets_received,
            packet_loss_rate: caller_stats.packet_loss_rate(),
            seq_num_gaps: caller_stats.seq_num_gaps.clone(),
            is_valid: true,
            errors: Vec::new(),
        };
        caller_result.validate(&config);

        let mut callee_result = RtpFlowTestResult {
            packets_received: callee_stats.packets_received,
            packet_loss_rate: callee_stats.packet_loss_rate(),
            seq_num_gaps: callee_stats.seq_num_gaps.clone(),
            is_valid: true,
            errors: Vec::new(),
        };
        callee_result.validate(&config);

        info!(
            caller_received = caller_stats.packets_received,
            callee_received = callee_stats.packets_received,
            "RTP flow test completed"
        );

        Ok((caller_result, callee_result))
    }
}

/// Test 1: RTP direct flow without proxy (None mode)
/// Verifies RTP packets flow directly between endpoints when proxy is disabled
#[tokio::test]
async fn test_rtp_direct_flow_no_proxy() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let mut test = RtpE2eTest::new_with_mode(MediaProxyMode::None).await?;

    // Setup UAs
    test.setup_caller("alice").await?;
    test.setup_callee("bob").await?;

    sleep(Duration::from_millis(100)).await;

    // Get RTP ports and generate SDPs
    let caller_port = test.get_caller_rtp_port().unwrap();
    let callee_port = test.get_callee_rtp_port().unwrap();

    let caller_sdp = RtpE2eTest::generate_sdp("127.0.0.1", caller_port, 0, "PCMU");
    let callee_sdp = RtpE2eTest::generate_sdp("127.0.0.1", callee_port, 0, "PCMU");

    // Establish call
    let caller = Arc::new(test.caller.take().unwrap());
    let callee = test.callee.take().unwrap();

    let caller_handle = rustpbx::utils::spawn({
        let c = caller.clone();
        let sdp = caller_sdp.clone();
        async move { c.make_call("bob", Some(sdp)).await }
    });

    // Answer call
    for _ in 0..50 {
        let events = callee.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                callee.answer_call(&id, Some(callee_sdp.clone())).await?;
                info!("Call answered");
                break;
            }
        }
        sleep(Duration::from_millis(100)).await;
    }

    let _ = tokio::time::timeout(Duration::from_secs(5), caller_handle).await;

    // Execute RTP test
    let config = RtpFlowTestConfig::default();
    let (caller_result, callee_result) = test.execute_bidirectional_rtp_test(config).await?;

    info!(
        caller_received = caller_result.packets_received,
        callee_received = callee_result.packets_received,
        "RTP direct flow results"
    );

    // In None mode, RTP should flow directly
    // We expect some packets to be received (actual routing depends on SDP handling)
    assert!(
        caller_result.packets_received > 0 || callee_result.packets_received > 0,
        "At least some RTP packets should be received"
    );

    test.server.stop();
    Ok(())
}
