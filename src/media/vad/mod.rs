use crate::event::{EventSender, SessionEvent};
use crate::media::processor::Processor;
use crate::{AudioFrame, PcmBuf, Samples};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::cell::RefCell;

#[cfg(feature = "vad_silero")]
mod silero;
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
    /// Padding before speech detection
    pub speech_padding: u64,
    /// Padding after silence detection
    pub silence_padding: u64,
    pub ratio: f32,
    pub voice_threshold: f32,
    pub max_buffer_duration_secs: u64,
}

impl Default for VADOption {
    fn default() -> Self {
        Self {
            #[cfg(feature = "vad_webrtc")]
            r#type: VadType::WebRTC,
            #[cfg(not(any(feature = "vad_webrtc", feature = "vad_silero")))]
            r#type: VadType::Other("nop".to_string()),
            samplerate: 16000,
            speech_padding: 160,
            silence_padding: 200,
            ratio: 0.5,
            voice_threshold: 0.5,
            max_buffer_duration_secs: 50,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub enum VadType {
    #[serde(rename = "webrtc")]
    #[cfg(feature = "vad_webrtc")]
    WebRTC,
    #[serde(rename = "silero")]
    #[cfg(feature = "vad_silero")]
    Silero,
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
            VadType::Silero => write!(f, "voiceapi"),
            VadType::Other(provider) => write!(f, "{}", provider),
        }
    }
}

enum VadState {
    Speaking,
    Silence,
}

struct SpeechBuf {
    samples: PcmBuf,
    timestamp: u64,
    is_speaking: bool,
}

struct VadProcessorInner {
    vad: Box<dyn VadEngine>,
    event_sender: EventSender,
    option: VADOption,
    state: VadState,
    window_bufs: Vec<SpeechBuf>,
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
        let (is_speaking, timestamp) = match self.vad.process(frame) {
            Some((is_speaking, timestamp)) => (is_speaking, timestamp),
            None => return Ok(()),
        };

        let current_buf = SpeechBuf {
            samples,
            timestamp,
            is_speaking,
        };

        self.window_bufs.push(current_buf);
        let diff_duration = self.window_bufs.last().unwrap().timestamp
            - self.window_bufs.first().unwrap().timestamp;
        if diff_duration < self.option.speech_padding {
            return Ok(());
        }
        let mut speaking_count = 0;
        let mut silence_count = 0;
        let max_padding = self.option.speech_padding.max(self.option.silence_padding);

        let mut speaking_check_count = 0;
        let mut silence_check_count = 0;
        let speech_duration = frame.timestamp - self.option.speech_padding.min(frame.timestamp);
        let silence_duration = frame.timestamp - self.option.silence_padding.min(frame.timestamp);

        for buf in self.window_bufs.iter().rev() {
            if frame.timestamp > max_padding && buf.timestamp < frame.timestamp - max_padding {
                break;
            }
            if buf.timestamp > speech_duration {
                speaking_check_count += 1;
                if buf.is_speaking {
                    speaking_count += 1;
                }
            }
            if buf.timestamp > silence_duration {
                silence_check_count += 1;
                if !buf.is_speaking {
                    silence_count += 1;
                }
            }
        }

        let speaking_threshold = (speaking_check_count as f32 * self.option.ratio) as usize;
        let silence_threshold = (silence_check_count as f32 * self.option.ratio) as usize;
        if speaking_count >= speaking_threshold {
            match self.state {
                VadState::Silence => {
                    self.window_bufs
                        .retain(|b| b.timestamp > frame.timestamp - self.option.speech_padding);
                    if self.window_bufs.len() == 0 {
                        return Ok(());
                    }
                    self.state = VadState::Speaking;
                    let event = SessionEvent::Speaking {
                        track_id: frame.track_id.clone(),
                        timestamp: crate::get_timestamp(),
                        start_time: self.window_bufs.first().unwrap().timestamp,
                    };
                    self.event_sender.send(event).ok();
                }
                _ => {}
            }
        } else if silence_count >= silence_threshold {
            match self.state {
                VadState::Speaking => {
                    self.state = VadState::Silence;
                    // trim right non-speaking samples
                    while let Some(last_buf) = self.window_bufs.last() {
                        if last_buf.is_speaking {
                            break;
                        }
                        self.window_bufs.pop();
                    }
                    if self.window_bufs.len() <= 0 {
                        return Ok(());
                    }
                    let start_time = self.window_bufs.first().unwrap().timestamp;
                    let diff_duration = self.window_bufs.last().unwrap().timestamp - start_time;
                    let samples = self
                        .window_bufs
                        .iter()
                        .flat_map(|buf| buf.samples.iter())
                        .cloned()
                        .collect();
                    let event = SessionEvent::Silence {
                        track_id: frame.track_id.clone(),
                        timestamp: crate::get_timestamp(),
                        start_time,
                        duration: diff_duration,
                        samples: Some(samples),
                    };
                    self.event_sender.send(event).ok();
                    self.window_bufs.clear();
                }
                _ => {
                    self.window_bufs.remove(0);
                }
            }
        }
        Ok(())
    }
}

impl VadProcessor {
    pub fn new(event_sender: EventSender, option: VADOption) -> Result<Self> {
        let vad: Box<dyn VadEngine> = match option.r#type {
            #[cfg(feature = "vad_webrtc")]
            VadType::WebRTC => Box::new(webrtc::WebRtcVad::new(option.samplerate)?),
            #[cfg(feature = "vad_silero")]
            VadType::Silero => Box::new(silero::SileroVad::new(option.samplerate)?),
            _ => Box::new(NopVad::new()?),
        };
        let inner = VadProcessorInner {
            vad,
            event_sender,
            option,
            state: VadState::Silence,
            window_bufs: Vec::new(),
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
