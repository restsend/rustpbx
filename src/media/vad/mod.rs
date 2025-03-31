use crate::event::{EventSender, SessionEvent};
use crate::media::processor::Processor;
use crate::AudioFrame;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::sync::Mutex;
use webrtc_vad::Vad;

mod silero;
#[cfg(test)]
mod tests;
mod webrtc;

// Thread-safe wrapper for Vad
pub(crate) struct ThreadSafeVad(Mutex<Vad>);
unsafe impl Send for ThreadSafeVad {}
unsafe impl Sync for ThreadSafeVad {}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VADConfig {
    /// Minimum duration of silence to consider speech ended (in milliseconds)
    pub silence_duration_threshold: u64,
    /// Duration of audio to keep before speech starts (in milliseconds)
    /// - Pre-speech padding (150ms):
    /// - Captures speech onset including plosive sounds (like 'p', 'b', 't')
    /// - Helps preserve the natural beginning of utterances
    /// - Typical plosive onset is 50-100ms, so 150ms gives some margin
    pub pre_speech_padding: u64,
    /// Duration of audio to keep after speech ends (in milliseconds)
    /// Post-speech padding (150ms):
    // - Equal to pre-speech for symmetry
    // - Sufficient to capture trailing sounds and natural decay
    // - Avoids cutting off final consonants
    pub post_speech_padding: u64,
    /// Threshold for voice activity detection (0.0 to 1.0)
    pub voice_threshold: f32,
}

impl Default for VADConfig {
    fn default() -> Self {
        Self {
            silence_duration_threshold: 500,
            pre_speech_padding: 150,  // Keep 150ms before speech
            post_speech_padding: 200, // Keep 200ms after speech
            voice_threshold: 0.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VadType {
    #[serde(rename = "webrtc")]
    WebRTC,
    #[serde(rename = "silero")]
    Silero,
}

pub struct VadProcessor {
    vad: Mutex<Box<dyn VadEngine>>,
    event_sender: EventSender,
}

pub trait VadEngine: Send + Sync + Any {
    fn process(&mut self, frame: &mut AudioFrame) -> Result<bool>;

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl VadProcessor {
    pub fn new(track_id: String, vad_type: VadType, event_sender: EventSender) -> Self {
        let vad: Box<dyn VadEngine> = match vad_type {
            VadType::WebRTC => Box::new(webrtc::WebRtcVad::new()),
            VadType::Silero => Box::new(silero::SileroVad::new()),
        };

        Self {
            vad: Mutex::new(vad),
            event_sender,
        }
    }

    #[cfg(test)]
    pub fn set_test_mode(&self, vad_type: &VadType, is_speech: bool) {
        if let VadType::Silero = vad_type {
            let mut vad = self.vad.lock().unwrap();
            let vad_any = vad.as_any_mut();
            if let Some(voice_activity_vad) = vad_any.downcast_mut::<silero::SileroVad>() {
                voice_activity_vad.set_test_mode(is_speech);
            }
        }
    }
}

impl Processor for VadProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        let is_speech = self.vad.lock().unwrap().as_mut().process(frame)?;

        // Send VAD events
        let event = if is_speech {
            SessionEvent::StartSpeaking {
                track_id: frame.track_id.clone(),
                timestamp: frame.timestamp as u64,
            }
        } else {
            SessionEvent::Silence {
                track_id: frame.track_id.clone(),
                timestamp: frame.timestamp as u64,
            }
        };
        self.event_sender.send(event).ok();
        Ok(())
    }
}
