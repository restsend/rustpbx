//! Bounded sequence evidence at the plaintext input boundary, before media processing.

use serde::Serialize;
use std::net::SocketAddr;
use std::time::Instant;

// A fixed history bounds hot-path work and memory. Older arrivals cannot be
// distinguished from duplicates, and must not be reported as recovered loss.
const HISTORY_PACKETS: usize = 512;
// RFC 3550's dropout bound: a large jump is a discontinuity, not proof of loss.
const MAX_FORWARD_GAP: u16 = 3000;
// Source churn must not grow diagnostics without bound between log deadlines.
const MAX_PREVIOUS_STREAMS: usize = 8;

#[derive(Default, Serialize)]
pub(crate) struct IngressInterval {
    pub measurement: &'static str,
    pub direction: &'static str,
    pub sequence_scope: &'static str,
    pub interval_ms: u64,
    pub peer_addr: Option<SocketAddr>,
    pub ssrc: Option<u32>,
    pub last_payload_type: Option<u8>,
    pub packets: u64,
    pub audio_packets: u64,
    pub payload_bytes: u64,
    pub expected_sequence_packets: u64,
    pub new_gap_events: u64,
    pub new_missing_packets: u64,
    pub new_gap_percent: Option<f64>,
    pub reordered_packets: u64,
    pub recovered_packets: u64,
    pub duplicate_packets: u64,
    pub late_untracked_packets: u64,
    pub max_reorder_depth_packets: u16,
    pub current_stream_unresolved_gaps: u64,
    pub stream_changes: u64,
    pub sequence_discontinuities: u64,
    pub unresolved_gaps_at_reset: u64,
    pub history_packets: usize,
    pub highest_sequence: Option<u16>,
    pub interarrival_samples: u64,
    pub interarrival_avg_ms: Option<f64>,
    pub interarrival_max_ms: Option<f64>,
    pub last_audio_packet_age_ms: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub previous_streams: Vec<IngressInterval>,
    pub omitted_source_segments: u64,
    pub omitted_source_packets: u64,
    pub omitted_source_missing_packets: u64,
}

pub(crate) struct IngressStats {
    stream: Option<(u32, SocketAddr)>,
    highest: Option<u16>,
    seen: [u64; HISTORY_PACKETS / 64],
    history_len: usize,
    unresolved: u64,
    last_audio: Option<Instant>,
    start: Instant,
    interval: IngressInterval,
    interarrival_sum_ms: f64,
    previous_streams: Vec<IngressInterval>,
    omitted_source_segments: u64,
    omitted_source_packets: u64,
    omitted_source_missing_packets: u64,
}

impl IngressStats {
    pub fn new(now: Instant) -> Self {
        Self {
            stream: None,
            highest: None,
            seen: [0; HISTORY_PACKETS / 64],
            history_len: 0,
            unresolved: 0,
            last_audio: None,
            start: now,
            interval: IngressInterval::default(),
            interarrival_sum_ms: 0.0,
            previous_streams: Vec::with_capacity(MAX_PREVIOUS_STREAMS),
            omitted_source_segments: 0,
            omitted_source_packets: 0,
            omitted_source_missing_packets: 0,
        }
    }

