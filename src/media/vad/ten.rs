use super::{VADOption, VadEngine};
use crate::media::{AudioFrame, PcmBuf, Samples};
use anyhow::Result;
use ort::session::{Session, builder::GraphOptimizationLevel};
use tracing::debug;

pub struct TenVad {
    config: VADOption,
    buffer: PcmBuf,
    last_timestamp: u64,
    chunk_size: usize,
    session: Session,
    hidden_states: Vec<ndarray::Array2<f32>>,
    feature_buffer: ndarray::Array2<f32>,
    pre_emphasis_prev: f32,
    mel_filters: ndarray::Array2<f32>,
    window: Vec<f32>,
    last_score: Option<f32>,
}

const MODEL: &[u8] = include_bytes!("./ten_vad.onnx");

// Constants from Python implementation
const SAMPLE_RATE: u32 = 16000;
const HOP_SIZE: usize = 256; // 16ms per frame
const FFT_SIZE: usize = 1024;
const WINDOW_SIZE: usize = 768;
const MEL_FILTER_BANK_NUM: usize = 40;
const FEATURE_LEN: usize = MEL_FILTER_BANK_NUM + 1; // 40 mel features + 1 pitch feature
const CONTEXT_WINDOW_LEN: usize = 3;
const MODEL_HIDDEN_DIM: usize = 64;
const MODEL_IO_NUM: usize = 5;
const EPS: f32 = 1e-20;
const PRE_EMPHASIS_COEFF: f32 = 0.97;

// Feature normalization parameters from Python code
const FEATURE_MEANS: [f32; FEATURE_LEN] = [
    -8.198_236,
    -6.265_716_6,
    -5.483_818_5,
    -4.758_691_3,
    -4.417_089,
    -4.142_893,
    -3.912_850_4,
    -3.845_928,
    -3.657_090_4,
    -3.723_418_7,
    -3.876_134_2,
    -3.843_891,
    -3.690_405_1,
    -3.756_065_8,
    -3.698_696_1,
    -3.650_463,
    -3.700_468_8,
    -3.567_321_3,
    -3.498_900_2,
    -3.477_807,
    -3.458_816,
    -3.444_923_9,
    -3.401_328_6,
    -3.306_261_3,
    -3.278_556_8,
    -3.233_250_9,
    -3.198_616,
    -3.204_526_4,
    -3.208_798_6,
    -3.257_838,
    -3.381_376_7,
    -3.534_021_4,
    -3.640_868,
    -3.726_858_9,
    -3.773_731,
    -3.804_667_2,
    -3.832_901,
    -3.871_120_5,
    -3.990_593,
    -4.480_289_5,
    9.235_69e1,
];

const FEATURE_STDS: [f32; FEATURE_LEN] = [
    5.166_064,
    4.977_21,
    4.698_896,
    4.630_621_4,
    4.634_348,
    4.641_156,
    4.640_676_5,
    4.666_367,
    4.650_534_6,
    4.640_021,
    4.637_4,
    4.620_099,
    4.596_316_3,
    4.562_655,
    4.554_36,
    4.566_910_7,
    4.562_49,
    4.562_413,
    4.585_299_5,
    4.600_179_7,
    4.592_846,
    4.585_923,
    4.583_496_6,
    4.626_093,
    4.626_958,
    4.626_289_4,
    4.637_006,
    4.683_016,
    4.726_814,
    4.734_29,
    4.753_227,
    4.849_723,
    4.869_435,
    4.884_483,
    4.921_327,
    4.959_212_3,
    4.996_619,
    5.044_823_6,
    5.072_217,
    5.096_439_4,
    1.152_136_9e2,
];

impl TenVad {
    pub fn new(config: VADOption) -> Result<Self> {
        // Only support 16kHz audio
        if config.samplerate != SAMPLE_RATE {
            return Err(anyhow::anyhow!(
                "TenVad only supports 16kHz audio, got: {}",
                config.samplerate
            ));
        }

        let chunk_size = HOP_SIZE;

        // Create new session instance
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(1)?
            .with_inter_threads(1)?
            .with_log_level(ort::logging::LogLevel::Warning)?
            .commit_from_memory(MODEL)?;

        // Model initialization successful

        // Initialize hidden states
        let hidden_states =
            vec![ndarray::Array2::<f32>::zeros((1, MODEL_HIDDEN_DIM)); MODEL_IO_NUM - 1];

        // Initialize feature buffer
        let feature_buffer = ndarray::Array2::<f32>::zeros((CONTEXT_WINDOW_LEN, FEATURE_LEN));

        // Generate mel filter bank
        let mel_filters = Self::generate_mel_filters();

        // Generate Hann window
        let window = Self::generate_hann_window();

        debug!("TenVad created with chunk size: {}", chunk_size);
        Ok(Self {
            session,
            hidden_states,
            feature_buffer,
            config,
            buffer: Vec::new(),
            chunk_size,
            last_timestamp: 0,
            pre_emphasis_prev: 0.0,
            mel_filters,
            window,
            last_score: None,
        })
    }

