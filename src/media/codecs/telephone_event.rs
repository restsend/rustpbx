use super::{Decoder, Encoder};
use crate::{PcmBuf, Sample};

#[derive(Default)]
pub struct TelephoneEventDecoder {}

impl TelephoneEventDecoder {
    pub fn new() -> Self {
        Self {}
    }
}

impl Decoder for TelephoneEventDecoder {
    fn decode(&mut self, _samples: &[u8]) -> PcmBuf {
        vec![]
    }

    fn sample_rate(&self) -> u32 {
        8000
    }

    fn channels(&self) -> u16 {
        1
    }
}

#[derive(Default)]
pub struct TelephoneEventEncoder {}

impl TelephoneEventEncoder {
    pub fn new() -> Self {
        Self {}
    }
}

impl Encoder for TelephoneEventEncoder {
    fn encode(&mut self, _samples: &[Sample]) -> Vec<u8> {
        vec![]
    }

    fn sample_rate(&self) -> u32 {
        8000
    }

    fn channels(&self) -> u16 {
        1
    }
}
