use crate::AudioFrame;
use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct JitterStats {
    pub buffer_size: usize,
    pub total_received: u64,
    pub total_dropped: u64,
    pub total_late: u64,
    pub current_delay: u32,
}

pub struct JitterBuffer {
    // Use VecDeque for better memory efficiency
    frames: VecDeque<AudioFrame>,
    max_size: usize,
    last_popped_timestamp: Option<u64>,

    // Add statistics
    total_received: u64,
    total_dropped: u64,
    total_late: u64,

    // Buffer configuration
    target_delay_ms: u32, // Target buffering delay
    max_delay_ms: u32,    // Maximum acceptable delay
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self::with_max_size(100)
    }

    pub fn with_max_size(max_size: usize) -> Self {
        Self::with_config(max_size, 60, 200) // 60ms target, 200ms max
    }

    pub fn with_config(max_size: usize, target_delay_ms: u32, max_delay_ms: u32) -> Self {
        Self {
            frames: VecDeque::new(),
            max_size,
            last_popped_timestamp: None,
            total_received: 0,
            total_dropped: 0,
            total_late: 0,
            target_delay_ms,
            max_delay_ms,
        }
    }

    pub fn push(&mut self, frame: AudioFrame) -> bool {
        self.total_received += 1;

        // Handle timestamp wraparound and reject very old frames
        if let Some(last_ts) = self.last_popped_timestamp {
            let ts_diff = frame.timestamp.wrapping_sub(last_ts);

            // Reject very old frames (handle wraparound)
            if ts_diff > (u64::MAX / 2) {
                self.total_late += 1;
                return false;
            }

            // Don't add frames with timestamps earlier than the last popped timestamp
            if frame.timestamp <= last_ts {
                self.total_late += 1;
                return false;
            }
        }

        // Maintain buffer size limit
        while self.frames.len() >= self.max_size {
            if let Some(oldest) = self.frames.front() {
                if frame.timestamp > oldest.timestamp {
                    self.frames.pop_front();
                    self.total_dropped += 1;
                } else {
                    // New frame is older than our oldest frame, don't add it
                    self.total_late += 1;
                    return false;
                }
            } else {
                break;
            }
        }

        // Insert in sorted order
        let pos = self
            .frames
            .binary_search_by_key(&frame.timestamp, |f| f.timestamp)
            .unwrap_or_else(|pos| pos);

        // Handle duplicate timestamps by replacing
        if pos < self.frames.len() && self.frames[pos].timestamp == frame.timestamp {
            self.frames[pos] = frame;
        } else {
            self.frames.insert(pos, frame);
        }

        true
    }

    pub fn pop(&mut self) -> Option<AudioFrame> {
        if let Some(frame) = self.frames.pop_front() {
            self.last_popped_timestamp = Some(frame.timestamp);
            Some(frame)
        } else {
            None
        }
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.last_popped_timestamp = None;
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn pull_frames(&mut self, duration_ms: u32) -> Vec<AudioFrame> {
        self.pull_frames_with_duration(duration_ms, 20)
    }

    // Improved pull_frames with configurable frame duration
    pub fn pull_frames_with_duration(
        &mut self,
        duration_ms: u32,
        frame_duration_ms: u32,
    ) -> Vec<AudioFrame> {
        let mut frames = Vec::new();
        let frames_to_pull = (duration_ms / frame_duration_ms).max(1) as usize;

        for _ in 0..frames_to_pull {
            if let Some(frame) = self.pop() {
                frames.push(frame);
            } else {
                break;
            }
        }
        frames
    }

    // New: Check if ready to output (has enough buffer)
    pub fn is_ready(&self) -> bool {
        if self.frames.is_empty() {
            return false;
        }

        let now = crate::get_timestamp();
        let oldest_ts = self.frames.front().unwrap().timestamp;
        let buffer_delay = now.saturating_sub(oldest_ts);

        buffer_delay >= self.target_delay_ms as u64
    }

    // New: Check if buffer has excessive delay
    pub fn has_excessive_delay(&self) -> bool {
        if self.frames.is_empty() {
            return false;
        }

        let now = crate::get_timestamp();
        let oldest_ts = self.frames.front().unwrap().timestamp;
        let buffer_delay = now.saturating_sub(oldest_ts);

        buffer_delay > self.max_delay_ms as u64
    }

    // New: Get buffer statistics
    pub fn stats(&self) -> JitterStats {
        JitterStats {
            buffer_size: self.frames.len(),
            total_received: self.total_received,
            total_dropped: self.total_dropped,
            total_late: self.total_late,
            current_delay: self.current_delay(),
        }
    }

    // New: Get current buffer delay
    pub fn current_delay(&self) -> u32 {
        if let Some(oldest) = self.frames.front() {
            let now = crate::get_timestamp();
            now.saturating_sub(oldest.timestamp) as u32
        } else {
            0
        }
    }

    // New: Adaptive buffer management - remove old frames if delay is excessive
    pub fn adaptive_cleanup(&mut self) -> usize {
        let mut removed = 0;

        if self.has_excessive_delay() {
            let now = crate::get_timestamp();
            let max_age = now.saturating_sub(self.max_delay_ms as u64);

            while let Some(oldest) = self.frames.front() {
                if oldest.timestamp < max_age {
                    self.frames.pop_front();
                    self.total_dropped += 1;
                    removed += 1;
                } else {
                    break;
                }
            }
        }

        removed
    }
}
