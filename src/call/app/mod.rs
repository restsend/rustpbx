//! # Call Application Framework
//!
//! A unified framework for building telephony applications (Voicemail, IVR,
//! Conference, Queue, Fax, etc.) on top of the RustPBX proxy call layer.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────┐
//! │ Application Layer                                    │
//! │  VoicemailApp │ IvrApp │ ConferenceApp │ CustomApp  │
//! │                    ↑ implements CallApp trait         │
//! └─────────────────────────────────────────────────────┘
//!                          ▲
//! ┌─────────────────────────────────────────────────────┐
//! │ Call Application Framework                           │
//! │  CallController ─ high-level call control API        │
//! │  ApplicationContext ─ shared resources                │
//! │  AppEventLoop ─ event dispatch loop                   │
//! └─────────────────────────────────────────────────────┘
//!                          ▲ sends SessionAction
//! ┌─────────────────────────────────────────────────────┐
//! │ Proxy Call Layer                                     │
//! │  CallSession │ MediaBridge │ FileTrack │ Recorder    │
//! └─────────────────────────────────────────────────────┘
//! ```
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use rustpbx::call::app::*;
//! use async_trait::async_trait;
//!
//! struct MyApp {
//!     state: MyState,
//! }
//!
//! #[async_trait]
//! impl CallApp for MyApp {
//!     fn app_type(&self) -> CallAppType { CallAppType::Custom }
//!     fn name(&self) -> &str { "my-app" }
//!
//!     async fn on_enter(
//!         &mut self,
//!         ctrl: &mut CallController,
//!         ctx: &ApplicationContext,
//!     ) -> anyhow::Result<AppAction> {
//!         ctrl.answer().await?;
//!         ctrl.play_audio("sounds/welcome.wav", false).await?;
//!         Ok(AppAction::Continue)
//!     }
//!
//!     async fn on_dtmf(
//!         &mut self, digit: String,
//!         ctrl: &mut CallController,
//!         ctx: &ApplicationContext,
//!     ) -> anyhow::Result<AppAction> {
//!         match digit.as_str() {
//!             "1" => Ok(AppAction::Transfer("sip:sales@local".to_string())),
//!             "#" => Ok(AppAction::Hangup { reason: None }),
//!             _   => Ok(AppAction::Continue),
//!         }
//!     }
//! }
//! ```
//!
//! ## Lifecycle
//!
//! 1. Call is routed to an application via `RouteResult::Application`
//! 2. `AppEventLoop` calls `on_enter()` — the app answers and starts its flow
//! 3. Events (DTMF, audio complete, recording complete, timeout) are dispatched
//!    to the corresponding `on_*` handler
//! 4. Each handler returns an `AppAction` directing the loop what to do next
//! 5. When `AppAction::Exit` or `AppAction::Hangup` is returned, `on_exit()` is called

use crate::callrecord::CallRecordHangupReason;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

mod app_context;
mod controller;
mod event_loop;

pub use app_context::{AppSharedState, ApplicationContext, CallInfo};
pub use controller::{
    CallController, ControllerEvent, DtmfCollectConfig, PlaybackHandle, RecordingHandle,
    RecordingInfo,
};
pub use event_loop::AppEventLoop;

// ─── CallApp Trait ──────────────────────────────────────────────────────────

/// The type of call application.
///
/// Used for logging, metrics, and routing decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallAppType {
    Voicemail,
    Ivr,
    Conference,
    Queue,
    Fax,
    Custom,
}

impl fmt::Display for CallAppType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallAppType::Voicemail => write!(f, "voicemail"),
            CallAppType::Ivr => write!(f, "ivr"),
            CallAppType::Conference => write!(f, "conference"),
            CallAppType::Queue => write!(f, "queue"),
            CallAppType::Fax => write!(f, "fax"),
            CallAppType::Custom => write!(f, "custom"),
        }
    }
}

/// Action returned by [`CallApp`] event handlers to control the event loop.
///
/// # Examples
///
/// ```rust,ignore
/// // Continue waiting for next event
/// Ok(AppAction::Continue)
///
/// // Exit the app cleanly, proceeding to next route stage
/// Ok(AppAction::Exit)
///
/// // Hang up the call
/// Ok(AppAction::Hangup { reason: None })
///
/// // Transfer to another destination
/// Ok(AppAction::Transfer("sip:1002@local".to_string()))
///
/// // Chain to another application (e.g., IVR → Voicemail)
/// Ok(AppAction::Chain(Box::new(VoicemailApp::new("1001"))))
/// ```
#[derive(Debug)]
pub enum AppAction {
    /// Continue the event loop, waiting for the next event.
    Continue,

    /// Exit the application gracefully.
    Exit,

    /// Transfer the call to the given SIP URI string.
    Transfer(String),

    /// Chain to another CallApp (the current app exits, the next starts).
    Chain(Box<dyn CallApp>),

    /// Hang up the call.
    Hangup {
        reason: Option<CallRecordHangupReason>,
    },

    /// Sleep for a duration, then re-enter the application.
    Sleep(Duration),
}

