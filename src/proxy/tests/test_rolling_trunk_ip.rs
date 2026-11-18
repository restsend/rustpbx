//! Rolling UDP ingress must expose the REAL datagram source, so inbound
//! trunks configured by IP are recognised without digest auth.
//!
//! Regression for the 2026-11 production symptom: with rolling enabled the
//! successor's UDP ingress was a single `ChannelConnection` addressed with the
//! LOCAL bind address, so `tx.connection.get_remote_addr()` was `0.0.0.0:PORT`.
//! Trunk-by-IP resolution never matched, the ACL layer fell through to its
//! rules, and the auth module answered trunk INVITEs with endless 407s.

use crate::config::{ProxyConfig, RollingConfig};
use crate::log_capture::LogCapture;
use crate::proxy::acl::AclModule;
use crate::proxy::locator::MemoryLocator;
use crate::proxy::routing::{TrunkConfig, TrunkDirection};
use crate::proxy::server::SipServerBuilder;
use crate::proxy::user::MemoryUserBackend;
use crate::rolling::RollingRuntime;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe tcp port")
        .local_addr()
        .expect("probe addr")
        .port()
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("probe udp port")
        .local_addr()
        .expect("probe addr")
        .port()
}

/// Trunk-by-IP must be evaluated against the sender's address on the rolling
/// UDP transport. The ACL explicitly DENIES 127.0.0.1, so only the trunk
/// bypass can produce the "IP is from trunk" decision — a local-address
/// connection (the bug) would fall through to `deny all` and never match.
#[tokio::test]
async fn rolling_udp_ingress_matches_inbound_trunk_by_ip() {
    let (logs, _log_guard, _interest) = LogCapture::start_filtered("rustpbx::proxy::acl=debug");

    let sip_port = free_udp_port();
    let mut config = ProxyConfig {
        // Wildcard bind, like a container deployment: the old channel
        // transport reported exactly this local address as the peer.
        addr: "0.0.0.0".to_string(),
        udp_port: Some(sip_port),
        modules: Some(vec!["acl".to_string()]),
        acl_rules: Some(vec!["deny 127.0.0.1".to_string(), "deny all".to_string()]),
        ..Default::default()
    };
    config.trunks.insert(
        "inbound-rolling".to_string(),
        TrunkConfig {
            direction: Some(TrunkDirection::Inbound),
            inbound_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        },
    );
    let config = Arc::new(config);

    let rolling = RollingRuntime::new(
        RollingConfig {
            enabled: true,
            predecessor_port: free_tcp_port(),
            ..Default::default()
        },
        &format!("127.0.0.1:{}", free_tcp_port()),
    );

    let token = CancellationToken::new();
    let server = Arc::new(
        SipServerBuilder::new(config)
            .with_cancel_token(token.clone())
            .with_rolling(Some(rolling.clone()))
            .with_user_backend(Box::new(MemoryUserBackend::new(None)))
            .with_locator(Box::new(MemoryLocator::new()))
            .register_module("acl", AclModule::create)
            .build()
            .await
            .expect("build rolling sip server"),
    );
    rolling.attach_sip_server(server.inner.clone());
    let serving = tokio::spawn({
        let server = server.clone();
        async move {
            server.serve().await.ok();
        }
    });

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("client socket");
    let client_addr = client.local_addr().expect("client addr");
    let invite = format!(
        "INVITE sip:100@127.0.0.1 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {client_addr};rport;branch=z9hG4bK-rolling-trunk-ip\r\n\
         From: <sip:pbx@{client_addr}>;tag=trunk-tag\r\n\
         To: <sip:100@127.0.0.1>\r\n\
         Call-ID: rolling-trunk-ip@127.0.0.1\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:pbx@{client_addr}>\r\n\
         Max-Forwards: 70\r\n\
         Content-Length: 0\r\n\
         \r\n"
    );
    client
        .send_to(invite.as_bytes(), ("127.0.0.1", sip_port))
        .await
        .expect("send invite");

    let outcome = super::wait::wait_until(Duration::from_secs(5), || {
        let text = logs.text();
        async move {
            text.contains("IP is from trunk, bypassing acl check")
                && text.contains("source_ip=127.0.0.1")
        }
    })
    .await;
    token.cancel();
    serving.abort();
    outcome.expect(
        "rolling UDP ingress must resolve the real source IP and match the \
         inbound trunk (got the local bind address instead)",
    );
}
