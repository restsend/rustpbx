pub mod common;
#[cfg(test)]
mod locator_db_test;
#[cfg(test)]
mod test_acl;
#[cfg(test)]
mod test_auth;
#[cfg(test)]
mod test_inbound_refer;
#[cfg(test)]
mod test_presence;
#[cfg(test)]
mod test_presence_e2e;
#[cfg(test)]
mod test_presence_subscription_leak;
#[cfg(test)]
mod test_proxy;
#[cfg(test)]
mod test_registrar;
pub mod test_ua;
#[cfg(test)]
mod user_db_test;
#[cfg(test)]
mod user_http_test;

pub mod test_helpers;

// E2E testing infrastructure
pub mod cdr_capture;
pub mod e2e_test_server;
#[cfg(test)]
mod rtp_packet_tests;
pub mod rtp_utils;
#[cfg(test)]
pub(crate) mod test_sip_session_regressions;
#[cfg(test)]
mod test_trunk_config_tests;
