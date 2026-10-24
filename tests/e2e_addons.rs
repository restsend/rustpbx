//! Aggregated addon E2E suite (feature-gated).
//!
//! The CC suite moved into the addon itself (`src/addons/cc/tests/e2e`,
//! parent `[[test]] name = "cc_e2e"`); this binary now covers wholesale.
//! With a feature off, its module compiles out entirely and the binary runs
//! zero tests for it.

mod common;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale.rs"]
mod wholesale;

// The wholesale suite files reach their shared helpers via
// `crate::wholesale_helpers` (that is where the old root binary declared
// them); re-export under the same name now that the suite is namespaced.
#[cfg(feature = "addon-wholesale")]
pub(crate) use wholesale::wholesale_helpers;
