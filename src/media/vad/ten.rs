use super::{VADOption, VadEngine};
use crate::{AudioFrame, PcmBuf, Samples};
use anyhow::Result;
use ort::session::{builder::GraphOptimizationLevel, Session};
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
    -8.198236465454e+00,
    -6.265716552734e+00,
    -5.483818531036e+00,
    -4.758691310883e+00,
    -4.417088985443e+00,
    -4.142892837524e+00,
    -3.912850379944e+00,
    -3.845927953720e+00,
    -3.657090425491e+00,
    -3.723418712616e+00,
    -3.876134157181e+00,
    -3.843890905380e+00,
    -3.690405130386e+00,
    -3.756065845490e+00,
    -3.698696136475e+00,
    -3.650463104248e+00,
    -3.700468778610e+00,
    -3.567321300507e+00,
    -3.498900175095e+00,
    -3.477807044983e+00,
    -3.458816051483e+00,
    -3.444923877716e+00,
    -3.401328563690e+00,
    -3.306261301041e+00,
    -3.278556823730e+00,
    -3.233250856400e+00,
    -3.198616027832e+00,
    -3.204526424408e+00,
    -3.208798646927e+00,
    -3.257838010788e+00,
    -3.381376743317e+00,
    -3.534021377563e+00,
    -3.640867948532e+00,
    -3.726858854294e+00,
    -3.773730993271e+00,
    -3.804667234421e+00,
    -3.832901000977e+00,
    -3.871120452881e+00,
    -3.990592956543e+00,
    -4.480289459229e+00,
    9.235690307617e+01,
];

const FEATURE_STDS: [f32; FEATURE_LEN] = [
    5.166063785553e+00,
    4.977209568024e+00,
    4.698895931244e+00,
    4.630621433258e+00,
    4.634347915649e+00,
    4.641156196594e+00,
    4.640676498413e+00,
    4.666367053986e+00,
    4.650534629822e+00,
    4.640020847321e+00,
    4.637400150299e+00,
    4.620099067688e+00,
    4.596316337585e+00,
    4.562654972076e+00,
    4.554360389709e+00,
    4.566910743713e+00,
    4.562489986420e+00,
    4.562412738800e+00,
    4.585299491882e+00,
    4.600179672241e+00,
    4.592845916748e+00,
    4.585922718048e+00,
    4.583496570587e+00,
    4.626092910767e+00,
    4.626957893372e+00,
    4.626289367676e+00,
    4.637005805969e+00,
    4.683015823364e+00,
    4.726813793182e+00,
    4.734289646149e+00,
    4.753227233887e+00,
    4.849722862244e+00,
    4.869434833527e+00,
    4.884482860565e+00,
    4.921327114105e+00,
    4.959212303162e+00,
    4.996619224548e+00,
    5.044823646545e+00,
    5.072216987610e+00,
    5.096439361572e+00,
    1.152136917114e+02,
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
        for i in 0..WINDOW_SIZE {
            padded[i] *= self.window[i];
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
                        ort::Error::new(&format!("Failed to reshape state array: {}", e))
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

        while self.buffer.len() >= self.chunk_size {
            let chunk: Vec<i16> = self.buffer.drain(..self.chunk_size).collect();
            let score = match self.predict(&chunk) {
                Ok(score) => score,
                Err(e) => {
                    #[cfg(debug_assertions)]
                    println!("TenVad prediction failed: {}", e);
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
