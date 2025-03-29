use anyhow::Result;
use axum::Router;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use webrtc::peer_connection::RTCPeerConnection;

use crate::event::{EventSender, SessionEvent};
use crate::media::{
    processor::{AudioFrame, Processor, Samples},
    stream::{MediaStream, MediaStreamEvent},
};

// Configuration for Language Model - use the one from llm module
pub use crate::llm::LlmConfig;

// Session state for active calls
#[derive(Clone)]
pub struct CallHandlerState {
    pub active_calls: Arc<Mutex<std::collections::HashMap<String, ActiveCall>>>,
}

// Active call information
#[derive(Clone)]
pub struct ActiveCall {
    pub session_id: String,
    pub media_stream: Arc<MediaStream>,
    pub peer_connection: Arc<RTCPeerConnection>,
    pub events: tokio::sync::broadcast::Sender<CallEvent>,
    pub pipeline_manager: Option<Arc<crate::media::pipeline::PipelineManager>>,
}

// Configuration for ASR (Automatic Speech Recognition)
#[derive(Debug, Clone, Deserialize)]
pub struct AsrConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub language: Option<String>,
}

// Configuration for Text-to-Speech
#[derive(Debug, Clone, Deserialize)]
pub struct SynthesisConfig {
    pub url: String,
    pub voice: Option<String>,
    pub rate: Option<f32>,
}

// Configuration for Voice Activity Detection
#[derive(Debug, Clone, Deserialize)]
pub struct VadConfig {
    pub enabled: bool,
    pub mode: Option<String>,
    pub min_speech_duration_ms: Option<u32>,
    pub min_silence_duration_ms: Option<u32>,
}

// Response for WebRTC SDP answer
#[derive(Debug, Serialize)]
pub struct CallResponse {
    pub session_id: String,
    pub sdp: String,
}

// Call Events
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum CallEvent {
    #[serde(rename = "vad")]
    VadEvent {
        track_id: String,
        timestamp: u32,
        is_speech: bool,
    },
    #[serde(rename = "asr")]
    AsrEvent {
        track_id: String,
        timestamp: u32,
        text: String,
        is_final: bool,
    },
    #[serde(rename = "llm")]
    LlmEvent {
        timestamp: u32,
        text: String,
        is_final: bool,
    },
    #[serde(rename = "tts")]
    TtsEvent { timestamp: u32, text: String },
    #[serde(rename = "metrics")]
    MetricsEvent {
        timestamp: u32,
        metrics: serde_json::Value,
    },
    #[serde(rename = "ringing")]
    RingingEvent { timestamp: u32 },
    #[serde(rename = "hangup")]
    HangupEvent { timestamp: u32, reason: String },
    #[serde(rename = "refer")]
    ReferEvent { timestamp: u32, target: String },
    #[serde(rename = "error")]
    ErrorEvent { timestamp: u32, error: String },
}

// WebSocket Commands
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command")]
pub enum WsCommand {
    #[serde(rename = "play_tts")]
    PlayTts { text: String },
    #[serde(rename = "play_wav")]
    PlayWav { url: String },
    #[serde(rename = "hangup")]
    Hangup {},
    #[serde(rename = "refer")]
    Refer { target: String },
    #[serde(rename = "pipeline_start")]
    PipelineStart {
        pipeline_type: String,
        config: Option<serde_json::Value>,
    },
    #[serde(rename = "pipeline_stop")]
    PipelineStop { pipeline_id: String },
    #[serde(rename = "send_text")]
    SendText { text: String },
    #[serde(rename = "mute")]
    Mute { track_id: Option<String> },
    #[serde(rename = "unmute")]
    Unmute { track_id: Option<String> },
}

// ASR Processor that integrates with the media stream system
pub struct AsrProcessor {
    config: AsrConfig,
    event_sender: tokio::sync::broadcast::Sender<CallEvent>,
}

impl AsrProcessor {
    pub fn new(config: AsrConfig, event_sender: tokio::sync::broadcast::Sender<CallEvent>) -> Self {
        Self {
            config,
            event_sender,
        }
    }
}

impl Processor for AsrProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        if self.config.enabled {
            // In a real implementation, this would process the audio data
            // and send it to an ASR service

            // For now, we'll just simulate ASR events occasionally
            if frame.timestamp % 3000 == 0 {
                let event = CallEvent::AsrEvent {
                    track_id: frame.track_id.clone(),
                    timestamp: frame.timestamp,
                    text: "Sample transcription".to_string(),
                    is_final: frame.timestamp % 6000 == 0,
                };
                let _ = self.event_sender.send(event);
            }
        }
        Ok(())
    }
}

// Adapt MediaStreamEvents to CallEvents
pub fn convert_media_event(event: MediaStreamEvent) -> Option<CallEvent> {
    match event {
        MediaStreamEvent::StartSpeaking(track_id, timestamp) => Some(CallEvent::VadEvent {
            track_id,
            timestamp,
            is_speech: true,
        }),
        MediaStreamEvent::Silence(track_id, timestamp) => Some(CallEvent::VadEvent {
            track_id,
            timestamp,
            is_speech: false,
        }),
        MediaStreamEvent::Transcription(track_id, timestamp, text) => Some(CallEvent::AsrEvent {
            track_id,
            timestamp,
            text,
            is_final: true,
        }),
        MediaStreamEvent::TranscriptionSegment(track_id, timestamp, text) => {
            Some(CallEvent::AsrEvent {
                track_id,
                timestamp,
                text,
                is_final: false,
            })
        }
        _ => None,
    }
}

