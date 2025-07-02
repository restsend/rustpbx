use crate::event::{EventSender, SessionEvent};
use crate::media::processor::Processor;
use crate::{AudioFrame, PcmBuf, Samples};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::cell::RefCell;
use tokio_util::sync::CancellationToken;
#[cfg(feature = "vad_silero")]
mod silero;
#[cfg(feature = "vad_ten")]
mod ten;
#[cfg(test)]
mod tests;
#[cfg(feature = "vad_webrtc")]
mod webrtc;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct VADOption {
    pub r#type: VadType,
    pub samplerate: u32,
    /// Padding before speech detection (in ms)
    pub speech_padding: u64,
    /// Padding after silence detection (in ms)
    pub silence_padding: u64,
    pub ratio: f32,
    pub voice_threshold: f32,
    pub max_buffer_duration_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_id: Option<String>,
}

impl Default for VADOption {
    fn default() -> Self {
        Self {
            #[cfg(feature = "vad_webrtc")]
            r#type: VadType::WebRTC,
            #[cfg(not(any(feature = "vad_webrtc", feature = "vad_silero")))]
            r#type: VadType::Other("nop".to_string()),
            samplerate: 16000,
            // Python defaults: min_speech_duration_ms=250, min_silence_duration_ms=100, speech_pad_ms=30
            speech_padding: 250,  // min_speech_duration_ms
            silence_padding: 100, // min_silence_duration_ms
            ratio: 0.5,
            voice_threshold: 0.5,
            max_buffer_duration_secs: 50,
            endpoint: None,
            secret_key: None,
            secret_id: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Eq, Hash, PartialEq)]
pub enum VadType {
    #[serde(rename = "webrtc")]
    #[cfg(feature = "vad_webrtc")]
    WebRTC,
    #[serde(rename = "silero")]
    #[cfg(feature = "vad_silero")]
    Silero,
    #[serde(rename = "ten")]
    #[cfg(feature = "vad_ten")]
    Ten,
    #[serde(rename = "other")]
    Other(String),
}

impl<'de> Deserialize<'de> for VadType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            #[cfg(feature = "vad_webrtc")]
            "webrtc" => Ok(VadType::WebRTC),
            #[cfg(feature = "vad_silero")]
            "silero" => Ok(VadType::Silero),
            #[cfg(feature = "vad_ten")]
            "ten" => Ok(VadType::Ten),
            _ => Ok(VadType::Other(value)),
        }
    }
}

impl std::fmt::Display for VadType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "vad_webrtc")]
            VadType::WebRTC => write!(f, "webrtc"),
            #[cfg(feature = "vad_silero")]
            VadType::Silero => write!(f, "silero"),
            #[cfg(feature = "vad_ten")]
            VadType::Ten => write!(f, "ten"),
            VadType::Other(provider) => write!(f, "{}", provider),
        }
    }
}

struct SpeechBuf {
    samples: PcmBuf,
    timestamp: u64,
}

struct VadProcessorInner {
    vad: Box<dyn VadEngine>,
    event_sender: EventSender,
    option: VADOption,
    window_bufs: Vec<SpeechBuf>,
    triggered: bool,
    current_speech_start: Option<u64>,
    temp_end: Option<u64>,
}
pub struct VadProcessor {
    inner: RefCell<VadProcessorInner>,
}
unsafe impl Send for VadProcessor {}
unsafe impl Sync for VadProcessor {}

pub trait VadEngine: Send + Sync + Any {
    fn process(&mut self, frame: &mut AudioFrame) -> Option<(bool, u64)>;
}

