//! Per-leg RTCP-derived media quality (jitter / RTT / fraction lost) plus the
//! remote Sender Report packet count used to estimate receive-direction loss.
//!
//! This re-introduces what the pre-refactor `bridge.rs` captured with
//! `DirectionRtcpStats` / `spawn_sender_rtcp_listener` / `SrTimeTracker`: the
//! refactor_media rewrite dropped RTCP stats entirely (the RTCP relay only
//! forwarded PLI/NACK). `RtpSender::subscribe_rtcp()` is a broadcast channel,
//! so a stats listener can coexist with the relay forwarder.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use rustrtc::rtp::{ReportBlock, RtcpPacket};
use rustrtc::{RtpSender, RtpSenderInterceptor};

/// Snapshot of RTCP-derived quality for one leg.
#[derive(Debug, Clone, Default)]
pub struct LegRtcpSnapshot {
    /// Latest jitter in microseconds (0 = unknown).
    pub jitter_us: u64,
    /// Latest round-trip time in microseconds (0 = unknown).
    pub rtt_us: u64,
    /// Latest fraction lost (0..=255, where 255 = 100%).
    pub fraction_lost: u8,
    /// Latest cumulative packet count reported by the remote Sender Report.
    /// Zero when no SR has been observed.
    pub sr_packet_count: u64,
    /// The remote SSRC the latest Sender Report describes.
    pub sr_ssrc: u32,
    /// Whether a remote Sender Report has been observed at all.
    pub has_sr: bool,
}

impl LegRtcpSnapshot {
    pub fn jitter_ms(&self) -> Option<f64> {
        if self.jitter_us == 0 {
            None
        } else {
            Some(self.jitter_us as f64 / 1000.0)
        }
    }

    pub fn rtt_ms(&self) -> Option<f64> {
        if self.rtt_us == 0 {
            None
        } else {
            Some(self.rtt_us as f64 / 1000.0)
        }
    }

    /// Fraction lost as a percentage (0.0 ..= 100.0).
    pub fn loss_pct(&self) -> f64 {
        self.fraction_lost as f64 / 255.0 * 100.0
    }
}

/// Per-leg media quality report, captured at call end for the call record.
/// Plain (non-serde) so this crate needs no extra derives; the host serializes.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegQualityReport {
    /// Which side of the bridge ("A" = caller, "B" = callee).
    pub side: &'static str,
    /// Negotiated audio codec name (e.g. "PCMU"), when known.
    pub codec: Option<String>,
    /// Plaintext packets received from the remote peer (ingress tap).
    pub ingress_packets: u64,
    /// Plaintext packets sent to the remote peer (egress tap).
    pub egress_packets: u64,
    /// Transport-level inbound RTP packets (post-SRTP).
    pub transport_rx_packets: u64,
    /// Latest RTCP jitter in microseconds (0 = unknown).
    pub jitter_us: u64,
    /// Latest RTCP round-trip time in microseconds (0 = unknown).
    pub rtt_us: u64,
    /// Latest RTCP fraction lost as a percentage (0..=100).
    pub loss_pct: f64,
}

/// Live RTCP stats for one leg, updated lock-free by a background listener.
pub struct LegRtcpStats {
    jitter_us: AtomicU64,
    rtt_us: AtomicU64,
    fraction_lost: AtomicU8,
    sr_packet_count: AtomicU64,
    /// The remote SSRC the latest Sender Report describes (the stream we
    /// receive). Lets the stats task tell audio vs video SRs apart.
    sr_ssrc: AtomicU32,
    has_sr: std::sync::atomic::AtomicBool,
}

impl Default for LegRtcpStats {
    fn default() -> Self {
        Self {
            jitter_us: AtomicU64::new(0),
            rtt_us: AtomicU64::new(0),
            fraction_lost: AtomicU8::new(0),
            sr_packet_count: AtomicU64::new(0),
            sr_ssrc: AtomicU32::new(0),
            has_sr: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl LegRtcpStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            jitter_us: AtomicU64::new(0),
            rtt_us: AtomicU64::new(0),
            fraction_lost: AtomicU8::new(0),
            sr_packet_count: AtomicU64::new(0),
            sr_ssrc: AtomicU32::new(0),
            has_sr: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn snapshot(&self) -> LegRtcpSnapshot {
        LegRtcpSnapshot {
            jitter_us: self.jitter_us.load(Ordering::Relaxed),
            rtt_us: self.rtt_us.load(Ordering::Relaxed),
            fraction_lost: self.fraction_lost.load(Ordering::Relaxed),
            sr_packet_count: self.sr_packet_count.load(Ordering::Relaxed),
            sr_ssrc: self.sr_ssrc.load(Ordering::Relaxed),
            has_sr: self.has_sr.load(Ordering::Relaxed),
        }
    }
}

/// Tracks SR send timestamps so RTT can be computed from incoming Receiver
/// Reports (LSR/DLSR fields per RFC 3550 §6.4.1). Injected as a sender
/// interceptor on each PeerConnection; shared state is referenced by the
/// per-leg RTCP listener task.
pub struct SrTimeTracker {
    /// SR send-time map, shared with the RTCP listener for RTT computation.
    pub times: Arc<parking_lot::Mutex<std::collections::HashMap<u32, std::time::Instant>>>,
}

impl SrTimeTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            times: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        })
    }
}

impl RtpSenderInterceptor for SrTimeTracker {
    fn on_sr_sent(&self, _ssrc: u32, ntp_least: u32) {
        self.times
            .lock()
            .insert(ntp_least, std::time::Instant::now());
    }
}

