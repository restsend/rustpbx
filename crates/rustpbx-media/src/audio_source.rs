use anyhow::{Result, anyhow};
use audio_codec::{CodecType, Resampler, create_decoder};
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::wav_reader::{WavFormat, WavReader, format_issues};
pub trait AudioSource: Send + Sync {
    fn read_samples(&mut self, buffer: &mut [i16]) -> usize;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn has_data(&self) -> bool;
    fn reset(&mut self) -> Result<()>;
}

pub struct FileAudioSource {
    loop_playback: bool,
    eof_reached: bool,
    /// Pre-decoded mono PCM (native sample rate), populated once at
    /// construction by reading the whole file asynchronously. All
    /// `read_samples` calls copy from here — no file I/O on the hot path.
    pub(crate) pcm_cache: Vec<i16>,
    pub(crate) pcm_cache_pos: usize,
    cached_channels: u16,
    cached_sample_rate: u32,
}

impl FileAudioSource {
    /// Read + decode the audio file. File I/O is async (`tokio::fs` for local,
    /// `reqwest` for http) so this never blocks the async runtime; callers can
    /// `.await` it directly (no `spawn_blocking` / `block_on` needed). After
    /// construction, [`AudioSource::read_samples`] serves from the in-memory
    /// `pcm_cache`.
    pub async fn new(file_path: String, loop_playback: bool) -> Result<Self> {
        let (bytes, label) =
            if file_path.starts_with("http://") || file_path.starts_with("https://") {
                (Self::download_bytes(&file_path).await?, file_path.clone())
            } else {
                if !Path::new(&file_path).exists() {
                    return Err(anyhow!("Audio file not found: {}", file_path));
                }
                let b = tokio::fs::read(&file_path)
                    .await
                    .map_err(|e| anyhow!("Audio file read error {file_path}: {e}"))?;
                (b, file_path.clone())
            };

        let extension = Path::new(&file_path)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        let (pcm, channels, sample_rate) = decode_bytes(&bytes, &extension, &label)?;
        // An empty PCM buffer is valid (e.g. a 0-sample WAV): the source acts
        // as silence / loops silence. Don't reject it.
        Ok(Self {
            loop_playback,
            eof_reached: false,
            pcm_cache: pcm,
            pcm_cache_pos: 0,
            cached_channels: channels,
            cached_sample_rate: sample_rate,
        })
    }

    async fn download_bytes(url: &str) -> Result<Vec<u8>> {
        let response = rustpbx_http_util::shared_keepalive_client()
            .get(url)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to download audio file: {}", e))?;
        if !response.status().is_success() {
            return Err(anyhow!("HTTP error: {}", response.status()));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| anyhow!("Failed to read response body: {}", e))?;
        Ok(bytes.to_vec())
    }
}

