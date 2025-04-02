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
    fn decode(&mut self, samples: &[u8]) -> Vec<i16> {
        samples.iter().map(|sample| decode_a_law(*sample)).collect()
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

    fn linear2alaw(&self, pcm_val: i16) -> u8 {
        // Special case handling for small negative values [-8, -1]
        if pcm_val < 0 && pcm_val >= -8 {
            return 0xD5;
        }

        // Determine sign mask and prepare the positive sample value
        let (mask, abs_val) = if pcm_val >= 0 {
            (0xD5, pcm_val) // sign bit = 1
        } else {
            (0x55, -pcm_val - 8) // sign bit = 0
        };

        // Convert the scaled magnitude to segment number
        let seg = self.search(abs_val, &SEG_END, 8);

        // If out of range, return maximum value
        if seg >= 8 {
            return (0x7F ^ mask) as u8;
        }

        // Calculate the base value from segment
        let aval = (seg as i16) << SEG_SHIFT;

        // Combine the segment value with the quantization bits
        let shift = if seg < 2 { 4 } else { seg as i16 + 3 };
        let result = aval | ((abs_val >> shift) & QUANT_MASK);

        // Apply the mask to set the sign bit
        (result ^ mask) as u8
    }
}

impl Encoder for PcmaEncoder {
    fn encode(&mut self, samples: &[i16]) -> Vec<u8> {
        let mut output = Vec::with_capacity(samples.len());
        for &sample in samples {
            output.push(self.linear2alaw(sample));
        }
        output
    }

    fn sample_rate(&self) -> u32 {
        8000
    }

    fn channels(&self) -> u16 {
        1
    }
}
