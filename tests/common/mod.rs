//! Shared integration-test harness compiled into many `tests/*.rs` binaries.
//! Each binary only uses a subset of the API, so allow dead_code here rather
//! than duplicating helpers or splitting into a separate crate.
#![allow(dead_code)]

pub mod audio_mocks;
pub mod cdr_capture;
pub mod e2e_test_server;
pub mod rtp_utils;
pub mod test_helpers;
pub mod test_ua;
pub mod webhook_capture;