/// Decode an in-memory audio byte buffer (wav / mp3 / raw) into mono PCM at
/// the source's native sample rate. Pure computation — no file I/O.
fn decode_bytes(bytes: &[u8], extension: &str, label: &str) -> Result<(Vec<i16>, u16, u32)> {
    match extension {
        "wav" => {
            let mut reader = WavReader::new(std::io::Cursor::new(bytes.to_vec()))?;
            let (channels, sample_rate, format) = {
                let spec = reader.spec();
                (spec.channels, spec.sample_rate, reader.format())
            };
            let bits = reader.spec().bits_per_sample;
            for issue in format_issues(format, reader.spec()) {
                warn!(file = %label, format_tag = ?format, bits_per_sample = bits, issue = %issue,
                    "WAV header inconsistent with data — playback may sound like static/noise");
            }
            let pcm: Vec<i16> = reader.samples().filter_map(|s| s.ok()).collect();
            let pcm = mix_stereo_to_mono(&pcm, channels as usize);
            if looks_like_pcm_bytes_under_g711(format, &pcm) {
                warn!(
                    file = %label,
                    "G.711 WAV whose even/odd samples have mismatched noise — the data is \
                     likely 16-bit PCM bytes mislabeled as μ-law/a-law (classic loud-static symptom)"
                );
            }
            debug!(file = %label, samples = pcm.len(), rate = sample_rate, channels, format_tag = ?format, bits_per_sample = bits, "Decoded WAV");
            Ok((pcm, 1, sample_rate))
        }
        "mp3" => {
            let mut decoder = minimp3::Decoder::new(std::io::Cursor::new(bytes.to_vec()));
            let mut pcm: Vec<i16> = Vec::new();
            let mut sample_rate = 44100u32;
            let mut channels = 2u16;
            loop {
                match decoder.next_frame() {
                    Ok(frame) => {
                        sample_rate = frame.sample_rate as u32;
                        channels = frame.channels as u16;
                        pcm.extend_from_slice(&frame.data);
                    }
                    Err(minimp3::Error::Eof) => break,
                    Err(e) => {
                        warn!(file = %label, error = %e, "MP3 decode error");
                        break;
                    }
                }
            }
            let pcm = mix_stereo_to_mono(&pcm, channels as usize);
            debug!(file = %label, samples = pcm.len(), rate = sample_rate, "Decoded MP3");
            Ok((pcm, 1, sample_rate))
        }
        _ => {
            let codec = match CodecType::try_from(extension) {
                Ok(c) => c,
                Err(_) => match extension {
                    "u" | "ulaw" => CodecType::PCMU,
                    "a" | "alaw" => CodecType::PCMA,
                    _ => {
                        warn!(extension = %extension, "Unknown raw extension, assuming PCMU");
                        CodecType::PCMU
                    }
                },
            };
            let mut decoder = create_decoder(codec);
            let frame_size = match codec {
                CodecType::PCMU | CodecType::PCMA | CodecType::G722 => 160,
                CodecType::G729 => 20,
                _ => 160,
            };
            let mut pcm: Vec<i16> = Vec::new();
            for chunk in bytes.chunks(frame_size) {
                pcm.extend_from_slice(&decoder.decode(chunk));
            }
            let rate = codec.samplerate();
            debug!(file = %label, samples = pcm.len(), rate = rate, "Decoded raw codec file");
            Ok((pcm, 1, rate))
        }
    }
}

/// Mix interleaved multichannel PCM down to mono.
pub(crate) fn mix_stereo_to_mono(samples: &[i16], channels: usize) -> Vec<i16> {
    if channels == 1 {
        return samples.to_vec();
    }
    let mut mono = Vec::with_capacity(samples.len() / channels);
    for chunk in samples.chunks(channels) {
        let sum: i32 = chunk.iter().map(|&s| s as i32).sum();
        mono.push((sum / channels as i32) as i16);
    }
    mono
}

/// Zero-crossing rate of the first difference: fraction of non-zero adjacent
/// steps that change sign. Smooth band-limited signals (speech / tones) have a
/// low rate; high-frequency or random content approaches 1.0.
fn zero_crossing_rate(samples: &[i16]) -> f64 {
    let mut sign_changes = 0usize;
    let mut prev: Option<bool> = None;
    let mut non_zero = 0usize;
    for w in samples.windows(2) {
        let d = w[1] as i32 - w[0] as i32;
        if d == 0 {
            continue;
        }
        let pos = d > 0;
        if let Some(p) = prev
            && p != pos
        {
            sign_changes += 1;
        }
        prev = Some(pos);
        non_zero += 1;
    }
    if non_zero < 2 {
        return 0.0;
    }
    sign_changes as f64 / (non_zero - 1) as f64
}

/// Heuristic that flags a G.711 (μ-law / a-law) WAV whose payload is actually
/// *16-bit linear PCM bytes* stored under a companding header (format tag 7/6) —
/// the "decoded as loud static" case.
///
/// Reading 16-bit PCM little-endian bytes one byte at a time, the even-indexed
/// samples decode the PCM *low* bytes (essentially quantization noise) while the
/// odd-indexed samples decode the PCM *high* bytes (the real signal). The two
/// subsequences therefore have very different spectral density. A genuine G.711
/// stream interleaves two samples of the *same* smooth signal, so its even/odd
/// subsequences have nearly identical zero-crossing rates.
pub(crate) fn looks_like_pcm_bytes_under_g711(format: WavFormat, samples: &[i16]) -> bool {
    if !matches!(format, WavFormat::Pcmu | WavFormat::Pcma) || samples.len() < 512 {
        return false;
    }
    let even: Vec<i16> = samples.iter().step_by(2).copied().collect();
    let odd: Vec<i16> = samples.iter().skip(1).step_by(2).copied().collect();
    let (rate_even, rate_odd) = (zero_crossing_rate(&even), zero_crossing_rate(&odd));
    let (hi, lo) = (rate_even.max(rate_odd), rate_even.min(rate_odd));
    // The noisier subsequence must be clearly noisier (>1.5x), and the cleaner
    // one must actually be smooth (<50% zero crossings) — otherwise both halves
    // are just legitimately high-frequency audio and we stay quiet.
    hi > 1.5 * lo && lo < 0.5
}

