use crate::sipflow::SipFlowMediaStats;

pub const RTP_REORDER_WINDOW: u16 = 64;

#[derive(Debug, Clone, Copy)]
pub struct RtpStatsHeader {
    pub payload_type: u8,
    pub sequence_number: u16,
    pub rtp_timestamp: u32,
    pub ssrc: u32,
}

#[derive(Default)]
pub struct MediaStatsAccumulator {
    pub leg: i32,
    pub src: String,
    pub packet_count: usize,
    pub lost_packets: u64,
    pub ssrc: Option<u32>,
    pub payload_type: Option<u8>,
    pub clock_rate: Option<u32>,
    pub first_sequence: Option<u16>,
    pub last_sequence: Option<u16>,
    pub pending_missing_sequences: std::collections::BTreeSet<u16>,
    pub prev_arrival_micros: Option<u64>,
    pub prev_rtp_timestamp: Option<u32>,
    pub jitter_rtp_units: f64,
    pub jitter_samples: u64,
}

impl MediaStatsAccumulator {
    pub fn new(leg: i32, src: String, ssrc: Option<u32>) -> Self {
        Self {
            leg,
            src,
            ssrc,
            ..Self::default()
        }
    }

    pub fn observe(&mut self, arrival_micros: u64, header: Option<RtpStatsHeader>) {
        self.packet_count += 1;

        let Some(header) = header else {
            return;
        };

        self.payload_type.get_or_insert(header.payload_type);
        let clock_rate = *self
            .clock_rate
            .get_or_insert_with(|| rtp_clock_rate_for_payload_type(header.payload_type));

        self.observe_sequence(header.sequence_number);
        self.observe_jitter(arrival_micros, header.rtp_timestamp, clock_rate);
    }

    fn observe_sequence(&mut self, sequence_number: u16) {
        if self.first_sequence.is_none() {
            self.first_sequence = Some(sequence_number);
            self.last_sequence = Some(sequence_number);
            return;
        }

        let Some(last_sequence) = self.last_sequence else {
            self.last_sequence = Some(sequence_number);
            return;
        };

        let diff = sequence_number.wrapping_sub(last_sequence);
        if diff == 0 {
            return;
        }

        if diff < 0x8000 {
            if diff > 1 {
                self.defer_missing_sequences(last_sequence, sequence_number);
            }
            self.last_sequence = Some(sequence_number);
            self.expire_missing_sequences();
        } else {
            self.pending_missing_sequences.remove(&sequence_number);
        }
    }

    fn defer_missing_sequences(&mut self, previous_sequence: u16, current_sequence: u16) {
        let missing_count = current_sequence.wrapping_sub(previous_sequence) - 1;
        let buffered_count = missing_count.min(RTP_REORDER_WINDOW);

        self.lost_packets += u64::from(missing_count - buffered_count);

        let first_buffered_offset = missing_count - buffered_count + 1;
        for offset in first_buffered_offset..=missing_count {
            self.pending_missing_sequences
                .insert(previous_sequence.wrapping_add(offset));
        }
    }

    fn expire_missing_sequences(&mut self) {
        let Some(last_sequence) = self.last_sequence else {
            return;
        };

        let expired: Vec<u16> = self
            .pending_missing_sequences
            .iter()
            .copied()
            .filter(|sequence| {
                let age = last_sequence.wrapping_sub(*sequence);
                age > RTP_REORDER_WINDOW && age < 0x8000
            })
            .collect();

        self.lost_packets += expired.len() as u64;
        for sequence in expired {
            self.pending_missing_sequences.remove(&sequence);
        }
    }

