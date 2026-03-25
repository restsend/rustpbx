//! # Unified Session Runtime
//!
//! This module provides the unified runtime for session control.
//! It serves as the execution layer between the command adapters and the
//! underlying session implementation.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                     Command Sources                              │
//! │   RWI (WebSocket)  │  Console (HTTP)  │  Internal Events        │
//! └────────────┬────────────────┴────────┬────────┴────────────────┘
//!              │                         │
//!              ▼                         ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                     Adapters                                     │
//! │   rwi_adapter      │  console_adapter  │  (internal)            │
//! │   RwiCommandPayload│  CallCommandPayload│                       │
//! │        ───────►    │     ───────►      │                        │
//! │      CallCommand   │    CallCommand    │                        │
//! └────────────┬────────────────────────┴────────┬────────┴────────────────┘
//!              │                         │
//!              ▼                         ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                  CommandExecutor                                 │
//! │   execute(session_id, command) -> Result<CommandResult>         │
//! └────────────┬────────────────┴────────┬────────┴────────────────┘
//!              │                         │
//!              ▼                         ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │               Session Runtime                                    │
//! │   SessionActionExecutor ──► CallSession::apply_session_action   │
//! │   SipSession             ──► Direct command handling            │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

mod app_runtime;
mod command_dispatch;
mod command_executor;
mod conference_manager;
mod default_app_runtime;
mod queue_manager;
mod registry_runtime;
mod session_action_executor;
mod session_runtime;

#[cfg(test)]
mod app_runtime_contract_tests;

#[cfg(test)]
mod integration_tests;

pub use app_runtime::*;
pub use command_dispatch::*;
pub use command_executor::*;
pub use conference_manager::*;
pub use default_app_runtime::*;
pub use queue_manager::*;
pub use registry_runtime::*;
pub use session_action_executor::*;
pub use session_runtime::*;
