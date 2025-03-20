use std::sync::Mutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use webrtc_vad::Vad;

use crate::media::{
    processor::{AudioFrame, Processor},
    stream::{EventSender, MediaStreamEvent},
};

mod voice_activity;
mod webrtc;

// Thread-safe wrapper for Vad
pub(crate) struct ThreadSafeVad(Vad);
unsafe impl Send for ThreadSafeVad {}
unsafe impl Sync for ThreadSafeVad {}

#[derive(Clone)]
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
    WebRTC,
    VoiceActivity,
}

pub struct VadProcessor {
    vad: Mutex<Box<dyn VadEngine>>,
    event_sender: EventSender,
}

pub trait VadEngine: Send + Sync {
    fn process(&mut self, frame: &mut AudioFrame) -> Result<bool>;
}

impl VadProcessor {
    pub fn new(track_id: String, vad_type: VadType, event_sender: EventSender) -> Self {
        let vad: Box<dyn VadEngine> = match vad_type {
            VadType::WebRTC => Box::new(webrtc::WebRtcVad::new()),
            VadType::VoiceActivity => Box::new(voice_activity::VoiceActivityVad::new()),
        };

        Self {
            vad: Mutex::new(vad),
            event_sender,
        }
    }
}

impl Processor for VadProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        let is_speech = self.vad.lock().unwrap().as_mut().process(frame)?;

        // Send VAD events
        let event = if is_speech {
            MediaStreamEvent::StartSpeaking(frame.track_id.clone(), frame.timestamp)
        } else {
            MediaStreamEvent::Silence(frame.track_id.clone(), frame.timestamp)
        };
        self.event_sender.send(event).ok();
        Ok(())
    }
}
