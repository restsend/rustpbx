use anyhow::Result;
use bytes::{Bytes, BytesMut};

use super::{Decoder, Encoder};

const CLIP: i16 = 32767;
const RATE_48000: u32 = 48000;
const RATE_56000: u32 = 56000;
const RATE_64000: u32 = 64000;

// QMF coefficients for analysis filter
const QMF_COEFF: [i16; 24] = [
    3, -11, 12, 32, -210, 951, 3876, -805, 362, -156, 53, -11, 3, -11, 12, 32, -210, 951, 3876,
    -805, 362, -156, 53, -11,
];

// Quantizer lookup tables
const Q6: [i32; 64] = [
    0, 35, 72, 110, 150, 190, 233, 276, 323, 370, 422, 473, 530, 587, 650, 714, 786, 858, 940,
    1023, 1121, 1219, 1339, 1458, 1612, 1765, 1980, 2195, 2557, 2919, 3407, 3895, 4633, 5371, 6407,
    7443, 9139, 10835, 13330, 15825, 19365, 22905, 28181, 33457, 41199, 48941, 60387, 71833, 88843,
    105853, 130683, 155513, 192559, 229605, 284959, 340313, 425253, 510193, 636159, 762125, 953255,
    1144385, 1430417, 1716449,
];

const IQ6: [i32; 64] = [
    0, 35, 72, 110, 150, 190, 233, 276, 323, 370, 422, 473, 530, 587, 650, 714, 786, 858, 940,
    1023, 1121, 1219, 1339, 1458, 1612, 1765, 1980, 2195, 2557, 2919, 3407, 3895, 4633, 5371, 6407,
    7443, 9139, 10835, 13330, 15825, 19365, 22905, 28181, 33457, 41199, 48941, 60387, 71833, 88843,
    105853, 130683, 155513, 192559, 229605, 284959, 340313, 425253, 510193, 636159, 762125, 953255,
    1144385, 1430417, 1716449,
];

const W: [i32; 8] = [-12, -10, -8, -6, -4, -2, 0, 2];
const WLIMIT: [i32; 8] = [-12, -10, -8, -6, -4, -2, 0, 2];

pub struct G722Encoder {
    rate: u32,
    sample_rate: u32,
    channels: u16,
    xd: i32,        // Signal at the input of QMF
    xs: i32,        // Signal at the output of QMF
    y1: [i32; 24],  // Delayed input signal for QMF
    y2: [i32; 24],  // Delayed input signal for QMF
    band: [i32; 2], // Band value
    det: [i32; 2],  // Detector value
    nb: [i32; 2],   // Number of bits per band
    bp: [i32; 2],   // Band pointer
    code: [i32; 2], // Code value
    slew: [i32; 2], // Slew rate
    delt: [i32; 2], // Delta value
    wd1: [i32; 2],  // Working data
    wd2: [i32; 2],  // Working data
    wd3: [i32; 2],  // Working data
    sp: [i32; 2],   // Sign predictor
    szl: [i32; 2],  // Sez
    rlt1: [i32; 2], // Delayed quantizer output
    rlt2: [i32; 2], // Delayed quantizer output
    al: [i32; 2],   // Coefficient array
    plt: [i32; 2],  // Pole coefficient array
    dlt: [i32; 2],  // Delayed prediction difference
}

impl G722Encoder {
    pub fn new(rate: u32) -> Self {
        let nb = match rate {
            RATE_48000 => [6, 2],
            RATE_56000 => [6, 3],
            _ => [7, 3], // RATE_64000
        };

        Self {
            rate,
            sample_rate: 16000,
            channels: 1,
            xd: 0,
            xs: 0,
            y1: [0; 24],
            y2: [0; 24],
            band: [0; 2],
            det: [32; 2],
            nb,
            bp: [0; 2],
            code: [0; 2],
            slew: [0; 2],
            delt: [0; 2],
            wd1: [0; 2],
            wd2: [0; 2],
            wd3: [0; 2],
            sp: [0; 2],
            szl: [0; 2],
            rlt1: [0; 2],
            rlt2: [0; 2],
            al: [0; 2],
            plt: [0; 2],
            dlt: [0; 2],
        }
    }

    fn qmf_tx(&mut self, sample: i16) -> (i32, i32) {
        // Shift delay line
        for i in (1..24).rev() {
            self.y1[i] = self.y1[i - 1];
            self.y2[i] = self.y2[i - 1];
        }

        // Input new sample
        self.y1[0] = sample as i32;
        self.y2[0] = sample as i32;

        // Calculate output
        let mut xl = 0;
        let mut xh = 0;

        for i in 0..24 {
            xl += QMF_COEFF[i] as i32 * self.y1[i];
            xh += QMF_COEFF[i] as i32 * self.y2[i];
        }

        // Scale outputs
        xl >>= 13;
        xh >>= 13;

        (xl, xh)
    }

    fn encode_band(&mut self, band: usize, xin: i32) -> i32 {
        // Calculate predictor for this band
        let mut diff = xin - self.szl[band];
        let mut pred = self.rlt1[band] * 2;

        // Saturate predictor
        if pred > 32767 {
            pred = 32767;
        } else if pred < -32768 {
            pred = -32768;
        }

        // Calculate quantizer step size
        let mut det = self.det[band];
        let mut code = 0;

        // Quantize prediction difference
        if diff < 0 {
            code = (((-diff << 1) + det) / (det << 1)) as i32;
            if code > 31 {
                code = 31;
            }
            code = -code;
        } else {
            code = ((diff << 1) + det) / (det << 1);
            if code > 31 {
                code = 31;
            }
        }

        // Inverse quantize prediction difference
        diff = if code < 0 {
            ((-code * det) << 1) + det
        } else {
            (code * det) << 1
        };

        // Update predictor poles
        self.rlt2[band] = self.rlt1[band];
        self.rlt1[band] = diff;

        // Update quantizer step size
        self.det[band] = (det * W[(code + 4) as usize & 0x0F] as i32) >> 5;
        if self.det[band] < 11 {
            self.det[band] = 11;
        } else if self.det[band] > 32767 {
            self.det[band] = 32767;
        }

        code
    }
}