    pub fn observe(
        &mut self,
        ssrc: u32,
        peer: SocketAddr,
        sequence: u16,
        payload_type: u8,
        payload_bytes: usize,
        audio: bool,
        now: Instant,
    ) {
        let key = (ssrc, peer);
        if self.stream != Some(key) {
            if self.stream.is_some() {
                // Preserve the old source before resetting: its gaps must never
                // be attributed to the new IP/SSRC in the periodic report.
                let mut previous = self.take_current_interval(now);
                previous.unresolved_gaps_at_reset += self.unresolved;
                if self.previous_streams.len() < MAX_PREVIOUS_STREAMS {
                    self.previous_streams.push(previous);
                } else {
                    self.omitted_source_segments += 1;
                    self.omitted_source_packets += previous.packets;
                    self.omitted_source_missing_packets += previous.new_missing_packets;
                }
                self.unresolved = 0;
                self.interval.stream_changes = 1;
            }
            self.reset_sequence();
            self.last_audio = None;
            self.stream = Some(key);
        }
        self.interval.packets += 1;
        self.interval.payload_bytes += payload_bytes as u64;
        self.interval.last_payload_type = Some(payload_type);
        if audio {
            self.interval.audio_packets += 1;
            if let Some(last) = self.last_audio {
                let gap = now.duration_since(last).as_secs_f64() * 1000.0;
                self.interarrival_sum_ms += gap;
                self.interval.interarrival_samples += 1;
                self.interval.interarrival_max_ms =
                    Some(self.interval.interarrival_max_ms.unwrap_or(0.0).max(gap));
            }
            self.last_audio = Some(now);
        }

        if let Some(highest) = self.highest {
            let forward = sequence.wrapping_sub(highest);
            if forward != 0 && forward < 32768 {
                if forward >= MAX_FORWARD_GAP {
                    self.interval.sequence_discontinuities += 1;
                    self.reset_sequence();
                } else {
                    self.interval.expected_sequence_packets += forward as u64;
                    if forward > 1 {
                        self.interval.new_gap_events += 1;
                        self.interval.new_missing_packets += (forward - 1) as u64;
                        self.unresolved += (forward - 1) as u64;
                    }
                    // Clear the reused ring slots, including holes, before marking
                    // the new high packet. Sequence wrap is normal RTP behavior.
                    for step in 1..=forward.min(HISTORY_PACKETS as u16) {
                        self.set_seen(highest.wrapping_add(step), false);
                    }
                    self.history_len = (self.history_len + forward as usize).min(HISTORY_PACKETS);
                    self.highest = Some(sequence);
                    self.set_seen(sequence, true);
                    return;
                }
            } else {
                let depth = highest.wrapping_sub(sequence);
                if depth as usize >= self.history_len {
                    self.interval.late_untracked_packets += 1;
                } else if self.is_seen(sequence) {
                    self.interval.duplicate_packets += 1;
                } else {
                    self.set_seen(sequence, true);
                    self.interval.reordered_packets += 1;
                    self.interval.recovered_packets += 1;
                    self.interval.max_reorder_depth_packets =
                        self.interval.max_reorder_depth_packets.max(depth);
                    self.unresolved = self.unresolved.saturating_sub(1);
                }
                return;
            }
        }
        self.highest = Some(sequence);
        self.history_len = 1;
        self.interval.expected_sequence_packets += 1;
        self.set_seen(sequence, true);
    }

    fn reset_sequence(&mut self) {
        self.interval.unresolved_gaps_at_reset += self.unresolved;
        self.unresolved = 0;
        self.highest = None;
        self.history_len = 0;
        self.seen.fill(0);
    }

    fn set_seen(&mut self, sequence: u16, received: bool) {
        let index = sequence as usize % HISTORY_PACKETS;
        let mask = 1u64 << (index % 64);
        if received {
            self.seen[index / 64] |= mask;
        } else {
            self.seen[index / 64] &= !mask;
        }
    }

    fn is_seen(&self, sequence: u16) -> bool {
        let index = sequence as usize % HISTORY_PACKETS;
        self.seen[index / 64] & (1u64 << (index % 64)) != 0
    }

    pub fn take_interval(&mut self, now: Instant) -> IngressInterval {
        let mut report = self.take_current_interval(now);
        if !self.previous_streams.is_empty() {
            report.previous_streams = std::mem::replace(
                &mut self.previous_streams,
                Vec::with_capacity(MAX_PREVIOUS_STREAMS),
            );
        }
        report.omitted_source_segments = std::mem::take(&mut self.omitted_source_segments);
        report.omitted_source_packets = std::mem::take(&mut self.omitted_source_packets);
        report.omitted_source_missing_packets =
            std::mem::take(&mut self.omitted_source_missing_packets);
        report
    }

