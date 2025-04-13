use super::codecs::resample::LinearResampler;
use crate::{media::processor::Processor, AudioFrame, Sample, PcmBuf, Samples};
use anyhow::Result;
use nnnoiseless::DenoiseState;
use std::cell::RefCell;

pub struct NoiseReducer {
    resampler_target: RefCell<LinearResampler>,
    resampler_source: RefCell<LinearResampler>,
    denoiser: RefCell<Box<DenoiseState<'static>>>,
}

impl NoiseReducer {
    pub fn new(input_sample_rate: usize) -> Result<Self> {
        let resampler48k = LinearResampler::new(48000, input_sample_rate)?;
        let resampler16k = LinearResampler::new(input_sample_rate, 48000 as usize)?;
        let denoiser = DenoiseState::new();
        Ok(Self {
            resampler_target: RefCell::new(resampler48k),
            resampler_source: RefCell::new(resampler16k),
            denoiser: RefCell::new(denoiser),
        })
    }
}
unsafe impl Send for NoiseReducer {}
unsafe impl Sync for NoiseReducer {}

impl Processor for NoiseReducer {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        // If empty frame, nothing to do
        if frame.samples.is_empty() {
            return Ok(());
        }

        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()),
        };
        let samples = self.resampler_source.borrow_mut().resample(samples);
        let input_size = samples.len();

        let output_padding_size = input_size + DenoiseState::FRAME_SIZE;
        let mut output_buf = vec![0.0; output_padding_size];
        let input_f32: Vec<f32> = samples.iter().map(|&s| s.into()).collect();

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
            self.denoiser.borrow_mut().process_frame(
                &mut output_buf[offset..offset + DenoiseState::FRAME_SIZE],
                &input_chunk,
            );

            offset += chunk_len;
        }

        let samples = output_buf[..input_size]
            .iter()
            .map(|&s| s as Sample)
            .collect::<PcmBuf>();

        frame.samples = Samples::PCM {
            samples: self.resampler_target.borrow_mut().resample(&samples),
        };

        Ok(())
    }
}
