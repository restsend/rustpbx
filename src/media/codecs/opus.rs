use super::{Decoder, Encoder};
use crate::{PcmBuf, Sample};
use opus::{Application, Channels, Decoder as OpusDecoderCore, Encoder as OpusEncoderCore};

/// Opus audio decoder
pub struct OpusDecoder {
    decoder: OpusDecoderCore,
    sample_rate: u32,
    channels: u16,
}

impl OpusDecoder {
    /// Create a new Opus decoder instance
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let channels = if channels == 1 {
            Channels::Mono
        } else {
            Channels::Stereo
        };

        let decoder = match OpusDecoderCore::new(sample_rate, channels) {
            Ok(decoder) => decoder,
            Err(e) => {
                panic!("Failed to create Opus decoder: {}", e);
            }
        };

        Self {
            decoder,
            sample_rate,
            channels: if matches!(channels, Channels::Mono) {
                1
            } else {
                2
            },
        }
    }

    /// Create a default Opus decoder (48kHz, mono)
    pub fn new_default() -> Self {
        Self::new(48000, 1)
    }
}

unsafe impl Send for OpusDecoder {}
unsafe impl Sync for OpusDecoder {}

impl Decoder for OpusDecoder {
    fn decode(&mut self, data: &[u8]) -> PcmBuf {
        // Allocate output buffer - Opus can decode up to 120ms of audio
        // 48kHz * 0.12s * 2(stereo) = 11520 samples
        let max_samples = 11520;
        let mut output = vec![0i16; max_samples];

        match self.decoder.decode(data, &mut output, false) {
            Ok(len) => {
                output.truncate(len);
                output
            }
            Err(_) => {
                // If decoding fails, return empty buffer
                vec![]
            }
        }
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

/// Opus audio encoder
pub struct OpusEncoder {
    encoder: OpusEncoderCore,
    sample_rate: u32,
    channels: u16,
}

impl OpusEncoder {
    /// Create a new Opus encoder instance
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let channels_enum = if channels == 1 {
            Channels::Mono
        } else {
            Channels::Stereo
        };

        let encoder = match OpusEncoderCore::new(sample_rate, channels_enum, Application::Voip) {
            Ok(encoder) => encoder,
            Err(e) => {
                panic!("Failed to create Opus encoder: {}", e);
            }
        };

        Self {
            encoder,
            sample_rate,
            channels,
        }
    }

    /// Create a default Opus encoder (48kHz, mono)
    pub fn new_default() -> Self {
        Self::new(48000, 1)
    }
}

unsafe impl Send for OpusEncoder {}
unsafe impl Sync for OpusEncoder {}

impl Encoder for OpusEncoder {
    fn encode(&mut self, samples: &[Sample]) -> Vec<u8> {
        // Allocate output buffer - Opus encoded data is typically smaller than raw data
        let mut output = vec![0u8; samples.len()];

        match self.encoder.encode(samples, &mut output) {
            Ok(len) => {
                output.truncate(len);
                output
            }
            Err(_) => {
                // If encoding fails, return empty buffer
                vec![]
            }
        }
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}
