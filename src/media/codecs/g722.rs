use super::{Decoder, Encoder};
use crate::{PcmBuf, Sample};
pub enum Bitrate {
    Mode1_64000,
    Mode2_56000,
    Mode3_48000,
}

// Quantization decision thresholds used in the encoder
const QUANT_DECISION_LEVEL: [i32; 32] = [
    0, 35, 72, 110, 150, 190, 233, 276, 323, 370, 422, 473, 530, 587, 650, 714, 786, 858, 940,
    1023, 1121, 1219, 1339, 1458, 1612, 1765, 1980, 2195, 2557, 2919, 0, 0,
];

// Negative quantization interval indices
const QUANT_INDEX_NEG: [i32; 32] = [
    0, 63, 62, 31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11,
    10, 9, 8, 7, 6, 5, 4, 0,
];

// Positive quantization interval indices
const QUANT_INDEX_POS: [i32; 32] = [
    0, 61, 60, 59, 58, 57, 56, 55, 54, 53, 52, 51, 50, 49, 48, 47, 46, 45, 44, 43, 42, 41, 40, 39,
    38, 37, 36, 35, 34, 33, 32, 0,
];

// Scale factor adaptation table for low band
const SCALE_FACTOR_ADJUST_LOW: [i32; 8] = [-60, -30, 58, 172, 334, 538, 1198, 3042];

// Mapping from 4 bits of low band code to 3 bits of logarithmic scale factor
const LOG_SCALE_FACTOR_MAP: [i32; 16] = [0, 7, 6, 5, 4, 3, 2, 1, 7, 6, 5, 4, 3, 2, 1, 0];

// Inverse logarithmic base for computing the scale factor
const INV_LOG_BASE: [i32; 32] = [
    2048, 2093, 2139, 2186, 2233, 2282, 2332, 2383, 2435, 2489, 2543, 2599, 2656, 2714, 2774, 2834,
    2896, 2960, 3025, 3091, 3158, 3228, 3298, 3371, 3444, 3520, 3597, 3676, 3756, 3838, 3922, 4008,
];

// Quantizer multipliers for 4-bit low band
const QUANT_MULT_LOW_4BIT: [i32; 16] = [
    0, -20456, -12896, -8968, -6288, -4240, -2584, -1200, 20456, 12896, 8968, 6288, 4240, 2584,
    1200, 0,
];

// Quantizer multipliers for 2-bit high band
const QUANT_MULT_HIGH_2BIT: [i32; 4] = [-7408, -1616, 7408, 1616];

// QMF filter coefficients for band splitting and reconstruction
const QMF_FILTER_COEFS: [i32; 12] = [3, -11, 12, 32, -210, 951, 3876, -805, 362, -156, 53, -11];

// Negative high band quantization indices
const HIGH_QUANT_INDEX_NEG: [i32; 3] = [0, 1, 0];

// Positive high band quantization indices
const HIGH_QUANT_INDEX_POS: [i32; 3] = [0, 3, 2];

// Scale factor adaptation table for high band
const SCALE_FACTOR_ADJUST_HIGH: [i32; 3] = [0, -214, 798];

// Mapping from 2 bits of high band code to 2 bits of logarithmic scale factor
const HIGH_LOG_SCALE_MAP: [i32; 4] = [2, 1, 2, 1];

// Quantizer multipliers for 5-bit quantization (56kbps mode)
const QUANT_MULT_56K: [i32; 32] = [
    -280, -280, -23352, -17560, -14120, -11664, -9752, -8184, -6864, -5712, -4696, -3784, -2960,
    -2208, -1520, -880, 23352, 17560, 14120, 11664, 9752, 8184, 6864, 5712, 4696, 3784, 2960, 2208,
    1520, 880, 280, -280,
];

