use crate::media::processor::AudioFrame;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const DEFAULT_JITTER_BUFFER_SIZE: Duration = Duration::from_millis(50);
const MAX_JITTER_BUFFER_SIZE: Duration = Duration::from_millis(200);
const MIN_JITTER_BUFFER_SIZE: Duration = Duration::from_millis(20);

pub struct JitterBuffer {
    // Packets stored by timestamp
    packets: BTreeMap<u64, AudioFrame>,
    // Current jitter buffer size
    buffer_size: Duration,
    // Last packet timestamp
    last_timestamp: Option<u64>,
    // Statistics for dynamic buffer sizing
    jitter_stats: JitterStats,
    // Sample rate for timing calculations
    sample_rate: u32,
}

struct JitterStats {
    last_arrival: Option<Instant>,
    variance_sum: f64,
    variance_count: u32,
}

impl JitterBuffer {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            packets: BTreeMap::new(),
            buffer_size: DEFAULT_JITTER_BUFFER_SIZE,
            last_timestamp: None,
            jitter_stats: JitterStats {
                last_arrival: None,
                variance_sum: 0.0,
                variance_count: 0,
            },
            sample_rate,
        }
    }

    pub fn push(&mut self, timestamp: u64, frame: AudioFrame) {
        // Update jitter statistics
        let now = Instant::now();
        if let Some(last_arrival) = self.jitter_stats.last_arrival {
            let arrival_diff = now.duration_since(last_arrival).as_secs_f64() * 1000.0;
            if let Some(last_ts) = self.last_timestamp {
                let expected_diff = if timestamp > last_ts {
                    timestamp - last_ts
                } else {
                    last_ts - timestamp
                } as f64;
                let jitter = (arrival_diff - expected_diff).abs();

                self.jitter_stats.variance_sum += jitter;
                self.jitter_stats.variance_count += 1;

                // Adjust buffer size based on jitter
                if self.jitter_stats.variance_count >= 100 {
                    let avg_jitter =
                        self.jitter_stats.variance_sum / self.jitter_stats.variance_count as f64;
                    let new_size = Duration::from_millis((avg_jitter * 2.0) as u64)
                        .max(MIN_JITTER_BUFFER_SIZE)
                        .min(MAX_JITTER_BUFFER_SIZE);
                    self.buffer_size = new_size;

                    // Reset statistics
                    self.jitter_stats.variance_sum = 0.0;
                    self.jitter_stats.variance_count = 0;
                }
            }
        }
        self.jitter_stats.last_arrival = Some(now);

        // Only update last_timestamp if this packet is newer
        if let Some(last_ts) = self.last_timestamp {
            if timestamp > last_ts {
                self.last_timestamp = Some(timestamp);
            }
        } else {
            self.last_timestamp = Some(timestamp);
        }

        // Store packet if it's not too old
        if let Some(first_ts) = self.packets.keys().next().copied() {
            if timestamp >= first_ts {
                self.packets.insert(timestamp, frame);
            }
        } else {
            self.packets.insert(timestamp, frame);
        }

        // Remove old packets
        let max_packets =
            (MAX_JITTER_BUFFER_SIZE.as_secs_f64() * self.sample_rate as f64 / 1000.0) as usize;
        while self.packets.len() > max_packets {
            if let Some((&first_ts, _)) = self.packets.first_key_value() {
                self.packets.remove(&first_ts);
            }
        }
    }

    pub fn pop(&mut self) -> Option<AudioFrame> {
        if let Some((&first_ts, _)) = self.packets.first_key_value() {
            return self.packets.remove(&first_ts);
        }
        None
    }

    pub fn clear(&mut self) {
        self.packets.clear();
        self.last_timestamp = None;
        self.jitter_stats = JitterStats {
            last_arrival: None,
            variance_sum: 0.0,
            variance_count: 0,
        };
        self.buffer_size = DEFAULT_JITTER_BUFFER_SIZE;
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
}
