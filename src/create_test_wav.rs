use hound::{SampleFormat, WavSpec, WavWriter};
use std::f32::consts::PI;
use std::path::Path;

fn create_test_wav(
    path: &Path,
    sample_rate: u32,
    duration_ms: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    let mut writer = WavWriter::create(path, spec)?;

    // Generate sine wave
    let num_samples = (sample_rate * duration_ms) / 1000;
    let frequency = 440.0; // A4 note

    for t in 0..num_samples {
        let sample = (t as f32 / sample_rate as f32 * frequency * 2.0 * PI).sin();
        let amplitude = 0.5;
        let sample_i16 = (sample * amplitude * 32767.0) as i16;
        writer.write_sample(sample_i16)?;
    }

    writer.finalize()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create the fixtures directory if it doesn't exist
    let fixtures_dir = Path::new("src/media/tests/fixtures");
    if !fixtures_dir.exists() {
        std::fs::create_dir_all(fixtures_dir)?;
    }

    // Create test WAV files with different sample rates
    let test_wav_8k = fixtures_dir.join("test_8k.wav");
    let test_wav_16k = fixtures_dir.join("test_16k.wav");
    let test_wav_44k = fixtures_dir.join("test_44k.wav");

    create_test_wav(&test_wav_8k, 8000, 1000)?;
    create_test_wav(&test_wav_16k, 16000, 1000)?;
    create_test_wav(&test_wav_44k, 44100, 1000)?;

    println!("Created test WAV files in {:?}", fixtures_dir);

    Ok(())
}
