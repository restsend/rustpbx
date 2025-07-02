use super::{VADOption, VadEngine};
use crate::{AudioFrame, PcmBuf, Samples};
use anyhow::Result;
use ort::session::{builder::GraphOptimizationLevel, Session};

pub struct SileroVad {
    config: VADOption,
    buffer: PcmBuf,
    last_timestamp: u64,
    chunk_size: usize,
    session: Session,
    state: ndarray::Array3<f32>,
}

const MODEL: &[u8] = include_bytes!("./silero_vad.onnx");

impl SileroVad {
    pub fn new(config: VADOption) -> Result<Self> {
        // Create detector with default settings for 16kHz audio
        // Using window size according to Python implementation: 512 samples for 16kHz, 256 for 8kHz
        let chunk_size = match config.samplerate {
            8000 => 256,
            16000 => 512,
            _ => {
                return Err(anyhow::anyhow!(
                    "Unsupported sample rate: {}",
                    config.samplerate
                ))
            }
        };

        // Create new session instance
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(1)?
            .with_inter_threads(1)?
            .with_log_level(ort::logging::LogLevel::Warning)?
            .commit_from_memory(MODEL)?;

        Ok(Self {
            session,
            state: ndarray::Array3::<f32>::zeros((2, 1, 128)),
            config,
            buffer: Vec::new(),
            chunk_size,
            last_timestamp: 0,
        })
    }

    pub fn predict(&mut self, samples: &[i16]) -> Result<f32, ort::Error> {
        let mut input = ndarray::Array2::<f32>::zeros((1, samples.len()));
        for (i, sample) in samples.iter().enumerate() {
            input[[0, i]] = *sample as f32 / 32768.0;
        }
        let sample_rate = ndarray::arr1::<i64>(&[self.config.samplerate as i64]);

        let input_value = ort::value::Value::from_array(input)?;
        let sr_value = ort::value::Value::from_array(sample_rate)?;
        let state_value = ort::value::Value::from_array(self.state.clone())?;

        let inputs = ort::inputs![
            "input" => input_value,
            "sr" => sr_value,
            "state" => state_value,
        ];

        let outputs = self.session.run(inputs)?;
        let (_probability_shape, probability_data) = outputs
            .get("output")
            .ok_or_else(|| ort::Error::new("Output 'output' not found"))?
            .try_extract_tensor::<f32>()?;
        let probability = probability_data[0];
        let (state_shape, state_data) = outputs
            .get("stateN")
            .ok_or_else(|| ort::Error::new("Output 'stateN' not found"))?
            .try_extract_tensor::<f32>()?;

        // Reshape state_data to correct shape and assign to self.state
        let state_array = ndarray::Array3::<f32>::from_shape_vec(
            (
                state_shape[0] as usize,
                state_shape[1] as usize,
                state_shape[2] as usize,
            ),
            state_data.to_vec(),
        )
        .map_err(|e| ort::Error::new(&format!("Failed to reshape state array: {}", e)))?;
        self.state.assign(&state_array);

        Ok(probability)
    }
}

impl VadEngine for SileroVad {
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
                Err(_) => return Some((false, frame.timestamp)), // Return non-voice on error
            };
            let is_voice = score > self.config.voice_threshold;

            let chunk_duration_ms = (self.chunk_size as u64 * 1000) / (frame.sample_rate as u64);
            let chunk_timestamp = self.last_timestamp;
            self.last_timestamp += chunk_duration_ms;

            if chunk_timestamp == 0 && frame.timestamp > 0 {
                self.last_timestamp = frame.timestamp + chunk_duration_ms;
                return Some((is_voice, frame.timestamp));
            }

            return Some((is_voice, chunk_timestamp));
        }

        None
    }
}
