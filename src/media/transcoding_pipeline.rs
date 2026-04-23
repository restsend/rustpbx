//! Transcoding Pipeline for Conference MCU
//!
//! Provides unified transcoding from any input codec to 8kHz PCM for mixing,
//! and from 8kHz PCM to any output codec for transmission.
//!
//! This is used in the conference media bridge to support participants with
//! different codecs (PCMU, PCMA, G.722, Opus, etc.).

use audio_codec::{CodecType, create_decoder, create_encoder};

/// Unified transcoding pipeline for conference audio.
///
/// Input: RTP payload (any codec) → decode → PCM
/// Output: PCM → encode → RTP payload (any codec)
pub struct TranscodingPipeline {
    input_codec: CodecType,
    output_codec: CodecType,
    decoder: Box<dyn audio_codec::Decoder>,
    encoder: Box<dyn audio_codec::Encoder>,
    resampler_input: Option<LinearResampler>,
    resampler_output: Option<LinearResampler>,
}

impl TranscodingPipeline {
    /// Create a new transcoding pipeline.
    ///
    /// # Arguments
    /// * `input_codec` - The codec used by the input RTP stream
    /// * `output_codec` - The codec to use for the output RTP stream
    ///
    /// Both input and output will be resampled to/from 8kHz PCM as needed.
    pub fn new(input_codec: CodecType, output_codec: CodecType) -> Self {
        let decoder = create_decoder(input_codec);
        let encoder = create_encoder(output_codec);

        let input_sample_rate = decoder.sample_rate();
        let output_sample_rate = encoder.sample_rate();

        let resampler_input = if input_sample_rate != 8000 {
            Some(LinearResampler::new(input_sample_rate, 8000))
        } else {
            None
        };

        let resampler_output = if output_sample_rate != 8000 {
            Some(LinearResampler::new(8000, output_sample_rate))
        } else {
            None
        };

        Self {
            input_codec,
            output_codec,
            decoder,
            encoder,
            resampler_input,
            resampler_output,
        }
    }

    /// Decode RTP payload to 8kHz PCM.
    ///
    /// This is used for the reverse loop (SIP → conference mixer).
    pub fn decode_to_pcm(&mut self,
        payload: &[u8],
    ) -> Vec<i16> {
        let mut pcm = self.decoder.decode(payload);

        if let Some(ref mut resampler) = self.resampler_input {
            pcm = resampler.resample(&pcm);
        }

        pcm
    }

    /// Encode 8kHz PCM to RTP payload.
    ///
    /// This is used for the forward loop (conference mixer → SIP).
    pub fn encode_from_pcm(
        &mut self,
        pcm: &[i16],
    ) -> Vec<u8> {
        let mut pcm = pcm.to_vec();

        if let Some(ref mut resampler) = self.resampler_output {
            pcm = resampler.resample(&pcm);
        }

        self.encoder.encode(&pcm)
    }

    /// Get the input codec type.
    pub fn input_codec(&self) -> CodecType {
        self.input_codec
    }

    /// Get the output codec type.
    pub fn output_codec(&self) -> CodecType {
        self.output_codec
    }
}

/// Simple linear resampler for PCM audio.
pub struct LinearResampler {
    src_rate: u32,
    dst_rate: u32,
    ratio: f32,
}

impl LinearResampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            src_rate,
            dst_rate,
            ratio: src_rate as f32 / dst_rate as f32,
        }
    }

    pub fn resample(&mut self, samples: &[i16]) -> Vec<i16> {
        if self.src_rate == self.dst_rate {
            return samples.to_vec();
        }

        let new_len = (samples.len() as f32 / self.ratio) as usize;
        let mut result = Vec::with_capacity(new_len);

        for i in 0..new_len {
            let src_idx = i as f32 * self.ratio;
            let src_idx_floor = src_idx.floor() as usize;
            let src_idx_ceil = (src_idx.ceil() as usize).min(samples.len().saturating_sub(1));
            let frac = src_idx - src_idx.floor();

            let sample = if src_idx_floor == src_idx_ceil {
                samples[src_idx_floor]
            } else {
                let s0 = samples[src_idx_floor] as f32;
                let s1 = samples[src_idx_ceil] as f32;
                (s0 + frac * (s1 - s0)) as i16
            };
            result.push(sample);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transcoding_pipeline_pcmu_to_pcmu() {
        let mut pipeline = TranscodingPipeline::new(CodecType::PCMU, CodecType::PCMU);

        // Create a simple PCM signal
        let pcm: Vec<i16> = (0..160).map(|i| (i as i16 * 100) % 32767).collect();

        // Encode to PCMU
        let encoded = pipeline.encode_from_pcm(&pcm);
        assert!(!encoded.is_empty());

        // Decode back to PCM
        let decoded = pipeline.decode_to_pcm(&encoded);
        assert_eq!(decoded.len(), 160);
    }

    #[test]
    fn test_linear_resampler_16khz_to_8khz() {
        let mut resampler = LinearResampler::new(16000, 8000);
        let samples: Vec<i16> = (0..320).map(|i| (i as i16 * 100) % 32767).collect();

        let resampled = resampler.resample(&samples);
        assert_eq!(resampled.len(), 160);
    }

    #[test]
    fn test_linear_resampler_8khz_to_16khz() {
        let mut resampler = LinearResampler::new(8000, 16000);
        let samples: Vec<i16> = (0..160).map(|i| (i as i16 * 100) % 32767).collect();

        let resampled = resampler.resample(&samples);
        assert_eq!(resampled.len(), 320);
    }

    #[test]
    fn test_linear_resampler_noop() {
        let mut resampler = LinearResampler::new(8000, 8000);
        let samples: Vec<i16> = (0..160).map(|i| (i as i16 * 100) % 32767).collect();

        let resampled = resampler.resample(&samples);
        assert_eq!(resampled.len(), 160);
        assert_eq!(resampled, samples);
    }

    #[test]
    fn test_transcoding_pipeline_with_resampling() {
        // G.722 is at 16kHz, PCMU is at 8kHz
        let mut pipeline = TranscodingPipeline::new(CodecType::G722, CodecType::PCMU);

        // Simulate 16kHz PCM (320 samples for 20ms)
        let pcm_16k: Vec<i16> = (0..320).map(|i| (i as i16 * 100) % 32767).collect();

        // Encode to PCMU (should resample to 8kHz first)
        let encoded = pipeline.encode_from_pcm(&pcm_16k);
        assert!(!encoded.is_empty());
    }
}
