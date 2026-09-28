//! Inbound REFER configuration. Real subscription/lifecycle coverage lives in proxy_e2e.

use crate::config::ProxyConfig;

#[test]
fn inbound_refer_in_session_defaults_enabled() {
    let config = ProxyConfig::default();
    assert!(
        config.inbound_refer_in_session,
        "blind inbound REFERs must execute in-session by default"
    );
}