impl Encoder for G722Encoder {
    fn encode(&mut self, samples: &[i16]) -> Result<Bytes> {
        let mut output = BytesMut::with_capacity(samples.len() / 2);

        for &sample in samples {
            let (xl, xh) = self.qmf_tx(sample);

            // Encode each band
            let code1 = self.encode_band(0, xh);
            let code2 = self.encode_band(1, xl);

            // Pack codes into output byte
            let byte = match self.rate {
                RATE_48000 => ((code1 & 0x3F) << 2) | ((code2 & 0x03) as i32),
                RATE_56000 => ((code1 & 0x3F) << 3) | ((code2 & 0x07) as i32),
                _ => ((code1 & 0x7F) << 3) | ((code2 & 0x07) as i32), // RATE_64000
            };

            output.extend_from_slice(&[byte as u8]);
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

pub struct G722Decoder {
    rate: u32,
    sample_rate: u32,
    channels: u16,
    xd: i32,
    xs: i32,
    y1: [i32; 24],
    y2: [i32; 24],
    band: [i32; 2],
    det: [i32; 2],
    nb: [i32; 2],
    bp: [i32; 2],
    code: [i32; 2],
    slew: [i32; 2],
    delt: [i32; 2],
    wd1: [i32; 2],
    wd2: [i32; 2],
    wd3: [i32; 2],
    sp: [i32; 2],
    szl: [i32; 2],
    rlt1: [i32; 2],
    rlt2: [i32; 2],
    al: [i32; 2],
    plt: [i32; 2],
    dlt: [i32; 2],
}

impl G722Decoder {
    pub fn new(rate: u32) -> Self {
        let nb = match rate {
            RATE_48000 => [6, 2],
            RATE_56000 => [6, 3],
            _ => [7, 3], // RATE_64000
        };

        Self {
            rate,
            sample_rate: 16000,
            channels: 1,
            xd: 0,
            xs: 0,
            y1: [0; 24],
            y2: [0; 24],
            band: [0; 2],
            det: [32; 2],
            nb,
            bp: [0; 2],
            code: [0; 2],
            slew: [0; 2],
            delt: [0; 2],
            wd1: [0; 2],
            wd2: [0; 2],
            wd3: [0; 2],
            sp: [0; 2],
            szl: [0; 2],
            rlt1: [0; 2],
            rlt2: [0; 2],
            al: [0; 2],
            plt: [0; 2],
            dlt: [0; 2],
        }
    }

    fn qmf_rx(&mut self, xl: i32, xh: i32) -> i16 {
        // Shift delay line
        for i in (1..24).rev() {
            self.y1[i] = self.y1[i - 1];
            self.y2[i] = self.y2[i - 1];
        }

        // Input new samples
        self.y1[0] = xl;
        self.y2[0] = xh;

        // Calculate output
        let mut y = 0;

        for i in 0..24 {
            y += QMF_COEFF[i] as i32 * (self.y1[i] + self.y2[i]);
        }

        // Scale output
        y >>= 14;

        // Saturate output
        if y > 32767 {
            y = 32767;
        } else if y < -32768 {
            y = -32768;
        }

        y as i16
    }

    fn decode_band(&mut self, band: usize, code: i32) -> i32 {
        // Calculate predictor for this band
        let mut diff = if code < 0 {
            ((-code * self.det[band]) << 1) + self.det[band]
        } else {
            (code * self.det[band]) << 1
        };

        // Update predictor poles
        self.rlt2[band] = self.rlt1[band];
        self.rlt1[band] = diff;

        // Update quantizer step size
        self.det[band] = (self.det[band] * W[(code + 4) as usize & 0x0F] as i32) >> 5;
        if self.det[band] < 11 {
            self.det[band] = 11;
        } else if self.det[band] > 32767 {
            self.det[band] = 32767;
        }

        // Calculate signal estimate
        let mut y = self.rlt1[band] * 2;

        // Saturate signal estimate
        if y > 32767 {
            y = 32767;
        } else if y < -32768 {
            y = -32768;
        }

        y
    }
}

impl Decoder for G722Decoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<i16>> {
        let mut output = Vec::with_capacity(data.len() * 2);

        for &byte in data {
            // Unpack codes from input byte
            let (code1, code2) = match self.rate {
                RATE_48000 => ((byte >> 2) as i32, (byte & 0x03) as i32),
                RATE_56000 => ((byte >> 3) as i32, (byte & 0x07) as i32),
                _ => ((byte >> 3) as i32, (byte & 0x07) as i32), // RATE_64000
            };

            // Sign extend codes
            let code1 = if code1 & 0x20 != 0 {
                code1 | -0x40
            } else {
                code1
            };
            let code2 = if code2 & 0x04 != 0 {
                code2 | -0x08
            } else {
                code2
            };

            // Decode each band
            let xh = self.decode_band(0, code1);
            let xl = self.decode_band(1, code2);

            // Combine bands through synthesis QMF
            output.push(self.qmf_rx(xl, xh));
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