// Quantizer multipliers for 6-bit quantization (64kbps mode)
const QUANT_MULT_64K: [i32; 64] = [
    -136, -136, -136, -136, -24808, -21904, -19008, -16704, -14984, -13512, -12280, -11192, -10232,
    -9360, -8576, -7856, -7192, -6576, -6000, -5456, -4944, -4464, -4008, -3576, -3168, -2776,
    -2400, -2032, -1688, -1360, -1040, -728, 24808, 21904, 19008, 16704, 14984, 13512, 12280,
    11192, 10232, 9360, 8576, 7856, 7192, 6576, 6000, 5456, 4944, 4464, 4008, 3576, 3168, 2776,
    2400, 2032, 1688, 1360, 1040, 728, 432, 136, -432, -136,
];

impl Bitrate {
    fn bits_per_sample(&self) -> i32 {
        match self {
            Bitrate::Mode1_64000 => 8,
            Bitrate::Mode2_56000 => 7,
            Bitrate::Mode3_48000 => 6,
        }
    }
}

/// G.722 ADPCM band state structure used in the codec
/// Each band (lower and upper) uses an independent state structure
#[derive(Default)]
struct G722Band {
    /// Current signal prediction value (signal estimate)
    signal_estimate: i32,
    /// Pole filter output (result from IIR filter part)
    pole_filter_output: i32,
    /// Zero filter output (result from FIR filter part)
    zero_filter_output: i32,
    /// Reconstructed signal history [current, previous, previous-1]
    reconstructed_signal: [i32; 3],
    /// Pole filter coefficients [unused(0), a1, a2]
    pole_coefficients: [i32; 3],
    /// Temporary pole filter coefficients [unused(0), a1', a2']
    pole_coefficients_temp: [i32; 3],
    /// Partially reconstructed signal history [current, previous, previous-1]
    partial_reconstructed: [i32; 3],
    /// Difference signal history [current, previous, ..., previous-5]
    difference_signal: [i32; 7],
    /// Zero filter coefficients [unused(0), b1, b2, ..., b6]
    zero_coefficients: [i32; 7],
    /// Temporary zero filter coefficients [unused(0), b1', b2', ..., b6']
    zero_coefficients_temp: [i32; 7],
    /// Sign bits storage [current signal, previous, ..., previous-5]
    sign_bits: [i32; 7],
    /// Log scale factor (used for quantization and dequantization)
    log_scale_factor: i32,
    /// Quantizer step size (used for adaptive quantization)
    quantizer_step_size: i32,
}

fn saturate(amp: i32) -> i32 {
    amp.clamp(i16::MIN as i32, i16::MAX as i32)
}

