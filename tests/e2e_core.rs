//! Aggregated E2E core suite — one test binary for all non-addon integration
//! suites (they used to be 12 separate binaries, each paying a full link of
//! the `rustpbx` crate; merging them cuts incremental link time ~4x and lets
//! the runner parallelize one process instead of twelve).
//!
//! Every former root file keeps its own `#[path]` submodule declarations and
//! is namespaced under a module named after the old suite, so test paths read
//! `e2e_core::proxy_e2e::test_call_e2e::...` and name filters
//! (`cargo test-dev --test e2e_core -- ringback_mode`) keep working.
//!
//! `mod common` / `mod helpers` are declared once here (the per-suite roots
//! had their own copies stripped) and are reachable as `crate::common` /
//! `crate::helpers` from the suite modules exactly as before.
//!
//! Addon suites live in `tests/e2e_addons.rs` (feature-gated); the OpenAPI
//! contract test stays its own binary (`cc_openapi_contract_test.rs`).

mod common;
mod helpers;

#[path = "call.rs"]
mod call;
#[path = "common_selftest.rs"]
mod common_selftest;
#[path = "ivr_e2e.rs"]
mod ivr_e2e;
#[path = "proxy_e2e.rs"]
mod proxy_e2e;
#[path = "proxy_flow.rs"]
mod proxy_flow;
#[path = "proxy_routing.rs"]
mod proxy_routing;
#[path = "proxy_rwi.rs"]
mod proxy_rwi;
#[path = "proxy_session.rs"]
mod proxy_session;
#[path = "proxy_trunk_b2bua.rs"]
mod proxy_trunk_b2bua;
#[path = "queue_e2e.rs"]
mod queue_e2e;
#[path = "realtime_bridge.rs"]
mod realtime_bridge;
#[path = "rwi.rs"]
mod rwi;