impl AudioSource for FileAudioSource {
    fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
        if self.eof_reached && !self.loop_playback {
            return 0;
        }
        if self.eof_reached
            && let Err(e) = self.reset()
        {
            warn!("Failed to reset file source: {}", e);
            return 0;
        }

        let remaining = self.pcm_cache.len().saturating_sub(self.pcm_cache_pos);
        if remaining == 0 {
            self.eof_reached = true;
            return 0;
        }
        let copy = remaining.min(buffer.len());
        buffer[..copy]
            .copy_from_slice(&self.pcm_cache[self.pcm_cache_pos..self.pcm_cache_pos + copy]);
        self.pcm_cache_pos += copy;
        if self.pcm_cache_pos >= self.pcm_cache.len() {
            self.eof_reached = true;
        }
        copy
    }

    fn sample_rate(&self) -> u32 {
        self.cached_sample_rate
    }

    fn channels(&self) -> u16 {
        self.cached_channels
    }

    fn has_data(&self) -> bool {
        self.pcm_cache_pos < self.pcm_cache.len() || self.loop_playback
    }

    fn reset(&mut self) -> Result<()> {
        self.eof_reached = false;
        self.pcm_cache_pos = 0;
        Ok(())
    }
}

pub struct SilenceSource {
    sample_rate: u32,
}

impl SilenceSource {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate }
    }
}

impl AudioSource for SilenceSource {
    fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
        for sample in buffer.iter_mut() {
            *sample = 0;
        }
        buffer.len()
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        1
    }

    fn has_data(&self) -> bool {
        true
    }

    fn reset(&mut self) -> Result<()> {
        Ok(())
    }
}

pub struct ResamplingAudioSource {
    source: Box<dyn AudioSource>,
    resampler: Option<Resampler>,
    source_sample_rate: u32,
    target_sample_rate: u32,
    intermediate_buffer: Vec<i16>,
}

impl ResamplingAudioSource {
    pub fn new(source: Box<dyn AudioSource>, target_sample_rate: u32) -> Self {
        let source_rate = source.sample_rate();
        let resampler = if source_rate != target_sample_rate {
            Some(Resampler::new(
                source_rate as usize,
                target_sample_rate as usize,
            ))
        } else {
            None
        };

        Self {
            source_sample_rate: source_rate,
            source,
            resampler,
            target_sample_rate,
            intermediate_buffer: Vec::new(),
        }
    }
}

impl AudioSource for ResamplingAudioSource {
    fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
        if let Some(ref mut resampler) = self.resampler {
            let needed_source = (buffer.len() as u64 * self.source_sample_rate as u64)
                .div_ceil(self.target_sample_rate as u64) as usize;

            self.intermediate_buffer.resize(needed_source, 0);
            let read = self.source.read_samples(&mut self.intermediate_buffer);

            if read == 0 {
                return 0;
            }

            let resampled = resampler.resample(&self.intermediate_buffer[..read]);
            let copy_len = resampled.len().min(buffer.len());
            buffer[..copy_len].copy_from_slice(&resampled[..copy_len]);
            copy_len
        } else {
            self.source.read_samples(buffer)
        }
    }

    fn sample_rate(&self) -> u32 {
        self.target_sample_rate
    }

    fn channels(&self) -> u16 {
        self.source.channels()
    }

    fn has_data(&self) -> bool {
        self.source.has_data()
    }

    fn reset(&mut self) -> Result<()> {
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
        }
        self.source.reset()
    }
}

pub struct AudioSourceManager {
    current_source: Arc<Mutex<Option<Box<dyn AudioSource>>>>,
    target_sample_rate: u32,
    completion_notify: Arc<Notify>,
}

impl AudioSourceManager {
    pub fn new(target_sample_rate: u32) -> Self {
        Self {
            current_source: Arc::new(Mutex::new(None)),
            target_sample_rate,
            completion_notify: Arc::new(Notify::new()),
        }
    }