    fn take_current_interval(&mut self, now: Instant) -> IngressInterval {
        let mut report = std::mem::take(&mut self.interval);
        report.measurement = "plaintext_rtp_before_forwarding_and_depacketization";
        report.direction = "ingress";
        report.sequence_scope = "negotiated_audio_and_telephone_event";
        report.interval_ms = now.duration_since(self.start).as_millis() as u64;
        report.peer_addr = self.stream.map(|(_, peer)| peer);
        report.ssrc = self.stream.map(|(ssrc, _)| ssrc);
        report.highest_sequence = self.highest;
        report.current_stream_unresolved_gaps = self.unresolved;
        report.history_packets = HISTORY_PACKETS;
        report.new_gap_percent = (report.expected_sequence_packets > 0).then(|| {
            report.new_missing_packets as f64 / report.expected_sequence_packets as f64 * 100.0
        });
        report.interarrival_avg_ms = (report.interarrival_samples > 0)
            .then(|| self.interarrival_sum_ms / report.interarrival_samples as f64);
        report.last_audio_packet_age_ms = self
            .last_audio
            .map(|last| now.duration_since(last).as_secs_f64() * 1000.0);
        self.interarrival_sum_ms = 0.0;
        self.start = now;
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn observe(stats: &mut IngressStats, sequence: u16, now: Instant) {
        stats.observe(
            42,
            "127.0.0.1:4000".parse().unwrap(),
            sequence,
            18,
            20,
            true,
            now,
        );
    }

    #[test]
    fn intervals_distinguish_gaps_recovery_duplicates_and_idle() {
        let now = Instant::now();
        let mut stats = IngressStats::new(now);
        observe(&mut stats, 100, now);
        observe(&mut stats, 103, now + Duration::from_millis(60));
        let first = stats.take_interval(now + Duration::from_millis(100));
        assert_eq!(
            (
                first.packets,
                first.expected_sequence_packets,
                first.new_missing_packets
            ),
            (2, 4, 2)
        );
        assert_eq!(first.new_gap_percent, Some(50.0));
        assert_eq!(first.interarrival_max_ms, Some(60.0));
        observe(&mut stats, 101, now + Duration::from_millis(120));
        observe(&mut stats, 101, now + Duration::from_millis(125));
        observe(&mut stats, 102, now + Duration::from_millis(130));
        let second = stats.take_interval(now + Duration::from_millis(200));
        assert_eq!(second.packets, 3);
        assert_eq!(
            (
                second.reordered_packets,
                second.recovered_packets,
                second.duplicate_packets
            ),
            (2, 2, 1)
        );
        assert_eq!(second.max_reorder_depth_packets, 2);
        assert_eq!(second.current_stream_unresolved_gaps, 0);
        assert_eq!(second.new_gap_percent, None);
        let idle = stats.take_interval(now + Duration::from_millis(300));
        assert_eq!(idle.interval_ms, 100);
        assert_eq!(idle.packets, 0);
        assert_eq!(idle.interarrival_max_ms, None);
        assert_eq!(idle.last_audio_packet_age_ms, Some(170.0));
    }

    #[test]
    fn wrap_and_bounded_history_do_not_invent_recovery() {
        let now = Instant::now();
        let mut stats = IngressStats::new(now);
        for sequence in [65534, 0, 65535, 1, 0] {
            observe(&mut stats, sequence, now);
        }
        let wrapped = stats.take_interval(now);
        assert_eq!(
            (
                wrapped.new_missing_packets,
                wrapped.recovered_packets,
                wrapped.duplicate_packets
            ),
            (1, 1, 1)
        );
        for sequence in 2..=600 {
            observe(&mut stats, sequence, now);
        }
        observe(&mut stats, 0, now);
        let old = stats.take_interval(now);
        assert_eq!(old.late_untracked_packets, 1);
        assert_eq!(old.recovered_packets, 0);
        assert_eq!(old.new_missing_packets, 0);
    }

    #[test]
    fn source_changes_and_large_jumps_are_not_mass_loss() {
        let now = Instant::now();
        let mut stats = IngressStats::new(now);
        observe(&mut stats, 100, now);
        observe(&mut stats, 102, now);
        observe(&mut stats, 10000, now);
        stats.observe(43, "127.0.0.1:4001".parse().unwrap(), 2, 0, 160, true, now);
        let report = stats.take_interval(now);
        assert_eq!(report.new_missing_packets, 0);
        assert_eq!(report.previous_streams[0].new_missing_packets, 1);
        assert_eq!(report.previous_streams[0].unresolved_gaps_at_reset, 1);
        assert_eq!(report.previous_streams[0].sequence_discontinuities, 1);
        assert_eq!(report.stream_changes, 1);
        assert_eq!(report.ssrc, Some(43));
        assert_eq!(report.current_stream_unresolved_gaps, 0);
    }

    #[test]
    fn source_switches_keep_gap_evidence_with_its_actual_source() {
        let now = Instant::now();
        let mut stats = IngressStats::new(now);
        let a = "192.0.2.1:4000".parse().unwrap();
        let b = "192.0.2.2:5000".parse().unwrap();
        stats.observe(1, a, 10, 18, 20, true, now);
        stats.observe(1, a, 13, 18, 20, true, now + Duration::from_millis(60));
        stats.observe(2, b, 100, 18, 20, true, now + Duration::from_millis(80));
        let report =
            serde_json::to_value(stats.take_interval(now + Duration::from_millis(100))).unwrap();
        assert_eq!(report["peer_addr"], "192.0.2.2:5000");
        assert_eq!(report["ssrc"], 2);
        assert_eq!(
            report["packets"], 1,
            "A's packets must not be attributed to B"
        );
        assert_eq!(report["new_missing_packets"], 0);
        let previous = &report["previous_streams"][0];
        assert_eq!(previous["peer_addr"], "192.0.2.1:4000");
        assert_eq!(previous["ssrc"], 1);
        assert_eq!(previous["packets"], 2);
        assert_eq!(previous["new_missing_packets"], 2);
        assert_eq!(previous["interarrival_max_ms"], 60.0);
        let next =
            serde_json::to_value(stats.take_interval(now + Duration::from_millis(120))).unwrap();
        assert!(next.get("previous_streams").is_none());
        assert_eq!(next["packets"], 0);
    }

    #[test]
    fn repeated_source_switches_bound_history_without_misattributing_overflow() {
        let now = Instant::now();
        let mut stats = IngressStats::new(now);
        for ssrc in 1..=20 {
            stats.observe(
                ssrc,
                "192.0.2.1:4000".parse().unwrap(),
                10,
                18,
                20,
                true,
                now,
            );
            stats.observe(
                ssrc,
                "192.0.2.1:4000".parse().unwrap(),
                13,
                18,
                20,
                true,
                now,
            );
        }
        let report = serde_json::to_value(stats.take_interval(now)).unwrap();
        let previous = report["previous_streams"].as_array().unwrap();
        assert_eq!(report["ssrc"], 20);
        assert_eq!(report["packets"], 2);
        assert!(
            previous.len() <= 8,
            "source churn must not grow the receive-side history"
        );
        assert_eq!(
            previous
                .iter()
                .map(|segment| segment["packets"].as_u64().unwrap())
                .sum::<u64>()
                + report["packets"].as_u64().unwrap()
                + report["omitted_source_packets"].as_u64().unwrap(),
            40
        );
        assert!(report["omitted_source_segments"].as_u64().unwrap() > 0);
        assert_eq!(report["omitted_source_missing_packets"], 22);
    }

    #[test]
    fn telephone_events_share_sequence_space_without_audio_timing() {
        let now = Instant::now();
        let mut stats = IngressStats::new(now);
        observe(&mut stats, 100, now);
        stats.observe(
            42,
            "127.0.0.1:4000".parse().unwrap(),
            101,
            101,
            4,
            false,
            now + Duration::from_millis(10),
        );
        observe(&mut stats, 102, now + Duration::from_millis(20));
        let report = stats.take_interval(now + Duration::from_millis(30));
        assert_eq!(
            (
                report.packets,
                report.audio_packets,
                report.new_missing_packets
            ),
            (3, 2, 0)
        );
        assert_eq!(report.interarrival_avg_ms, Some(20.0));
    }
}
