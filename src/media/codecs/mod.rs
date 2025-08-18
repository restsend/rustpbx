use crate::{PcmBuf, Sample};
pub mod g722;
#[cfg(feature = "opus")]
pub mod opus;
pub mod pcma;
pub mod pcmu;
pub mod resample;
pub mod telephone_event;
#[cfg(test)]
mod tests;
#[derive(Debug, Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub enum CodecType {
    PCMU,
    PCMA,
    G722,
    #[cfg(feature = "opus")]
    Opus,
    TelephoneEvent,
}

pub trait Decoder: Send + Sync {
    /// Decode encoded audio data into PCM samples
    fn decode(&mut self, data: &[u8]) -> PcmBuf;

    /// Get the sample rate of the decoded audio
    fn sample_rate(&self) -> u32;

    /// Get the number of channels
    fn channels(&self) -> u16;
}

pub trait Encoder: Send + Sync {
    /// Encode PCM samples into codec-specific format
    fn encode(&mut self, samples: &[Sample]) -> Vec<u8>;

    /// Get the sample rate expected for input samples
    fn sample_rate(&self) -> u32;

    /// Get the number of channels expected for input
    fn channels(&self) -> u16;
}

pub fn create_decoder(codec: CodecType) -> Box<dyn Decoder> {
    match codec {
        CodecType::PCMU => Box::new(pcmu::PcmuDecoder::new()),
        CodecType::PCMA => Box::new(pcma::PcmaDecoder::new()),
        CodecType::G722 => Box::new(g722::G722Decoder::new()),
        #[cfg(feature = "opus")]
        CodecType::Opus => Box::new(opus::OpusDecoder::new_default()),
        CodecType::TelephoneEvent => Box::new(telephone_event::TelephoneEventDecoder::new()),
    }
}

pub fn create_encoder(codec: CodecType) -> Box<dyn Encoder> {
    match codec {
        CodecType::PCMU => Box::new(pcmu::PcmuEncoder::new()),
        CodecType::PCMA => Box::new(pcma::PcmaEncoder::new()),
        CodecType::G722 => Box::new(g722::G722Encoder::new()),
        #[cfg(feature = "opus")]
        CodecType::Opus => Box::new(opus::OpusEncoder::new_default()),
        CodecType::TelephoneEvent => Box::new(telephone_event::TelephoneEventEncoder::new()),
    }
}

impl CodecType {
    pub fn mime_type(&self) -> &str {
        match self {
            CodecType::PCMU => "audio/PCMU",
            CodecType::PCMA => "audio/PCMA",
            CodecType::G722 => "audio/G722",
            #[cfg(feature = "opus")]
            CodecType::Opus => "audio/opus",
            CodecType::TelephoneEvent => "audio/telephone-event",
        }
    }
    pub fn rtpmap(&self) -> &str {
        match self {
            CodecType::PCMU => "PCMU/8000",
            CodecType::PCMA => "PCMA/8000",
            CodecType::G722 => "G722/16000",
            #[cfg(feature = "opus")]
            CodecType::Opus => "opus/48000",
            CodecType::TelephoneEvent => "telephone-event/8000",
        }
    }

    pub fn clock_rate(&self) -> u32 {
        match self {
            CodecType::PCMU => 8000,
            CodecType::PCMA => 8000,
            CodecType::G722 => 8000,
            #[cfg(feature = "opus")]
            CodecType::Opus => 48000,
            CodecType::TelephoneEvent => 8000,
        }
    }
    pub fn payload_type(&self) -> u8 {
        match self {
            CodecType::PCMU => 0,
            CodecType::PCMA => 8,
            CodecType::G722 => 9,
            CodecType::Opus => 111, // Dynamic payload type
            CodecType::TelephoneEvent => 101,
        }
    }
    pub fn samplerate(&self) -> u32 {
        match self {
            CodecType::PCMU => 8000,
            CodecType::PCMA => 8000,
            CodecType::G722 => 16000,
            #[cfg(feature = "opus")]
            CodecType::Opus => 48000,
            CodecType::TelephoneEvent => 8000,
        }
    }
    pub fn is_audio(&self) -> bool {
        match self {
            CodecType::PCMU | CodecType::PCMA | CodecType::G722 => true,
            #[cfg(feature = "opus")]
            CodecType::Opus => true,
            _ => false,
        }
    }
}

impl TryFrom<&String> for CodecType {
    type Error = anyhow::Error;

    fn try_from(value: &String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "0" => Ok(CodecType::PCMU),
            "8" => Ok(CodecType::PCMA),
            "9" => Ok(CodecType::G722),
            #[cfg(feature = "opus")]
            "111" => Ok(CodecType::Opus), // Dynamic payload type
            "101" => Ok(CodecType::TelephoneEvent),
            _ => Err(anyhow::anyhow!("Invalid codec type: {}", value)),
        }
    }
}
#[cfg(target_endian = "little")]
pub fn samples_to_bytes(samples: &[Sample]) -> Vec<u8> {
    unsafe {
        std::slice::from_raw_parts(
            samples.as_ptr() as *const u8,
            samples.len() * std::mem::size_of::<Sample>(),
        )
        .to_vec()
    }
}

#[cfg(target_endian = "big")]
pub fn samples_to_bytes(samples: &[Sample]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

#[cfg(target_endian = "little")]
pub fn bytes_to_samples(u8_data: &[u8]) -> PcmBuf {
    unsafe {
        std::slice::from_raw_parts(
            u8_data.as_ptr() as *const Sample,
            u8_data.len() / std::mem::size_of::<Sample>(),
        )
        .to_vec()
    }
}
#[cfg(target_endian = "big")]
pub fn bytes_to_samples(u8_data: &[u8]) -> PcmBuf {
    u8_data
        .chunks(2)
        .map(|chunk| (chunk[0] as i16) | ((chunk[1] as i16) << 8))
        .collect()
}
