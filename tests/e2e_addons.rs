//! Aggregated addon E2E suite (feature-gated).
//!
//! `cargo test-dev` (commerce,wholesale,contact-center) covers both modules;
//! with a feature off, its module compiles out entirely and the binary runs
//! zero tests for it.

mod common;

#[cfg(feature = "addon-cc")]
#[path = "cc_e2e.rs"]
mod cc_e2e;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale.rs"]
mod wholesale;

// The wholesale suite files reach their shared helpers via
// `crate::wholesale_helpers` (that is where the old root binary declared
// them); re-export under the same name now that the suite is namespaced.
#[cfg(feature = "addon-wholesale")]
pub(crate) use wholesale::wholesale_helpers;