/// Reason for application exit, passed to [`CallApp::on_exit`].
#[derive(Debug, Clone)]
pub enum ExitReason {
    /// Normal completion — app returned `AppAction::Exit`.
    Normal,
    /// Local hangup — app returned `AppAction::Hangup`.
    Hangup,
    /// Remote party hung up.
    RemoteHangup(Option<CallRecordHangupReason>),
    /// Transferred to another destination.
    Transferred,
    /// Chained to another application.
    Chained,
    /// Cancelled by system (e.g., shutdown).
    Cancelled,
    /// Error during execution.
    Error(String),
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitReason::Normal => write!(f, "normal"),
            ExitReason::Hangup => write!(f, "hangup"),
            ExitReason::RemoteHangup(r) => write!(f, "remote_hangup({:?})", r),
            ExitReason::Transferred => write!(f, "transferred"),
            ExitReason::Chained => write!(f, "chained"),
            ExitReason::Cancelled => write!(f, "cancelled"),
            ExitReason::Error(e) => write!(f, "error: {}", e),
        }
    }
}

/// External events that can be delivered to a [`CallApp`].
///
/// These are events originating from outside the call session itself,
/// such as HTTP callbacks, timer events, or conference state changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppEvent {
    /// HTTP webhook response received
    HttpResponse { body: String },
    /// Conference participant joined
    ParticipantJoined { room_id: String, count: usize },
    /// Conference participant left
    ParticipantLeft { room_id: String, count: usize },
    /// Conference ended by moderator
    ConferenceEnded { room_id: String },
    /// Custom event with arbitrary JSON data
    Custom {
        name: String,
        data: serde_json::Value,
    },
}

/// The core trait that all call applications must implement.
///
/// Each method receives a mutable reference to [`CallController`] for
/// performing call-control operations, and an [`ApplicationContext`] for
/// accessing shared resources (database, storage, config).
///
/// # Event Handlers
///
/// | Method | Triggered When |
/// |--------|---------------|
/// | `on_enter` | Call is routed to this app |
/// | `on_dtmf` | DTMF digit received from caller |
/// | `on_audio_complete` | Audio playback finished |
/// | `on_record_complete` | Recording finished |
/// | `on_external_event` | External event (HTTP, conference, custom) |
/// | `on_timeout` | A named timeout expired |
/// | `on_exit` | App is exiting (cleanup) |
///
/// # Example: Simple Greeting App
///
/// ```rust,ignore
/// struct GreetingApp;
///
/// #[async_trait]
/// impl CallApp for GreetingApp {
///     fn app_type(&self) -> CallAppType { CallAppType::Custom }
///     fn name(&self) -> &str { "greeting" }
///
///     async fn on_enter(
///         &mut self, ctrl: &mut CallController, _ctx: &ApplicationContext,
///     ) -> anyhow::Result<AppAction> {
///         ctrl.answer().await?;
///         ctrl.play_audio("sounds/hello.wav", false).await?;
///         Ok(AppAction::Continue)
///     }
///
///     async fn on_audio_complete(
///         &mut self, _track_id: String,
///         _ctrl: &mut CallController, _ctx: &ApplicationContext,
///     ) -> anyhow::Result<AppAction> {
///         Ok(AppAction::Hangup { reason: None })
///     }
/// }
/// ```
#[async_trait]
pub trait CallApp: Send + Sync {
    /// Returns the application type identifier.
    fn app_type(&self) -> CallAppType;

    /// Returns the application name (used in logs and metrics).
    fn name(&self) -> &str;

    /// Called when the call is first routed to this application.
    ///
    /// Typical actions: answer the call, play a greeting, start recording.
    async fn on_enter(
        &mut self,
        controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction>;

    /// Called when a DTMF digit is received from the caller.
    ///
    /// The `digit` parameter is a single character: `0`-`9`, `*`, `#`, `A`-`D`.
    async fn on_dtmf(
        &mut self,
        digit: String,
        controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let _ = (digit, controller, context);
        Ok(AppAction::Continue)
    }

    /// Called when an audio playback completes.
    async fn on_audio_complete(
        &mut self,
        track_id: String,
        controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let _ = (track_id, controller, context);
        Ok(AppAction::Continue)
    }

    /// Called when a recording completes (by max duration, silence, or DTMF stop).
    async fn on_record_complete(
        &mut self,
        info: RecordingInfo,
        controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let _ = (info, controller, context);
        Ok(AppAction::Continue)
    }

    /// Called when an external event is received (HTTP callback, conference event, etc.).
    async fn on_external_event(
        &mut self,
        event: AppEvent,
        controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let _ = (event, controller, context);
        Ok(AppAction::Continue)
    }

    /// Called when a named timeout expires.
    async fn on_timeout(
        &mut self,
        timeout_id: String,
        controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let _ = (timeout_id, controller, context);
        Ok(AppAction::Continue)
    }

    /// Called when the application exits. Use for cleanup.
    async fn on_exit(&mut self, reason: ExitReason) -> anyhow::Result<()> {
        let _ = reason;
        Ok(())
    }
}

impl fmt::Debug for dyn CallApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallApp")
            .field("type", &self.app_type())
            .field("name", &self.name())
            .finish()
    }
}
