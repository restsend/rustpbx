use crate::{
    media::codecs::{
        g722::{G722Decoder, G722Encoder},
        pcma::{PcmaDecoder, PcmaEncoder},
        pcmu::{PcmuDecoder, PcmuEncoder},
        Decoder, Encoder,
    },
    AudioFrame, Samples,
};
use anyhow::Result;
use bytes::Bytes;
use std::cell::RefCell;

pub struct TrackCodec {
    pub pcmu_encoder: RefCell<PcmuEncoder>,
    pub pcmu_decoder: RefCell<PcmuDecoder>,
    pub pcma_encoder: RefCell<PcmaEncoder>,
    pub pcma_decoder: RefCell<PcmaDecoder>,

    pub g722_encoder: RefCell<G722Encoder>,
    pub g722_decoder: RefCell<G722Decoder>,
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

    pub fn decode(&self, payload_type: u8, payload: &[u8]) -> Result<Vec<i16>> {
        match payload_type {
            0 => self.pcmu_decoder.borrow().decode(payload),
            8 => self.pcma_decoder.borrow().decode(payload),
            9 => self.g722_decoder.borrow().decode(payload),
            _ => Err(anyhow::anyhow!(
                "Unsupported payload type: {}",
                payload_type
            )),
        }
    }

    pub fn encode(&self, payload_type: u8, audio_frame: AudioFrame) -> Result<Bytes> {
        let payload = match audio_frame.samples {
            Samples::PCM(pcm) => pcm,
            Samples::RTP(_, payload) => return Ok(Bytes::from(payload)),
            Samples::Empty => return Err(anyhow::anyhow!("Empty payload")),
        };

        match payload_type {
            0 => self.pcmu_encoder.borrow_mut().encode(&payload),
            8 => self.pcma_encoder.borrow_mut().encode(&payload),
            9 => self.g722_encoder.borrow_mut().encode(&payload),
            _ => Err(anyhow::anyhow!(
                "Unsupported payload type: {}",
                payload_type
            )),
        }
    }
}
