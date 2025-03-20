use anyhow::Result;
use nnnoiseless::DenoiseState;

use crate::media::processor::{AudioFrame, Processor};

pub struct NoiseReducer {
    // We use the default built-in model for the denoiser
    frame_size: usize,
}

impl NoiseReducer {
    pub fn new() -> Self {
        Self {
            frame_size: DenoiseState::FRAME_SIZE,
        }
    }
}

impl Processor for NoiseReducer {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        // If empty frame, nothing to do
        if frame.samples.is_empty() {
            return Ok(());
        }

        // Convert i16 samples to f32 (required by nnnoiseless)
        // Note: nnnoiseless expects values in i16 range (-32768.0 to 32767.0)
        let input_f32: Vec<f32> = frame.samples.iter().map(|&s| s as f32).collect();

        // Original frame length
        let original_len = input_f32.len();

        // Prepare output buffer with exact same size as input
        let mut output_f32 = vec![0.0; original_len];

        if original_len <= DenoiseState::FRAME_SIZE {
            // For small frames that fit in one processing block
            let mut denoiser = DenoiseState::new();
            let mut frame_buf = vec![0.0; DenoiseState::FRAME_SIZE];
            let mut output_buf = vec![0.0; DenoiseState::FRAME_SIZE];

            // Copy input to frame buffer
            frame_buf[..original_len].copy_from_slice(&input_f32);

            // Process through denoiser
            denoiser.process_frame(&mut output_buf, &frame_buf);

            // Copy processed output back
            output_f32[..original_len].copy_from_slice(&output_buf[..original_len]);
        } else {
            // For larger frames that need multiple processing blocks
            let mut denoiser = DenoiseState::new();
            let mut offset = 0;

            // Process each chunk
            while offset < original_len {
                let chunk_len = (original_len - offset).min(DenoiseState::FRAME_SIZE);
                let end_offset = offset + chunk_len;

                let mut frame_buf = vec![0.0; DenoiseState::FRAME_SIZE];
                let mut output_buf = vec![0.0; DenoiseState::FRAME_SIZE];

                // Copy input chunk to frame buffer
                frame_buf[..chunk_len].copy_from_slice(&input_f32[offset..end_offset]);

                // Process through denoiser
                denoiser.process_frame(&mut output_buf, &frame_buf);

                // Copy processed output back to appropriate position
                output_f32[offset..end_offset].copy_from_slice(&output_buf[..chunk_len]);

                // Move to next chunk
                offset += chunk_len;
            }
        }

        // Convert f32 samples back to i16
        frame.samples = output_f32
            .iter()
            .map(|&s| s.clamp(-32768.0, 32767.0) as i16)
            .collect();

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
                samples: vec![100; *size],
                sample_rate: 16000,
                ..Default::default()
            };

            // Process the frame
            reducer.process_frame(&mut frame).unwrap();

            // Should maintain the same number of samples
            assert_eq!(frame.samples.len(), *size);
        }
    }
}
