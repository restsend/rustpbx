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
    config: VADConfig,
    is_speaking: Mutex<bool>,
    last_speech_time: Mutex<Option<std::time::Instant>>,
    // Buffer for storing frames for pre and post speech padding
    speech_padding_buffer: Mutex<Vec<AudioFrame>>,
    buffer_started_at: Mutex<Option<std::time::Instant>>,
}

pub trait VadEngine: Send + Sync + Any {
    fn process(&mut self, frame: &mut AudioFrame) -> Result<bool>;
}

impl VadProcessor {
    pub fn new(vad_type: VadType, event_sender: EventSender, config: VADConfig) -> Self {
        let vad: Box<dyn VadEngine> = match vad_type {
            VadType::WebRTC => Box::new(webrtc::WebRtcVad::new()),
            VadType::Silero => Box::new(silero::SileroVad::new()),
        };

        Self {
            vad: Mutex::new(vad),
            event_sender,
            config,
            is_speaking: Mutex::new(false),
            last_speech_time: Mutex::new(None),
            speech_padding_buffer: Mutex::new(Vec::new()),
            buffer_started_at: Mutex::new(None),
        }
    }

    // Add a frame to the speech padding buffer
    fn add_to_buffer(&self, frame: &AudioFrame) {
        let mut buffer = self.speech_padding_buffer.lock().unwrap();
        let mut buffer_started_at = self.buffer_started_at.lock().unwrap();

        // Initialize buffer start time if this is the first frame
        if buffer_started_at.is_none() {
            *buffer_started_at = Some(std::time::Instant::now());
        }

        // Add frame to buffer
        buffer.push(frame.clone());

        // Remove oldest frames if buffer exceeds the maximum pre-speech padding duration
        let max_buffer_duration = self.config.pre_speech_padding;
        if let Some(start_time) = *buffer_started_at {
            let elapsed = std::time::Instant::now()
                .duration_since(start_time)
                .as_millis() as u64;

            // If buffer duration exceeds max, trim it
            if elapsed > max_buffer_duration && !buffer.is_empty() {
                // Calculate how many frames to keep based on frame duration
                // Assuming 20ms per frame for simplicity (standard for many audio systems)
                let frame_duration_ms = 20;
                let frames_to_keep = (max_buffer_duration / frame_duration_ms) as usize;

                // Keep only the newest frames up to frames_to_keep
                if buffer.len() > frames_to_keep {
                    let buffer_len = buffer.len();
                    // Create a new vector with just the frames we want to keep
                    let new_buffer: Vec<AudioFrame> =
                        buffer.drain(buffer_len - frames_to_keep..).collect();
                    *buffer = new_buffer;
                }
            }
        }
    }
}

impl Processor for VadProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        let is_voice = self.vad.lock().unwrap().as_mut().process(frame)?;
        let mut is_speaking = self.is_speaking.lock().unwrap();
        let mut last_speech_time = self.last_speech_time.lock().unwrap();

        // Add the current frame to buffer for potential pre-speech padding
        if !*is_speaking {
            self.add_to_buffer(frame);
        }

        if is_voice {
            // Update the last speech time when we detect voice
            *last_speech_time = Some(std::time::Instant::now());

            // If we weren't speaking before, emit a StartSpeaking event
            if !*is_speaking {
                *is_speaking = true;

                // Emit StartSpeaking event
                let event = SessionEvent::StartSpeaking {
                    track_id: frame.track_id.clone(),
                    timestamp: frame.timestamp,
                };
                self.event_sender.send(event).ok();

                // Reset buffer start time once we start speaking
                *self.buffer_started_at.lock().unwrap() = Some(std::time::Instant::now());
            }
        } else {
            // Not currently voice - check if we need to transition to silence
            if *is_speaking {
                // Check if silence has lasted longer than the threshold
                if let Some(last_time) = *last_speech_time {
                    let silence_duration = std::time::Instant::now().duration_since(last_time);

                    if silence_duration
                        > std::time::Duration::from_millis(self.config.silence_duration_threshold)
                    {
                        // Silence has lasted long enough, transition to silence state
                        *is_speaking = false;

                        // Emit Silence event
                        let event = SessionEvent::Silence {
                            track_id: frame.track_id.clone(),
                            timestamp: frame.timestamp,
                        };
                        self.event_sender.send(event).ok();

                        // Reset speech padding buffer for next utterance
                        let mut buffer = self.speech_padding_buffer.lock().unwrap();
                        buffer.clear();
                        *self.buffer_started_at.lock().unwrap() = None;
                    }
                }
            } else if last_speech_time.is_none() {
                // Initial silence - needed for tests
                *last_speech_time = Some(std::time::Instant::now());

                let event = SessionEvent::Silence {
                    track_id: frame.track_id.clone(),
                    timestamp: frame.timestamp,
                };
                self.event_sender.send(event).ok();
            }
        }

        Ok(())
    }
}
