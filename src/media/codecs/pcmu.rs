use super::{Decoder, Encoder};
use crate::{PcmBuf, Sample};

const BIAS: i16 = 0x84;
const CLIP: i16 = 32635;

static MULAW_DECODE_TABLE: [i16; 256] = [
    -32124, -31100, -30076, -29052, -28028, -27004, -25980, -24956, -23932, -22908, -21884, -20860,
    -19836, -18812, -17788, -16764, -15996, -15484, -14972, -14460, -13948, -13436, -12924, -12412,
    -11900, -11388, -10876, -10364, -9852, -9340, -8828, -8316, -7932, -7676, -7420, -7164, -6908,
    -6652, -6396, -6140, -5884, -5628, -5372, -5116, -4860, -4604, -4348, -4092, -3900, -3772,
    -3644, -3516, -3388, -3260, -3132, -3004, -2876, -2748, -2620, -2492, -2364, -2236, -2108,
    -1980, -1884, -1820, -1756, -1692, -1628, -1564, -1500, -1436, -1372, -1308, -1244, -1180,
    -1116, -1052, -988, -924, -876, -844, -812, -780, -748, -716, -684, -652, -620, -588, -556,
    -524, -492, -460, -428, -396, -372, -356, -340, -324, -308, -292, -276, -260, -244, -228, -212,
    -196, -180, -164, -148, -132, -120, -112, -104, -96, -88, -80, -72, -64, -56, -48, -40, -32,
    -24, -16, -8, 0, 32124, 31100, 30076, 29052, 28028, 27004, 25980, 24956, 23932, 22908, 21884,
    20860, 19836, 18812, 17788, 16764, 15996, 15484, 14972, 14460, 13948, 13436, 12924, 12412,
    11900, 11388, 10876, 10364, 9852, 9340, 8828, 8316, 7932, 7676, 7420, 7164, 6908, 6652, 6396,
    6140, 5884, 5628, 5372, 5116, 4860, 4604, 4348, 4092, 3900, 3772, 3644, 3516, 3388, 3260, 3132,
    3004, 2876, 2748, 2620, 2492, 2364, 2236, 2108, 1980, 1884, 1820, 1756, 1692, 1628, 1564, 1500,
    1436, 1372, 1308, 1244, 1180, 1116, 1052, 988, 924, 876, 844, 812, 780, 748, 716, 684, 652,
    620, 588, 556, 524, 492, 460, 428, 396, 372, 356, 340, 324, 308, 292, 276, 260, 244, 228, 212,
    196, 180, 164, 148, 132, 120, 112, 104, 96, 88, 80, 72, 64, 56, 48, 40, 32, 24, 16, 8, 0,
];

/// Decodes a single μ-law encoded byte to a 16-bit PCM sample
fn decode_mu_law(mu_law_sample: u8) -> i16 {
    MULAW_DECODE_TABLE[mu_law_sample as usize]
}

/// Decoder for μ-law (PCMU) format
#[derive(Default)]
pub struct PcmuDecoder {}

impl PcmuDecoder {
    /// Creates a new PcmuDecoder instance
    pub fn new() -> Self {
        Self {}
    }
}

impl Decoder for PcmuDecoder {
    fn decode(&mut self, data: &[u8]) -> PcmBuf {
        data.iter().map(|&sample| decode_mu_law(sample)).collect()
    }

    fn sample_rate(&self) -> u32 {
        8000
    }

    fn channels(&self) -> u16 {
        1
    }
}

/// Encoder for μ-law (PCMU) format
#[derive(Default)]
pub struct PcmuEncoder {}

impl PcmuEncoder {
    /// Creates a new PcmuEncoder instance
    pub fn new() -> Self {
        Self {}
    }

    /// Converts a linear 16-bit PCM sample to a μ-law encoded byte
    fn linear2ulaw(&self, sample: i16) -> u8 {
        // Clip the sample to prevent overflow
        let mut sample = if sample > CLIP {
            CLIP
        } else if sample < -CLIP {
            -CLIP
        } else {
            sample
        };

        // Get the sample's sign and make it positive
        let sign = if sample < 0 { 0x80 } else { 0x00 };
        if sign == 0x80 {
            sample = -sample;
        }

        // Add bias
        sample += BIAS;

        // Compute segment number and step size
        let mut segment = 0;
        let mut value = sample;
        while value >= 256 {
            segment += 1;
            value >>= 1;
        }

        // Combine sign, segment, and quantization
        let uval = if segment >= 8 {
            0x7F ^ sign
        } else {
            let shift = if segment == 0 { 7 } else { segment + 3 };
            let mask = 0xFF ^ (0xFF >> shift);
            (sign | (segment << 4) | ((sample >> (segment + 3)) & 0x0F)) ^ mask
        };

        uval as u8
    }
}

impl Encoder for PcmuEncoder {
    fn encode(&mut self, samples: &[Sample]) -> Vec<u8> {
        samples
            .iter()
            .map(|&sample| self.linear2ulaw(sample))
            .collect()
    }

    fn sample_rate(&self) -> u32 {
        8000
    }

    fn channels(&self) -> u16 {
        1
    }
}