/// Process Block 4 operations for G.722 ADPCM algorithm
/// This function performs the predictor adaptation and reconstruction steps
/// as defined in the G.722 standard
fn block4(band: &mut G722Band, d: i32) {
    // Block 4, RECONS - Reconstruct the signal
    // Set current prediction difference
    band.difference_signal[0] = d;
    // Reconstruct signal by adding signal estimate to difference signal
    band.reconstructed_signal[0] = saturate(band.signal_estimate + d);

    // Block 4, PARREC - Partial reconstruction
    // Used for predictor adaptation
    band.partial_reconstructed[0] = saturate(band.zero_filter_output + d);

    // Block 4, UPPOL2 - Update second predictor coefficient
    // Extract sign bits for adaptation logic
    for i in 0..3 {
        band.sign_bits[i] = band.partial_reconstructed[i] >> 15;
    }

    // Scale first predictor coefficient
    let a1_scaled = saturate(band.pole_coefficients[1] << 2);

    // Apply sign correlation logic for adaptation direction
    let mut a2_update = if band.sign_bits[0] == band.sign_bits[1] {
        -a1_scaled
    } else {
        a1_scaled
    };
    a2_update = a2_update.min(32767);

    // Apply second level of adaptation based on older samples
    let mut a2_adj = a2_update >> 7;
    if band.sign_bits[0] == band.sign_bits[2] {
        a2_adj += 128;
    } else {
        a2_adj -= 128;
    }

    // Complete second coefficient update with leakage factor
    a2_adj += (band.pole_coefficients[2] * 32512) >> 15;

    // Limit the coefficient range to prevent instability
    band.pole_coefficients_temp[2] = a2_adj.clamp(-12288, 12288);

    // Block 4, UPPOL1 - Update first predictor coefficient
    band.sign_bits[0] = band.partial_reconstructed[0] >> 15; // Current sign
    band.sign_bits[1] = band.partial_reconstructed[1] >> 15; // Previous sign

    // Apply sign correlation logic for first coefficient
    let sign_factor = if band.sign_bits[0] == band.sign_bits[1] {
        192
    } else {
        -192
    };
    let leakage = (band.pole_coefficients[1] * 32640) >> 15;

    // Calculate new coefficient with both sign and leakage factors
    band.pole_coefficients_temp[1] = saturate(sign_factor + leakage);

    // Calculate adjustment limit based on second coefficient
    let limit = saturate(15360 - band.pole_coefficients_temp[2]);

    // Constrain first coefficient based on second coefficient
    // This prevents instability in the filter
    if band.pole_coefficients_temp[1] > limit {
        band.pole_coefficients_temp[1] = limit;
    } else if band.pole_coefficients_temp[1] < -limit {
        band.pole_coefficients_temp[1] = -limit;
    }

    // Block 4, UPZERO - Update zero section (FIR) coefficients
    let step_size = if d == 0 { 0 } else { 128 };
    band.sign_bits[0] = d >> 15; // Sign of current difference

    // Update each zero filter coefficient
    for i in 1..7 {
        band.sign_bits[i] = band.difference_signal[i] >> 15; // Extract sign
                                                             // Apply sign correlation logic for adaptation direction
        let adj = if band.sign_bits[i] == band.sign_bits[0] {
            step_size
        } else {
            -step_size
        };
        // Apply leakage factor
        let leakage = (band.zero_coefficients[i] * 32640) >> 15;
        // Calculate new coefficient
        band.zero_coefficients_temp[i] = saturate(adj + leakage);
    }

    // Block 4, DELAYA - Delay updates for filter memory
    // Shift the difference signal memory
    for i in (1..7).rev() {
        band.difference_signal[i] = band.difference_signal[i - 1];
    }

    // Update filter coefficients
    for i in 1..7 {
        band.zero_coefficients[i] = band.zero_coefficients_temp[i];
    }

    // Shift pole filter memory
    for i in (1..3).rev() {
        band.reconstructed_signal[i] = band.reconstructed_signal[i - 1];
        band.partial_reconstructed[i] = band.partial_reconstructed[i - 1];
        band.pole_coefficients[i] = band.pole_coefficients_temp[i];
    }

    // Block 4, FILTEP - Pole section (IIR) filtering
    // Calculate contribution of the pole section to the signal estimate
    let r1_adj = saturate(band.reconstructed_signal[1] + band.reconstructed_signal[1]); // Scale by 2
    let pole1 = (band.pole_coefficients[1] * r1_adj) >> 15;

    let r2_adj = saturate(band.reconstructed_signal[2] + band.reconstructed_signal[2]); // Scale by 2
    let pole2 = (band.pole_coefficients[2] * r2_adj) >> 15;

    // Combined pole section output
    band.pole_filter_output = saturate(pole1 + pole2);

    // Block 4, FILTEZ - Zero section (FIR) filtering
    // Calculate contribution of the zero section to the signal estimate
    band.zero_filter_output = 0;
    for i in 1..7 {
        let d_adj = saturate(band.difference_signal[i] + band.difference_signal[i]); // Scale by 2
        band.zero_filter_output += (band.zero_coefficients[i] * d_adj) >> 15;
    }
    band.zero_filter_output = saturate(band.zero_filter_output);

    // Block 4, PREDIC - Prediction
    // Final signal estimate is sum of pole and zero section outputs
    band.signal_estimate = saturate(band.pole_filter_output + band.zero_filter_output);
}

pub struct G722Encoder {
    packed: bool,
    eight_k: bool,
    bits_per_sample: i32,
    x: [i32; 24],
    band: [G722Band; 2],
    out_buffer: u32,
    out_bits: i32,
}

pub struct G722Decoder {
    packed: bool,
    eight_k: bool,
    bits_per_sample: i32,
    x: [i32; 24],
    band: [G722Band; 2],
    in_buffer: u32,
    in_bits: i32,
}