    pub async fn switch_to_file(&self, file_path: String, loop_playback: bool) -> Result<()> {
        let file_source = FileAudioSource::new(file_path.clone(), loop_playback).await?;
        let resampling_source =
            ResamplingAudioSource::new(Box::new(file_source), self.target_sample_rate);

        let mut current = self.current_source.lock();
        *current = Some(Box::new(resampling_source));

        debug!(
            file_path = %file_path,
            loop_playback,
            "Switched to file audio source"
        );

        Ok(())
    }

    pub fn switch_to_silence(&self) {
        let silence = SilenceSource::new(self.target_sample_rate);
        let mut current = self.current_source.lock();
        *current = Some(Box::new(silence));

        debug!("Switched to silence audio source");
    }

    pub fn read_samples(&self, buffer: &mut [i16]) -> usize {
        let mut current = self.current_source.lock();
        if let Some(ref mut source) = *current {
            let read = source.read_samples(buffer);
            if read == 0 {
                self.completion_notify.notify_one();
            }
            read
        } else {
            for sample in buffer.iter_mut() {
                *sample = 0;
            }
            buffer.len()
        }
    }

    pub fn has_active_source(&self) -> bool {
        let current = self.current_source.lock();
        current.is_some()
    }

    pub async fn wait_for_completion(&self) {
        self.completion_notify.notified().await;
    }
}