/// Extract jitter, fraction-lost and RTT from a slice of ReportBlocks and
/// update the shared [`LegRtcpStats`] atomics.
fn update_from_report_blocks(
    blocks: &[ReportBlock],
    ssrc: u32,
    stats: &LegRtcpStats,
    sr_times: &parking_lot::Mutex<std::collections::HashMap<u32, std::time::Instant>>,
    clock_rate: u32,
) {
    for block in blocks {
        if block.ssrc != ssrc {
            continue;
        }
        if block.jitter != 0 && clock_rate != 0 {
            let jitter_us = block.jitter as u64 * 1_000_000 / clock_rate as u64;
            stats.jitter_us.store(jitter_us, Ordering::Relaxed);
        }
        stats
            .fraction_lost
            .store(block.fraction_lost, Ordering::Relaxed);
        // RTT = now − SR_sent_time − DLSR  (RFC 3550 §6.4.1)
        if block.last_sender_report != 0 {
            let times = sr_times.lock();
            if let Some(&sent_instant) = times.get(&block.last_sender_report) {
                let dlsr = block.delay_since_last_sender_report as f64 / 65536.0;
                let rtt = sent_instant.elapsed().as_secs_f64() - dlsr;
                if rtt > 0.0 {
                    stats
                        .rtt_us
                        .store((rtt * 1_000_000.0) as u64, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Spawn a task that subscribes to an `RtpSender`'s RTCP channel and extracts
/// Receiver/Sender Report statistics (jitter, fraction lost, RTT, remote SR
/// packet count) into the shared [`LegRtcpStats`].
///
/// The broadcast channel closes when the sender (and its PeerConnection) is
/// dropped, so the task exits naturally at leg teardown. The caller stores the
/// returned handle and aborts it on `stop` for prompt shutdown.
pub fn spawn_rtcp_listener(
    sender: Arc<RtpSender>,
    stats: Arc<LegRtcpStats>,
    sr_times: Arc<parking_lot::Mutex<std::collections::HashMap<u32, std::time::Instant>>>,
    clock_rate: u32,
) -> tokio::task::JoinHandle<()> {
    let ssrc = sender.ssrc();
    let mut rx = sender.subscribe_rtcp();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(RtcpPacket::ReceiverReport(rr)) => {
                    update_from_report_blocks(
                        &rr.report_blocks,
                        ssrc,
                        &stats,
                        &sr_times,
                        clock_rate,
                    );
                }
                Ok(RtcpPacket::SenderReport(sr)) => {
                    // The remote's SR describes the stream it sends US (the
                    // receive direction): `sender_ssrc` is the REMOTE ssrc, not
                    // ours. Record the latest cumulative packet count so the
                    // stats task can estimate receive-direction loss vs.
                    // transport RX. (Audio-only calls have a single SR; for
                    // audio+video the latest SR wins, which makes the rx-loss
                    // estimate approximate — see stats task doc.)
                    stats.sr_ssrc.store(sr.sender_ssrc, Ordering::Relaxed);
                    stats
                        .sr_packet_count
                        .store(sr.packet_count as u64, Ordering::Relaxed);
                    stats.has_sr.store(true, Ordering::Relaxed);
                    update_from_report_blocks(
                        &sr.report_blocks,
                        ssrc,
                        &stats,
                        &sr_times,
                        clock_rate,
                    );
                }
                Err(_) => break,
                _ => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_formats_cleanly() {
        let stats = LegRtcpStats::new();
        let snap = stats.snapshot();
        assert_eq!(snap.jitter_ms(), None);
        assert_eq!(snap.rtt_ms(), None);
        assert_eq!(snap.loss_pct(), 0.0);
        assert!(!snap.has_sr);
    }

    #[test]
    fn report_blocks_update_atomics() {
        let stats = LegRtcpStats::new();
        let sr_times = SrTimeTracker::new();
        let blocks = vec![ReportBlock {
            ssrc: 1001,
            fraction_lost: 40,
            packets_lost: 10,
            highest_sequence: 5000,
            jitter: 1600,
            last_sender_report: 0,
            delay_since_last_sender_report: 0,
        }];
        update_from_report_blocks(&blocks, 1001, &stats, &sr_times.times.clone(), 8000);
        let snap = stats.snapshot();
        assert_eq!(snap.jitter_us, 200_000); // 1600 * 1e6 / 8000
        assert_eq!(snap.fraction_lost, 40);
        assert!((snap.loss_pct() - 40.0 / 255.0 * 100.0).abs() < 0.01);
        // Unrelated SSRC must not update.
        let blocks2 = vec![ReportBlock {
            ssrc: 9999,
            fraction_lost: 200,
            packets_lost: 1,
            highest_sequence: 1,
            jitter: 0,
            last_sender_report: 0,
            delay_since_last_sender_report: 0,
        }];
        update_from_report_blocks(&blocks2, 1001, &stats, &sr_times.times.clone(), 8000);
        assert_eq!(stats.snapshot().fraction_lost, 40);
    }

    #[test]
    fn rtt_computed_from_lsr() {
        let stats = LegRtcpStats::new();
        let sr_times = SrTimeTracker::new();
        sr_times.times.lock().insert(42, std::time::Instant::now());
        let blocks = vec![ReportBlock {
            ssrc: 1001,
            fraction_lost: 0,
            packets_lost: 0,
            highest_sequence: 1,
            jitter: 0,
            last_sender_report: 42,
            delay_since_last_sender_report: 0,
        }];
        update_from_report_blocks(&blocks, 1001, &stats, &sr_times.times.clone(), 8000);
        assert!(stats.snapshot().rtt_us > 0);
    }
}
