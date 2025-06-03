use crate::{
    media::codecs::{
        bytes_to_samples,
        g722::{G722Decoder, G722Encoder},
        pcma::{PcmaDecoder, PcmaEncoder},
        pcmu::{PcmuDecoder, PcmuEncoder},
        resample::LinearResampler,
        samples_to_bytes, Decoder, Encoder,
    },
    AudioFrame, PcmBuf, Samples,
};
use std::cell::RefCell;

pub struct TrackCodec {
    pub pcmu_encoder: RefCell<PcmuEncoder>,
    pub pcmu_decoder: RefCell<PcmuDecoder>,
    pub pcma_encoder: RefCell<PcmaEncoder>,
    pub pcma_decoder: RefCell<PcmaDecoder>,

    pub g722_encoder: RefCell<G722Encoder>,
    pub g722_decoder: RefCell<G722Decoder>,
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
            resampler: RefCell::new(None),
        }
    }

    pub fn decode(&self, payload_type: u8, payload: &[u8], target_sample_rate: u32) -> PcmBuf {
        let payload = match payload_type {
            0 => self.pcmu_decoder.borrow_mut().decode(payload),
            8 => self.pcma_decoder.borrow_mut().decode(payload),
            9 => self.g722_decoder.borrow_mut().decode(payload),
            _ => bytes_to_samples(payload),
        };
        let sample_rate = match payload_type {
            0 => 8000,
            8 => 8000,
            9 => 16000,
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

    pub fn encode(&self, payload_type: u8, frame: AudioFrame) -> Vec<u8> {
        match frame.samples {
            Samples::PCM { samples: mut pcm } => {
                let target_samplerate = match payload_type {
                    0 => 8000,
                    8 => 8000,
                    9 => 16000,
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

                match payload_type {
                    0 => self.pcmu_encoder.borrow_mut().encode(&pcm),
                    8 => self.pcma_encoder.borrow_mut().encode(&pcm),
                    9 => self.g722_encoder.borrow_mut().encode(&pcm),
                    _ => samples_to_bytes(&pcm),
                }
            }
            Samples::RTP { payload, .. } => payload,
            Samples::Empty => vec![],
        }
    }
}
