use rubato::{Resampler, SincFixedIn, SincInterpolationType, WindowFunction};

pub fn resample(
    input: &[i16],
    input_sample_rate: u32,
    output_sample_rate: u32,
    channels: usize,
) -> Vec<i16> {
    if input_sample_rate == output_sample_rate {
        return input.to_vec();
    }

    if input.len() % channels != 0 {
        return Vec::new();
    }

    let frames = input.len() / channels;
    let expected_output_frames =
        (frames as f64 * output_sample_rate as f64 / input_sample_rate as f64).round() as usize;

    let params = rubato::SincInterpolationParameters {
        sinc_len: 64,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::Blackman,
    };

    let mut resampler = match SincFixedIn::<f64>::new(
        output_sample_rate as f64 / input_sample_rate as f64,
        1.0,
        params,
        frames,
        channels,
    ) {
        Ok(r) => r,
        Err(_) => {
            return Vec::new();
        }
    };

    let mut channel_data: Vec<Vec<f64>> = vec![Vec::with_capacity(frames); channels];

    for (i, &sample) in input.iter().enumerate() {
        let channel = i % channels;
        channel_data[channel].push(sample as f64 / i16::MAX as f64);
    }

    let resampled = match resampler.process(&channel_data, None) {
        Ok(res) => res,
        Err(_) => {
            return Vec::new();
        }
    };

    let mut result = Vec::with_capacity(expected_output_frames * channels);
    let actual_frames = resampled[0].len().min(expected_output_frames);

    for frame in 0..actual_frames {
        for channel in 0..channels {
            let value = resampled[channel][frame];
            let clamped = if value > 1.0 {
                1.0
            } else if value < -1.0 {
                -1.0
            } else {
                value
            };
            result.push((clamped * i16::MAX as f64) as i16);
        }
    }

    while result.len() < expected_output_frames * channels {
        result.push(0);
    }

    result
}

pub fn resample_mono(input: &[i16], input_sample_rate: u32, output_sample_rate: u32) -> Vec<i16> {
    resample(input, input_sample_rate, output_sample_rate, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::codecs::convert_s16_to_u8;
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
        let decoded_bytes = convert_s16_to_u8(&resampled);
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
