//! Consolidated call / media / SIP integration tests.
//!
//! One test binary instead of several, so `cargo test` links a single
//! executable for the call-oriented suites. Run a subset with:
//!   cargo test --test call -- ringback_mode
//!   cargo test --test call -- media_task_leak -- --nocapture

mod helpers;

#[path = "call/audio_feature.rs"]
mod audio_feature;
#[path = "call/mcu_three_way.rs"]
mod mcu_three_way;
#[path = "call/media_task_leak.rs"]
mod media_task_leak;
#[path = "call/outbound_e2e.rs"]
mod outbound_e2e;
#[path = "call/ringback_mode.rs"]
mod ringback_mode;
#[path = "call/step_provider_retry_config.rs"]
mod step_provider_retry_config;
#[cfg(feature = "addon-sbc")]
#[path = "call/trunk_health_e2e.rs"]
mod trunk_health_e2e;
