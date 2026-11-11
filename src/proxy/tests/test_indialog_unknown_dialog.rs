//! Regression tests for in-dialog requests whose dialog no longer exists on
//! this proxy.
//!
//! Production incident (xwork_1, 2026-10-30 09:43 CST): the AI side CANCELled
//! a refer dispatch 1.3s after ringing; the agent's SDK raced the CANCEL and
//! answered afterwards, leaving a zombie session. The zombie's periodic
//! session-refresh UPDATE hit the proxy where the dialog was already gone —
//! and we silently dropped it (`final_status=None`), so the peer's Timer F ran
//! the full 32s before "Request Timeout" fired. That timeout landed while the
//! agent was already ~40s into his NEXT call, and the dispatch engine misread
//! the client-side "ended" as "agent idle", double-booking the agent.
//!
//! RFC 3261 §15.1.2 / §12.2: a request for a dialog that does not exist must
//! be answered 481 (Dialog/Transaction Does Not Exist) so the peer terminates
//! its transaction immediately instead of hanging for Timer F.
//!
//! Ported from the 0.1.10 hotfix line (3ec9fce7, deployed as zapbx
//! refer-fix-1110-v3 / viafix-1110-v1). On this line BYE on an unknown dialog
//! already replies 481 ("so peer clears"), so the 0.1.10 line's
//! `bye_..._still_gets_200` guard is adapted to expect 481 here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::UdpSocket;
use tokio::time::sleep;

use super::e2e_test_server::E2eTestServer;
use super::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::config::MediaProxyMode;

async fn create_ua(
    username: &str,
    password: &str,
    proxy_addr: SocketAddr,
    port: u16,
) -> Result<TestUa> {
    let config = TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: password.to_string(),
        realm: proxy_addr.ip().to_string(),
        local_port: port,
        proxy_addr,
        extra_invite_headers: Vec::new(),
    };
    let mut ua = TestUa::new(config);
    ua.start().await?;
    Ok(ua)
}

/// Drain the UA's dialog events until an IncomingCall arrives (or timeout).
async fn next_incoming(
    ua: &mut TestUa,
    timeout_ms: u64,
) -> Option<rsipstack::dialog::DialogId> {
    for _ in 0..(timeout_ms / 25) {
        let events = ua.process_dialog_events().await.ok()?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                return Some(id);
            }
        }
        sleep(Duration::from_millis(25)).await;
    }
    None
}

/// Build an in-dialog request with tags that (may) match no dialog.
fn build_indialog_request(
    server_addr: SocketAddr,
    method: &str,
    call_id: &str,
    from_tag: &str,
    to_tag: &str,
    cseq: u32,
) -> String {
    format!(
        "{method} sip:peer@{host}:{port} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:50999;branch=z9hG4bKzombie{cseq}\r\n\
         Max-Forwards: 70\r\n\
         Call-ID: {call_id}\r\n\
         From: <sip:zombie@127.0.0.1>;tag={from_tag}\r\n\
         To: <sip:peer@127.0.0.1>;tag={to_tag}\r\n\
         CSeq: {cseq} {method}\r\n\
         Content-Length: 0\r\n\
         \r\n",
        method = method,
        host = server_addr.ip(),
        port = server_addr.port(),
        call_id = call_id,
        from_tag = from_tag,
        to_tag = to_tag,
        cseq = cseq,
    )
}

/// Send a raw SIP datagram and wait for the first final (>=200) response.
/// Returns None on timeout (i.e. the request was silently dropped).
async fn send_and_collect(
    server_addr: SocketAddr,
    msg: &str,
    timeout: Duration,
) -> Option<String> {
    let sock = UdpSocket::bind("127.0.0.1:0").await.ok()?;
    sock.send_to(msg.as_bytes(), server_addr).await.ok()?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 65535];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let first = text.split("\r\n").next().unwrap_or("").to_string();
                // Provisional responses (1xx) are not final; keep waiting.
                if first.starts_with("SIP/2.0 1") {
                    continue;
                }
                return Some(text);
            }
            Ok(Err(_)) | Err(_) => return None,
        }
    }
}

fn first_line(resp: &str) -> String {
    resp.split("\r\n").next().unwrap_or("").to_string()
}

/// THE FIX: an in-dialog UPDATE for a dialog we no longer have must be
/// answered 481 immediately, so the peer's transaction does not hang for
/// Timer F (32s) and fire "Request Timeout" mid-way through the agent's
/// next call (production incident 2026-10-30).
#[tokio::test]
async fn update_on_unknown_dialog_gets_481() {
    let proxy = E2eTestServer::start_with_mode(MediaProxyMode::None)
        .await
        .unwrap();

    let msg = build_indialog_request(
        proxy.proxy_addr,
        "UPDATE",
        "zombie-update@test",
        "zombieFromTag",
        "zombieToTag",
        2,
    );
    let resp = send_and_collect(proxy.proxy_addr, &msg, Duration::from_secs(3)).await;

    let resp = resp.unwrap_or_else(|| {
        panic!(
            "UPDATE on unknown dialog got NO response within 3s \
             (pre-fix silent-drop behavior: peer Timer F would hang 32s)"
        )
    });
    let line = first_line(&resp);
    assert!(
        line.starts_with("SIP/2.0 481"),
        "expected 481 Call/Transaction Does Not Exist, got: {}",
        line
    );
    proxy.stop();
}