    fn generate_mel_filters() -> ndarray::Array2<f32> {
        let n_bins = FFT_SIZE / 2 + 1;

        // Generate mel frequency points
        let low_mel = 2595.0_f32 * (1.0_f32 + 0.0_f32 / 700.0_f32).log10();
        let high_mel = 2595.0_f32 * (1.0_f32 + 8000.0_f32 / 700.0_f32).log10();

        let mut mel_points = Vec::new();
        for i in 0..=MEL_FILTER_BANK_NUM + 1 {
            let mel = low_mel + (high_mel - low_mel) * i as f32 / (MEL_FILTER_BANK_NUM + 1) as f32;
            mel_points.push(mel);
        }

        // Convert to Hz
        let mut hz_points = Vec::new();
        for mel in mel_points {
            let hz = 700.0_f32 * (10.0_f32.powf(mel / 2595.0_f32) - 1.0_f32);
            hz_points.push(hz);
        }

        // Convert to FFT bin indices
        let mut bin_points = Vec::new();
        for hz in hz_points {
            let bin = ((FFT_SIZE + 1) as f32 * hz / SAMPLE_RATE as f32).floor() as usize;
            bin_points.push(bin);
        }

        // Build mel filter bank
        let mut mel_filters = ndarray::Array2::<f32>::zeros((MEL_FILTER_BANK_NUM, n_bins));

        for i in 0..MEL_FILTER_BANK_NUM {
            // Left slope
            for j in bin_points[i]..bin_points[i + 1] {
                if j < n_bins {
                    mel_filters[[i, j]] =
                        (j - bin_points[i]) as f32 / (bin_points[i + 1] - bin_points[i]) as f32;
                }
            }

            // Right slope
            for j in bin_points[i + 1]..bin_points[i + 2] {
                if j < n_bins {
                    mel_filters[[i, j]] = (bin_points[i + 2] - j) as f32
                        / (bin_points[i + 2] - bin_points[i + 1]) as f32;
                }
            }
        }

        mel_filters
    }

