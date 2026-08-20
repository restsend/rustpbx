use crate::call::domain::PlayOptions;
use crate::call::domain::{CallCommand, HangupCommand, LegId, MediaSource};
use crate::callrecord::CallRecordHangupReason;
use crate::proxy::proxy_call::sip_session::SipSessionHandle;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};

/// An audio playback session. Playback completion is delivered to the app via
/// [`ControllerEvent::AudioComplete`] (keyed by [`PlaybackToken::track_id`]).
#[derive(Debug, Clone)]
pub struct PlaybackToken {
    pub(crate) track_id: String,
}

impl PlaybackToken {
    pub fn track_id(&self) -> &str {
        &self.track_id
    }
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
/// Wraps the underlying `SipSessionHandle` but provides a simplified,
/// async interface tailored for IVR/Voicemail logic.
///
/// # Timer system
///
/// Use [`set_timeout`](Self::set_timeout) to schedule named one-shot timers.
/// When the delay elapses, [`CallApp::on_timeout`] is invoked with the same id.
/// Use [`cancel_timeout`](Self::cancel_timeout) to suppress a pending fire.
pub struct CallController {
    pub(crate) session: SipSessionHandle,
    pub(crate) event_rx: mpsc::UnboundedReceiver<ControllerEvent>,
    /// Sends fired timer IDs to the AppEventLoop.
    pub(crate) fired_timer_tx: mpsc::UnboundedSender<String>,
    /// Set of timer IDs that have been cancelled and should be suppressed.
    pub(crate) cancelled_timers: Arc<Mutex<HashSet<String>>>,
    /// JoinHandles of pending timer tasks, keyed by timer id, so they can be
    /// aborted on cancel/re-register instead of sleeping for the full delay
    /// (which previously kept Arc clones + the channel sender alive past call
    /// end and skewed task metrics).
    pub(crate) timer_tasks: Arc<DashMap<String, JoinHandle<()>>>,
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

    /// An application-owned transfer reached a terminal outcome.
    TransferResult(crate::call::domain::TransferOutcome),

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
        session: SipSessionHandle,
        event_rx: mpsc::UnboundedReceiver<ControllerEvent>,
    ) -> (Self, mpsc::UnboundedReceiver<String>) {
        let (fired_timer_tx, fired_timer_rx) = mpsc::unbounded_channel();
        let ctrl = Self {
            session,
            event_rx,
            fired_timer_tx,
            cancelled_timers: Arc::new(Mutex::new(HashSet::new())),
            timer_tasks: Arc::new(DashMap::new()),
        };
        (ctrl, fired_timer_rx)
    }

    /// Answer the call (send 200 OK).
    pub async fn answer(&self) -> anyhow::Result<()> {
        self.session.send_command(CallCommand::Answer {
            leg_id: LegId::from("caller"),
        })?;
        Ok(())
    }

    /// Append an event to the session's call trace timeline.
    ///
    /// Allows call applications (voicemail, IVR, ...) to contribute entries
    /// to the call-record `metadata["trace"]` JSON array.
    pub fn record_trace(&self, event: crate::call_errors::TraceEvent) {
        let _ = self.session.send_command(CallCommand::Trace { event });
    }

    pub async fn hangup(
        &self,
        reason: Option<CallRecordHangupReason>,
        code: Option<u16>,
    ) -> anyhow::Result<()> {
        self.session
            .send_command_async(CallCommand::Hangup(HangupCommand::all(reason, code)))
            .await?;
        Ok(())
    }

    pub async fn transfer(&self, target: impl Into<String>) -> anyhow::Result<()> {
        let target = target.into();
        self.session.send_command(CallCommand::Transfer {
            leg_id: LegId::from("caller"),
            target,
            attended: false,
        })?;
        Ok(())
    }

