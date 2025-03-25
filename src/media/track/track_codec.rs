use crate::media::{
    codecs::{
        g722::{G722Decoder, G722Encoder},
        pcma::{PcmaDecoder, PcmaEncoder},
        pcmu::{PcmuDecoder, PcmuEncoder},
        Encoder,
    },
    processor::{AudioFrame, AudioPayload},
};
use anyhow::Result;
use bytes::Bytes;
use std::sync::Mutex;

pub struct TrackCodec {
    pub pcmu_encoder: Mutex<PcmuEncoder>,
    pub pcmu_decoder: Mutex<PcmuDecoder>,
    pub pcma_encoder: Mutex<PcmaEncoder>,
    pub pcma_decoder: Mutex<PcmaDecoder>,

    pub g722_encoder: Mutex<G722Encoder>,
    pub g722_decoder: Mutex<G722Decoder>,
}

impl TrackCodec {
    pub fn new() -> Self {
        Self {
            pcmu_encoder: Mutex::new(PcmuEncoder::new()),
            pcmu_decoder: Mutex::new(PcmuDecoder::new()),
            pcma_encoder: Mutex::new(PcmaEncoder::new()),
            pcma_decoder: Mutex::new(PcmaDecoder::new()),
            g722_encoder: Mutex::new(G722Encoder::new()),
            g722_decoder: Mutex::new(G722Decoder::new()),
        }
    }

    pub fn encode(&self, payload_type: u8, audio_frame: AudioFrame) -> Result<Bytes> {
        let payload = match audio_frame.samples {
            AudioPayload::PCM(pcm) => pcm,
            AudioPayload::RTP(_, payload) => return Ok(Bytes::from(payload)),
            AudioPayload::Empty => return Err(anyhow::anyhow!("Empty payload")),
        };

        match payload_type {
            0 => self.pcmu_encoder.lock().unwrap().encode(&payload),
            8 => self.pcma_encoder.lock().unwrap().encode(&payload),
            9 => self.g722_encoder.lock().unwrap().encode(&payload),
            _ => Err(anyhow::anyhow!(
                "Unsupported payload type: {}",
                payload_type
            )),
        }
    }
}
