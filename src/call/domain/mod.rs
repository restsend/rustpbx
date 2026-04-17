//! # Unified Session Domain Types
//!
//! This module defines the core domain types for the unified session architecture.
//! These types are designed to be protocol-agnostic and can be adapted from
//! various sources (RWI, Console/HTTP, internal events).
//!
//! ## Design Principles
//!
//! 1. **Domain Model Unification**: Single source of truth for session state
//! 2. **Protocol Compatibility**: Adapters translate external protocols to domain types
//! 3. **Explicit State Machine**: State transitions are derivable and verifiable
//! 4. **Media Path Awareness**: Native support for both anchored and bypass media paths

mod command;
mod hangup;
mod leg;
mod policy;
mod state;
mod transfer_event;

pub use command::*;
pub use hangup::*;
pub use leg::*;
pub use policy::*;
pub use state::*;
pub use transfer_event::*;