impl G722Encoder {
    pub fn new() -> Self {
        Self::with_options(Bitrate::Mode1_64000, false, false)
    }

    /// Creates an encoder with specified bitrate and options
    pub fn with_options(rate: Bitrate, eight_k: bool, packed: bool) -> Self {
        let mut encoder = Self {
            packed,
            eight_k,
            bits_per_sample: rate.bits_per_sample(),
            x: [0; 24],
            band: [G722Band::default(), G722Band::default()],
            out_buffer: 0,
            out_bits: 0,
        };

        // Initialize band states with correct starting values
        encoder.band[0].log_scale_factor = 32 << 2; // Initial det value for lower band
        encoder.band[1].log_scale_factor = 8 << 2; // Initial det value for upper band

        encoder
    }

    /// Encode 16-bit PCM samples into G.722 format
    /// This function follows the G.722 standard algorithm exactly
    fn g722_encode(&mut self, amp: &[i16]) -> Vec<u8> {
        // Pre-allocate output buffer with appropriate capacity
        let mut output = Vec::with_capacity(amp.len() / 2 + 1);

        // Initialize processing variables for low and high bands
        let mut xlow: i32;
        let mut xhigh: i32 = 0;
        let mut input_idx = 0;

        // Process all input samples
        while input_idx < amp.len() {
            // Split input into low and high bands based on mode
            if self.eight_k {
                // 8kHz mode - Just use input directly with scaling
                xlow = amp[input_idx] as i32 >> 1;
                input_idx += 1;
            } else {
                // 16kHz mode - Apply QMF analysis filter to split bands
                // Shuffle buffer down to make room for new samples
                for i in 0..22 {
                    self.x[i] = self.x[i + 2];
                }

                // Add new samples to buffer
                self.x[22] = amp[input_idx] as i32;
                input_idx += 1;
                self.x[23] = if input_idx < amp.len() {
                    amp[input_idx] as i32
                } else {
                    0 // Handle edge case at end of buffer
                };
                input_idx += 1;

                // Apply QMF filter to split input into bands
                let mut sumeven = 0;
                let mut sumodd = 0;
                for i in 0..12 {
                    sumodd += self.x[2 * i] * QMF_FILTER_COEFS[i];
                    sumeven += self.x[2 * i + 1] * QMF_FILTER_COEFS[11 - i];
                }

                // Scale filter outputs to get low and high bands
                xlow = (sumeven + sumodd) >> 14;
                xhigh = (sumeven - sumodd) >> 14;
            }

            // Process low band (always performed)
            let code = if self.eight_k {
                // 8kHz mode - only low band matters
                self.encode_low_band(xlow, true)
            } else {
                // 16kHz mode - encode both bands
                let ilow = self.encode_low_band(xlow, false);
                let ihigh = self.encode_high_band(xhigh);
                (ihigh << 6 | ilow) >> (8 - self.bits_per_sample)
            };

            // Output the encoded code
            self.output_code(code, &mut output);
        }

        // Handle any remaining bits in the output buffer
        if self.packed && self.out_bits > 0 {
            output.push((self.out_buffer & 0xFF) as u8);
        }

        output
    }