/// Regression guard: BYE on unknown dialog replies 481 ("so peer clears").
#[tokio::test]
async fn bye_on_unknown_dialog_gets_481() {
    let proxy = E2eTestServer::start_with_mode(MediaProxyMode::None)
        .await
        .unwrap();

    let msg = build_indialog_request(
        proxy.proxy_addr,
        "BYE",
        "zombie-bye@test",
        "byeFromTag",
        "byeToTag",
        2,
    );
    let resp = send_and_collect(proxy.proxy_addr, &msg, Duration::from_secs(3)).await;

    let resp = resp
        .unwrap_or_else(|| panic!("BYE on unknown dialog got NO response"));
    let line = first_line(&resp);
    assert!(
        line.starts_with("SIP/2.0 481"),
        "expected 481 for BYE on unknown dialog, got: {}",
        line
    );
    proxy.stop();
}

/// Regression guard: REFER on unknown dialog already replied 481 — keep it.
#[tokio::test]
async fn refer_on_unknown_dialog_gets_481() {
    let proxy = E2eTestServer::start_with_mode(MediaProxyMode::None)
        .await
        .unwrap();

    let msg = build_indialog_request(
        proxy.proxy_addr,
        "REFER",
        "zombie-refer@test",
        "referFromTag",
        "referToTag",
        2,
    );
    let resp = send_and_collect(proxy.proxy_addr, &msg, Duration::from_secs(3)).await;

    let resp = resp
        .unwrap_or_else(|| panic!("REFER on unknown dialog got NO response"));
    let line = first_line(&resp);
    assert!(
        line.starts_with("SIP/2.0 481"),
        "expected 481 for REFER on unknown dialog, got: {}",
        line
    );
    proxy.stop();
}

/// Realistic shape of the production incident: a call is established and
/// torn down (dialog entry removed from the proxy's table), then the zombie
/// peer sends a session-refresh UPDATE carrying the real Call-ID. The proxy
/// must answer 481 instead of dropping it.
#[tokio::test]
async fn zombie_update_on_terminated_call_gets_481() {
    let proxy = E2eTestServer::start_with_mode(MediaProxyMode::None)
        .await
        .unwrap();
    let proxy_addr = proxy.proxy_addr;

    let caller_port = portpicker::pick_unused_port().unwrap_or(25130);
    let caller = Arc::new(
        create_ua("alice", "password123", proxy_addr, caller_port)
            .await
            .unwrap(),
    );
    let agent_port = portpicker::pick_unused_port().unwrap_or(25131);
    let mut agent = create_ua("bob", "password456", proxy_addr, agent_port)
        .await
        .unwrap();

    caller.register().await.unwrap();
    agent.register().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let caller_handle = crate::utils::spawn({
        let caller = caller.clone();
        async move {
            let offer = super::test_ua::create_test_sdp("192.168.1.100", 5004, true);
            caller.make_call("bob", Some(offer)).await
        }
    });

    let incoming_id = next_incoming(&mut agent, 2000)
        .await
        .expect("agent should receive IncomingCall");
    agent
        .answer_call(
            &incoming_id,
            Some(super::test_ua::create_test_sdp(
                "192.168.1.200",
                5006,
                true,
            )),
        )
        .await
        .ok();
    sleep(Duration::from_millis(200)).await;

    let dialog_id = caller_handle
        .await
        .expect("make_call join")
        .expect("make_call ok");

    // Tear the call down: the proxy's dialog entry is removed.
    caller.hangup(&dialog_id).await.ok();
    sleep(Duration::from_millis(300)).await;

    // The zombie sends a session-refresh UPDATE on the terminated dialog.
    let msg = build_indialog_request(
        proxy_addr,
        "UPDATE",
        &dialog_id.call_id,
        "lateFromTag",
        "lateToTag",
        2,
    );
    let resp = send_and_collect(proxy_addr, &msg, Duration::from_secs(3)).await;

    let resp = resp.unwrap_or_else(|| {
        panic!(
            "zombie UPDATE on terminated call got NO response within 3s \
             (pre-fix silent-drop behavior)"
        )
    });
    let line = first_line(&resp);
    assert!(
        line.starts_with("SIP/2.0 481"),
        "expected 481 for zombie UPDATE on terminated call, got: {}",
        line
    );

    caller.stop();
    agent.stop();
    proxy.stop();
}
