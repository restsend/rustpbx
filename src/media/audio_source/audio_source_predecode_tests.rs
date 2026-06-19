//! Tests for FileAudioSource pre-decode cache behaviour.

use super::AudioSource;
use super::FileAudioSource;
use super::mix_stereo_to_mono;
use crate::media::wav_reader::{SampleFormat, WavSpec, WavWriter};
use tempfile::NamedTempFile;

fn write_wav(sample_rate: u32, samples: &[i16]) -> NamedTempFile {
    let mut tmp = NamedTempFile::with_suffix(".wav").expect("tempfile");
    {
        let spec = WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer =
            WavWriter::new(std::io::BufWriter::new(tmp.as_file_mut()), spec).expect("WavWriter");
        for &s in samples {
            writer.write_sample(s).expect("write_sample");
        }
        writer.finalize().expect("finalize");
    }
    tmp
}

fn write_stereo_wav(sample_rate: u32, samples: &[i16]) -> NamedTempFile {
    let mut tmp = NamedTempFile::with_suffix(".wav").expect("tempfile");
    {
        let spec = WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer =
            WavWriter::new(std::io::BufWriter::new(tmp.as_file_mut()), spec).expect("WavWriter");
        for &s in samples {
            writer.write_sample(s).expect("write_sample");
        }
        writer.finalize().expect("finalize");
    }
    tmp
}

#[tokio::test]
async fn test_predecode_small_wav() {
    let samples: Vec<i16> = (0..4000).map(|i| (i % 1024) as i16).collect();
    let tmp = write_wav(8000, &samples);
    let path = tmp.path().to_string_lossy().to_string();

    let mut src = FileAudioSource::new(path, false).await.unwrap();
    assert!(
        !src.pcm_cache.is_empty(),
        "small file should be pre-decoded"
    );
    assert_eq!(src.pcm_cache.len(), 4000);
    assert_eq!(src.cached_channels, 1);
    assert_eq!(src.cached_sample_rate, 8000);

    let mut buf = vec![0i16; 320];
    let n = src.read_samples(&mut buf);
    assert_eq!(n, 320);
    assert_eq!(buf[0], 0);
    assert_eq!(buf[159], 159);
    assert_eq!(src.pcm_cache_pos, 320);

    let remaining = src.pcm_cache.len() - src.pcm_cache_pos;
    let mut big = vec![0i16; 5000];
    let n = src.read_samples(&mut big);
    assert_eq!(n, remaining);
    assert!(src.eof_reached);

    let mut buf2 = vec![0i16; 160];
    assert_eq!(src.read_samples(&mut buf2), 0);
}

#[tokio::test]
async fn test_predecode_loop_reset() {
    let samples: Vec<i16> = (0..800).map(|i| i as i16).collect();
    let tmp = write_wav(8000, &samples);
    let path = tmp.path().to_string_lossy().to_string();

    let mut src = FileAudioSource::new(path, true).await.unwrap();
    assert!(!src.pcm_cache.is_empty());

    let mut buf = vec![0i16; 400];
    assert_eq!(src.read_samples(&mut buf), 400);
    assert_eq!(src.read_samples(&mut buf), 400);
    assert!(src.eof_reached);

    let mut buf2 = vec![0i16; 160];
    assert_eq!(src.read_samples(&mut buf2), 160);
    assert_eq!(buf2[0], 0);
    assert!(!src.eof_reached);
}

#[tokio::test]
async fn test_predecode_skip_large_file() {
    let samples = vec![0i16; 4_000_000];
    let tmp = write_wav(8000, &samples);
    let path = tmp.path().to_string_lossy().to_string();

    let src = FileAudioSource::new(path, false).await.unwrap();
    assert!(
        src.pcm_cache.is_empty(),
        "file > 5 MB should skip pre-decode"
    );
    assert!(
        src.wav_reader.is_some(),
        "streaming reader should be present"
    );
}

#[test]
fn test_mix_stereo_to_mono_two_channels() {
    let stereo = vec![10i16, 20, 30, 40, 50, 60];
    let mono = mix_stereo_to_mono(&stereo, 2);
    assert_eq!(mono.len(), 3);
    assert_eq!(mono[0], 15);
    assert_eq!(mono[1], 35);
    assert_eq!(mono[2], 55);
}

#[test]
fn test_mix_stereo_to_mono_identity() {
    let mono_in = vec![42i16, 100, -50];
    let mono_out = mix_stereo_to_mono(&mono_in, 1);
    assert_eq!(mono_out, mono_in);
}

#[tokio::test]
async fn test_predecode_respects_channels() {
    let samples: Vec<i16> = (0..800).map(|i| i as i16).collect();
    let tmp = write_stereo_wav(8000, &samples);
    let path = tmp.path().to_string_lossy().to_string();

    let src = FileAudioSource::new(path, false).await.unwrap();
    assert!(!src.pcm_cache.is_empty());
    assert_eq!(src.pcm_cache.len(), 400);
    assert_eq!(src.cached_channels, 1);
}
