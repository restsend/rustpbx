/// Audio sources for unified PC architecture
///
/// This module provides a trait-based audio source system that enables dynamic
/// audio source switching without recreating PeerConnections or renegotiating SDP.
///
/// # Supported Formats
/// - **WAV files**: Using `hound` crate for reading PCM data
/// - **MP3 files**: Using `minimp3` crate for decoding to PCM
/// - **Raw audio**: PCMU, PCMA, G.722, G.729 encoded files
///
/// # File Sources
/// - **Local files**: Direct file path (e.g., "/path/to/audio.wav")
/// - **HTTP/HTTPS**: Remote files are automatically downloaded to temporary storage
///   (e.g., "https://example.com/audio.mp3")
///
/// # Architecture
/// The `AudioSource` trait defines a common interface for all audio sources.
/// `FileAudioSource` handles actual file I/O and decoding, with format-specific
/// implementations for WAV, MP3, and raw audio files.
///
/// `ResamplingAudioSource` wraps any `AudioSource` and provides automatic
/// sample rate conversion using the `audio_codec::Resampler`.
///
/// `AudioSourceManager` manages the current active source and allows thread-safe
/// switching between different audio sources at runtime.
///
/// # Usage in Queue System
/// Queue hold music and prompts use the unified PC architecture:
/// 1. Create a FileTrack with an initial audio file
/// 2. Switch audio sources dynamically via `FileTrack::switch_audio_source()`
/// 3. No re-INVITE or SDP renegotiation required
///
/// # Example
/// ```ignore
/// let manager = AudioSourceManager::new(8000, cancel_token);
/// manager.switch_to_file("hold_music.wav".to_string(), true)?;
/// // Later, switch to a different file
/// manager.switch_to_file("announcement.mp3".to_string(), false)?;
/// ```
use anyhow::{Result, anyhow};
use audio_codec::{CodecType, Decoder, Resampler, create_decoder};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tracing::{debug, warn};

pub trait AudioSource: Send + Sync {
    fn read_samples(&mut self, buffer: &mut [i16]) -> usize;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn has_data(&self) -> bool;
    fn reset(&mut self) -> Result<()>;
}

pub struct FileAudioSource {
    decoder: Box<dyn Decoder>,
    file_path: String,
    loop_playback: bool,
    eof_reached: bool,
    wav_reader: Option<hound::WavReader<BufReader<File>>>,
    mp3_decoder: Option<minimp3::Decoder<BufReader<File>>>,
    mp3_buffer: Vec<i16>,
    mp3_buffer_pos: usize,
    /// Actual sample rate detected from the first MP3 frame (replaces the old 44100 hardcode).
    mp3_sample_rate: u32,
    /// Actual channel count detected from the first MP3 frame (replaces the old hardcoded 2).
    mp3_channels: u16,
    raw_file: Option<BufReader<File>>,
    raw_frame_size: usize,
    temp_file_path: Option<String>,
}

