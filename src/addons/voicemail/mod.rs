//! Voicemail addon stub.
//!
//! The full Voicemail Pro implementation lives in the `rustpbx-addons` repository
//! and is compiled into the main crate via symlink at `src/voicemail/`.
//!
//! This module re-exports from `crate::voicemail` for use by the addon registry.

// Re-export VoicemailAddon from the main voicemail module
pub use crate::voicemail::VoicemailAddon;