pub fn estimate_audio_duration(file_path: &str) -> std::time::Duration {
    use std::path::Path;

    let ext = Path::new(file_path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "wav" => {
            if let Ok(reader) = WavReader::open(file_path) {
                let spec = reader.spec();
                let duration = reader.duration();
                let secs = if spec.sample_rate > 0 {
                    duration as f64 / spec.sample_rate as f64
                } else {
                    5.0
                };
                std::time::Duration::from_secs_f64(secs.max(0.005))
            } else {
                std::time::Duration::from_secs(5)
            }
        }
        "mp3" => {
            // Decode and count samples for an accurate duration — the old
            // 128 kbps file-size heuristic is wrong for low-bitrate/VBR MP3s
            // (e.g. the shipped trilingual announcement is ~32 kbps, so a
            // bitrate estimate would cut the prompt to ~1/4 of its length).
            if let Ok(bytes) = std::fs::read(file_path) {
                let mut decoder = minimp3::Decoder::new(std::io::Cursor::new(bytes));
                let mut total_samples = 0u64;
                let mut sample_rate = 44100u32;
                let mut channels = 2u16;
                loop {
                    match decoder.next_frame() {
                        Ok(frame) => {
                            sample_rate = frame.sample_rate as u32;
                            channels = frame.channels as u16;
                            total_samples += frame.data.len() as u64;
                        }
                        Err(minimp3::Error::Eof) => break,
                        Err(_) => break,
                    }
                }
                let rate = sample_rate.max(1) as f64 * channels.max(1) as f64;
                if total_samples > 0 {
                    let secs = total_samples as f64 / rate;
                    return std::time::Duration::from_secs_f64(secs.max(0.1));
                }
            }
            std::time::Duration::from_secs(5)
        }
        "pcmu" | "ulaw" | "u" | "pcma" | "alaw" | "a" => {
            if let Ok(meta) = std::fs::metadata(file_path) {
                std::time::Duration::from_millis(meta.len().max(100))
            } else {
                std::time::Duration::from_secs(5)
            }
        }
        "g722" => {
            if let Ok(meta) = std::fs::metadata(file_path) {
                let frames = meta.len() / 160;
                std::time::Duration::from_millis(frames.max(1) * 20)
            } else {
                std::time::Duration::from_secs(5)
            }
        }
        "g729" => {
            if let Ok(meta) = std::fs::metadata(file_path) {
                let frames = meta.len() / 10;
                std::time::Duration::from_millis(frames.max(1) * 10)
            } else {
                std::time::Duration::from_secs(5)
            }
        }
        _ => {
            if let Ok(meta) = std::fs::metadata(file_path) {
                let secs = meta.len() as f64 / 16_000.0;
                std::time::Duration::from_secs_f64(secs.max(0.1))
            } else {
                std::time::Duration::from_secs(5)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav_reader::{SampleFormat, WavSpec, WavWriter};
    use audio_codec::create_encoder;
    use std::io::Write;
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
            let mut writer = WavWriter::new(std::io::BufWriter::new(tmp.as_file_mut()), spec)
                .expect("WavWriter");
            for &s in samples {
                writer.write_sample(s).expect("write_sample");
            }
            writer.finalize().expect("finalize");
        }
        tmp
    }

    fn write_bytes_wav(bytes: &[u8]) -> NamedTempFile {
        let mut tmp = NamedTempFile::with_suffix(".wav").expect("tempfile");
        tmp.write_all(bytes).expect("write bytes");
        tmp
    }

    fn build_wav(
        format_tag: u16,
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        data: &[u8],
    ) -> Vec<u8> {
        let block_align = channels * (bits_per_sample / 8);
        let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&format_tag.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits_per_sample.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(data);
        wav
    }

    fn sine_pcm(n: usize, rate: u32, freq: f32, amp: f32) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                (amp * (2.0 * std::f32::consts::PI * freq * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn test_silence_source_fills_zeros() {
        let mut source = SilenceSource::new(8000);
        let mut buffer = vec![999i16; 160];
        let read = source.read_samples(&mut buffer);

        assert_eq!(read, 160);
        assert!(buffer.iter().all(|&s| s == 0), "silence must be all zeros");
        assert!(source.has_data(), "silence never ends");
        assert_eq!(source.sample_rate(), 8000);
        assert_eq!(source.channels(), 1);
    }

    #[test]
    fn test_silence_source_reset() {
        let mut source = SilenceSource::new(16000);
        source.reset().expect("reset");
        let mut buffer = vec![1i16; 320];
        source.read_samples(&mut buffer);
        assert!(buffer.iter().all(|&s| s == 0));
    }

    #[test]
    fn test_resampling_downsample_44100_to_8000() {
        struct FixedRateSource {
            rate: u32,
            data: Vec<i16>,
            pos: usize,
        }
        impl AudioSource for FixedRateSource {
            fn read_samples(&mut self, buf: &mut [i16]) -> usize {
                let avail = self.data.len() - self.pos;
                let n = buf.len().min(avail);
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                n
            }
            fn sample_rate(&self) -> u32 {
                self.rate
            }
            fn channels(&self) -> u16 {
                1
            }
            fn has_data(&self) -> bool {
                self.pos < self.data.len()
            }
            fn reset(&mut self) -> Result<()> {
                self.pos = 0;
                Ok(())
            }
        }

        let samples_44k: Vec<i16> = (0..4410).map(|i| (i % 1000) as i16).collect();
        let src = FixedRateSource {
            rate: 44100,
            data: samples_44k,
            pos: 0,
        };
        let mut resampler = ResamplingAudioSource::new(Box::new(src), 8000);

        let mut out = vec![0i16; 160];
        let read = resampler.read_samples(&mut out);
        assert!(
            read > 0,
            "downsample 44100→8000: expected non-zero output, got 0"
        );
    }

    #[test]
    fn test_resampling_upsample_8000_to_16000() {
        let silence = SilenceSource::new(8000);
        let mut resampling = ResamplingAudioSource::new(Box::new(silence), 16000);

        assert_eq!(resampling.sample_rate(), 16000);
        let mut buffer = vec![0i16; 320];
        let read = resampling.read_samples(&mut buffer);
        assert!(read > 0, "upsample 8000→16000 must produce output");
    }

    #[test]
    fn test_resampling_same_rate_passthrough() {
        let silence = SilenceSource::new(8000);
        let mut resampling = ResamplingAudioSource::new(Box::new(silence), 8000);
        let mut buf = vec![0i16; 160];
        let read = resampling.read_samples(&mut buf);
        assert_eq!(read, 160);
    }

    #[test]
    fn test_resampling_reset_clears_internal_resampler_state() {
        // A fixed-rate source that reports how many samples it has produced.
        struct FixedRateSource {
            rate: u32,
            produced: u64,
        }
        impl AudioSource for FixedRateSource {
            fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
                let n = buffer.len();
                self.produced += n as u64;
                buffer.fill(1000);
                n
            }
            fn sample_rate(&self) -> u32 {
                self.rate
            }
            fn channels(&self) -> u16 {
                1
            }
            fn has_data(&self) -> bool {
                true
            }
            fn reset(&mut self) -> Result<()> {
                self.produced = 0;
                Ok(())
            }
        }

        let mut resampling = ResamplingAudioSource::new(
            Box::new(FixedRateSource {
                rate: 24000,
                produced: 0,
            }),
            48000,
        );

        // After a full read the resampler holds internal history/phase. Reset
        // must clear it so a second read starts from a clean slate (looping).
        let mut buf = vec![0i16; 960];
        let read = resampling.read_samples(&mut buf);
        assert!(read > 0, "upsample 24000→48000 must produce output");
        resampling.reset().expect("reset");

        // Consume again — a stale resampler would offset phase; we only assert
        // that reads still succeed with sane output after reset.
        let mut buf2 = vec![0i16; 960];
        let read2 = resampling.read_samples(&mut buf2);
        assert!(read2 > 0, "read after reset must still produce output");
        assert_eq!(
            read2, 960,
            "20ms @48kHz should yield a full 960-sample frame after reset"
        );
    }

    #[test]
    fn test_audio_source_manager_silence() {
        let manager = AudioSourceManager::new(8000);
        manager.switch_to_silence();
        assert!(manager.has_active_source());

        let mut buffer = vec![0i16; 160];
        let read = manager.read_samples(&mut buffer);
        assert_eq!(read, 160, "silence source always fills the buffer");
    }

    #[test]
    fn test_audio_source_manager_no_source_returns_silence() {
        let manager = AudioSourceManager::new(8000);
        let mut buf = vec![999i16; 160];
        let read = manager.read_samples(&mut buf);
        assert_eq!(read, 160);
        assert!(
            buf.iter().all(|&s| s == 0),
            "no-source path must produce silence"
        );
    }

    #[tokio::test]
    async fn test_audio_source_manager_completion_notify_on_exhaustion() {
        struct OneShotSource {
            data: Vec<i16>,
            pos: usize,
        }
        impl AudioSource for OneShotSource {
            fn read_samples(&mut self, buf: &mut [i16]) -> usize {
                let avail = self.data.len() - self.pos;
                let n = buf.len().min(avail);
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                n
            }
            fn sample_rate(&self) -> u32 {
                8000
            }
            fn channels(&self) -> u16 {
                1
            }
            fn has_data(&self) -> bool {
                self.pos < self.data.len()
            }
            fn reset(&mut self) -> Result<()> {
                self.pos = 0;
                Ok(())
            }
        }

        let manager = Arc::new(AudioSourceManager::new(8000));
        {
            let src = OneShotSource {
                data: vec![1i16; 160],
                pos: 0,
            };
            let resampled = ResamplingAudioSource::new(Box::new(src), 8000);
            let mut current = manager.current_source.lock();
            *current = Some(Box::new(resampled));
        }

        let manager_clone = manager.clone();
        let notified = tokio::spawn(async move {
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                manager_clone.wait_for_completion(),
            )
            .await
        });

        let mut buf = vec![0i16; 160];
        let read = manager.read_samples(&mut buf);
        assert_eq!(read, 160, "should read all 160 samples");

        let read2 = manager.read_samples(&mut buf);
        assert_eq!(read2, 0, "source should be exhausted");

        notified
            .await
            .expect("join")
            .expect("completion notify not fired within 500 ms");
    }

    #[tokio::test]
    async fn test_wav_file_source_reads_samples() {
        let pcm: Vec<i16> = (0i16..160).collect();
        let tmp = write_wav(8000, &pcm);

        let mut src = FileAudioSource::new(tmp.path().to_str().unwrap().to_string(), false)
            .await
            .expect("FileAudioSource::new for WAV");

        assert_eq!(src.sample_rate(), 8000);
        assert_eq!(src.channels(), 1);
        assert!(src.has_data());

        let mut buf = vec![0i16; 160];
        let read = src.read_samples(&mut buf);
        assert_eq!(read, 160, "should read all 160 samples");
        assert_eq!(&buf[..], &pcm[..], "samples must match what was written");
    }

    #[tokio::test]
    async fn test_wav_file_source_eof_no_loop() {
        let pcm: Vec<i16> = vec![42i16; 80];
        let tmp = write_wav(8000, &pcm);

        let mut src = FileAudioSource::new(tmp.path().to_str().unwrap().to_string(), false)
            .await
            .expect("FileAudioSource::new");

        let mut buf = vec![0i16; 160];
        let _read1 = src.read_samples(&mut buf);
        assert!(!src.has_data(), "no loop → EOF marks source as exhausted");

        let read2 = src.read_samples(&mut buf);
        assert_eq!(read2, 0);
    }

    #[tokio::test]
    async fn test_wav_file_source_loop() {
        let pcm: Vec<i16> = vec![1i16; 80];
        let tmp = write_wav(8000, &pcm);

        let mut src = FileAudioSource::new(tmp.path().to_str().unwrap().to_string(), true)
            .await
            .expect("FileAudioSource::new");

        let mut buf = vec![0i16; 240];
        let _read = src.read_samples(&mut buf);
        assert!(src.has_data(), "looping source must always have data");
    }

    #[tokio::test]
    async fn test_wav_file_source_missing_file() {
        let result = FileAudioSource::new("/nonexistent/path/sample.wav".to_string(), false).await;
        assert!(result.is_err(), "missing file must return an error");
    }

    #[tokio::test]
    async fn test_estimate_duration_wav_exact() {
        let pcm: Vec<i16> = vec![0i16; 8000];
        let tmp = write_wav(8000, &pcm);
        let dur = estimate_audio_duration(tmp.path().to_str().unwrap());
        assert!(
            dur.as_millis() >= 995 && dur.as_millis() <= 1005,
            "WAV 1-second file: expected ~1000 ms, got {} ms",
            dur.as_millis()
        );
    }

    #[test]
    fn test_estimate_duration_wav_short() {
        let pcm: Vec<i16> = vec![0i16; 160];
        let tmp = write_wav(8000, &pcm);
        let dur = estimate_audio_duration(tmp.path().to_str().unwrap());
        assert!(
            dur.as_millis() >= 15 && dur.as_millis() <= 25,
            "WAV 160-sample/8k file: expected ~20 ms, got {} ms",
            dur.as_millis()
        );
    }

    #[test]
    fn test_estimate_duration_pcmu_raw() {
        let data = vec![0u8; 8000];
        let mut tmp = NamedTempFile::with_suffix(".pcmu").expect("tempfile");
        tmp.write_all(&data).unwrap();
        let dur = estimate_audio_duration(tmp.path().to_str().unwrap());
        assert!(
            dur.as_millis() >= 7900 && dur.as_millis() <= 8100,
            "PCMU 8000-byte file: expected ~8000 ms, got {} ms",
            dur.as_millis()
        );
    }

    #[test]
    fn test_estimate_duration_g722() {
        let data = vec![0u8; 1600];
        let mut tmp = NamedTempFile::with_suffix(".g722").expect("tempfile");
        tmp.write_all(&data).unwrap();
        let dur = estimate_audio_duration(tmp.path().to_str().unwrap());
        assert!(
            dur.as_millis() >= 190 && dur.as_millis() <= 210,
            "G.722 1600-byte file: expected ~200 ms, got {} ms",
            dur.as_millis()
        );
    }

    #[test]
    fn test_estimate_duration_g729() {
        let data = vec![0u8; 100];
        let mut tmp = NamedTempFile::with_suffix(".g729").expect("tempfile");
        tmp.write_all(&data).unwrap();
        let dur = estimate_audio_duration(tmp.path().to_str().unwrap());
        assert!(
            dur.as_millis() >= 90 && dur.as_millis() <= 110,
            "G.729 100-byte file: expected ~100 ms, got {} ms",
            dur.as_millis()
        );
    }

    #[test]
    fn test_estimate_duration_mp3_decodes_actual_length() {
        // The shipped English service-unavailable announcement (~5 s, low
        // bitrate). A bitrate-based estimate would undercount it; decoding must
        // return the real duration so the prompt plays in full before a reject.
        let path = Path::new("config/sounds/service_unavailable_en.mp3");
        if !path.exists() {
            eprintln!("skipping: config/sounds/service_unavailable_en.mp3 absent (not in workspace root)");
            return;
        }
        let dur = estimate_audio_duration(path.to_str().unwrap());
        assert!(
            (dur.as_secs_f64() - 5.18).abs() < 0.5,
            "service_unavailable_en.mp3: expected ~5.18 s, got {:.2}s",
            dur.as_secs_f64()
        );
    }

    #[test]
    fn test_estimate_duration_missing_file_returns_default() {
        let dur = estimate_audio_duration("/nonexistent/phantom.wav");
        assert_eq!(
            dur.as_secs(),
            5,
            "missing file must return 5-second default"
        );
    }

    #[test]
    fn test_estimate_duration_unknown_extension_uses_pcm_formula() {
        let data = vec![0u8; 16000];
        let mut tmp = NamedTempFile::with_suffix(".xyz").expect("tempfile");
        tmp.write_all(&data).unwrap();
        let dur = estimate_audio_duration(tmp.path().to_str().unwrap());
        assert!(
            dur.as_millis() >= 900 && dur.as_millis() <= 1100,
            "Unknown extension 16000-byte file: expected ~1000 ms, got {} ms",
            dur.as_millis()
        );
    }

    #[tokio::test]
    async fn test_pcmu_wav_round_trip() {
        // A genuine μ-law WAV (format tag 7, 8-bit, mono, 8 kHz) must decode to
        // the same linear PCM that was encoded, so playback is always correct
        // regardless of what codec the RTP leg negotiates afterwards.
        let pcm = sine_pcm(1600, 8000, 440.0, 16_000.0);
        let ulaw = create_encoder(CodecType::PCMU).encode(&pcm);
        let wav = build_wav(0x0007, 8000, 1, 8, &ulaw);
        let tmp = write_bytes_wav(&wav);

        let mut src = FileAudioSource::new(tmp.path().to_str().unwrap().to_string(), false)
            .await
            .expect("FileAudioSource::new for μ-law wav");
        assert_eq!(src.sample_rate(), 8000);
        assert_eq!(src.channels(), 1);

        let mut buf = vec![0i16; pcm.len()];
        let read = src.read_samples(&mut buf);
        assert_eq!(read, pcm.len(), "μ-law wav should decode all samples");

        for (decoded, original) in buf.iter().zip(pcm.iter()) {
            assert!(
                (*decoded as i32 - *original as i32).abs() <= 600,
                "μ-law round-trip drifted too far: {decoded} vs {original}"
            );
        }
    }

    #[test]
    fn test_g711_wav_with_pcm_payload_is_flagged() {
        // Linear 16-bit PCM bytes stored under a μ-law header (tag 7) — the
        // "decode garbage to loud static" mislabel. Decoding each 16-bit PCM
        // byte as a μ-law code produces wildly jumping samples that must be
        // detected.
        let pcm = sine_pcm(1600, 8000, 440.0, 16_000.0);
        let mut pcm_bytes = Vec::new();
        for s in &pcm {
            pcm_bytes.extend_from_slice(&s.to_le_bytes());
        }
        let mislabeled = build_wav(0x0007, 8000, 1, 8, &pcm_bytes);
        let mut reader = WavReader::new(std::io::Cursor::new(mislabeled)).unwrap();
        let samples: Vec<i16> = reader.samples().filter_map(|s| s.ok()).collect();
        assert!(
            looks_like_pcm_bytes_under_g711(reader.format(), &samples),
            "linear PCM bytes under a μ-law header must be detected as static-prone"
        );
    }

    #[test]
    fn test_genuine_g711_wav_not_flagged() {
        // A real μ-law WAV (encoded then decoded) must NOT be flagged: its
        // decoded PCM is smooth, so the heuristic must stay quiet.
        let pcm = sine_pcm(1600, 8000, 440.0, 16_000.0);
        let ulaw = create_encoder(CodecType::PCMU).encode(&pcm);
        let genuine = build_wav(0x0007, 8000, 1, 8, &ulaw);
        let mut reader = WavReader::new(std::io::Cursor::new(genuine)).unwrap();
        let samples: Vec<i16> = reader.samples().filter_map(|s| s.ok()).collect();
        assert!(
            !looks_like_pcm_bytes_under_g711(reader.format(), &samples),
            "genuine μ-law audio must not be flagged as a PCM mislabel"
        );
    }

    #[test]
    fn test_g711_heuristic_ignores_non_g711_profiles() {
        let pcm = sine_pcm(1600, 8000, 440.0, 16_000.0);
        let ulaw = create_encoder(CodecType::PCMU).encode(&pcm);
        let mislabeled = build_wav(0x0007, 8000, 1, 8, &ulaw);
        let mut reader = WavReader::new(std::io::Cursor::new(mislabeled)).unwrap();
        let samples: Vec<i16> = reader.samples().filter_map(|s| s.ok()).collect();
        // Non-G.711 format tag → never flagged.
        assert!(!looks_like_pcm_bytes_under_g711(WavFormat::Pcm, &samples));
        // Too few samples → never flagged.
        assert!(!looks_like_pcm_bytes_under_g711(WavFormat::Pcmu, &samples[..100]));
    }
}

#[cfg(test)]
mod audio_source_predecode_tests;
