use crate::event::{EventSender, SessionEvent};
use crate::media::processor::Processor;
use crate::{AudioFrame, Samples};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::sync::Mutex;

mod silero;
#[cfg(test)]
mod tests;
mod webrtc;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct VADConfig {
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

impl Default for VADConfig {
    fn default() -> Self {
        Self {
            r#type: VadType::WebRTC,
            samplerate: 16000,
            speech_padding: 160,
            silence_padding: 200,
            ratio: 0.5,
            voice_threshold: 0.5,
            max_buffer_duration_secs: 50,
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum VadType {
    #[serde(rename = "webrtc")]
    WebRTC,
    #[serde(rename = "silero")]
    Silero,
}
enum VadState {
    Speaking,
    Silence,
}

struct SpeechBuf {
    samples: Vec<i16>,
    timestamp: u64,
    is_speaking: bool,
}

struct VadProcessorInner {
    vad: Box<dyn VadEngine>,
    event_sender: EventSender,
    config: VADConfig,
    state: VadState,
    window_bufs: Vec<SpeechBuf>,
}
pub struct VadProcessor {
    inner: Mutex<VadProcessorInner>,
}

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
        if diff_duration < self.config.speech_padding {
            return Ok(());
        }
        let mut speaking_count = 0;
        let mut silence_count = 0;
        let max_padding = self.config.speech_padding.max(self.config.silence_padding);

        let mut speaking_check_count = 0;
        let mut silence_check_count = 0;
        let speech_duration = frame.timestamp - self.config.speech_padding.min(frame.timestamp);
        let silence_duration = frame.timestamp - self.config.silence_padding.min(frame.timestamp);

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

        let speaking_threshold = (speaking_check_count as f32 * self.config.ratio) as usize;
        let silence_threshold = (silence_check_count as f32 * self.config.ratio) as usize;
        // trace!(
        //     "timestamp: {}, speaking: {}/{} silence: {}/{}  time: {} is_speaking: {}",
        //     self.window_bufs.first().unwrap().timestamp,
        //     speaking_count,
        //     speaking_threshold,
        //     silence_count,
        //     silence_threshold,
        //     self.window_bufs.last().unwrap().timestamp,
        //     self.window_bufs.last().unwrap().is_speaking,
        // );
        if speaking_count >= speaking_threshold {
            match self.state {
                VadState::Silence => {
                    self.window_bufs
                        .retain(|b| b.timestamp > frame.timestamp - self.config.speech_padding);
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
    pub fn new(vad_type: VadType, event_sender: EventSender, config: VADConfig) -> Result<Self> {
        let vad: Box<dyn VadEngine> = match vad_type {
            VadType::WebRTC => Box::new(webrtc::WebRtcVad::new(config.samplerate)?),
            VadType::Silero => Box::new(silero::SileroVad::new(config.samplerate)?),
        };
        let inner = VadProcessorInner {
            vad,
            event_sender,
            config,
            state: VadState::Silence,
            window_bufs: Vec::new(),
        };
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }
}

impl Processor for VadProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        self.inner.lock().unwrap().process_frame(frame)
    }
}
