pub mod common;
mod locator_db_test;
mod test_acl;
mod test_auth;
mod test_b2bua_flow;
mod test_call;
mod test_cdr;
mod test_media_proxy;
mod test_presence;
mod test_proxy;
mod test_proxy_integration;
mod test_queue;
mod test_registrar;
mod test_reinvite;
pub mod test_ua;
mod user_db_test;
mod user_http_test;

// E2E testing infrastructure
pub mod cdr_capture;
pub mod e2e_test_server;
pub mod rtp_utils;
mod test_call_e2e;
mod test_ivr_queue_e2e;
mod test_media_e2e;
mod test_rtp_e2e;
mod test_trunk_e2e;
mod test_wholesale_e2e;
