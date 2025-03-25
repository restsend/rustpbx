use rubato::Resampler;

pub fn resample(
    input: &[i16],
    input_sample_rate: u32,
    output_sample_rate: u32,
    channels: usize,
) -> Vec<i16> {
    if input_sample_rate == output_sample_rate {
        return input.to_vec();
    }
    let resample_ratio = output_sample_rate as f64 / input_sample_rate as f64;
    let params = rubato::SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: rubato::SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: rubato::WindowFunction::BlackmanHarris2,
    };

    if input.len() % channels != 0 {
        return Vec::new();
    }

    let frames = input.len() / channels;

    let mut resampler =
        match rubato::SincFixedIn::<f64>::new(resample_ratio, 2.0, params, frames, channels) {
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

    let mut result = Vec::with_capacity(resampled[0].len() * channels);
    for frame in 0..resampled[0].len() {
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
    result
}

pub fn resample_mono(input: &[i16], input_sample_rate: u32, output_sample_rate: u32) -> Vec<i16> {
    resample(input, input_sample_rate, output_sample_rate, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::codecs::convert_s16_to_u8;
    use hound::WavReader;
    use std::fs::File;
    use std::io::BufReader;
    use std::io::Write;

    #[test]
    fn test_resampler() {
        let reader =
            BufReader::new(File::open("testdata/sample.wav").expect("Failed to open file"));
        let mut wav_reader = WavReader::new(reader).expect("Failed to read wav file");
        let spec = wav_reader.spec();
        let channels = spec.channels as usize;

        let mut all_samples = Vec::new();
        for sample in wav_reader.samples::<i16>() {
            all_samples.push(sample.unwrap_or(0));
        }

        let resampled = resample(&all_samples, 16000, 8000, channels);

        let mut file = File::create("testdata/sample.8k.decoded").expect("Failed to create file");
        let decoded_bytes = convert_s16_to_u8(&resampled);
        file.write_all(&decoded_bytes)
            .expect("Failed to write file");
        println!(
            "resampled {}->{} samples",
            all_samples.len(),
            resampled.len()
        );
        println!("ffplay -f s16le -ar 8000 -i testdata/sample.8k.decoded");
    }
}
