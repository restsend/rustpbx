//! Test harness for [`CallApp`] implementations.
//!
//! Provides [`MockCallStack`]: a fully assembled in-memory call stack that
//! runs a `CallApp` without any real SIP session, media, or database.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use crate::call::app::testing::MockCallStack;
//! use crate::proxy::proxy_call::state::SessionAction;
//!
//! let mut stack = MockCallStack::run(Box::new(MyApp::new()), "1001", "1002");
//!
//! // App answers on enter → assert AcceptCall
//! stack.assert_cmd(50, "answer", |c| matches!(c, SessionAction::AcceptCall { .. })).await;
//!
//! // Simulate audio playback finishing
//! stack.audio_complete("default");
//!
//! // App should now hang up
//! stack.assert_cmd(50, "hangup", |c| matches!(c, SessionAction::Hangup { .. })).await;
//! ```

use super::{AppEventLoop, CallApp, CallController, ControllerEvent, RecordingInfo};
use crate::call::app::{ApplicationContext, CallInfo};
use crate::call::DialDirection;
use crate::config::Config;
use crate::proxy::proxy_call::state::{CallSessionHandle, CallSessionShared, SessionAction};
use chrono::Utc;
use sea_orm::DatabaseConnection;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Fully assembled, in-memory [`CallApp`] harness — no SIP stack required.
///
/// The harness wires up:
/// - A real [`CallSessionHandle`] backed by an in-memory channel (`cmd_rx`).
/// - A synthetic event channel (`event_tx`) for injecting DTMF, audio events, etc.
/// - A real [`AppEventLoop`] running the app in a background Tokio task.
pub struct MockCallStack {
    /// Inject SIP-originated events into the running app.
    event_tx: tokio::sync::mpsc::UnboundedSender<ControllerEvent>,
    /// Observe [`SessionAction`]s emitted by the app toward the SIP layer.
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<SessionAction>,
    /// Cancel token wired to the AppEventLoop's child token.
    cancel: CancellationToken,
    /// Background task running the AppEventLoop.
    join_handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl MockCallStack {
    /// Build the call stack and start the app in a background task.
    ///
    /// # Arguments
    /// * `app`    – The `CallApp` under test.
    /// * `caller` – Simulated caller-ID (e.g. `"1001"` or `"sip:alice@test"`).
    /// * `callee` – Simulated callee-ID (e.g. `"1002"`).
    pub fn run(app: Box<dyn CallApp>, caller: &str, callee: &str) -> Self {
        // Real CallSessionHandle — backed only by channels, no SIP socket.
        let shared = CallSessionShared::new(
            "test-session".to_string(),
            DialDirection::Inbound,
            Some(caller.to_string()),
            Some(callee.to_string()),
            None,
        );
        let (handle, cmd_rx) = CallSessionHandle::with_shared(shared);

        // Synthetic event channel: test → controller.
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<ControllerEvent>();

        // Controller + companion timer-fire channel for AppEventLoop.
        let (controller, timer_rx) = CallController::new(handle, event_rx);

        // Minimal ApplicationContext (no DB, temp-dir storage).
        let storage = crate::storage::Storage::new(&crate::storage::StorageConfig::Local {
            path: std::env::temp_dir()
                .join("rustpbx-mock-test")
                .to_string_lossy()
                .to_string(),
        })
        .expect("temp storage for mock");

        let ctx = ApplicationContext::new(
            DatabaseConnection::Disconnected,
            CallInfo {
                session_id: "test-session".into(),
                caller: caller.into(),
                callee: callee.into(),
                direction: "inbound".into(),
                started_at: Utc::now(),
            },
            Arc::new(Config::default()),
            storage,
        );

        let cancel = CancellationToken::new();
        let event_loop =
            AppEventLoop::new(app, controller, ctx, cancel.child_token(), timer_rx);

        let join_handle = tokio::spawn(event_loop.run());

        Self { event_tx, cmd_rx, cancel, join_handle }
    }

    // ── Event injection ───────────────────────────────────────────────────────

    /// Inject a DTMF digit received from the remote party.
    pub fn dtmf(&self, digit: impl Into<String>) -> &Self {
        let _ = self.event_tx.send(ControllerEvent::DtmfReceived(digit.into()));
        self
    }

    /// Inject an audio-playback-complete event.
    pub fn audio_complete(&self, track_id: impl Into<String>) -> &Self {
        let _ = self.event_tx.send(ControllerEvent::AudioComplete {
            track_id: track_id.into(),
            interrupted: false,
        });
        self
    }

    /// Inject an interrupted audio event (e.g. barge-in).
    pub fn audio_interrupted(&self, track_id: impl Into<String>) -> &Self {
        let _ = self.event_tx.send(ControllerEvent::AudioComplete {
            track_id: track_id.into(),
            interrupted: true,
        });
        self
    }

    /// Inject a recording-complete event.
    pub fn record_complete(
        &self,
        path: impl Into<String>,
        duration: Duration,
        size_bytes: u64,
    ) -> &Self {
        let _ = self.event_tx.send(ControllerEvent::RecordingComplete(RecordingInfo {
            path: path.into(),
            duration,
            size_bytes,
        }));
        self
    }

    /// Inject a remote-hangup event (no specific reason).
    pub fn remote_hangup(&self) -> &Self {
        let _ = self.event_tx.send(ControllerEvent::Hangup(None));
        self
    }

    /// Inject a custom event.
    pub fn custom(&self, name: impl Into<String>, data: serde_json::Value) -> &Self {
        let _ = self.event_tx.send(ControllerEvent::Custom(name.into(), data));
        self
    }

    // ── Command observation ───────────────────────────────────────────────────

    /// Wait up to `timeout_ms` milliseconds for the next [`SessionAction`] sent by the app.
    ///
    /// Returns `None` on timeout.
    pub async fn next_cmd(&mut self, timeout_ms: u64) -> Option<SessionAction> {
        tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            self.cmd_rx.recv(),
        )
        .await
        .ok()
        .flatten()
    }

    /// Assert the next command satisfies `matcher` within `timeout_ms` ms.
    ///
    /// # Panics
    /// If the timeout elapses or the matcher returns `false`.
    pub async fn assert_cmd<F>(&mut self, timeout_ms: u64, label: &str, matcher: F)
    where
        F: FnOnce(&SessionAction) -> bool,
    {
        let cmd = self
            .next_cmd(timeout_ms)
            .await
            .unwrap_or_else(|| panic!("timed out waiting for command '{label}'"));
        assert!(matcher(&cmd), "command mismatch for '{label}': got {cmd:?}");
    }

    /// Drain all immediately-available commands without blocking.
    pub fn drain_cmds(&mut self) -> Vec<SessionAction> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    /// Cancel the running event loop (simulate system shutdown).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Wait for the event loop task to complete and surface any error.
    pub async fn join(self) -> anyhow::Result<()> {
        self.join_handle.await.expect("MockCallStack task panicked")
    }
}
