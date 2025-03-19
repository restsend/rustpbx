pub fn convert_f32_to_pcm(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&sample| (sample * 32767.0) as i16)
        .collect()
}
