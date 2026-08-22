//! Single home for the shared `tests/common` harness self-tests.
//!
//! The `mod common;` include used to drag each module's embedded
//! `#[cfg(test)] mod tests` into every aggregator binary, re-running the
//! same ~34 self-tests (24+ real-SIP UA tests among them, ~23s each pass)
//! once per binary. They now live only here.

mod common;

#[path = "common_selftest/rtp_utils_tests.rs"]
mod rtp_utils_tests;

#[path = "common_selftest/cdr_capture_tests.rs"]
mod cdr_capture_tests;

#[path = "common_selftest/e2e_server_tests.rs"]
mod e2e_server_tests;

#[path = "common_selftest/test_ua_tests.rs"]
mod test_ua_tests;
