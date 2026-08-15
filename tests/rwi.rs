//! Consolidated RWI WebSocket / gateway integration tests.
//!
//! One test binary instead of six, so `cargo test` links a single executable
//! for the whole RWI suite. Run a subset with:
//!   cargo test --test rwi -- server
//!   cargo test --test rwi -- comprehensive_event -- --nocapture

mod helpers;

#[path = "rwi/comprehensive_event.rs"]
mod comprehensive_event;
#[path = "rwi/integration.rs"]
mod integration;
#[path = "rwi/leg_timeline_tests.rs"]
mod leg_timeline_tests;
#[path = "rwi/resume_e2e.rs"]
mod resume_e2e;
#[path = "rwi/server.rs"]
mod server;

#[path = "rwi/wire_format.rs"]
mod wire_format;
