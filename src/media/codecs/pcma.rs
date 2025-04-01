use anyhow::Result;
use bytes::{Bytes, BytesMut};

use super::{Decoder, Encoder};

const SEG_SHIFT: i16 = 4;
const QUANT_MASK: i16 = 0x0F;
static SEG_END: [i16; 8] = [0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF];

// A-law decode table (same as Go's alaw2lpcm)
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

pub struct PcmaDecoder {}

impl PcmaDecoder {
    pub fn new() -> Self {
        Self {}
    }
}

impl Decoder for PcmaDecoder {
    fn decode(&mut self, samples: &[u8]) -> Result<Vec<i16>> {
        let output = samples
            .iter()
            .map(|sample| decode_a_law(*sample))
            .collect::<Vec<i16>>();
        Ok(output)
    }

    fn sample_rate(&self) -> u32 {
        8000
    }

    fn channels(&self) -> u16 {
        1
    }
}

// Function to decode A-law encoded audio
fn decode_a_law(a_law_sample: u8) -> i16 {
    ALAW_DECODE_TABLE[a_law_sample as usize]
}

pub struct PcmaEncoder {}

impl PcmaEncoder {
    pub fn new() -> Self {
        Self {}
    }

    fn search(&self, val: i16, table: &[i16], size: usize) -> usize {
        for i in 0..size {
            if val <= table[i] {
                return i;
            }
        }
        size
    }

    fn linear2alaw(&self, mut pcm_val: i16) -> u8 {
        let mask: i16;
        let seg: usize;
        let aval: i16;

        if pcm_val >= 0 {
            mask = 0xD5; /* sign (7th) bit = 1 */
        } else if pcm_val < -8 {
            mask = 0x55; /* sign bit = 0 */
            pcm_val = -pcm_val - 8;
        } else {
            // For all values in range [-7; -1] end result will be 0^0xD5,
            // so we may just return it from here.
            // NOTE: This is not just optimization! For values in that range
            //       expression (-pcm_val - 8) will return negative result,
            //       messing all following calculations. This is old bug,
            //       coming from original code from Sun Microsystems, Inc.
            // Btw, seems there is no code for 0, value after decoding will be
            // either 8 (if we return 0xD5 here) or -8 (if we return 0x55 here).
            return 0xD5;
        }

        /* Convert the scaled magnitude to segment number. */
        seg = self.search(pcm_val, &SEG_END, 8);

        /* Combine the sign, segment, and quantization bits. */
        if seg >= 8 {
            /* out of range, return maximum value. */
            (0x7F ^ mask) as u8
        } else {
            aval = (seg as i16) << SEG_SHIFT;
            if seg < 2 {
                ((aval | ((pcm_val >> 4) & QUANT_MASK)) ^ mask) as u8
            } else {
                ((aval | ((pcm_val >> (seg + 3)) & QUANT_MASK)) ^ mask) as u8
            }
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
        8000
    }

    fn channels(&self) -> u16 {
        1
    }
}