// Configure Router function now moved to individual modules
pub fn router() -> Router<CallHandlerState> {
    // Combine routers from webrtc and sip modules
    let state = CallHandlerState {
        active_calls: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };

    let webrtc_router = crate::handler::webrtc::router();
    let sip_router = crate::handler::sip::router();

    webrtc_router.merge(sip_router).with_state(state)
}

// Handler for ws commands moved to individual modules

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    // Test WsCommand serialization
    #[test]
    fn test_ws_command_serialization() {
        // Test PlayTts command
        let cmd = WsCommand::PlayTts {
            text: "Hello, world!".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("play_tts"));
        assert!(json.contains("Hello, world!"));

        // Test PlayWav command
        let cmd = WsCommand::PlayWav {
            url: "https://example.com/audio.wav".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("play_wav"));
        assert!(json.contains("https://example.com/audio.wav"));

        // Test Hangup command
        let cmd = WsCommand::Hangup {};
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("hangup"));

        // Test PipelineStart command
        let cmd = WsCommand::PipelineStart {
            pipeline_type: "transcription".to_string(),
            config: Some(serde_json::json!({ "model": "whisper" })),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("pipeline_start"));
        assert!(json.contains("transcription"));
        assert!(json.contains("whisper"));

        // Test deserialization
        let json = r#"{"command":"play_tts","text":"Test TTS"}"#;
        let cmd: WsCommand = serde_json::from_str(json).unwrap();
        match cmd {
            WsCommand::PlayTts { text } => {
                assert_eq!(text, "Test TTS");
            }
            _ => panic!("Unexpected command type"),
        }

        // Test PipelineStart deserialization
        let json =
            r#"{"command":"pipeline_start","pipeline_type":"llm","config":{"model":"gpt-4"}}"#;
        let cmd: WsCommand = serde_json::from_str(json).unwrap();
        match cmd {
            WsCommand::PipelineStart {
                pipeline_type,
                config,
            } => {
                assert_eq!(pipeline_type, "llm");
                assert_eq!(config.unwrap()["model"], "gpt-4");
            }
            _ => panic!("Unexpected command type"),
        }
    }

    // Test CallEvent serialization
    #[test]
    fn test_call_event_serialization() {
        let event = CallEvent::AsrEvent {
            track_id: "test-track".to_string(),
            timestamp: 12345,
            text: "Test ASR".to_string(),
            is_final: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("asr"));
        assert!(json.contains("test-track"));
        assert!(json.contains("Test ASR"));
        assert!(json.contains("12345"));
        assert!(json.contains("true"));
    }

    // Test media event conversion
    #[test]
    fn test_convert_media_event() {
        // Test StartSpeaking event conversion
        let track_id = "test-track".to_string();
        let timestamp = 12345u32;
        let event = MediaStreamEvent::StartSpeaking(track_id.clone(), timestamp);

        let converted = convert_media_event(event);
        assert!(converted.is_some());

        if let Some(CallEvent::VadEvent {
            track_id: tid,
            timestamp: ts,
            is_speech,
        }) = converted
        {
            assert_eq!(tid, track_id);
            assert_eq!(ts, timestamp);
            assert!(is_speech);
        } else {
            panic!("Converted event is not the expected type");
        }

        // Test Silence event conversion
        let event = MediaStreamEvent::Silence(track_id.clone(), timestamp);
        let converted = convert_media_event(event);
        assert!(converted.is_some());

        if let Some(CallEvent::VadEvent {
            track_id: tid,
            timestamp: ts,
            is_speech,
        }) = converted
        {
            assert_eq!(tid, track_id);
            assert_eq!(ts, timestamp);
            assert!(!is_speech);
        } else {
            panic!("Converted event is not the expected type");
        }
    }

    // Test ASR processor
    #[test]
    fn test_asr_processor() {
        let (sender, _) = broadcast::channel(10);
        let config = AsrConfig {
            enabled: true,
            model: Some("test".to_string()),
            language: Some("en".to_string()),
        };

        let processor = AsrProcessor::new(config, sender.clone());

        // Create test audio frame
        let mut frame = AudioFrame {
            track_id: "test".to_string(),
            samples: Samples::PCM(vec![0; 160]),
            timestamp: 3000, // Choose a timestamp divisible by 3000
            sample_rate: 16000,
        };

        // Test processor processing audio frame
        let result = processor.process_frame(&mut frame);
        assert!(result.is_ok());
    }

    // Test CallResponse structure
    #[test]
    fn test_call_response_structure() {
        // Create a call response
        let response = CallResponse {
            session_id: "test-session-id".to_string(),
            sdp: "test-sdp-content".to_string(),
        };

        // Serialize response to JSON
        let json = serde_json::to_string(&response).unwrap();

        // Test JSON structure contains expected fields
        assert!(json.contains("session_id"));
        assert!(json.contains("test-session-id"));
        assert!(json.contains("sdp"));
        assert!(json.contains("test-sdp-content"));
    }
}