impl FileAudioSource {
    pub fn new(file_path: String, loop_playback: bool) -> Result<Self> {
        let (actual_path, temp_file_path) =
            if file_path.starts_with("http://") || file_path.starts_with("https://") {
                debug!("Downloading audio file from URL: {}", file_path);
                let temp_path = Self::download_file(&file_path)?;
                (temp_path.clone(), Some(temp_path))
            } else {
                if !Path::new(&file_path).exists() {
                    return Err(anyhow!("Audio file not found: {}", file_path));
                }
                (file_path.clone(), None)
            };

        let codec_type = Self::detect_codec(&actual_path)?;
        let decoder = create_decoder(codec_type);

        let extension = Path::new(&actual_path)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Detect actual MP3 metadata by pre-reading the first frame.
        let mut mp3_sample_rate = 44100u32;
        let mut mp3_channels = 2u16;
        let mut initial_mp3_buffer: Vec<i16> = Vec::new();

        let (wav_reader, mp3_decoder, raw_file) = match extension.as_str() {
            "wav" => {
                let reader = hound::WavReader::open(&actual_path)?;
                (Some(reader), None, None)
            }
            "mp3" => {
                let file = File::open(&actual_path)?;
                let buf_reader = BufReader::new(file);
                let mut mp3_dec = minimp3::Decoder::new(buf_reader);
                // Pre-read the first frame to detect the actual sample rate and channel
                // count.  The legacy code hard-coded 44100 / 2, which is wrong for
                // telephony MP3s that are commonly 8 kHz mono.
                match mp3_dec.next_frame() {
                    Ok(frame) => {
                        mp3_sample_rate = frame.sample_rate as u32;
                        mp3_channels = frame.channels as u16;
                        initial_mp3_buffer = frame.data;
                        debug!(
                            file = %actual_path,
                            sample_rate = mp3_sample_rate,
                            channels = mp3_channels,
                            "Detected MP3 stream parameters from first frame"
                        );
                    }
                    Err(e) => {
                        debug!(file = %actual_path, error = ?e, "Could not pre-read first MP3 frame, using fallback parameters");
                    }
                }
                (None, Some(mp3_dec), None)
            }
            _ => {
                let file = File::open(&actual_path)?;
                let buf_reader = BufReader::new(file);
                (None, None, Some(buf_reader))
            }
        };

        let raw_frame_size = match codec_type {
            CodecType::PCMU | CodecType::PCMA => 160,
            CodecType::G722 => 160,
            CodecType::G729 => 20,
            _ => 160,
        };

        Ok(Self {
            decoder,
            file_path: actual_path,
            loop_playback,
            eof_reached: false,
            wav_reader,
            mp3_decoder,
            mp3_buffer: initial_mp3_buffer,
            mp3_buffer_pos: 0,
            mp3_sample_rate,
            mp3_channels,
            raw_file,
            raw_frame_size,
            temp_file_path,
        })
    }

    /// Download file from HTTP URL to temporary location
    fn download_file(url: &str) -> Result<String> {
        let temp_dir = std::env::temp_dir();
        let file_name = url
            .split('/')
            .last()
            .unwrap_or("audio_file")
            .split('?')
            .next()
            .unwrap_or("audio_file");
        let temp_path = temp_dir.join(format!("rustpbx_audio_{}", file_name));

        debug!("Downloading to temporary file: {:?}", temp_path);

        let response = reqwest::blocking::get(url)
            .map_err(|e| anyhow!("Failed to download audio file: {}", e))?;

        if !response.status().is_success() {
            return Err(anyhow!("HTTP error: {}", response.status()));
        }

        let bytes = response
            .bytes()
            .map_err(|e| anyhow!("Failed to read response body: {}", e))?;

        let mut file = File::create(&temp_path)
            .map_err(|e| anyhow!("Failed to create temporary file: {}", e))?;
        file.write_all(&bytes)
            .map_err(|e| anyhow!("Failed to write temporary file: {}", e))?;

        debug!("Downloaded {} bytes to {:?}", bytes.len(), temp_path);

        Ok(temp_path.to_string_lossy().to_string())
    }

    fn detect_codec(file_path: &str) -> Result<CodecType> {
        let ext = Path::new(file_path)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        match ext.to_lowercase().as_str() {
            "wav" | "mp3" => Ok(CodecType::PCMU),
            _ => match CodecType::try_from(ext) {
                Ok(codec) => Ok(codec),
                Err(_) => match ext {
                    "u" | "ulaw" => Ok(CodecType::PCMU),
                    "a" | "alaw" => Ok(CodecType::PCMA),
                    _ => {
                        warn!("Unknown file extension '{}', assuming PCMU", ext);
                        Ok(CodecType::PCMU)
                    }
                },
            },
        }
    }
}

impl AudioSource for FileAudioSource {
    fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
        if self.eof_reached && !self.loop_playback {
            return 0;
        }

        if self.eof_reached {
            if let Err(e) = self.reset() {
                warn!("Failed to reset file source: {}", e);
                return 0;
            }
        }

