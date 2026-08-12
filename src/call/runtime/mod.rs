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
//! │   dispatch_command(session_id, command) -> Result<CommandResult>│
//! └────────────┬────────────────┴────────┬────────┴────────────────┘
//!              │                         │
//!              ▼                         ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │               Session Runtime                                    │
//! │   SipSession             ──► Direct command handling            │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

mod app_runtime;
mod command_dispatch;
mod command_executor;
mod conference_manager;
pub mod conference_media_bridge;
mod conference_server;
mod conference_strategy;
mod db_session_registry;
mod default_app_runtime;
mod media_path_strategy;
mod memory_session_registry;
mod session_runtime;
pub mod session_registry;
pub mod test_utils;

#[cfg(test)]
mod integration_tests;

pub use app_runtime::*;
pub use command_dispatch::*;
pub use command_executor::*;
pub use conference_manager::*;
pub use conference_media_bridge::*;
pub use conference_server::*;
pub use conference_strategy::*;
pub use db_session_registry::*;
pub use default_app_runtime::*;
pub use media_path_strategy::*;
pub use memory_session_registry::*;
pub use session_registry::*;
pub use session_runtime::*;