    /// Encode low band sample and update state
    /// Returns the encoded low band bits
    fn encode_low_band(&mut self, xlow: i32, is_eight_k: bool) -> i32 {
        // Block 1L, SUBTRA - Calculate difference signal
        let el = saturate(xlow - self.band[0].signal_estimate);

        // Block 1L, QUANTL - Quantize difference signal
        let wd = if el >= 0 { el } else { -(el + 1) };

        // Find quantization interval
        let mut quantization_idx = 1;
        while quantization_idx < 30 {
            let decision_level =
                (QUANT_DECISION_LEVEL[quantization_idx] * self.band[0].log_scale_factor) >> 12;
            if wd < decision_level {
                break;
            }
            quantization_idx += 1;
        }

        // Select output bits based on sign
        let ilow = if el < 0 {
            QUANT_INDEX_NEG[quantization_idx]
        } else {
            QUANT_INDEX_POS[quantization_idx]
        };

        // Block 2L, INVQAL - Inverse quantize for prediction
        let ril = ilow >> 2;
        let wd2 = QUANT_MULT_LOW_4BIT[ril as usize];
        let dlow = (self.band[0].log_scale_factor * wd2) >> 15;

        // Block 3L, LOGSCL - Update scale factor
        let il4 = LOG_SCALE_FACTOR_MAP[ril as usize];
        let mut nb = (self.band[0].quantizer_step_size * 127) >> 7;
        nb += SCALE_FACTOR_ADJUST_LOW[il4 as usize];
        self.band[0].quantizer_step_size = nb.clamp(0, 18432);

        // Block 3L, SCALEL - Compute new quantizer scale factor
        let wd1 = self.band[0].quantizer_step_size >> 6 & 31;
        let wd2 = 8 - (self.band[0].quantizer_step_size >> 11);
        let wd3 = if wd2 < 0 {
            INV_LOG_BASE[wd1 as usize] << -wd2
        } else {
            INV_LOG_BASE[wd1 as usize] >> wd2
        };
        self.band[0].log_scale_factor = wd3 << 2;

        // Apply predictor adaptation (ADPCM core algorithm)
        block4(&mut self.band[0], dlow);

        // Return appropriate value based on mode
        if is_eight_k {
            ((0xc0 | ilow) >> 8) - self.bits_per_sample
        } else {
            ilow
        }
    }

    /// Encode high band sample and update state
    /// Returns the encoded high band bits
    fn encode_high_band(&mut self, xhigh: i32) -> i32 {
        // Block 1H, SUBTRA - Calculate difference signal
        let eh = saturate(xhigh - self.band[1].signal_estimate);

        // Block 1H, QUANTH - Quantize difference signal
        let wd = if eh >= 0 { eh } else { -(eh + 1) };
        let decision_level = (564 * self.band[1].log_scale_factor) >> 12;

        // Determine quantization level for high band (2-bit)
        let mih = if wd >= decision_level { 2 } else { 1 };
        let ihigh = if eh < 0 {
            HIGH_QUANT_INDEX_NEG[mih as usize]
        } else {
            HIGH_QUANT_INDEX_POS[mih as usize]
        };

        // Block 2H, INVQAH - Inverse quantize for prediction
        let wd2 = QUANT_MULT_HIGH_2BIT[ihigh as usize];
        let dhigh = (self.band[1].log_scale_factor * wd2) >> 15;

        // Block 3H, LOGSCH - Update scale factor
        let ih2 = HIGH_LOG_SCALE_MAP[ihigh as usize];
        let mut nb = (self.band[1].quantizer_step_size * 127) >> 7;
        nb += SCALE_FACTOR_ADJUST_HIGH[ih2 as usize];
        self.band[1].quantizer_step_size = nb.clamp(0, 22528);

        // Block 3H, SCALEH - Compute quantizer scale factor
        let wd1 = self.band[1].quantizer_step_size >> 6 & 31;
        let wd2 = 10 - (self.band[1].quantizer_step_size >> 11);
        let wd3 = if wd2 < 0 {
            INV_LOG_BASE[wd1 as usize] << -wd2
        } else {
            INV_LOG_BASE[wd1 as usize] >> wd2
        };
        self.band[1].log_scale_factor = wd3 << 2;

        // Apply predictor adaptation (ADPCM core algorithm)
        block4(&mut self.band[1], dhigh);

        ihigh
    }

    /// Add encoded bits to the output buffer
    fn output_code(&mut self, code: i32, output: &mut Vec<u8>) {
        if self.packed {
            // Pack the code bits across byte boundaries
            self.out_buffer |= (code as u32) << self.out_bits;
            self.out_bits += self.bits_per_sample;

            // When we have at least 8 bits, output a byte
            if self.out_bits >= 8 {
                output.push((self.out_buffer & 0xFF) as u8);
                self.out_bits -= 8;
                self.out_buffer >>= 8;
            }
        } else {
            // Direct byte-aligned output
            output.push(code as u8);
        }
    }
}