        if let Some(ref mut reader) = self.wav_reader {
            let mut samples_read = 0;
            for sample in buffer.iter_mut() {
                match reader.samples::<i16>().next() {
                    Some(Ok(s)) => {
                        *sample = s;
                        samples_read += 1;
                    }
                    Some(Err(e)) => {
                        warn!("WAV read error: {}", e);
                        self.eof_reached = true;
                        break;
                    }
                    None => {
                        self.eof_reached = true;
                        break;
                    }
                }
            }
            samples_read
        } else if let Some(ref mut decoder) = self.mp3_decoder {
            let mut samples_read = 0;

            while samples_read < buffer.len() {
                if self.mp3_buffer_pos < self.mp3_buffer.len() {
                    let available = (self.mp3_buffer.len() - self.mp3_buffer_pos)
                        .min(buffer.len() - samples_read);
                    buffer[samples_read..samples_read + available].copy_from_slice(
                        &self.mp3_buffer[self.mp3_buffer_pos..self.mp3_buffer_pos + available],
                    );
                    self.mp3_buffer_pos += available;
                    samples_read += available;

                    if samples_read >= buffer.len() {
                        break;
                    }
                }

                match decoder.next_frame() {
                    Ok(frame) => {
                        self.mp3_buffer = frame.data;
                        self.mp3_buffer_pos = 0;
                    }
                    Err(minimp3::Error::Eof) => {
                        self.eof_reached = true;
                        break;
                    }
                    Err(e) => {
                        warn!("MP3 decode error: {}", e);
                        self.eof_reached = true;
                        break;
                    }
                }
            }
            samples_read
        } else if let Some(ref mut reader) = self.raw_file {
            let mut encoded_buf = vec![0u8; self.raw_frame_size];
            match reader.read_exact(&mut encoded_buf) {
                Ok(_) => {
                    let pcm = self.decoder.decode(&encoded_buf);
                    let copy_len = pcm.len().min(buffer.len());
                    buffer[..copy_len].copy_from_slice(&pcm[..copy_len]);
                    copy_len
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        self.eof_reached = true;
                    } else {
                        warn!("Raw file read error: {}", e);
                        self.eof_reached = true;
                    }
                    0
                }
            }
        } else {
            for sample in buffer.iter_mut() {
                *sample = 0;
            }
            buffer.len()
        }
    }

    fn sample_rate(&self) -> u32 {
        if let Some(ref reader) = self.wav_reader {
            reader.spec().sample_rate
        } else if self.mp3_decoder.is_some() {
            // Use the rate detected during construction (was wrongly hardcoded to 44100).
            self.mp3_sample_rate
        } else {
            self.decoder.sample_rate()
        }
    }

    fn channels(&self) -> u16 {
        if let Some(ref reader) = self.wav_reader {
            reader.spec().channels
        } else if self.mp3_decoder.is_some() {
            // Use the channels detected during construction (was wrongly hardcoded to 2).
            self.mp3_channels
        } else {
            1
        }
    }

    fn has_data(&self) -> bool {
        !self.eof_reached || self.loop_playback
    }

    fn reset(&mut self) -> Result<()> {
        self.eof_reached = false;

        if let Some(ref mut reader) = self.wav_reader {
            *reader = hound::WavReader::open(&self.file_path)?;
        } else if self.mp3_decoder.is_some() {
            let file = File::open(&self.file_path)?;
            let buf_reader = BufReader::new(file);
            self.mp3_decoder = Some(minimp3::Decoder::new(buf_reader));
            self.mp3_buffer.clear();
            self.mp3_buffer_pos = 0;
        } else if let Some(ref mut reader) = self.raw_file {
            reader.seek(SeekFrom::Start(0))?;
        }

        Ok(())
    }
}

impl Drop for FileAudioSource {
    fn drop(&mut self) {
        if let Some(ref temp_path) = self.temp_file_path {
            if let Err(e) = std::fs::remove_file(temp_path) {
                warn!("Failed to remove temporary file {}: {}", temp_path, e);
            } else {
                debug!("Cleaned up temporary file: {}", temp_path);
            }
        }
    }
}

