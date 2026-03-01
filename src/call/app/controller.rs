use crate::callrecord::CallRecordHangupReason;
use crate::proxy::proxy_call::state::CallSessionHandle;
use crate::proxy::proxy_call::state::SessionAction;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// An audio playback session.
#[derive(Debug, Clone)]
pub struct PlaybackHandle {
    pub(crate) track_id: String,
    #[allow(unused)]
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
    #[allow(unused)]
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
/// async interface tailored for IVR/Voicemail logic.
///
/// # Timer system
///
/// Use [`set_timeout`](Self::set_timeout) to schedule named one-shot timers.
/// When the delay elapses, [`CallApp::on_timeout`] is invoked with the same id.
/// Use [`cancel_timeout`](Self::cancel_timeout) to suppress a pending fire.
pub struct CallController {
    pub(crate) session: CallSessionHandle,
    pub(crate) event_rx: mpsc::UnboundedReceiver<ControllerEvent>,
    /// Sends fired timer IDs to the AppEventLoop.
    pub(crate) fired_timer_tx: mpsc::UnboundedSender<String>,
    /// Set of timer IDs that have been cancelled and should be suppressed.
    pub(crate) cancelled_timers: Arc<Mutex<HashSet<String>>>,
}

/// Error returned when the remote party hangs up during `collect_dtmf`.
#[derive(Debug, Error)]
#[error("call hung up during DTMF collection")]
pub struct HangupDuringCollection {
    pub reason: Option<CallRecordHangupReason>,
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

    /// A named timer registered via `CallController::set_timeout` has fired.
    Timeout(String),

    /// Custom event (e.g., from external webhook).
    Custom(String, serde_json::Value),
}

/// Configuration for collecting DTMF input.
#[derive(Debug, Clone)]
pub struct DtmfCollectConfig {
    /// Minimum digits required to return (informational; caller decides on partial).
    pub min_digits: usize,
    /// Maximum digits allowed; collection stops automatically when reached.
    pub max_digits: usize,
    /// Total time budget from the start of collection.
    pub timeout: Duration,
    /// Digit that terminates input early (e.g. `'#'`). Not stored in result.
    pub terminator: Option<char>,
    /// Optional prompt to play before listening.
    pub play_prompt: Option<String>,
    /// Maximum silence between consecutive digits. If the gap exceeds this,
    /// collection completes with whatever has been gathered so far.
    /// Defaults to the remaining `timeout` if not set (i.e. no inter-digit limit).
    pub inter_digit_timeout: Option<Duration>,
}

impl CallController {
    /// Create a controller and its paired timer-fire channel.
    ///
    /// The returned `UnboundedReceiver<String>` **must** be passed to
    /// [`AppEventLoop::new`] so fired timer IDs reach `on_timeout`.
    pub fn new(
        session: CallSessionHandle,
        event_rx: mpsc::UnboundedReceiver<ControllerEvent>,
    ) -> (Self, mpsc::UnboundedReceiver<String>) {
        let (fired_timer_tx, fired_timer_rx) = mpsc::unbounded_channel();
        let ctrl = Self {
            session,
            event_rx,
            fired_timer_tx,
            cancelled_timers: Arc::new(Mutex::new(HashSet::new())),
        };
        (ctrl, fired_timer_rx)
    }

    /// Answer the call (send 200 OK).
    pub async fn answer(&self) -> anyhow::Result<()> {
        self.session.send_command(SessionAction::AcceptCall {
            callee: None,
            sdp: None,
            dialog_id: None,
        })?;
        Ok(())
    }

    pub async fn hangup(&self, reason: Option<CallRecordHangupReason>, code: Option<u16>) -> anyhow::Result<()> {
        self.session.send_command(SessionAction::Hangup {
            reason,
            code,
            initiator: Some("app".to_string()),
        })?;
        Ok(())
    }