impl G722Decoder {
    pub fn new() -> Self {
        Self::with_options(Bitrate::Mode1_64000, false, false)
    }

    pub fn with_options(rate: Bitrate, packed: bool, eight_k: bool) -> Self {
        Self {
            packed,
            eight_k,
            bits_per_sample: rate.bits_per_sample(),
            x: Default::default(),
            band: Default::default(),
            in_buffer: 0,
            in_bits: 0,
        }
    }

    /// Extracts the next G.722 code from the input data stream
    fn extract_code(&mut self, data: &[u8], idx: &mut usize) -> i32 {
        if self.packed {
            // When packed, bits are combined across bytes
            if self.in_bits < self.bits_per_sample {
                self.in_buffer |= (data[*idx] as u32) << self.in_bits;
                *idx += 1;
                self.in_bits += 8;
            }
            let code = (self.in_buffer & ((1 << self.bits_per_sample) - 1) as u32) as i32;
            self.in_buffer >>= self.bits_per_sample;
            self.in_bits -= self.bits_per_sample;
            code
        } else {
            // Direct byte-based access when not packed
            let code = data[*idx] as i32;
            *idx += 1;
            code
        }
    }

    /// Parses the G.722 code into low-band word and high-band index based on bit rate
    fn parse_code(&self, code: i32) -> (i32, i32, i32) {
        // Returns (wd1, ihigh, wd2) tuple: low-band word, high-band index, and scaled value
        match self.bits_per_sample {
            7 => {
                // 56 kbit/s mode
                let wd1 = code & 0x1f;
                let ihigh = (code >> 5) & 0x3;
                let wd2 = QUANT_MULT_56K[wd1 as usize];
                (wd1 >> 1, ihigh, wd2)
            }
            6 => {
                // 48 kbit/s mode
                let wd1 = code & 0xf;
                let ihigh = (code >> 4) & 0x3;
                let wd2 = QUANT_MULT_LOW_4BIT[wd1 as usize];
                (wd1, ihigh, wd2)
            }
            _ => {
                // 64 kbit/s mode (default)
                let wd1 = code & 0x3f;
                let ihigh = (code >> 6) & 0x3;
                let wd2 = QUANT_MULT_64K[wd1 as usize];
                (wd1 >> 2, ihigh, wd2)
            }
        }
    }

    /// Process the low band component of the G.722 stream
    fn process_low_band(&mut self, wd1: i32, wd2: i32) -> i32 {
        // Block 5L, LOW BAND INVQBL - Inverse quantization for low band
        let dequant = (self.band[0].log_scale_factor * wd2) >> 15;

        // Block 5L, RECONS - Reconstruction of low band signal
        let rlow = self.band[0].signal_estimate + dequant;

        // Block 6L, LIMIT - Limiting to valid range
        let rlow = rlow.clamp(-16384, 16383);

        // Block 2L, INVQAL - Inverse adaptive quantizer for prediction
        let wd2 = QUANT_MULT_LOW_4BIT[wd1 as usize];
        let dlowt = (self.band[0].log_scale_factor * wd2) >> 15;

        // Block 3L, LOGSCL - Compute log scale factor
        let wd2 = LOG_SCALE_FACTOR_MAP[wd1 as usize];
        let mut wd1 = (self.band[0].quantizer_step_size * 127) >> 7;
        wd1 += SCALE_FACTOR_ADJUST_LOW[wd2 as usize];
        self.band[0].quantizer_step_size = wd1.clamp(0, 18432);

        // Block 3L, SCALEL - Compute quantizer scale factor
        let wd1 = (self.band[0].quantizer_step_size >> 6) & 31;
        let wd2 = 8 - (self.band[0].quantizer_step_size >> 11);
        let wd3 = if wd2 < 0 {
            INV_LOG_BASE[wd1 as usize] << -wd2
        } else {
            INV_LOG_BASE[wd1 as usize] >> wd2
        };
        self.band[0].log_scale_factor = wd3 << 2;

        // Apply predictor adaptation
        block4(&mut self.band[0], dlowt);

        rlow
    }

