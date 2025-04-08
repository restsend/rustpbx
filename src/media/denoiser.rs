use super::codecs::resample::resample_mono;
use crate::{media::processor::Processor, AudioFrame, Samples};
use anyhow::Result;
use nnnoiseless::DenoiseState;
use std::sync::Mutex;

pub struct NoiseReducer {
    denoiser: Mutex<Box<DenoiseState<'static>>>,
}

impl NoiseReducer {
    pub fn new() -> Self {
        Self {
            denoiser: Mutex::new(DenoiseState::new()),
        }
    }
}

impl Processor for NoiseReducer {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        // If empty frame, nothing to do
        if frame.samples.is_empty() {
            return Ok(());
        }

        // Extract PCM samples or return if not PCM format
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()),
        };
        let samples = resample_mono(samples, frame.sample_rate, 48000);
        let input_size = samples.len();

        let output_padding_size = input_size + DenoiseState::FRAME_SIZE;
        let mut output_buf = vec![0.0; output_padding_size];
        let input_f32: Vec<f32> = samples.iter().map(|&s| s as f32).collect();

        // Process audio in chunks of FRAME_SIZE
        let mut offset = 0;
        let mut buf;

        while offset < input_size {
            let remaining_size = input_size - offset;
            let chunk_len = remaining_size.min(DenoiseState::FRAME_SIZE);
            let end_offset = offset + chunk_len;

            let input_chunk = if chunk_len < DenoiseState::FRAME_SIZE {
                buf = vec![0.0; DenoiseState::FRAME_SIZE];
                buf[..chunk_len].copy_from_slice(&input_f32[offset..end_offset]);
                &buf
            } else {
                &input_f32[offset..end_offset]
            };

            // Process the current frame
            self.denoiser.lock().unwrap().process_frame(
                &mut output_buf[offset..offset + DenoiseState::FRAME_SIZE],
                &input_chunk,
            );

            offset += chunk_len;
        }

        let samples = output_buf[..input_size]
            .iter()
            .map(|&s| s as i16)
            .collect::<Vec<i16>>();

        frame.samples = Samples::PCM {
            samples: resample_mono(&samples, 48000, frame.sample_rate),
        };

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_processing() {
        let reducer = NoiseReducer::new();

        // Test with different frame sizes
        for size in &[160, 320, 480, 960] {
            // Create a sample frame with test data
            let mut frame = AudioFrame {
                samples: Samples::PCM {
                    samples: vec![100; *size],
                },
                sample_rate: 16000,
                ..Default::default()
            };

            // Process the frame
            reducer.process_frame(&mut frame).unwrap();

            // Should maintain the same number of samples
            let samples = match frame.samples {
                Samples::PCM { samples } => samples,
                _ => panic!("Expected PCM samples"),
            };
            assert_eq!(samples.len(), *size);
        }
    }
}
