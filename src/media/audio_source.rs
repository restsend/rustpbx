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

        let (wav_reader, mp3_decoder, raw_file) = match extension.as_str() {
            "wav" => {
                let reader = hound::WavReader::open(&actual_path)?;
                (Some(reader), None, None)
            }
            "mp3" => {
                let file = File::open(&actual_path)?;
                let buf_reader = BufReader::new(file);
                let decoder = minimp3::Decoder::new(buf_reader);
                (None, Some(decoder), None)
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
            mp3_buffer: Vec::new(),
            mp3_buffer_pos: 0,
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
            44100
        } else {
            self.decoder.sample_rate()
        }
    }

    fn channels(&self) -> u16 {
        if let Some(ref reader) = self.wav_reader {
            reader.spec().channels
        } else if self.mp3_decoder.is_some() {
            2
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
            // Read from source into intermediate buffer
            self.intermediate_buffer.resize(buffer.len(), 0);
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
            source.read_samples(buffer)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_silence_source() {
        let mut source = SilenceSource::new(8000);
        let mut buffer = vec![999i16; 160];
        let read = source.read_samples(&mut buffer);

        assert_eq!(read, 160);
        assert!(buffer.iter().all(|&s| s == 0));
    }

    #[test]
    fn test_resampling_source() {
        let silence = SilenceSource::new(8000);
        let mut resampling = ResamplingAudioSource::new(Box::new(silence), 16000);

        assert_eq!(resampling.sample_rate(), 16000);

        let mut buffer = vec![0i16; 320];
        let read = resampling.read_samples(&mut buffer);
        assert!(read > 0);
    }

    #[test]
    fn test_audio_source_manager() {
        let manager = AudioSourceManager::new(8000);

        manager.switch_to_silence();
        assert!(manager.has_active_source());

        let mut buffer = vec![0i16; 160];
        let read = manager.read_samples(&mut buffer);
        assert_eq!(read, 160);
    }
}
