// tests/helpers/mod.rs
// Shared helpers for integration and E2E tests.
//
// Each integration test target compiles as its own crate, so a helper used
// by *some* tests but not all shows up as `dead_code` for the tests that
// don't touch it. The helpers below form a shared utility API intentionally
// exposed for any test to pick from; we silence the lint crate-wide here
// instead of littering individual items with attributes.
#![allow(dead_code)]

pub mod audio_verifier;
pub mod cdr_verifier;
pub mod rwi_collector;
pub mod sipbot_helper;
pub mod test_server;