/// Silence audio source
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

/// Audio source with resampling support
pub struct ResamplingAudioSource {
    source: Box<dyn AudioSource>,
    resampler: Option<Resampler>,
    /// Sample rate of the wrapped source — cached so `read_samples` can compute
    /// the correct intermediate buffer size without holding a borrow on `source`.
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
            // Calculate how many source samples we need to fill `buffer`.
            // The old code used `buffer.len()` which is the *target* size — when
            // upsampling (e.g. 8 kHz → 44.1 kHz) that drastically under-reads the
            // source.  Use ceiling division to avoid off-by-one shortfalls.
            let needed_source = ((buffer.len() as u64 * self.source_sample_rate as u64
                + self.target_sample_rate as u64
                - 1)
                / self.target_sample_rate as u64) as usize;

            self.intermediate_buffer.resize(needed_source, 0);
            let read = self.source.read_samples(&mut self.intermediate_buffer);

            if read == 0 {
                return 0;
            }

            // Resample
            let resampled = resampler.resample(&self.intermediate_buffer[..read]);
            let copy_len = resampled.len().min(buffer.len());
            buffer[..copy_len].copy_from_slice(&resampled[..copy_len]);
            copy_len
        } else {
            // No resampling needed
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
        self.source.reset()
    }
}

/// Audio source manager that handles source switching
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

    pub fn switch_to_file(&self, file_path: String, loop_playback: bool) -> Result<()> {
        let file_source = FileAudioSource::new(file_path.clone(), loop_playback)?;
        let resampling_source =
            ResamplingAudioSource::new(Box::new(file_source), self.target_sample_rate);

        let mut current = self.current_source.lock().unwrap();
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
        let mut current = self.current_source.lock().unwrap();
        *current = Some(Box::new(silence));

        debug!("Switched to silence audio source");
    }

    pub fn read_samples(&self, buffer: &mut [i16]) -> usize {
        let mut current = self.current_source.lock().unwrap();
        if let Some(ref mut source) = *current {
            let read = source.read_samples(buffer);
            if read == 0 {
                // Source is exhausted — wake any task waiting on completion.
                self.completion_notify.notify_one();
            }
            read
        } else {
            // No source, return silence
            for sample in buffer.iter_mut() {
                *sample = 0;
            }
            buffer.len()
        }
    }

    pub fn has_active_source(&self) -> bool {
        let current = self.current_source.lock().unwrap();
        current.is_some()
    }

    pub async fn wait_for_completion(&self) {
        self.completion_notify.notified().await;
    }
}

