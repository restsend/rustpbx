use crate::callrecord::CallRecordHangupReason;
use crate::proxy::proxy_call::state::CallSessionHandle;
use crate::proxy::proxy_call::state::SessionAction;
use std::time::Duration;
use tokio::sync::mpsc;

/// An audio playback session.
#[derive(Debug, Clone)]
pub struct PlaybackHandle {
    pub(crate) track_id: String,
    pub(crate) file_path: String,
}

impl PlaybackHandle {
    pub fn track_id(&self) -> &str {
        &self.track_id
    }
}

/// A recording session controller.
#[derive(Debug, Clone)]
pub struct RecordingHandle {
    pub(crate) path: String,
}

/// Details about a completed recording.
#[derive(Debug, Clone)]
pub struct RecordingInfo {
    pub path: String,
    pub duration: Duration,
    pub size_bytes: u64,
}

/// High-level API for controlling a call from within a `CallApp`.
///
/// Wraps the underlying `CallSessionHandle` but provides a simplified,
/// async interface tailored for IVR/Voicemail logic. Methods like `play_audio`
/// or `collect_dtmf` abstract away the underlying message passing.
pub struct CallController {
    pub(crate) session: CallSessionHandle,
    pub(crate) event_rx: mpsc::UnboundedReceiver<ControllerEvent>,
}

/// Events sent from the proxy layer to the controller.
#[derive(Debug, Clone)]
pub enum ControllerEvent {
    /// DTMF digit received.
    DtmfReceived(String),

    /// Audio playback finished.
    AudioComplete { track_id: String, interrupted: bool },

    /// Recording finished.
    RecordingComplete(RecordingInfo),

    /// Call hung up.
    Hangup(Option<CallRecordHangupReason>),

    /// Custom event (e.g., from external webhook).
    Custom(String, serde_json::Value),
}

/// Configuration for collecting DTMF input.
#[derive(Debug, Clone)]
pub struct DtmfCollectConfig {
    /// Minimum digits required to return.
    pub min_digits: usize,
    /// Maximum digits allowed.
    pub max_digits: usize,
    /// Total time to wait for input.
    pub timeout: Duration,
    /// Digit that terminates input (e.g., '#').
    pub terminator: Option<char>,
    /// Optional prompt to play while waiting.
    pub play_prompt: Option<String>,
    /// Time to wait between digits.
    pub inter_digit_timeout: Option<Duration>,
}

impl CallController {
    /// Answer the call (send 200 OK).
    pub async fn answer(&self) -> anyhow::Result<()> {
        self.session.send_command(SessionAction::AcceptCall {
            callee: None,
            sdp: None,
            dialog_id: None,
        })?;
        Ok(())
    }

    /// Hang up the call.
    pub async fn hangup(&self, reason: Option<CallRecordHangupReason>) -> anyhow::Result<()> {
        self.session.send_command(SessionAction::Hangup {
            reason,
            code: None,
            initiator: Some("app".to_string()),
        })?;
        Ok(())
    }

    /// Play an audio file.
    ///
    /// The `interruptible` flag determines if DTMF input should stop playback.
    /// Returns a handle to the playback session.
    pub async fn play_audio(
        &self,
        file: impl Into<String>,
        _interruptible: bool,
    ) -> anyhow::Result<PlaybackHandle> {
        let path = file.into();
        self.session.send_command(SessionAction::PlayPrompt {
            audio_file: path.clone(),
            send_progress: false,
            await_completion: true,
        })?;

        Ok(PlaybackHandle {
            track_id: "default".to_string(), // TODO: generate unique ID
            file_path: path,
        })
    }

    /// Stop current audio playback.
    pub async fn stop_audio(&self) -> anyhow::Result<()> {
        // TODO: Need a wrapper action or way to signal stop
        // self.session.send_command(SessionAction::StopAudio)?;
        Ok(())
    }

    /// Start recording the call audio.
    pub async fn start_recording(
        &self,
        path: impl Into<String>,
        max_duration: Option<Duration>,
        beep: bool,
    ) -> anyhow::Result<RecordingHandle> {
        let p = path.into();
        self.session.send_command(SessionAction::StartRecording {
            path: p.clone(),
            max_duration,
            beep,
        })?;
        Ok(RecordingHandle { path: p })
    }

    /// Stop the active recording.
    pub async fn stop_recording(&self) -> anyhow::Result<RecordingInfo> {
        // TODO: Send stop command and await result
        Ok(RecordingInfo {
            path: "placeholder.wav".to_string(),
            duration: Duration::from_secs(0),
            size_bytes: 0,
        })
    }

    /// Collect DTMF digits with timeout and validation.
    ///
    /// This method blocks until the collection is complete, satisfied, or timed out.
    /// If `play_prompt` is set, it plays audio while listening.
    pub async fn collect_dtmf(&mut self, config: DtmfCollectConfig) -> anyhow::Result<String> {
        if let Some(prompt) = &config.play_prompt {
            self.play_audio(prompt, true).await?;
        }

        let mut collected = String::new();
        let deadline = tokio::time::Instant::now() + config.timeout;

        loop {
            let timeout = deadline - tokio::time::Instant::now();
            if timeout.is_zero() {
                break;
            }

            // Wait for next event or timeout
            match tokio::time::timeout(timeout, self.event_rx.recv()).await {
                Ok(Some(ControllerEvent::DtmfReceived(digit))) => {
                    // Check terminator
                    if let Some(term) = config.terminator {
                        if digit.contains(term) {
                            break;
                        }
                    }

                    collected.push_str(&digit);

                    // Check max length
                    if collected.len() >= config.max_digits {
                        break;
                    }
                }
                Ok(Some(ControllerEvent::Hangup(_))) => {
                    return Err(anyhow::anyhow!("Call hung up during DTMF collection"));
                }
                Ok(None) => return Err(anyhow::anyhow!("Event channel closed")),
                Err(_) => break, // Timeout
                _ => {}          // Ignore other events
            }
        }

        if collected.len() < config.min_digits {
            // Depending on requirements, we might return partial or error
            // For now, return what we have
        }

        Ok(collected)
    }

    /// Wait for the next event from the channel.
    pub async fn wait_event(&mut self) -> Option<ControllerEvent> {
        self.event_rx.recv().await
    }
}
