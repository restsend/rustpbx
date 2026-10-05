//! Stable entry surface for addon and downstream-app crates.
//!
//! External crates should depend on these items (plus the `pub mod` roots
//! `models`, `proxy`, `console`, `call`, `config`, `addons`) instead of
//! reaching into internal modules. The set of re-exports here is the
//! compatibility contract tracked from v0.90 onward; additive changes are
//! minor bumps, removals or signature changes are major.

pub use crate::addons::registry::AddonRegistry;
pub use crate::addons::{
    Addon, AddonCategory, AddonInfo, AuthAttempt, AuthAttemptOutcome, ScriptInjection,
    SidebarItem,
};
pub use crate::app::{AppState, AppStateBuilder};
pub use crate::builder::{AppBuilder, Cli, Commands};
pub use crate::config::Config;
pub use crate::proxy::call::{DialplanInspector, DialplanVerdict, RouteError};
pub use rustpbx_models as models;
