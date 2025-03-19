use anyhow::Result;
use nnnoiseless::DenoiseState;

use crate::media::processor::{AudioFrame, Processor};

pub struct NoiseReducer {
    denoiser: Box<DenoiseState<'static>>,
    frame_size: usize,
}

impl NoiseReducer {
    pub fn new() -> Self {
        let frame_size = 480; // 30ms at 16kHz
        Self {
            denoiser: DenoiseState::new(),
            frame_size,
        }
    }
}

impl Processor for NoiseReducer {
    fn process_frame(&mut self, frame: AudioFrame) -> Result<AudioFrame> {
        let mut output = Vec::with_capacity(frame.samples.len());
        let mut frame_buf = vec![0.0; self.frame_size];
        let mut output_buf = vec![0.0; self.frame_size];

        // Process in chunks of frame_size
        for chunk in frame.samples.chunks(self.frame_size) {
            // Copy input chunk
            let chunk_len = chunk.len();
            frame_buf[..chunk_len].copy_from_slice(chunk);

            // Process through denoiser
            self.denoiser.process_frame(&mut output_buf, &frame_buf);

            // Extend output with processed samples
            output.extend_from_slice(&output_buf[..chunk_len]);
        }

        Ok(AudioFrame {
            samples: output,
            sample_rate: frame.sample_rate,
            channels: frame.channels,
            track_id: frame.track_id,
            timestamp: frame.timestamp,
        })
    }
}
