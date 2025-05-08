use crate::{PcmBuf, Sample};
use anyhow::Result;
use rubato::{FftFixedOut, Resampler};

pub struct LinearResampler {
    resampler: FftFixedOut<f64>,
    input_sample_rate: usize,
    output_sample_rate: usize,
    input_chunk_size: usize,
}

impl LinearResampler {
    pub fn new(input_sample_rate: usize, output_sample_rate: usize) -> Result<Self> {
        let rate = output_sample_rate as f64 / input_sample_rate as f64;
        let input_chunk_size = match input_sample_rate {
            8000 => 160,
            16000 => 320,
            44100 => 882,
            48000 => 960,
            _ => (input_sample_rate as f64 / 50.0).ceil() as usize,
        };
        let output_chunk_size = (input_chunk_size as f64 * rate) as usize;
        let resampler = FftFixedOut::<f64>::new(
            input_sample_rate,
            output_sample_rate,
            output_chunk_size,
            1,
            1,
        )?;
        Ok(Self {
            resampler,
            input_sample_rate,
            output_sample_rate,
            input_chunk_size,
        })
    }

    pub fn resample(&mut self, input: &[Sample]) -> PcmBuf {
        if self.input_sample_rate == self.output_sample_rate {
            return input.to_vec();
        }

        let mut input_f64 = Vec::with_capacity(input.len());
        for sample in input {
            input_f64.push(*sample as f64 / i16::MAX as f64);
        }

        let channel_data = input_f64.chunks(self.input_chunk_size).collect::<Vec<_>>();
        let resampled = match self.resampler.process(&channel_data, None) {
            Ok(res) => res,
            Err(_) => {
                return input.to_vec();
            }
        };

        let mut result = Vec::with_capacity(resampled[0].len());
        for sample in resampled[0].iter() {
            result.push((sample * i16::MAX as f64) as i16);
        }

        result
    }
}

pub fn resample_mono(input: &[Sample], input_sample_rate: u32, output_sample_rate: u32) -> PcmBuf {
    if input_sample_rate == output_sample_rate {
        return input.to_vec();
    }
    let mut resampler =
        match LinearResampler::new(input_sample_rate as usize, output_sample_rate as usize) {
            Ok(resampler) => resampler,
            Err(_) => {
                return input.to_vec();
            }
        };

    let mut result =
        Vec::with_capacity(input.len() * output_sample_rate as usize / input_sample_rate as usize);
    for chunk in input.chunks(resampler.input_chunk_size) {
        let resampled = resampler.resample(chunk);
        result.extend_from_slice(&resampled);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::codecs::samples_to_bytes;
    use crate::media::track::file::read_wav_file;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_resampler() {
        let (all_samples, samplerate) = read_wav_file("fixtures/sample.wav").unwrap();
        let frame_samples = all_samples[0..640].to_vec();
        let frame_resampled = resample_mono(&frame_samples, samplerate, 8000);
        assert_eq!(frame_resampled.len(), 320);

        let resampled = resample_mono(&all_samples, samplerate, 8000);
        let mut file = File::create("fixtures/sample.8k.decoded").expect("Failed to create file");
        let decoded_bytes = samples_to_bytes(&resampled);
        file.write_all(&decoded_bytes)
            .expect("Failed to write file");
        let rate = resampled.len() as f64 / all_samples.len() as f64;
        println!(
            "resampled {}->{} samples, rate: {}",
            all_samples.len(),
            resampled.len(),
            rate
        );
        println!("ffplay -f s16le -ar 8000 -i fixtures/sample.8k.decoded");
    }
}
