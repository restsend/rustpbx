use crate::{
    media::AudioFrame,
    media::PcmBuf,
    media::Samples,
    media::codecs::{
        Decoder, Encoder, bytes_to_samples,
        g722::{G722Decoder, G722Encoder},
        pcma::{PcmaDecoder, PcmaEncoder},
        pcmu::{PcmuDecoder, PcmuEncoder},
        resample::LinearResampler,
        samples_to_bytes,
    },
};
use std::cell::RefCell;

#[cfg(feature = "g729")]
use crate::media::codecs::g729::{G729Decoder, G729Encoder};
#[cfg(feature = "opus")]
use crate::media::codecs::opus::{OpusDecoder, OpusEncoder};

pub struct TrackCodec {
    pub pcmu_encoder: RefCell<PcmuEncoder>,
    pub pcmu_decoder: RefCell<PcmuDecoder>,
    pub pcma_encoder: RefCell<PcmaEncoder>,
    pub pcma_decoder: RefCell<PcmaDecoder>,

    pub g722_encoder: RefCell<G722Encoder>,
    pub g722_decoder: RefCell<G722Decoder>,

    #[cfg(feature = "g729")]
    pub g729_encoder: RefCell<G729Encoder>,
    #[cfg(feature = "g729")]
    pub g729_decoder: RefCell<G729Decoder>,

    #[cfg(feature = "opus")]
    pub opus_encoder: RefCell<Option<OpusEncoder>>,
    #[cfg(feature = "opus")]
    pub opus_decoder: RefCell<Option<OpusDecoder>>,

    pub resampler: RefCell<Option<LinearResampler>>,
}
unsafe impl Send for TrackCodec {}
unsafe impl Sync for TrackCodec {}

impl Clone for TrackCodec {
    fn clone(&self) -> Self {
        Self::new() // Since each codec has its own state, create a fresh instance
    }
}

impl TrackCodec {
    pub fn new() -> Self {
        Self {
            pcmu_encoder: RefCell::new(PcmuEncoder::new()),
            pcmu_decoder: RefCell::new(PcmuDecoder::new()),
            pcma_encoder: RefCell::new(PcmaEncoder::new()),
            pcma_decoder: RefCell::new(PcmaDecoder::new()),
            g722_encoder: RefCell::new(G722Encoder::new()),
            g722_decoder: RefCell::new(G722Decoder::new()),
            #[cfg(feature = "g729")]
            g729_encoder: RefCell::new(G729Encoder::new()),
            #[cfg(feature = "g729")]
            g729_decoder: RefCell::new(G729Decoder::new()),
            #[cfg(feature = "opus")]
            opus_encoder: RefCell::new(None),
            #[cfg(feature = "opus")]
            opus_decoder: RefCell::new(None),
            resampler: RefCell::new(None),
        }
    }

    pub fn is_audio(payload_type: u8) -> bool {
        match payload_type {
            0 | 8 | 9 => true,
            #[cfg(feature = "g729")]
            18 => true,
            #[cfg(feature = "opus")]
            111 => true,
            _ => false,
        }
    }

    pub fn decode(&self, payload_type: u8, payload: &[u8], target_sample_rate: u32) -> PcmBuf {
        let payload = match payload_type {
            0 => self.pcmu_decoder.borrow_mut().decode(payload),
            8 => self.pcma_decoder.borrow_mut().decode(payload),
            9 => self.g722_decoder.borrow_mut().decode(payload),
            #[cfg(feature = "g729")]
            18 => self.g729_decoder.borrow_mut().decode(payload),
            #[cfg(feature = "opus")]
            111 => {
                let mut opus_decoder = self.opus_decoder.borrow_mut();
                if opus_decoder.is_none() {
                    *opus_decoder = Some(OpusDecoder::new_default());
                }
                if let Some(ref mut decoder) = opus_decoder.as_mut() {
                    decoder.decode(payload)
                } else {
                    bytes_to_samples(payload)
                }
            }
            _ => bytes_to_samples(payload),
        };
        let sample_rate = match payload_type {
            0 => 8000,
            8 => 8000,
            9 => 16000,
            18 => 8000,
            111 => 48000, // Opus sample rate
            _ => 8000,
        };
        if sample_rate != target_sample_rate {
            if self.resampler.borrow().is_none() {
                self.resampler.borrow_mut().replace(
                    LinearResampler::new(sample_rate as usize, target_sample_rate as usize)
                        .unwrap(),
                );
            }
            self.resampler
                .borrow_mut()
                .as_mut()
                .unwrap()
                .resample(&payload)
        } else {
            payload
        }
    }

    pub fn encode(&self, payload_type: u8, frame: AudioFrame) -> (u8, Vec<u8>) {
        match frame.samples {
            Samples::PCM { samples: mut pcm } => {
                let target_samplerate = match payload_type {
                    0 => 8000,
                    8 => 8000,
                    9 => 16000,
                    18 => 8000,
                    111 => 48000, // Opus sample rate
                    _ => 8000,
                };

                if frame.sample_rate != target_samplerate {
                    if self.resampler.borrow().is_none() {
                        self.resampler.borrow_mut().replace(
                            LinearResampler::new(
                                frame.sample_rate as usize,
                                target_samplerate as usize,
                            )
                            .unwrap(),
                        );
                    }
                    pcm = self.resampler.borrow_mut().as_mut().unwrap().resample(&pcm);
                }

                let payload = match payload_type {
                    0 => self.pcmu_encoder.borrow_mut().encode(&pcm),
                    8 => self.pcma_encoder.borrow_mut().encode(&pcm),
                    9 => self.g722_encoder.borrow_mut().encode(&pcm),
                    #[cfg(feature = "g729")]
                    18 => self.g729_encoder.borrow_mut().encode(&pcm),
                    #[cfg(feature = "opus")]
                    111 => {
                        let mut opus_encoder = self.opus_encoder.borrow_mut();
                        if opus_encoder.is_none() {
                            *opus_encoder = Some(OpusEncoder::new_default());
                        }
                        if let Some(ref mut encoder) = opus_encoder.as_mut() {
                            encoder.encode(&pcm)
                        } else {
                            samples_to_bytes(&pcm)
                        }
                    }
                    _ => samples_to_bytes(&pcm),
                };
                (payload_type, payload)
            }
            Samples::RTP {
                payload_type,
                payload,
                ..
            } => (payload_type, payload),
            _ => (payload_type, vec![]),
        }
    }
}