impl VadProcessorInner {
    pub fn process_frame(&mut self, frame: &mut AudioFrame) -> Result<()> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()),
        };

        let samples = samples.to_owned();
        let result = self.vad.process(frame);
        if let Some((is_speaking, timestamp)) = result {
            self.process_vad_logic(is_speaking, timestamp, &frame.track_id)?;

            let current_buf = SpeechBuf { samples, timestamp };

            self.window_bufs.push(current_buf);

            // Clean up old buffers periodically
            if self.window_bufs.len() > 1000 {
                let cutoff = timestamp.saturating_sub(5000);
                self.window_bufs.retain(|buf| buf.timestamp > cutoff);
            }
        }

        Ok(())
    }

    fn process_vad_logic(
        &mut self,
        is_speaking: bool,
        timestamp: u64,
        track_id: &str,
    ) -> Result<()> {
        if is_speaking && !self.triggered {
            self.triggered = true;
            self.current_speech_start = Some(timestamp);
            let event = SessionEvent::Speaking {
                track_id: track_id.to_string(),
                timestamp: crate::get_timestamp(),
                start_time: timestamp,
            };
            self.event_sender.send(event).ok();
        } else if !is_speaking && self.triggered {
            if self.temp_end.is_none() {
                self.temp_end = Some(timestamp);
            }

            if let Some(temp_end) = self.temp_end {
                let silence_duration = timestamp - temp_end;
                if silence_duration >= self.option.silence_padding {
                    if let Some(start_time) = self.current_speech_start {
                        let duration = temp_end - start_time;
                        if duration >= self.option.speech_padding {
                            let samples_vec = self
                                .window_bufs
                                .iter()
                                .filter(|buf| {
                                    buf.timestamp >= start_time && buf.timestamp <= temp_end
                                })
                                .flat_map(|buf| buf.samples.iter())
                                .cloned()
                                .collect();

                            let event = SessionEvent::Silence {
                                track_id: track_id.to_string(),
                                timestamp: crate::get_timestamp(),
                                start_time,
                                duration,
                                samples: Some(samples_vec),
                            };
                            self.event_sender.send(event).ok();
                        }
                    }

                    self.triggered = false;
                    self.current_speech_start = None;
                    self.temp_end = None;
                }
            }
        } else if is_speaking && self.temp_end.is_some() {
            self.temp_end = None;
        }

        Ok(())
    }
}

impl VadProcessor {
    #[cfg(feature = "vad_webrtc")]
    pub fn create_webrtc(
        _token: CancellationToken,
        event_sender: EventSender,
        option: VADOption,
    ) -> Result<Box<dyn Processor>> {
        let vad: Box<dyn VadEngine> = match option.r#type {
            VadType::WebRTC => Box::new(webrtc::WebRtcVad::new(option.samplerate)?),
            _ => Box::new(NopVad::new()?),
        };
        Ok(Box::new(VadProcessor::new(vad, event_sender, option)?))
    }
    #[cfg(feature = "vad_silero")]
    pub fn create_silero(
        _token: CancellationToken,
        event_sender: EventSender,
        option: VADOption,
    ) -> Result<Box<dyn Processor>> {
        let vad: Box<dyn VadEngine> = match option.r#type {
            VadType::Silero => Box::new(silero::SileroVad::new(option.clone())?),
            _ => Box::new(NopVad::new()?),
        };
        Ok(Box::new(VadProcessor::new(vad, event_sender, option)?))
    }
    #[cfg(feature = "vad_ten")]
    pub fn create_ten(
        _token: CancellationToken,
        event_sender: EventSender,
        option: VADOption,
    ) -> Result<Box<dyn Processor>> {
        let vad: Box<dyn VadEngine> = match option.r#type {
            VadType::Ten => Box::new(ten::TenVad::new(option.clone())?),
            _ => Box::new(NopVad::new()?),
        };
        Ok(Box::new(VadProcessor::new(vad, event_sender, option)?))
    }

    pub fn new(
        engine: Box<dyn VadEngine>,
        event_sender: EventSender,
        option: VADOption,
    ) -> Result<Self> {
        let inner = VadProcessorInner {
            vad: engine,
            event_sender,
            option,
            window_bufs: Vec::new(),
            triggered: false,
            current_speech_start: None,
            temp_end: None,
        };
        Ok(Self {
            inner: RefCell::new(inner),
        })
    }
}

impl Processor for VadProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        self.inner.borrow_mut().process_frame(frame)
    }
}

struct NopVad {}

impl NopVad {
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }
}

impl VadEngine for NopVad {
    fn process(&mut self, frame: &mut AudioFrame) -> Option<(bool, u64)> {
        Some((false, frame.timestamp))
    }
}
