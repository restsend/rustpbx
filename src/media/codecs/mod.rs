use crate::media::{PcmBuf, Sample};
pub mod g722;
#[cfg(feature = "g729")]
pub mod g729;
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
    #[cfg(feature = "g729")]
    G729,
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
        #[cfg(feature = "g729")]
        CodecType::G729 => Box::new(g729::G729Decoder::new()),
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
        #[cfg(feature = "g729")]
        CodecType::G729 => Box::new(g729::G729Encoder::new()),
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
            #[cfg(feature = "g729")]
            CodecType::G729 => "audio/G729",
            #[cfg(feature = "opus")]
            CodecType::Opus => "audio/opus",
            CodecType::TelephoneEvent => "audio/telephone-event",
        }
    }
    pub fn rtpmap(&self) -> &str {
        match self {
            CodecType::PCMU => "PCMU/8000",
            CodecType::PCMA => "PCMA/8000",
            CodecType::G722 => "G722/8000",
            #[cfg(feature = "g729")]
            CodecType::G729 => "G729/8000",
            #[cfg(feature = "opus")]
            CodecType::Opus => "opus/48000/2",
            CodecType::TelephoneEvent => "telephone-event/8000",
        }
    }
    pub fn fmtp(&self) -> Option<&str> {
        match self {
            CodecType::PCMU => None,
            CodecType::PCMA => None,
            CodecType::G722 => None,
            #[cfg(feature = "g729")]
            CodecType::G729 => None,
            #[cfg(feature = "opus")]
            CodecType::Opus => Some("minptime=10;useinbandfec=1"),
            CodecType::TelephoneEvent => Some("0-16"),
        }
    }

    pub fn clock_rate(&self) -> u32 {
        match self {
            CodecType::PCMU => 8000,
            CodecType::PCMA => 8000,
            CodecType::G722 => 8000,
            #[cfg(feature = "g729")]
            CodecType::G729 => 8000,
            #[cfg(feature = "opus")]
            CodecType::Opus => 48000,
            CodecType::TelephoneEvent => 8000,
        }
    }

    pub fn channels(&self) -> u16 {
        match self {
            #[cfg(feature = "opus")]
            CodecType::Opus => 2,
            _ => 1,
        }
    }

    pub fn payload_type(&self) -> u8 {
        match self {
            CodecType::PCMU => 0,
            CodecType::PCMA => 8,
            CodecType::G722 => 9,
            #[cfg(feature = "g729")]
            CodecType::G729 => 18,
            #[cfg(feature = "opus")]
            CodecType::Opus => 111,
            CodecType::TelephoneEvent => 101,
        }
    }
    pub fn samplerate(&self) -> u32 {
        match self {
            CodecType::PCMU => 8000,
            CodecType::PCMA => 8000,
            CodecType::G722 => 16000,
            #[cfg(feature = "g729")]
            CodecType::G729 => 8000,
            #[cfg(feature = "opus")]
            CodecType::Opus => 48000,
            CodecType::TelephoneEvent => 8000,
        }
    }
    pub fn is_audio(&self) -> bool {
        match self {
            CodecType::PCMU | CodecType::PCMA | CodecType::G722 => true,
            #[cfg(feature = "g729")]
            CodecType::G729 => true,
            #[cfg(feature = "opus")]
            CodecType::Opus => true,
            _ => false,
        }
    }

    pub fn is_dynamic(&self) -> bool {
        match self {
            #[cfg(feature = "opus")]
            CodecType::Opus => true,
            CodecType::TelephoneEvent => true,
            _ => false,
        }
    }
}

impl TryFrom<u8> for CodecType {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(CodecType::PCMU),
            8 => Ok(CodecType::PCMA),
            9 => Ok(CodecType::G722),
            #[cfg(feature = "g729")]
            18 => Ok(CodecType::G729), // Static payload type
            #[cfg(feature = "opus")]
            // Dynamic payload type shoulw get from the rtpmap in sdp offer, leave this for backward compatibility
            101 => Ok(CodecType::TelephoneEvent),
            111 => Ok(CodecType::Opus), // Dynamic payload type
            _ => Err(anyhow::anyhow!("Invalid codec type: {}", value)),
        }
    }
}

/// Parse an rtpmap string like "96 opus/48000/2" into its components
///
/// Assumes RFC 4566/8866 compliant format:
/// `<payload type> <encoding name>/<clock rate>[/<encoding parameters>]`
///
/// Returns: (payload_type, codec_type, clock_rate, channel_count)
///
/// # Examples
/// ```
/// use rustpbx::media::codecs::{parse_rtpmap, CodecType};
///
/// let (pt, codec, rate, channels) = parse_rtpmap("96 opus/48000/2").unwrap();
/// assert_eq!(pt, 96);
/// assert_eq!(codec, CodecType::Opus);
/// assert_eq!(rate, 48000);
/// assert_eq!(channels, 2);
/// ```
pub fn parse_rtpmap(rtpmap: &str) -> Result<(u8, CodecType, u32, u16), anyhow::Error> {
    if let [payload_type_str, codec_spec] = rtpmap.split(' ').collect::<Vec<&str>>().as_slice() {
        // Parse payload type
        let payload_type = payload_type_str
            .parse::<u8>()
            .map_err(|e| anyhow::anyhow!("Failed to parse payload type: {}", e))?;
        let codec_parts: Vec<&str> = codec_spec.split('/').collect();

        if let [codec_name, clock_rate_str, channel_count @ ..] = codec_parts.as_slice() {
            let codec_type = match codec_name.to_lowercase().as_str() {
                "pcmu" => CodecType::PCMU,
                "pcma" => CodecType::PCMA,
                "g722" => CodecType::G722,
                #[cfg(feature = "g729")]
                "g729" => CodecType::G729,
                #[cfg(feature = "opus")]
                "opus" => CodecType::Opus,
                "telephone-event" => CodecType::TelephoneEvent,
                _ => return Err(anyhow::anyhow!("Unsupported codec name: {}", codec_name)),
            };

            let clock_rate = clock_rate_str
                .parse::<u32>()
                .map_err(|e| anyhow::anyhow!("Failed to parse clock rate: {}", e))?;

            let channel_count = match channel_count {
                ["2"] => 2,
                _ => 1,
            };
            Ok((payload_type, codec_type, clock_rate, channel_count))
        } else {
            return Err(anyhow::anyhow!("Invalid codec specification in rtpmap"));
        }
    } else {
        Err(anyhow::anyhow!(
            "Invalid rtpmap format: missing space between payload type and encoding name"
        ))
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