    /// Process the high band component of the G.722 stream
    fn process_high_band(&mut self, ihigh: i32) -> i32 {
        // Block 2H, INVQAH - Inverse quantizer for high band
        let wd2 = QUANT_MULT_HIGH_2BIT[ihigh as usize];
        let dhigh = (self.band[1].log_scale_factor * wd2) >> 15;

        // Block 5H, RECONS - Reconstruction of high band signal
        let rhigh = dhigh + self.band[1].signal_estimate;

        // Block 6H, LIMIT - Limiting to valid range
        let rhigh = rhigh.clamp(-16384, 16383);

        // Block 2H, INVQAH - Adaptation logic
        let wd2 = HIGH_LOG_SCALE_MAP[ihigh as usize];
        let mut wd1 = (self.band[1].quantizer_step_size * 127) >> 7;
        wd1 += SCALE_FACTOR_ADJUST_HIGH[wd2 as usize];
        self.band[1].quantizer_step_size = wd1.clamp(0, 22528);

        // Block 3H, SCALEH - Compute quantizer scale factor
        let wd1 = (self.band[1].quantizer_step_size >> 6) & 31;
        let wd2 = 10 - (self.band[1].quantizer_step_size >> 11);
        let wd3 = if wd2 < 0 {
            INV_LOG_BASE[wd1 as usize] << -wd2
        } else {
            INV_LOG_BASE[wd1 as usize] >> wd2
        };
        self.band[1].log_scale_factor = wd3 << 2;

        // Apply predictor adaptation
        block4(&mut self.band[1], dhigh);

        rhigh
    }

    /// Apply QMF synthesis filter to combine low and high band signals
    fn apply_qmf_synthesis(&mut self, rlow: i32, rhigh: i32) -> [i16; 2] {
        // Shift filter state
        for i in 0..22 {
            self.x[i] = self.x[i + 2];
        }

        // Set new filter state values
        self.x[22] = rlow + rhigh;
        self.x[23] = rlow - rhigh;

        // Apply QMF synthesis filter
        let mut xout1 = 0;
        let mut xout2 = 0;

        for i in 0..12 {
            xout2 += self.x[2 * i] * QMF_FILTER_COEFS[i];
            xout1 += self.x[2 * i + 1] * QMF_FILTER_COEFS[11 - i];
        }

        // Return reconstructed samples with proper scaling
        [saturate(xout1 >> 11) as i16, saturate(xout2 >> 11) as i16]
    }

    /// Decodes a G.722 frame and returns PCM samples
    /// This is the main decoding function that processes G.722 encoded data
    pub fn decode_frame(&mut self, data: &[u8]) -> PcmBuf {
        let mut output = Vec::with_capacity(data.len() * 2);
        let mut idx = 0;

        while idx < data.len() {
            // Extract the next code from input data
            let code = self.extract_code(data, &mut idx);

            // Parse the code into components based on bit rate mode
            let (wd1, ihigh, wd2) = self.parse_code(code);

            // Process the low band component
            let rlow = self.process_low_band(wd1, wd2);

            if self.eight_k {
                // 8kHz mode - use only low band with scaling
                output.push((rlow << 1) as i16);
            } else {
                // 16kHz mode - process high band and combine with QMF synthesis
                let rhigh = self.process_high_band(ihigh);

                // Apply QMF synthesis filter to get reconstructed samples
                let samples = self.apply_qmf_synthesis(rlow, rhigh);
                output.extend_from_slice(&samples);
            }
        }

        output
    }
}

impl Encoder for G722Encoder {
    fn encode(&mut self, samples: &[Sample]) -> Vec<u8> {
        self.g722_encode(samples)
    }

    fn sample_rate(&self) -> u32 {
        16000 // G.722 encoding sample rate is 16kHz
    }

    fn channels(&self) -> u16 {
        1 // G.722 is mono encoding
    }
}

impl Decoder for G722Decoder {
    fn decode(&mut self, data: &[u8]) -> PcmBuf {
        self.decode_frame(data)
    }

    fn sample_rate(&self) -> u32 {
        16000
    }

    fn channels(&self) -> u16 {
        1
    }
}
