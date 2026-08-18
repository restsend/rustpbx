pub mod common;
mod locator_db_test;
mod test_acl;
mod test_auth;
mod test_presence;
mod test_presence_e2e;
mod test_presence_subscription_leak;
mod test_proxy;
mod test_registrar;
pub mod test_ua;
mod user_db_test;
mod user_http_test;

pub mod test_helpers;

// E2E testing infrastructure
pub mod cdr_capture;
pub mod e2e_test_server;
mod rtp_packet_tests;
pub mod rtp_utils;
mod test_sip_session_regressions;
mod test_trunk_config_tests;