    fn generate_hann_window() -> Vec<f32> {
        let mut window = Vec::with_capacity(WINDOW_SIZE);
        for i in 0..WINDOW_SIZE {
            let val = 0.5
                * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (WINDOW_SIZE - 1) as f32).cos());
            window.push(val);
        }
        window
    }

    fn pre_emphasis(&mut self, audio_frame: &[i16]) -> Vec<f32> {
        let mut emphasized = Vec::with_capacity(audio_frame.len());

        if !audio_frame.is_empty() {
            let first_sample = audio_frame[0] as f32 / 32768.0;
            emphasized.push(first_sample - PRE_EMPHASIS_COEFF * self.pre_emphasis_prev);

            for i in 1..audio_frame.len() {
                let current_sample = audio_frame[i] as f32 / 32768.0;
                let previous_sample = audio_frame[i - 1] as f32 / 32768.0;
                emphasized.push(current_sample - PRE_EMPHASIS_COEFF * previous_sample);
            }

            if !audio_frame.is_empty() {
                self.pre_emphasis_prev = audio_frame[audio_frame.len() - 1] as f32 / 32768.0;
            }
        }

        emphasized
    }

    fn extract_features(&mut self, audio_frame: &[i16]) -> ndarray::Array1<f32> {
        // Pre-emphasis
        let emphasized = self.pre_emphasis(audio_frame);

        // Zero-padding to window size
        let mut padded = vec![0.0; WINDOW_SIZE];
        let copy_len = emphasized.len().min(WINDOW_SIZE);
        padded[..copy_len].copy_from_slice(&emphasized[..copy_len]);

        // Windowing
        for (i, sample) in padded.iter_mut().enumerate().take(WINDOW_SIZE) {
            *sample *= self.window[i];
        }

        // FFT - simple implementation using ndarray
        let mut fft_input = ndarray::Array1::<f32>::zeros(FFT_SIZE);
        fft_input
            .slice_mut(ndarray::s![..WINDOW_SIZE])
            .assign(&ndarray::Array1::from(padded));

        // For simplicity, we'll use a basic FFT approximation
        // In production, you'd want to use a proper FFT library
        let n_bins = FFT_SIZE / 2 + 1;
        let mut power_spectrum = ndarray::Array1::<f32>::zeros(n_bins);

        // Simple power spectrum estimation (not a real FFT)
        for i in 0..n_bins {
            let mut real = 0.0;
            let mut imag = 0.0;
            for k in 0..FFT_SIZE {
                let angle = -2.0 * std::f32::consts::PI * i as f32 * k as f32 / FFT_SIZE as f32;
                real += fft_input[k] * angle.cos();
                imag += fft_input[k] * angle.sin();
            }
            power_spectrum[i] = (real * real + imag * imag) / (32768.0 * 32768.0);
        }

        // Mel filter bank features
        let mut mel_features = ndarray::Array1::<f32>::zeros(MEL_FILTER_BANK_NUM);
        for i in 0..MEL_FILTER_BANK_NUM {
            let mut sum = 0.0;
            for j in 0..n_bins {
                sum += self.mel_filters[[i, j]] * power_spectrum[j];
            }
            mel_features[i] = (sum + EPS).ln();
        }

        // Simple pitch estimation (using 0 as in Python code)
        let pitch_freq = 0.0;

        // Combine features
        let mut features = ndarray::Array1::<f32>::zeros(FEATURE_LEN);
        features
            .slice_mut(ndarray::s![..MEL_FILTER_BANK_NUM])
            .assign(&mel_features);
        features[MEL_FILTER_BANK_NUM] = pitch_freq;

        // Feature normalization
        for i in 0..FEATURE_LEN {
            features[i] = (features[i] - FEATURE_MEANS[i]) / (FEATURE_STDS[i] + EPS);
        }

        features
    }

    pub fn predict(&mut self, samples: &[i16]) -> Result<f32, ort::Error> {
        // Extract features
        let features = self.extract_features(samples);

        // Update feature buffer (sliding window)
        // Shift existing features
        for i in 0..CONTEXT_WINDOW_LEN - 1 {
            for j in 0..FEATURE_LEN {
                self.feature_buffer[[i, j]] = self.feature_buffer[[i + 1, j]];
            }
        }

        // Add new features
        for j in 0..FEATURE_LEN {
            self.feature_buffer[[CONTEXT_WINDOW_LEN - 1, j]] = features[j];
        }

        // Prepare ONNX inference input
        let input_tensor = self.feature_buffer.clone().insert_axis(ndarray::Axis(0));
        let input_value = ort::value::Value::from_array(input_tensor)?;

        // Build inputs with hidden states using correct names
        let mut ort_inputs = std::collections::HashMap::new();
        ort_inputs.insert("input_1".to_string(), input_value);

        // Add hidden states with correct names
        let hidden_input_names = ["input_2", "input_3", "input_6", "input_7"];
        for (i, hidden_state) in self.hidden_states.iter().enumerate() {
            if i < hidden_input_names.len() {
                let hidden_value = ort::value::Value::from_array(hidden_state.clone())?;
                ort_inputs.insert(hidden_input_names[i].to_string(), hidden_value);
            }
        }

        let outputs = self.session.run(ort_inputs)?;

        // Get VAD score from correct output
        let (_, probability_data) = outputs
            .get("output_1")
            .ok_or_else(|| ort::Error::new("Output 'output_1' not found"))?
            .try_extract_tensor::<f32>()?;
        let probability = probability_data[0];

        // Update hidden states with correct output names
        let hidden_output_names = ["output_2", "output_3", "output_6", "output_7"];
        for (i, output_name) in hidden_output_names.iter().enumerate() {
            if let Some(output) = outputs.get(*output_name) {
                let (state_shape, state_data) = output.try_extract_tensor::<f32>()?;
                if state_shape.len() >= 2 && i < self.hidden_states.len() {
                    let state_array = ndarray::Array2::<f32>::from_shape_vec(
                        (state_shape[0] as usize, state_shape[1] as usize),
                        state_data.to_vec(),
                    )
                    .map_err(|e| {
                        ort::Error::new(format!("Failed to reshape state array: {}", e))
                    })?;
                    self.hidden_states[i].assign(&state_array);
                }
            }
        }

        self.last_score = Some(probability);
        Ok(probability)
    }
}

impl VadEngine for TenVad {
    fn process(&mut self, frame: &mut AudioFrame) -> Option<(bool, u64)> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Some((false, frame.timestamp)),
        };

        self.buffer.extend_from_slice(samples);

        if self.buffer.len() >= self.chunk_size {
            let chunk: Vec<i16> = self.buffer.drain(..self.chunk_size).collect();
            let score = match self.predict(&chunk) {
                Ok(score) => score,
                Err(_e) => {
                    #[cfg(debug_assertions)]
                    println!("TenVad prediction failed: {}", _e);
                    0.0 // Return neutral score on error
                }
            };
            let is_voice = score > self.config.voice_threshold;

            let chunk_duration_ms = (self.chunk_size as u64 * 1000) / (frame.sample_rate as u64);

            // Initialize timestamp management properly
            if self.last_timestamp == 0 {
                self.last_timestamp = frame.timestamp;
            }

            let chunk_timestamp = self.last_timestamp;
            self.last_timestamp += chunk_duration_ms;

            return Some((is_voice, chunk_timestamp));
        }

        None
    }
}