    pub(crate) async fn transfer_await_result(
        &self,
        target: impl Into<String>,
    ) -> anyhow::Result<()> {
        self.session
            .send_command(CallCommand::TransferAwaitResult {
                leg_id: LegId::from("caller"),
                target: target.into(),
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
    ) -> anyhow::Result<PlaybackToken> {
        self.play_audio_with_options(file, None, false, _interruptible)
            .await
    }

    /// Play an audio file with full control over loop and DTMF interruptibility.
    ///
    /// - `track_id` – caller-assigned unique ID; a UUID is generated when `None`.
    ///   Kept for API compatibility; playback is tracked by leg for completion.
    /// - `loop_playback` – when `true`, the file loops until explicitly stopped.
    /// - `interruptible` – whether DTMF should stop playback (handled by the app).
    pub async fn play_audio_with_options(
        &self,
        file: impl Into<String>,
        track_id: Option<String>,
        loop_playback: bool,
        interruptible: bool,
    ) -> anyhow::Result<PlaybackToken> {
        let path = file.into();
        let track_id = track_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let source = if path.starts_with("http://") || path.starts_with("https://") {
            MediaSource::Url { url: path.clone() }
        } else {
            MediaSource::File { path: path.clone() }
        };
        self.session.send_command(CallCommand::Play {
            leg_id: None,
            source,
            options: Some(PlayOptions {
                loop_playback,
                await_completion: false,
                interrupt_on_dtmf: interruptible,
                track_id: Some(track_id.clone()),
                send_progress: false,
                side_only: false,
            }),
        })?;

        Ok(PlaybackToken { track_id })
    }

    /// Play an audio file to the caller leg only (no mirror onto the opposite
    /// leg), for caller-exclusive announcements the agent must not hear.
    pub async fn play_audio_caller_only(
        &self,
        file: impl Into<String>,
        interruptible: bool,
    ) -> anyhow::Result<PlaybackToken> {
        let path = file.into();
        let track_id = uuid::Uuid::new_v4().to_string();
        let source = if path.starts_with("http://") || path.starts_with("https://") {
            MediaSource::Url { url: path.clone() }
        } else {
            MediaSource::File { path: path.clone() }
        };
        self.session.send_command(CallCommand::Play {
            leg_id: None,
            source,
            options: Some(PlayOptions {
                loop_playback: false,
                await_completion: false,
                interrupt_on_dtmf: interruptible,
                track_id: Some(track_id.clone()),
                send_progress: false,
                side_only: true,
            }),
        })?;

        Ok(PlaybackToken { track_id })
    }

    /// Stop current audio playback.
    pub async fn stop_audio(&self) -> anyhow::Result<()> {
        self.session
            .send_command(CallCommand::StopPlayback { leg_id: None })?;
        Ok(())
    }

    /// Register a named one-shot timer.
    ///
    /// After `delay`, [`CallApp::on_timeout`] will be invoked with `id`.
    ///
    /// Calling `set_timeout` with the same `id` before it fires **re-registers**
    /// the timer (the previous task is aborted). Use [`cancel_timeout`](Self::cancel_timeout)
    /// to suppress a pending fire without re-registering.
    pub fn set_timeout(&self, id: impl Into<String>, delay: Duration) {
        let id = id.into();
        // Re-registering: abort the previous timer task (if any) and clear any
        // cancellation flag so the new timer is armed fresh.
        if let Some((_, handle)) = self.timer_tasks.remove(&id) {
            handle.abort();
        }
        self.cancelled_timers.lock().remove(&id);

        let tx = self.fired_timer_tx.clone();
        let cancelled = self.cancelled_timers.clone();
        let tasks = self.timer_tasks.clone();
        let id_task = id.clone();
        let handle = crate::utils::spawn(async move {
            tokio::time::sleep(delay).await;
            // Self-remove so the handle map does not retain finished tasks.
            tasks.remove(&id_task);
            // Only fire if not cancelled in the meantime.
            let was_cancelled = cancelled.lock().remove(&id_task);
            if !was_cancelled {
                let _ = tx.send(id_task);
            }
        });
        self.timer_tasks.insert(id, handle);
    }

    /// Cancel a pending timer previously registered with [`set_timeout`](Self::set_timeout).
    ///
    /// If the timer has already fired, this is a no-op.
    pub fn cancel_timeout(&self, id: &str) {
        self.cancelled_timers.lock().insert(id.to_string());
        // Abort the sleeping task immediately rather than letting it run for the
        // remainder of its delay.
        if let Some((_, handle)) = self.timer_tasks.remove(id) {
            handle.abort();
        }
    }

    /// Start recording the call audio.
    pub async fn start_recording(
        &self,
        path: impl Into<String>,
        max_duration: Option<Duration>,
        beep: bool,
    ) -> anyhow::Result<()> {
        self.start_recording_with_layout(path, max_duration, beep, None, None)
            .await
    }

    /// Start a **mono caller-only** recording.
    ///
    /// Writes a single-channel WAV containing only the caller's ingress at
    /// full amplitude. Used by voicemail, where the egress leg is silence
    /// during the message — roughly halves the file size vs stereo.
    pub async fn start_recording_mono(
        &self,
        path: impl Into<String>,
        max_duration: Option<Duration>,
        beep: bool,
    ) -> anyhow::Result<()> {
        self.start_recording_with_layout(path, max_duration, beep, Some(1), Some(true))
            .await
    }

    async fn start_recording_with_layout(
        &self,
        path: impl Into<String>,
        max_duration: Option<Duration>,
        beep: bool,
        channels: Option<u16>,
        mono_caller_only: Option<bool>,
    ) -> anyhow::Result<()> {
        let p = path.into();
        let config = crate::call::domain::RecordConfig {
            path: p.clone(),
            max_duration_secs: max_duration.map(|d| d.as_secs() as u32),
            beep,
            format: None,
            channels,
            mono_caller_only,
            segment_type: None,
            segment_id: None,
            notify_app: Some(true),
        };
        self.session
            .send_command(CallCommand::StartRecording { config })?;
        Ok(())
    }

    /// Start a mid-call recording segment. Path is generated by SipSession from
    /// the root `session_id` + type/id. Does not notify the CallApp on stop
    /// (`notify_app = false`) so IVR flow is not hijacked.
    pub async fn start_recording_segment(
        &self,
        segment_type: impl Into<String>,
        segment_id: Option<String>,
        max_duration: Option<Duration>,
        beep: bool,
    ) -> anyhow::Result<()> {
        let config = crate::call::domain::RecordConfig {
            path: String::new(),
            max_duration_secs: max_duration.map(|d| d.as_secs() as u32),
            beep,
            format: None,
            channels: None,
            mono_caller_only: None,
            segment_type: Some(segment_type.into()),
            segment_id,
            notify_app: Some(false),
        };
        self.session
            .send_command(CallCommand::StartRecording { config })?;
        Ok(())
    }

    /// Stop recording without waiting for `RecordingComplete` on the
    /// controller event channel (segment stops are tracked by SipSession).
    pub fn stop_recording_nowait(&self) -> anyhow::Result<()> {
        self.session.send_command(CallCommand::StopRecording)?;
        Ok(())
    }

    /// Stop the active recording and wait for completion.
    ///
    /// Sends a stop command and waits for the `RecordingComplete` event.
    /// Returns the recording info including path, duration, and file size.
    ///
    /// # Errors
    /// Returns an error if the event channel is closed or a hangup occurs.
    pub async fn stop_recording(&mut self) -> anyhow::Result<RecordingInfo> {
        self.session.send_command(CallCommand::StopRecording)?;

        loop {
            match self.event_rx.recv().await {
                Some(ControllerEvent::RecordingComplete(info)) => {
                    return Ok(info);
                }
                Some(ControllerEvent::Hangup(reason)) => {
                    return Err(anyhow::anyhow!(
                        "Call hung up while stopping recording: {:?}",
                        reason
                    ));
                }
                Some(_) => {
                    // Ignore other events (DTMF, AudioComplete, etc.)
                }
                None => {
                    return Err(anyhow::anyhow!("Event channel closed"));
                }
            }
        }
    }

    /// Collect DTMF digits with timeout and inter-digit gap detection.

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
                    if let Some(term) = config.terminator
                        && digit.contains(term)
                    {
                        break;
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

    /// Send a command to originate a call to an agent.
    /// This creates a new leg and bridges it to the current call.
    pub async fn originate_call(
        &self,
        target_uri: impl Into<String>,
        _caller_id: Option<String>,
    ) -> anyhow::Result<String> {
        let target = target_uri.into();
        let call_id = uuid::Uuid::new_v4().to_string();

        self.session.send_command(CallCommand::LegAdd {
            target: target.clone(),
            leg_id: Some(LegId::from(call_id.clone())),
        })?;

        info!(target = %target, call_id = %call_id, "Queue: originated call to agent");
        Ok(call_id)
    }

    /// Send a custom event to notify external systems (e.g., WebSocket, RWI).
    pub async fn notify_event(
        &self,
        event_name: impl Into<String>,
        data: serde_json::Value,
    ) -> anyhow::Result<()> {
        let name = event_name.into();
        self.session.send_command(CallCommand::InjectAppEvent {
            event: crate::call::domain::AppEvent::Custom { name, data },
        })?;
        Ok(())
    }

    /// Place the caller (or a specific leg) on hold, optionally with hold music.
    pub async fn hold(
        &self,
        leg_id: impl Into<String>,
        music: Option<String>,
    ) -> anyhow::Result<()> {
        let source = music.map(|path| {
            if path.starts_with("http://") || path.starts_with("https://") {
                MediaSource::Url { url: path }
            } else {
                MediaSource::File { path }
            }
        });
        self.session.send_command(CallCommand::Hold {
            leg_id: LegId::from(leg_id.into()),
            music: source,
        })?;
        Ok(())
    }

    /// Release a leg from hold.
    pub async fn unhold(&self, leg_id: impl Into<String>) -> anyhow::Result<()> {
        self.session.send_command(CallCommand::Unhold {
            leg_id: LegId::from(leg_id.into()),
        })?;
        Ok(())
    }

    /// Supervisor listen mode: the supervisor's leg monitors the target leg.
    pub async fn supervisor_listen(
        &self,
        supervisor_leg: impl Into<String>,
        target_leg: impl Into<String>,
    ) -> anyhow::Result<()> {
        self.session.send_command(CallCommand::SupervisorListen {
            supervisor_leg: LegId::from(supervisor_leg.into()),
            target_leg: LegId::from(target_leg.into()),
            supervisor_session_id: None,
        })?;
        Ok(())
    }

    /// Supervisor whisper mode: the supervisor can talk to the agent only.
    pub async fn supervisor_whisper(
        &self,
        supervisor_leg: impl Into<String>,
        target_leg: impl Into<String>,
    ) -> anyhow::Result<()> {
        self.session.send_command(CallCommand::SupervisorWhisper {
            supervisor_leg: LegId::from(supervisor_leg.into()),
            target_leg: LegId::from(target_leg.into()),
            supervisor_session_id: None,
        })?;
        Ok(())
    }

    /// Supervisor barge mode: the supervisor joins the conversation.
    pub async fn supervisor_barge(
        &self,
        supervisor_leg: impl Into<String>,
        target_leg: impl Into<String>,
    ) -> anyhow::Result<()> {
        self.session.send_command(CallCommand::SupervisorBarge {
            supervisor_leg: LegId::from(supervisor_leg.into()),
            target_leg: LegId::from(target_leg.into()),
            supervisor_session_id: None,
        })?;
        Ok(())
    }

    /// Supervisor takeover mode: the supervisor replaces the agent.
    pub async fn supervisor_takeover(
        &self,
        supervisor_leg: impl Into<String>,
        target_leg: impl Into<String>,
    ) -> anyhow::Result<()> {
        self.session.send_command(CallCommand::SupervisorTakeover {
            supervisor_leg: LegId::from(supervisor_leg.into()),
            target_leg: LegId::from(target_leg.into()),
            supervisor_session_id: None,
        })?;
        Ok(())
    }

    /// Stop supervisor mode for a leg.
    pub async fn supervisor_stop(&self, supervisor_leg: impl Into<String>) -> anyhow::Result<()> {
        self.session.send_command(CallCommand::SupervisorStop {
            supervisor_leg: LegId::from(supervisor_leg.into()),
        })?;
        Ok(())
    }

    /// Remove (cancel) a set of call legs by their leg IDs.
    ///
    /// Each leg is sent a `LegRemove` command, which causes the SIP session
    /// to send a BYE/CANCEL and clean up the dialog.
    pub fn remove_legs(&self, leg_ids: &[String]) {
        for leg_id in leg_ids {
            if let Err(e) = self.session.send_command(CallCommand::LegRemove {
                leg_id: LegId::from(leg_id.as_str()),
            }) {
                warn!("Failed to send LegRemove for {}: {}", leg_id, e);
            }
        }
    }
}

impl Drop for CallController {
    fn drop(&mut self) {
        // Abort every still-pending timer task so they don't keep sleeping (and
        // holding their captured Arc/channel clones) after the call has ended.
        let keys: Vec<String> = self.timer_tasks.iter().map(|e| e.key().clone()).collect();
        for key in keys {
            if let Some((_, handle)) = self.timer_tasks.remove(&key) {
                handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::domain::CallCommand;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    /// Creates a controller with access to both the event sender and command receiver.
    /// Returns (controller, event_tx, cmd_rx)
    fn make_controller_with_channels() -> (
        CallController,
        mpsc::UnboundedSender<ControllerEvent>,
        mpsc::Receiver<CallCommand>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

        // Create a minimal SipSessionHandle for testing
        use crate::proxy::proxy_call::sip_session::SipSessionHandle;
        let handle = SipSessionHandle::new_for_test("test-session-id", cmd_tx);
        let (controller, _timer_rx) = CallController::new(handle, event_rx);
        (controller, event_tx, cmd_rx)
    }

    #[tokio::test]
    async fn test_stop_recording_returns_recording_info() {
        let (mut controller, event_tx, mut cmd_rx) = make_controller_with_channels();

        // Spawn a task that monitors commands and sends RecordingComplete when StopRecording is received
        let event_tx_clone = event_tx.clone();
        crate::utils::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if matches!(cmd, CallCommand::StopRecording) {
                    // Simulate the session processing the stop and sending back RecordingComplete
                    let _ =
                        event_tx_clone.send(ControllerEvent::RecordingComplete(RecordingInfo {
                            path: "/tmp/test.wav".to_string(),
                            duration: Duration::from_secs(5),
                            size_bytes: 1024,
                        }));
                    break;
                }
            }
        });

        let result = timeout(Duration::from_secs(1), controller.stop_recording()).await;
        assert!(result.is_ok());
        let info = result.unwrap().unwrap();
        assert_eq!(info.path, "/tmp/test.wav");
        assert_eq!(info.duration, Duration::from_secs(5));
        assert_eq!(info.size_bytes, 1024);
    }

    #[tokio::test]
    async fn test_stop_recording_handles_hangup() {
        let (mut controller, event_tx, mut cmd_rx) = make_controller_with_channels();

        // Spawn a task that sends Hangup instead of RecordingComplete
        crate::utils::spawn(async move {
            // Wait for StopRecording command
            while let Some(cmd) = cmd_rx.recv().await {
                if matches!(cmd, CallCommand::StopRecording) {
                    let _ = event_tx.send(ControllerEvent::Hangup(None));
                    break;
                }
            }
        });

        let result = timeout(Duration::from_secs(1), controller.stop_recording()).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("hung up"));
    }

    #[tokio::test]
    async fn test_stop_recording_ignores_other_events() {
        let (mut controller, event_tx, mut cmd_rx) = make_controller_with_channels();

        let event_tx_clone = event_tx.clone();
        crate::utils::spawn(async move {
            // Wait for StopRecording command
            while let Some(cmd) = cmd_rx.recv().await {
                if matches!(cmd, CallCommand::StopRecording) {
                    // Send some other events first (simulating concurrent events)
                    let _ = event_tx_clone.send(ControllerEvent::DtmfReceived("1".to_string()));
                    let _ = event_tx_clone.send(ControllerEvent::AudioComplete {
                        track_id: "caller".to_string(),
                        interrupted: false,
                    });
                    // Then send RecordingComplete
                    let _ =
                        event_tx_clone.send(ControllerEvent::RecordingComplete(RecordingInfo {
                            path: "/tmp/test2.wav".to_string(),
                            duration: Duration::from_secs(10),
                            size_bytes: 2048,
                        }));
                    break;
                }
            }
        });

        let result = timeout(Duration::from_secs(1), controller.stop_recording()).await;
        assert!(result.is_ok());
        let info = result.unwrap().unwrap();
        assert_eq!(info.path, "/tmp/test2.wav");
        assert_eq!(info.duration, Duration::from_secs(10));
        assert_eq!(info.size_bytes, 2048);
    }

    /// Contract guard: `play_audio_with_options` must always emit
    /// `await_completion: false`. Voicemail (and other apps) rely on
    /// event-driven sequencing via `on_audio_complete`, NOT on the session
    /// blocking on playback. If this ever flips to true, voicemail's
    /// greeting→beep→record chain would break (the session would block the
    /// event loop). This invariant must hold regardless of the `interruptible`
    /// argument.
    #[tokio::test]
    async fn play_audio_with_options_always_emits_await_completion_false() {
        let (controller, _event_tx, mut cmd_rx) = make_controller_with_channels();

        controller
            .play_audio_with_options("prompt.wav", None, false, true)
            .await
            .expect("play_audio_with_options should succeed");

        let cmd = timeout(Duration::from_secs(1), cmd_rx.recv())
            .await
            .expect("timed out waiting for Play command")
            .expect("cmd channel closed");
        match cmd {
            CallCommand::Play { options, .. } => {
                let opts = options.expect("Play must carry options");
                assert!(
                    !opts.await_completion,
                    "app-originated playback must never request await_completion \
                     (voicemail relies on event-driven sequencing)"
                );
            }
            other => panic!("expected CallCommand::Play, got {other:?}"),
        }
    }
}