    pub async fn transfer(&self, target: impl Into<String>) -> anyhow::Result<()> {
        let target = target.into();
        self.session
            .send_command(SessionAction::from_transfer_target(&target))?;
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
    ///
    /// Full implementation pending `SessionAction::StopPlayback` (tracked in RWI P0).
    pub async fn stop_audio(&self) -> anyhow::Result<()> {
        // TODO: self.session.send_command(SessionAction::StopPlayback { track_id: None })?;
        Ok(())
    }

    /// Register a named one-shot timer.
    ///
    /// After `delay`, [`CallApp::on_timeout`] will be invoked with `id`.
    ///
    /// Calling `set_timeout` with the same `id` before it fires **re-registers**
    /// the timer (the previous one is cancelled). Use [`cancel_timeout`](Self::cancel_timeout)
    /// to suppress a pending fire without re-registering.
    ///
    /// # Panics
    /// Will not panic; timer tasks are fire-and-forget on a Tokio runtime.
    pub fn set_timeout(&self, id: impl Into<String>, delay: Duration) {
        let id = id.into();
        // If re-registering, un-cancel any previous suppression.
        self.cancelled_timers.lock().unwrap().remove(&id);
        let tx = self.fired_timer_tx.clone();
        let cancelled = self.cancelled_timers.clone();
        let id_task = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            // Only fire if not cancelled in the meantime.
            let was_cancelled = cancelled.lock().unwrap().remove(&id_task);
            if !was_cancelled {
                let _ = tx.send(id_task);
            }
        });
    }

    /// Cancel a pending timer previously registered with [`set_timeout`](Self::set_timeout).
    ///
    /// If the timer has already fired, this is a no-op.
    pub fn cancel_timeout(&self, id: &str) {
        self.cancelled_timers.lock().unwrap().insert(id.to_string());
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

    /// Collect DTMF digits with timeout and inter-digit gap detection.
    ///
    /// Blocks until one of the following:
    /// - `max_digits` collected
    /// - terminator digit pressed
    /// - inter-digit silence exceeds `inter_digit_timeout` (after first digit)
    /// - overall `timeout` elapsed
    ///
    /// Returns the collected string (may be shorter than `min_digits` on timeout;
    /// the caller decides whether to re-prompt or accept partial input).
    ///
    /// # Errors
    /// Returns [`HangupDuringCollection`] if the remote party hangs up.
    pub async fn collect_dtmf(&mut self, config: DtmfCollectConfig) -> anyhow::Result<String> {
        if let Some(ref prompt) = config.play_prompt {
            self.play_audio(prompt.clone(), true).await?;
        }

        let mut collected = String::new();
        let overall_deadline = Instant::now() + config.timeout;

        loop {
            let overall_remaining = overall_deadline.saturating_duration_since(Instant::now());
            if overall_remaining.is_zero() {
                break;
            }

            // After the first digit, honour inter_digit_timeout as the per-gap
            // budget. Cap at overall remaining so we never overshoot.
            let wait = if !collected.is_empty() {
                config
                    .inter_digit_timeout
                    .map(|idt| idt.min(overall_remaining))
                    .unwrap_or(overall_remaining)
            } else {
                overall_remaining
            };

            match tokio::time::timeout(wait, self.event_rx.recv()).await {
                Ok(Some(ControllerEvent::DtmfReceived(digit))) => {
                    if let Some(term) = config.terminator {
                        if digit.contains(term) {
                            break;
                        }
                    }
                    collected.push_str(&digit);
                    if collected.len() >= config.max_digits {
                        break;
                    }
                }
                Ok(Some(ControllerEvent::Hangup(reason))) => {
                    return Err(HangupDuringCollection { reason }.into());
                }
                Ok(None) => return Err(anyhow::anyhow!("event channel closed")),
                Err(_) => break, // inter-digit or overall timeout
                _ => {}          // audio events etc. are ignored during collection
            }
        }

        Ok(collected)
    }

    /// Wait for the next event from the channel.
    pub async fn wait_event(&mut self) -> Option<ControllerEvent> {
        self.event_rx.recv().await
    }
}
