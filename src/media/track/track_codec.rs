use crate::{
    media::codecs::{
        convert_s16_to_u8, convert_u8_to_s16,
        g722::{G722Decoder, G722Encoder},
        pcma::{PcmaDecoder, PcmaEncoder},
        pcmu::{PcmuDecoder, PcmuEncoder},
        resample::resample_mono,
        Decoder, Encoder,
    },
    AudioFrame, Samples,
};
use std::cell::RefCell;

pub struct TrackCodec {
    pub pcmu_encoder: RefCell<PcmuEncoder>,
    pub pcmu_decoder: RefCell<PcmuDecoder>,
    pub pcma_encoder: RefCell<PcmaEncoder>,
    pub pcma_decoder: RefCell<PcmaDecoder>,

    pub g722_encoder: RefCell<G722Encoder>,
    pub g722_decoder: RefCell<G722Decoder>,
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
        }
    }

    pub fn decode(&self, payload_type: u8, payload: &[u8]) -> Vec<i16> {
        match payload_type {
            0 => self.pcmu_decoder.borrow_mut().decode(payload),
            8 => self.pcma_decoder.borrow_mut().decode(payload),
            9 => self.g722_decoder.borrow_mut().decode(payload),
            _ => convert_u8_to_s16(payload),
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
                    pcm = resample_mono(&pcm, frame.sample_rate, target_samplerate);
                }

                match payload_type {
                    0 => self.pcmu_encoder.borrow_mut().encode(&pcm),
                    8 => self.pcma_encoder.borrow_mut().encode(&pcm),
                    9 => self.g722_encoder.borrow_mut().encode(&pcm),
                    _ => convert_s16_to_u8(&pcm),
                }
            }
            Samples::RTP { payload, .. } => payload,
            Samples::Empty => vec![],
        }
    }
}
