use anyhow::Result;
use bytes::Bytes;

pub mod g722;
pub mod pcma;
pub mod pcmu;
pub mod resample;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CodecType {
    PCMU,
    PCMA,
    G722,
}

pub trait Decoder: Send + Sync {
    /// Decode encoded audio data into PCM samples
    fn decode(&self, data: &[u8]) -> Result<Vec<i16>>;

    /// Get the sample rate of the decoded audio
    fn sample_rate(&self) -> u32;

    /// Get the number of channels
    fn channels(&self) -> u16;
}

pub trait Encoder: Send + Sync {
    /// Encode PCM samples into codec-specific format
    fn encode(&mut self, samples: &[i16]) -> Result<Bytes>;

    /// Get the sample rate expected for input samples
    fn sample_rate(&self) -> u32;

    /// Get the number of channels expected for input
    fn channels(&self) -> u16;
}

pub fn create_decoder(codec: CodecType) -> Result<Box<dyn Decoder>> {
    match codec {
        CodecType::PCMU => Ok(Box::new(pcmu::PcmuDecoder::new())),
        CodecType::PCMA => Ok(Box::new(pcma::PcmaDecoder::new())),
        CodecType::G722 => Ok(Box::new(g722::G722Decoder::new())),
    }
}

pub fn create_encoder(codec: CodecType) -> Result<Box<dyn Encoder>> {
    match codec {
        CodecType::PCMU => Ok(Box::new(pcmu::PcmuEncoder::new())),
        CodecType::PCMA => Ok(Box::new(pcma::PcmaEncoder::new())),
        CodecType::G722 => Ok(Box::new(g722::G722Encoder::new())),
    }
}

pub struct DecoderFactory {}

impl DecoderFactory {
    pub fn new() -> Self {
        Self {}
    }

    pub fn create_decoder(&self, codec: CodecType) -> Result<Box<dyn Decoder>> {
        create_decoder(codec)
    }
}

pub fn convert_s16_to_u8(s16_data: &[i16]) -> Vec<u8> {
    let mut u8_data = Vec::with_capacity(s16_data.len() * 2);
    for &s in s16_data {
        u8_data.push((s & 0xFF) as u8);
        u8_data.push((s >> 8) as u8);
    }
    u8_data
}
