use super::processor::Processor;
use crate::media::AudioFrame;
use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

/// Volume control processor for audio streams
#[derive(Debug, Clone)]
pub struct VolumeControlProcessor {
    /// Volume level stored as bits of f32 (0.0 to 2.0, where 1.0 is normal volume)
    volume_level: Arc<AtomicU32>,
    /// Whether audio is muted
    muted: Arc<AtomicBool>,
}

impl Default for VolumeControlProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl VolumeControlProcessor {
    pub fn new() -> Self {
        Self {
            volume_level: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            muted: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_volume(&self, level: f32) {
        let clamped_level = level.clamp(0.0, 2.0);
        self.volume_level
            .store(clamped_level.to_bits(), Ordering::Relaxed);
    }

    pub fn get_volume(&self) -> f32 {
        f32::from_bits(self.volume_level.load(Ordering::Relaxed))
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn toggle_mute(&self) -> bool {
        // Use fetch_xor to atomically toggle the boolean
        !self.muted.fetch_xor(true, Ordering::Relaxed)
    }
}

impl Processor for VolumeControlProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        // Check if muted
        if self.is_muted() {
            // Mute the audio by zeroing out samples
            if let crate::media::Samples::PCM { samples } = &mut frame.samples {
                for sample in samples.iter_mut() {
                    *sample = 0;
                }
            }
            return Ok(());
        }

        // Apply volume control
        let volume = self.get_volume();
        if (volume - 1.0).abs() > f32::EPSILON {
            if let crate::media::Samples::PCM { samples } = &mut frame.samples {
                for sample in samples.iter_mut() {
                    let adjusted = (*sample as f32 * volume) as i16;
                    *sample = adjusted.clamp(i16::MIN, i16::MAX);
                }
            }
        }

        Ok(())
    }
}

/// Hold/Unhold processor for audio streams
#[derive(Debug, Clone)]
pub struct HoldProcessor {
    /// Whether the call is on hold
    on_hold: Arc<AtomicBool>,
}

impl Default for HoldProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl HoldProcessor {
    pub fn new() -> Self {
        Self {
            on_hold: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_hold(&self, hold: bool) {
        self.on_hold.store(hold, Ordering::Relaxed);
    }

    pub fn is_on_hold(&self) -> bool {
        self.on_hold.load(Ordering::Relaxed)
    }

    pub fn toggle_hold(&self) -> bool {
        // Use fetch_xor to atomically toggle the boolean
        !self.on_hold.fetch_xor(true, Ordering::Relaxed)
    }
}

impl Processor for HoldProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        if self.is_on_hold() {
            // When on hold, replace audio with silence or hold music
            if let crate::media::Samples::PCM { samples } = &mut frame.samples {
                // Replace with silence for now
                // TODO: Could be enhanced to play hold music
                for sample in samples.iter_mut() {
                    *sample = 0;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Samples;

    #[test]
    fn test_volume_control() {
        let processor = VolumeControlProcessor::new();

        // Test default volume
        assert!((processor.get_volume() - 1.0).abs() < f32::EPSILON);
        assert!(!processor.is_muted());

        // Test volume setting
        processor.set_volume(0.5);
        assert!((processor.get_volume() - 0.5).abs() < f32::EPSILON);

        // Test mute
        processor.set_muted(true);
        assert!(processor.is_muted());
    }

    #[test]
    fn test_volume_processing() {
        let processor = VolumeControlProcessor::new();
        processor.set_volume(0.5);

        let mut frame = AudioFrame {
            track_id: "test".to_string(),
            samples: Samples::PCM {
                samples: vec![1000, -1000, 500, -500],
            },
            timestamp: 0,
            sample_rate: 16000,
        };

        processor.process_frame(&mut frame).unwrap();

        if let Samples::PCM { samples } = frame.samples {
            assert_eq!(samples, vec![500, -500, 250, -250]);
        } else {
            panic!("Expected PCM samples");
        }
    }

    #[test]
    fn test_mute_processing() {
        let processor = VolumeControlProcessor::new();
        processor.set_muted(true);

        let mut frame = AudioFrame {
            track_id: "test".to_string(),
            samples: Samples::PCM {
                samples: vec![1000, -1000, 500, -500],
            },
            timestamp: 0,
            sample_rate: 16000,
        };

        processor.process_frame(&mut frame).unwrap();

        if let Samples::PCM { samples } = frame.samples {
            assert_eq!(samples, vec![0, 0, 0, 0]);
        } else {
            panic!("Expected PCM samples");
        }
    }

    #[test]
    fn test_hold_processor() {
        let processor = HoldProcessor::new();

        // Test default state
        assert!(!processor.is_on_hold());

        // Test hold setting
        processor.set_hold(true);
        assert!(processor.is_on_hold());

        // Test toggle
        let new_state = processor.toggle_hold();
        assert!(!new_state);
        assert!(!processor.is_on_hold());
    }
}
