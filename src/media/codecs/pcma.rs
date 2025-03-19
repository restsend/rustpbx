use anyhow::Result;
use bytes::{Bytes, BytesMut};

use super::{Decoder, Encoder};

const CLIP: i16 = 32767;
const A_LAW_COMPRESS_THRESHOLD: i16 = 128;

static ALAW_DECODE_TABLE: [i16; 256] = [
    -5504, -5248, -6016, -5760, -4480, -4224, -4992, -4736, -7552, -7296, -8064, -7808, -6528,
    -6272, -7040, -6784, -2752, -2624, -3008, -2880, -2240, -2112, -2496, -2368, -3776, -3648,
    -4032, -3904, -3264, -3136, -3520, -3392, -22016, -20992, -24064, -23040, -17920, -16896,
    -19968, -18944, -30208, -29184, -32256, -31232, -26112, -25088, -28160, -27136, -11008, -10496,
    -12032, -11520, -8960, -8448, -9984, -9472, -15104, -14592, -16128, -15616, -13056, -12544,
    -14080, -13568, -344, -328, -376, -360, -280, -264, -312, -296, -472, -456, -504, -488, -408,
    -392, -440, -424, -88, -72, -120, -104, -24, -8, -56, -40, -216, -200, -248, -232, -152, -136,
    -184, -168, -1376, -1312, -1504, -1440, -1120, -1056, -1248, -1184, -1888, -1824, -2016, -1952,
    -1632, -1568, -1760, -1696, -688, -656, -752, -720, -560, -528, -624, -592, -944, -912, -1008,
    -976, -816, -784, -880, -848, 5504, 5248, 6016, 5760, 4480, 4224, 4992, 4736, 7552, 7296, 8064,
    7808, 6528, 6272, 7040, 6784, 2752, 2624, 3008, 2880, 2240, 2112, 2496, 2368, 3776, 3648, 4032,
    3904, 3264, 3136, 3520, 3392, 22016, 20992, 24064, 23040, 17920, 16896, 19968, 18944, 30208,
    29184, 32256, 31232, 26112, 25088, 28160, 27136, 11008, 10496, 12032, 11520, 8960, 8448, 9984,
    9472, 15104, 14592, 16128, 15616, 13056, 12544, 14080, 13568, 344, 328, 376, 360, 280, 264,
    312, 296, 472, 456, 504, 488, 408, 392, 440, 424, 88, 72, 120, 104, 24, 8, 56, 40, 216, 200,
    248, 232, 152, 136, 184, 168, 1376, 1312, 1504, 1440, 1120, 1056, 1248, 1184, 1888, 1824, 2016,
    1952, 1632, 1568, 1760, 1696, 688, 656, 752, 720, 560, 528, 624, 592, 944, 912, 1008, 976, 816,
    784, 880, 848,
];

pub struct PcmaDecoder {
    sample_rate: u32,
    channels: u16,
}

impl PcmaDecoder {
    pub fn new() -> Self {
        Self {
            sample_rate: 8000,
            channels: 1,
        }
    }
}

impl Decoder for PcmaDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<i16>> {
        let mut output = Vec::with_capacity(data.len());
        for &byte in data {
            output.push(ALAW_DECODE_TABLE[byte as usize]);
        }
        Ok(output)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

pub struct PcmaEncoder {
    sample_rate: u32,
    channels: u16,
}

impl PcmaEncoder {
    pub fn new() -> Self {
        Self {
            sample_rate: 8000,
            channels: 1,
        }
    }

    fn linear2alaw(&self, sample: i16) -> u8 {
        let sign = if sample < 0 { 0x80_u8 } else { 0_u8 };
        let mut abs_sample = sample.abs().min(CLIP) as i16;

        if abs_sample > A_LAW_COMPRESS_THRESHOLD {
            let mut exponent = 7;
            let mut mask = 0x4000;
            while (abs_sample & mask) == 0 && exponent > 0 {
                exponent -= 1;
                mask >>= 1;
            }
            let mantissa = ((abs_sample >> ((exponent == 0) as usize + 3)) & 0x0F) as u8;
            (sign | ((exponent << 4) as u8) | mantissa) ^ 0x55
        } else {
            (sign | ((abs_sample >> 4) & 0x0F) as u8) ^ 0x55
        }
    }
}

impl Encoder for PcmaEncoder {
    fn encode(&mut self, samples: &[i16]) -> Result<Bytes> {
        let mut output = BytesMut::with_capacity(samples.len());
        for &sample in samples {
            output.extend_from_slice(&[self.linear2alaw(sample)]);
        }
        Ok(output.freeze())
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}
