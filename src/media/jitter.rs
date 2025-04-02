use crate::AudioFrame;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

pub struct JitterBuffer {
    frames: BTreeMap<u64, AudioFrame>,
    max_size: usize,
    last_popped_timestamp: Option<u64>,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self::with_max_size(100)
    }

    pub fn with_max_size(max_size: usize) -> Self {
        Self {
            frames: BTreeMap::new(),
            max_size,
            last_popped_timestamp: None,
        }
    }

    pub fn push(&mut self, frame: AudioFrame) {
        // Don't add frames with timestamps earlier than the last popped timestamp
        if let Some(last_ts) = self.last_popped_timestamp {
            if frame.timestamp <= last_ts {
                return;
            }
        }

        // If buffer is full, remove oldest frames when adding new ones
        if self.frames.len() >= self.max_size {
            if let Some(oldest_ts) = self.frames.keys().next().copied() {
                if frame.timestamp > oldest_ts {
                    self.frames.remove(&oldest_ts);
                } else {
                    // New frame is older than our oldest frame, don't add it
                    return;
                }
            }
        }

        self.frames.insert(frame.timestamp, frame);
    }

    pub fn pop(&mut self) -> Option<AudioFrame> {
        if let Some((ts, frame)) = self.frames.iter().next().map(|(k, v)| (*k, v.clone())) {
            self.frames.remove(&ts);
            self.last_popped_timestamp = Some(ts);
            Some(frame)
        } else {
            None
        }
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn pull_frames(&mut self, duration_ms: u32) -> Vec<AudioFrame> {
        let mut frames = Vec::new();
        let frames_to_pull = (duration_ms / 20).max(1) as usize;
        for _ in 0..frames_to_pull {
            if let Some(frame) = self.pop() {
                frames.push(frame);
            } else {
                break;
            }
        }
        frames
    }
}
