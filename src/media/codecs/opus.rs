use super::{Decoder, Encoder};
use crate::media::{PcmBuf, Sample};
use opusic_sys::{
    OPUS_APPLICATION_VOIP, OPUS_OK, OpusDecoder as OpusDecoderRaw, OpusEncoder as OpusEncoderRaw,
    opus_decode, opus_decoder_create, opus_decoder_destroy, opus_encode, opus_encoder_create,
    opus_encoder_destroy, opus_strerror,
};
use std::{ffi::CStr, os::raw::c_int, ptr::NonNull};

fn opus_error_message(code: c_int) -> String {
    if code == OPUS_OK {
        return "ok".to_string();
    }

    unsafe {
        let ptr = opus_strerror(code);
        if ptr.is_null() {
            format!("error code {code}")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

/// Opus audio decoder backed by opusic-sys
pub struct OpusDecoder {
    decoder: NonNull<OpusDecoderRaw>,
    sample_rate: u32,
    channels: u16,
}

impl OpusDecoder {
    /// Create a new Opus decoder instance
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let channel_count: c_int = if channels == 1 { 1 } else { 2 };
        let mut error: c_int = 0;
        let ptr = unsafe {
            opus_decoder_create(
                sample_rate as c_int,
                channel_count,
                &mut error as *mut c_int,
            )
        };

        if error != OPUS_OK {
            unsafe {
                if !ptr.is_null() {
                    opus_decoder_destroy(ptr);
                }
            }
            panic!(
                "Failed to create Opus decoder: {}",
                opus_error_message(error)
            );
        }

        let decoder = NonNull::new(ptr).unwrap_or_else(|| {
            panic!("Failed to create Opus decoder: null pointer returned");
        });

        Self {
            decoder,
            sample_rate,
            channels: if channel_count == 1 { 1 } else { 2 },
        }
    }

    /// Create a default Opus decoder (48kHz, stereo)
    pub fn new_default() -> Self {
        Self::new(48000, 2)
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe {
            opus_decoder_destroy(self.decoder.as_ptr());
        }
    }
}

unsafe impl Send for OpusDecoder {}
unsafe impl Sync for OpusDecoder {}

impl Decoder for OpusDecoder {
    fn decode(&mut self, data: &[u8]) -> PcmBuf {
        let channels = usize::from(self.channels);
        if channels == 0 {
            return Vec::new();
        }

        // Allow up to 120ms of audio as before: 48kHz * 0.12s * 2 channels = 11520 samples
        let max_samples = 11520;
        let mut output = vec![0i16; max_samples];
        let frame_size = (max_samples / channels) as c_int;

        let data_ptr = if data.is_empty() {
            std::ptr::null()
        } else {
            data.as_ptr()
        };

        let len = unsafe {
            opus_decode(
                self.decoder.as_ptr(),
                data_ptr.cast(),
                data.len() as c_int,
                output.as_mut_ptr().cast(),
                frame_size,
                0,
            )
        };

        if len < 0 {
            return Vec::new();
        }

        let total_samples = (len as usize) * channels;
        output.truncate(total_samples);

        if channels == 2 {
            output = output
                .chunks_exact(2)
                .map(|chunk| ((chunk[0] as i32 + chunk[1] as i32) / 2) as i16)
                .collect();
        }

        output
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

/// Opus audio encoder backed by opusic-sys
pub struct OpusEncoder {
    encoder: NonNull<OpusEncoderRaw>,
    sample_rate: u32,
    channels: u16,
}

impl OpusEncoder {
    /// Create a new Opus encoder instance
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let channel_count: c_int = if channels == 1 { 1 } else { 2 };
        let mut error: c_int = 0;
        let ptr = unsafe {
            opus_encoder_create(
                sample_rate as c_int,
                channel_count,
                OPUS_APPLICATION_VOIP,
                &mut error as *mut c_int,
            )
        };

        if error != OPUS_OK {
            unsafe {
                if !ptr.is_null() {
                    opus_encoder_destroy(ptr);
                }
            }
            panic!(
                "Failed to create Opus encoder: {}",
                opus_error_message(error)
            );
        }

        let encoder = NonNull::new(ptr).unwrap_or_else(|| {
            panic!("Failed to create Opus encoder: null pointer returned");
        });

        Self {
            encoder,
            sample_rate,
            channels: if channel_count == 1 { 1 } else { 2 },
        }
    }

    /// Create a default Opus encoder (48kHz, stereo)
    pub fn new_default() -> Self {
        Self::new(48000, 2)
    }

    fn encode_raw(&mut self, samples: &[Sample]) -> Vec<u8> {
        let channels = usize::from(self.channels);
        if samples.is_empty() || channels == 0 || samples.len() % channels != 0 {
            return Vec::new();
        }

        let frame_size = (samples.len() / channels) as c_int;
        let mut output = vec![0u8; samples.len()];
        let len = unsafe {
            opus_encode(
                self.encoder.as_ptr(),
                samples.as_ptr().cast(),
                frame_size,
                output.as_mut_ptr(),
                output.len() as c_int,
            )
        };

        if len < 0 {
            Vec::new()
        } else {
            output.truncate(len as usize);
            output
        }
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe {
            opus_encoder_destroy(self.encoder.as_ptr());
        }
    }
}

unsafe impl Send for OpusEncoder {}
unsafe impl Sync for OpusEncoder {}

impl Encoder for OpusEncoder {
    fn encode(&mut self, samples: &[Sample]) -> Vec<u8> {
        if self.channels == 2 {
            let mut stereo_samples = Vec::with_capacity(samples.len() * 2);
            for &sample in samples {
                stereo_samples.push(sample);
                stereo_samples.push(sample);
            }
            return self.encode_raw(&stereo_samples);
        }

        self.encode_raw(samples)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}
