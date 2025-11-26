use super::{Decoder, Encoder};
use crate::media::{PcmBuf, Sample};

const L_FRAME: usize = 80; // 10ms frame at 8kHz
const L_FRAME_COMPRESSED: usize = 10; // G.729 frame size in bytes

/// G.729 audio decoder using g729-sys
pub struct G729Decoder {
    decoder: g729_sys::Decoder,
}

impl G729Decoder {
    /// Create a new G.729 decoder instance
    pub fn new() -> Self {
        Self {
            decoder: g729_sys::Decoder::new().expect("Failed to create g729 decoder"),
        }
    }
}

unsafe impl Send for G729Decoder {}
unsafe impl Sync for G729Decoder {}

impl Decoder for G729Decoder {
    fn decode(&mut self, data: &[u8]) -> PcmBuf {
        if data.is_empty() {
            return vec![];
        }

        // G.729 processes 10-byte frames
        let mut output = Vec::new();
        let mut pos = 0;

        while pos + L_FRAME_COMPRESSED <= data.len() {
            let frame_data = &data[pos..pos + L_FRAME_COMPRESSED];
            // g729-sys: decode(frame, bfi, vad, dtx) -> [i16;80]
            let decoded_frame = self.decoder.decode(frame_data, false, false, false);
            output.extend_from_slice(&decoded_frame);

            pos += L_FRAME_COMPRESSED;
        }

        output
    }

    fn sample_rate(&self) -> u32 {
        8000 // G.729 operates at 8kHz
    }

    fn channels(&self) -> u16 {
        1 // G.729 is always mono
    }
}

/// G.729 audio encoder using g729-sys
pub struct G729Encoder {
    encoder: g729_sys::Encoder,
}

impl G729Encoder {
    /// Create a new G.729 encoder instance
    pub fn new() -> Self {
        Self {
            // The argument typically enables/disables Annex B (VAD/DTX). Use false by default.
            encoder: g729_sys::Encoder::new(false).expect("Failed to create g729 encoder"),
        }
    }
}

unsafe impl Send for G729Encoder {}
unsafe impl Sync for G729Encoder {}

impl Encoder for G729Encoder {
    fn encode(&mut self, samples: &[Sample]) -> Vec<u8> {
        if samples.is_empty() {
            return vec![];
        }

        let mut output = Vec::new();
        let mut pos = 0;

        // Process samples in 80-sample (10ms @ 8kHz) frames
        while pos + L_FRAME <= samples.len() {
            let mut frame_arr = [0i16; L_FRAME];
            frame_arr.copy_from_slice(&samples[pos..pos + L_FRAME]);

            let encoded_frame = self.encoder.encode(&frame_arr);
            output.extend_from_slice(&encoded_frame);

            pos += L_FRAME;
        }

        output
    }

    fn sample_rate(&self) -> u32 {
        8000 // G.729 operates at 8kHz
    }

    fn channels(&self) -> u16 {
        1 // G.729 is always mono
    }
}
