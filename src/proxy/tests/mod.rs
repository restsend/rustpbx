pub mod common;
#[cfg(test)]
mod locator_db_test;
#[cfg(test)]
mod test_acl;
#[cfg(test)]
mod test_auth;
#[cfg(test)]
mod test_proxy;
#[cfg(test)]
mod test_registrar;
#[cfg(test)]
mod user_db_test;
#[cfg(test)]
mod user_http_test;
// mod user_http_test;
// mod call_webrtc_sip_test;
#[cfg(test)]
mod test_b2bua_flow;
#[cfg(test)]
mod test_call;
#[cfg(test)]
mod test_cdr;
#[cfg(test)]
mod test_media_proxy;
#[cfg(test)]
mod test_presence;
#[cfg(test)]
mod test_proxy_integration;
#[cfg(test)]
mod test_queue;
#[cfg(test)]
mod test_reinvite;
pub mod test_ua;

// E2E testing infrastructure
#[cfg(test)]
pub mod rtp_utils;
#[cfg(test)]
pub mod cdr_capture;
#[cfg(test)]
pub mod e2e_test_server;
#[cfg(test)]
mod test_call_e2e;
#[cfg(test)]
mod test_rtp_e2e;