/// Estimate the playback duration of an audio file without decoding the entire stream.
///
/// # Supported formats
/// - **WAV**: reads the header to get exact duration.
/// - **MP3**: approximates based on file size assuming 128 kbps.
/// - **PCMU / PCMA** (extensions `.pcmu`, `.ulaw`, `.u`, `.pcma`, `.alaw`, `.a`):
///   8000 Hz × 8-bit samples → 1 byte per millisecond per channel.
/// - **G.722**: 8000 Hz encoded, 160 bytes per 20 ms frame.
/// - **G.729**: 8000 Hz encoded, 10 bytes per 10 ms frame.
/// - **Unknown**: defaults to 5 seconds.
///
/// Used by [`crate::proxy::proxy_call::session::CallSession::play_audio_file`] to
/// schedule the `AudioComplete` event at the right time.
pub fn estimate_audio_duration(file_path: &str) -> std::time::Duration {
    use std::path::Path;

    let ext = Path::new(file_path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "wav" => {
            if let Ok(reader) = hound::WavReader::open(file_path) {
                let spec = reader.spec();
                // `duration()` returns total frames (samples-per-channel), so
                // dividing by sample_rate gives seconds.
                let secs = reader.duration() as f64 / spec.sample_rate as f64;
                std::time::Duration::from_secs_f64(secs.max(0.005))
            } else {
                std::time::Duration::from_secs(5)
            }
        }
        "mp3" => {
            // No cheap header read for arbitrary MP3; estimate at 128 kbps.
            if let Ok(meta) = std::fs::metadata(file_path) {
                let bits = meta.len() * 8;
                let secs = bits as f64 / 128_000.0;
                std::time::Duration::from_secs_f64(secs.max(0.1))
            } else {
                std::time::Duration::from_secs(5)
            }
        }
        "pcmu" | "ulaw" | "u" | "pcma" | "alaw" | "a" => {
            // 8000 Hz, 8-bit samples → 1 byte per millisecond.
            if let Ok(meta) = std::fs::metadata(file_path) {
                std::time::Duration::from_millis(meta.len().max(100))
            } else {
                std::time::Duration::from_secs(5)
            }
        }
        "g722" => {
            // 160 bytes per 20 ms frame at 8000 Hz half-rate encoding.
            if let Ok(meta) = std::fs::metadata(file_path) {
                let frames = meta.len() / 160;
                std::time::Duration::from_millis(frames.max(1) * 20)
            } else {
                std::time::Duration::from_secs(5)
            }
        }
        "g729" => {
            // 10 bytes per 10 ms frame.
            if let Ok(meta) = std::fs::metadata(file_path) {
                let frames = meta.len() / 10;
                std::time::Duration::from_millis(frames.max(1) * 10)
            } else {
                std::time::Duration::from_secs(5)
            }
        }
        _ => {
            // Generic raw PCM: assume 8000 Hz, 16-bit linear → 16000 bytes/sec.
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
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Write a minimal, valid WAV file with `num_samples` mono 16-bit PCM samples
    /// at the given sample rate and return the path.
    fn write_wav(sample_rate: u32, samples: &[i16]) -> NamedTempFile {
        let mut tmp = NamedTempFile::with_suffix(".wav").expect("tempfile");
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer =
                hound::WavWriter::new(std::io::BufWriter::new(tmp.as_file_mut()), spec)
                    .expect("WavWriter");
            for &s in samples {
                writer.write_sample(s).expect("write_sample");
            }
            writer.finalize().expect("finalize");
        }
        tmp
    }

    /// Write raw i16 PCM bytes to a temp file and return the path.
    #[allow(dead_code)]
    fn write_raw_pcm(samples: &[i16]) -> NamedTempFile {
        let mut tmp = NamedTempFile::with_suffix(".pcmu").expect("tempfile");
        for &s in samples {
            tmp.write_all(&s.to_le_bytes()).expect("write");
        }
        tmp
    }

    // ── SilenceSource ────────────────────────────────────────────────────────

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

    // ── ResamplingAudioSource buffer-size correctness ────────────────────────

    /// The old code used `buffer.len()` for the intermediate buffer, which is
    /// the *target* size.  When the source is at a higher rate (e.g. 44100)
    /// and the target is lower (e.g. 8000), we must read MORE source samples
    /// than the requested target frames.  This test verifies that read_samples
    /// with a non-power-of-two ratio does not return 0 just because the
    /// intermediate buffer was too small.
    #[test]
    fn test_resampling_downsample_44100_to_8000() {
        // 44100 Hz source, 8000 Hz target.
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

        // Request 160 target samples (20 ms at 8000 Hz).
        // The source must provide ceil(160 * 44100 / 8000) = ceil(882) = 882 samples.
        let mut out = vec![0i16; 160];
        let read = resampler.read_samples(&mut out);
        assert!(
            read > 0,
            "downsample 44100→8000: expected non-zero output, got 0 \
             (likely intermediate buffer was too small)"
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
        // No resampler should be created.
        let mut buf = vec![0i16; 160];
        let read = resampling.read_samples(&mut buf);
        assert_eq!(read, 160);
    }

    // ── AudioSourceManager ───────────────────────────────────────────────────

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
        // No source installed yet.
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
        // A source that returns exactly one batch of samples and then EOF.
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
        // Install a source with exactly 160 samples.
        {
            let src = OneShotSource {
                data: vec![1i16; 160],
                pos: 0,
            };
            let resampled = ResamplingAudioSource::new(Box::new(src), 8000);
            let mut current = manager.current_source.lock().unwrap();
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

        // Drain the source completely.
        let mut buf = vec![0i16; 160];
        let read = manager.read_samples(&mut buf);
        assert_eq!(read, 160, "should read all 160 samples");

        // Next read returns 0 → triggers the notify.
        let read2 = manager.read_samples(&mut buf);
        assert_eq!(read2, 0, "source should be exhausted");

        notified
            .await
            .expect("join")
            .expect("completion notify not fired within 500 ms");
    }

    // ── WAV file reading ─────────────────────────────────────────────────────

    #[test]
    fn test_wav_file_source_reads_samples() {
        let pcm: Vec<i16> = (0i16..160).collect();
        let tmp = write_wav(8000, &pcm);

        let mut src = FileAudioSource::new(tmp.path().to_str().unwrap().to_string(), false)
            .expect("FileAudioSource::new for WAV");

        assert_eq!(src.sample_rate(), 8000);
        assert_eq!(src.channels(), 1);
        assert!(src.has_data());

        let mut buf = vec![0i16; 160];
        let read = src.read_samples(&mut buf);
        assert_eq!(read, 160, "should read all 160 samples");
        assert_eq!(&buf[..], &pcm[..], "samples must match what was written");
    }

    #[test]
    fn test_wav_file_source_eof_no_loop() {
        let pcm: Vec<i16> = vec![42i16; 80];
        let tmp = write_wav(8000, &pcm);

        let mut src = FileAudioSource::new(tmp.path().to_str().unwrap().to_string(), false)
            .expect("FileAudioSource::new");

        let mut buf = vec![0i16; 160];
        // First read – fills 80 samples, then EOF.
        let _read1 = src.read_samples(&mut buf);
        assert!(!src.has_data(), "no loop → EOF marks source as exhausted");

        // Second read returns 0.
        let read2 = src.read_samples(&mut buf);
        assert_eq!(read2, 0);
    }

    #[test]
    fn test_wav_file_source_loop() {
        let pcm: Vec<i16> = vec![1i16; 80];
        let tmp = write_wav(8000, &pcm);

        let mut src = FileAudioSource::new(tmp.path().to_str().unwrap().to_string(), true)
            .expect("FileAudioSource::new");

        // First fill might hit EOF and reset.
        let mut buf = vec![0i16; 240];
        let _read = src.read_samples(&mut buf);
        // has_data must remain true because loop=true.
        assert!(src.has_data(), "looping source must always have data");
    }

    #[test]
    fn test_wav_file_source_missing_file() {
        let result = FileAudioSource::new("/nonexistent/path/sample.wav".to_string(), false);
        assert!(result.is_err(), "missing file must return an error");
    }

    // ── estimate_audio_duration ──────────────────────────────────────────────

    #[test]
    fn test_estimate_duration_wav_exact() {
        // 8000 Hz, 160 samples → exactly 20 ms.
        let pcm: Vec<i16> = vec![0i16; 8000]; // 1 second
        let tmp = write_wav(8000, &pcm);
        let dur = estimate_audio_duration(tmp.path().to_str().unwrap());
        // Allow ±5 ms tolerance for any rounding.
        assert!(
            dur.as_millis() >= 995 && dur.as_millis() <= 1005,
            "WAV 1-second file: expected ~1000 ms, got {} ms",
            dur.as_millis()
        );
    }

    #[test]
    fn test_estimate_duration_wav_short() {
        let pcm: Vec<i16> = vec![0i16; 160]; // 20 ms at 8000 Hz
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
        // PCMU raw: 8000 Hz, 8-bit → 1 byte per millisecond.
        let data = vec![0u8; 8000]; // 1 second
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
        // G.722: 160 bytes per 20 ms frame.
        let data = vec![0u8; 1600]; // 10 frames = 200 ms
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
        // G.729: 10 bytes per 10 ms frame.
        let data = vec![0u8; 100]; // 10 frames = 100 ms
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
        // 16000 bytes / 16000 bytes_per_sec = 1 second.
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
}