    fn observe_jitter(&mut self, arrival_micros: u64, rtp_timestamp: u32, clock_rate: u32) {
        if let (Some(prev_arrival), Some(prev_rtp)) =
            (self.prev_arrival_micros, self.prev_rtp_timestamp)
        {
            let arrival_delta = arrival_micros as i128 - prev_arrival as i128;
            let arrival_delta_units = arrival_delta as f64 * clock_rate as f64 / 1_000_000.0;
            let rtp_delta_units = rtp_timestamp_delta(rtp_timestamp, prev_rtp) as f64;
            let delta = (arrival_delta_units - rtp_delta_units).abs();

            if delta.is_finite() {
                self.jitter_rtp_units += (delta - self.jitter_rtp_units) / 16.0;
                self.jitter_samples += 1;
            }
        }

        self.prev_arrival_micros = Some(arrival_micros);
        self.prev_rtp_timestamp = Some(rtp_timestamp);
    }

    pub fn into_stats(self) -> SipFlowMediaStats {
        let lost_packets = self.lost_packets + self.pending_missing_sequences.len() as u64;
        let expected_packets = self.packet_count as u64 + lost_packets;
        let loss_percent = if expected_packets > 0 {
            lost_packets as f64 / expected_packets as f64 * 100.0
        } else {
            0.0
        };
        let jitter_ms = match (self.clock_rate, self.jitter_samples > 0) {
            (Some(clock_rate), true) if clock_rate > 0 => {
                Some(self.jitter_rtp_units * 1000.0 / clock_rate as f64)
            }
            _ => None,
        };

        SipFlowMediaStats {
            leg: self.leg,
            src: self.src,
            packet_count: self.packet_count,
            lost_packets,
            expected_packets,
            loss_percent,
            jitter_ms,
            ssrc: self.ssrc,
            payload_type: self.payload_type,
            clock_rate: self.clock_rate,
        }
    }
}

pub fn parse_rtp_stats_header(raw: &[u8]) -> Option<RtpStatsHeader> {
    if raw.len() < 12 || raw[0] >> 6 != 2 {
        return None;
    }

    Some(RtpStatsHeader {
        payload_type: raw[1] & 0x7f,
        sequence_number: u16::from_be_bytes([raw[2], raw[3]]),
        rtp_timestamp: u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]),
        ssrc: u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]),
    })
}

pub fn rtp_clock_rate_for_payload_type(payload_type: u8) -> u32 {
    match payload_type {
        0 | 8 | 9 | 18 => 8000,
        96..=127 => 48000,
        _ => 8000,
    }
}

pub fn rtp_timestamp_delta(current: u32, previous: u32) -> i64 {
    let forward = current.wrapping_sub(previous);
    if forward <= i32::MAX as u32 {
        forward as i64
    } else {
        -(previous.wrapping_sub(current) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_media_stats_reordered_packet_clears_pending_loss() {
        let mut stats = MediaStatsAccumulator::new(0, "127.0.0.1:4000".to_string(), Some(1));

        for (sequence_number, rtp_timestamp, arrival_micros) in [
            (10, 1_600, 10_000),
            (12, 1_920, 30_000),
            (11, 1_760, 40_000),
        ] {
            stats.observe(
                arrival_micros,
                Some(RtpStatsHeader {
                    payload_type: 0,
                    sequence_number,
                    rtp_timestamp,
                    ssrc: 1,
                }),
            );
        }

        let stats = stats.into_stats();
        assert_eq!(stats.packet_count, 3);
        assert_eq!(stats.lost_packets, 0);
        assert_eq!(stats.expected_packets, 3);
    }

    #[test]
    fn test_media_stats_unfilled_gap_counts_as_loss() {
        let mut stats = MediaStatsAccumulator::new(0, "127.0.0.1:4000".to_string(), Some(1));

        for (sequence_number, rtp_timestamp, arrival_micros) in [
            (10, 1_600, 10_000),
            (12, 1_920, 30_000),
        ] {
            stats.observe(
                arrival_micros,
                Some(RtpStatsHeader {
                    payload_type: 0,
                    sequence_number,
                    rtp_timestamp,
                    ssrc: 1,
                }),
            );
        }

        let stats = stats.into_stats();
        assert_eq!(stats.packet_count, 2);
        assert_eq!(stats.lost_packets, 1);
        assert_eq!(stats.expected_packets, 3);
    }
}
