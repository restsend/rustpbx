//! Per-(leg, payload-type) decode + resample pipeline.
//!
//! Both recording pipelines decode heterogeneous ingress RTP into a single
//! target PCM domain; each (leg, PT) pair needs its own decoder state (and
//! a resampler when the decoded rate differs from the target).

use std::collections::HashMap;

use audio_codec::{CodecType, Decoder, Resampler, create_decoder};

/// Decode cache keyed by (leg, payload type).
pub struct LegCodecPipeline {
    decoders: HashMap<(i32, u8), Box<dyn Decoder>>,
    resamplers: HashMap<(i32, u8), Resampler>,
}

impl LegCodecPipeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode `payload` (of `codec`, sampled at `src_clock`) into PCM
    /// samples at `target_rate`. Decoder/resampler state is cached per
    /// (leg, PT).
    pub fn decode(
        &mut self,
        leg: i32,
        pt: u8,
        codec: CodecType,
        src_clock: u32,
        payload: &[u8],
        target_rate: u32,
    ) -> Vec<i16> {
        let decoder = self
            .decoders
            .entry((leg, pt))
            .or_insert_with(|| create_decoder(codec));
        let samples = decoder.decode(payload);

        let decoded_rate = decoder.sample_rate().max(src_clock);
        if decoded_rate != target_rate {
            let resampler = self
                .resamplers
                .entry((leg, pt))
                .or_insert_with(|| Resampler::new(decoded_rate as usize, target_rate as usize));
            resampler.resample(&samples)
        } else {
            samples
        }
    }
}

impl Default for LegCodecPipeline {
    fn default() -> Self {
        Self {
            decoders: HashMap::new(),
            resamplers: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_pcmu_to_pcm() {
        let mut pipe = LegCodecPipeline::new();
        let pcm: Vec<i16> = (0..160).map(|i| (i * 100) as i16).collect();
        let mut enc = audio_codec::create_encoder(CodecType::PCMU);
        let encoded = enc.encode(&pcm);

        let out = pipe.decode(1, 0, CodecType::PCMU, 8000, &encoded, 8000);
        assert!(!out.is_empty());
        // Same-rate passthrough must not resample.
        assert_eq!(out.len(), 160);
    }
}
