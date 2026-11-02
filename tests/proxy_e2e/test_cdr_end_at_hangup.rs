//! The CDR ends when the call is hung up, not when teardown finishes.
//!
//! Reproduces a production report: the callee hung up 0.2 s after answering,
//! before the caller's ACK, and the caller only ACKed and answered the BYE
//! ~15 s later. The session (and so the CDR, stamped at report time) stayed
//! open until that BYE completed, billing 16 s for a 0.2 s call. Here the
//! caller is a raw UDP socket that withholds its ACK and its BYE reply.

use crate::common::cdr_capture::CdrCapture;
use crate::common::test_helpers;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::common::wait::wait_for_value;
use anyhow::{Result, anyhow};
use rustpbx::config::MediaProxyMode;
use rustpbx::proxy::routing::TrunkConfig;
use rustpbx::proxy::{
    acl::AclModule, auth::AuthModule, call::CallModule, locator::MemoryLocator,
    registrar::RegistrarModule, server::SipServerBuilder, user::MemoryUserBackend,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How long the caller sits on its ACK and on the BYE reply.
const CALLER_DELAY: Duration = Duration::from_secs(2);

struct Harness {
    proxy_addr: SocketAddr,
    caller: UdpSocket,
    cdr: CdrCapture,
    cancel: CancellationToken,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Harness {
    async fn start() -> Result<Self> {
        let caller = UdpSocket::bind("127.0.0.1:0").await?;
        let caller_port = caller.local_addr()?.port();
        let port = portpicker::pick_unused_port().expect("free port");
        let mut config = test_helpers::test_proxy_config(port);
        config.modules = Some(vec![
            "acl".to_string(),
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]);
        config.media_proxy = MediaProxyMode::None;
        config.ensure_user = Some(false);
        config.trunks.insert(
            "carrier".to_string(),
            TrunkConfig {
                dest: format!("sip:127.0.0.1:{caller_port}"),
                inbound_hosts: vec!["127.0.0.1".to_string()],
                ..Default::default()
            },
        );

        let user_backend = MemoryUserBackend::new(None);
        for user in test_helpers::standard_test_users() {
            user_backend.create_user(user).await?;
        }
        let (cdr, cdr_sender) = CdrCapture::new();
        let cancel = CancellationToken::new();
        let server = SipServerBuilder::new(Arc::new(config))
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(MemoryLocator::new()))
            .with_callrecord_sender(Some(cdr_sender))
            .with_cancel_token(cancel.clone())
            .register_module("acl", AclModule::create)
            .register_module("registrar", |inner, config| {
                Ok(Box::new(RegistrarModule::new(inner, config)))
            })
            .register_module("auth", |inner, _config| {
                Ok(Box::new(AuthModule::new(
                    inner.clone(),
                    inner.proxy_config.load_full(),
                )))
            })
            .register_module("call", |inner, config| {
                Ok(Box::new(CallModule::new(config, inner)))
            })
            .build()
            .await?;
        let token = cancel.clone();
        rustpbx::utils::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {}
                _ = server.serve() => {}
            }
        });

        Ok(Self {
            proxy_addr: format!("127.0.0.1:{port}").parse()?,
            caller,
            cdr,
            cancel,
        })
    }

    async fn ua(&self, username: &str, password: &str) -> Result<Arc<TestUa>> {
        let mut ua = TestUa::new(TestUaConfig {
            webrtc: false,
            username: username.to_string(),
            password: password.to_string(),
            realm: "127.0.0.1".to_string(),
            local_port: portpicker::pick_unused_port().expect("free port"),
            proxy_addr: self.proxy_addr,
        });
        ua.start().await?;
        ua.register().await?;
        Ok(Arc::new(ua))
    }

    /// Everything the proxy sends the caller for `window`, unanswered.
    async fn collect(&self, window: Duration) -> Result<Vec<(String, SocketAddr)>> {
        let deadline = Instant::now() + window;
        let mut buf = vec![0u8; 65535];
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(received) = tokio::time::timeout(left, self.caller.recv_from(&mut buf)).await
            else {
                return Ok(out);
            };
            let (n, src) = received?;
            out.push((String::from_utf8_lossy(&buf[..n]).to_string(), src));
        }
    }
}

/// First value of header `name` (case-insensitive) in a raw SIP message.
fn header(msg: &str, name: &str) -> String {
    msg.lines()
        .find_map(|line| {
            let (n, v) = line.split_once(':')?;
            n.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
        .unwrap_or_default()
}

/// URI inside the first `<...>` of a header value.
fn angle_uri(value: &str) -> String {
    value
        .split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(uri, _)| uri.to_string())
        .unwrap_or_else(|| value.to_string())
}

/// 200 OK to a request, echoing its Via/From/To/Call-ID/CSeq.
fn ok_to(req: &str) -> String {
    let vias: String = req
        .lines()
        .filter(|l| {
            l.split(':')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("Via")
        })
        .map(|l| format!("{l}\r\n"))
        .collect();
    format!(
        "SIP/2.0 200 OK\r\n{vias}From: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nContent-Length: 0\r\n\r\n",
        header(req, "From"),
        header(req, "To"),
        header(req, "Call-ID"),
        header(req, "CSeq"),
    )
}

#[tokio::test]
async fn cdr_ends_at_callee_hangup_while_caller_ack_and_bye_reply_lag() -> Result<()> {
    let h = Harness::start().await?;
    let bob = h.ua("bob", "password456").await?;
    let caller_port = h.caller.local_addr()?.port();
    let call_id = format!("cdr-end-{caller_port}@test");
    let sdp = test_helpers::pcmu_sdp("127.0.0.1", 40000);
    let invite = format!(
        "INVITE sip:bob@{proxy} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{caller_port};branch=z9hG4bK-cdrend;rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:15550001@127.0.0.1>;tag=cdrend\r\n\
         To: <sip:bob@127.0.0.1>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:15550001@127.0.0.1:{caller_port}>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {len}\r\n\r\n{sdp}",
        proxy = h.proxy_addr,
        len = sdp.len(),
    );
    h.caller.send_to(invite.as_bytes(), h.proxy_addr).await?;

    let bob_dialog = wait_for_value(Duration::from_secs(5), || async {
        bob.process_dialog_events()
            .await
            .ok()?
            .into_iter()
            .find_map(|event| match event {
                TestUaEvent::IncomingCall(id, _) => Some(id),
                _ => None,
            })
    })
    .await
    .map_err(|_| anyhow!("bob never received the INVITE"))?;
    bob.answer_call(
        &bob_dialog,
        Some(test_helpers::pcmu_sdp("127.0.0.1", 40010)),
    )
    .await?;

    // The 200 OK reaches the caller, which does not ACK; bob hangs up.
    let answered = h.collect(Duration::from_millis(300)).await?;
    let ok = answered
        .iter()
        .map(|(m, _)| m)
        .find(|m| m.starts_with("SIP/2.0 200") && header(m, "CSeq").ends_with("INVITE"))
        .ok_or_else(|| anyhow!("the caller never got the 200 OK"))?
        .clone();
    bob.hangup(&bob_dialog).await?;
    let hung_up = Instant::now();

    // The caller sits on its ACK and on the BYE reply.
    let lagged = h.collect(CALLER_DELAY).await?;
    let (bye, bye_src) = lagged
        .iter()
        .find(|(m, _)| m.starts_with("BYE "))
        .ok_or_else(|| anyhow!("the proxy never sent the caller a BYE"))?
        .clone();
    let ack = format!(
        "ACK {target} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{caller_port};branch=z9hG4bK-cdrend-ack;rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:15550001@127.0.0.1>;tag=cdrend\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n",
        target = angle_uri(&header(&ok, "Contact")),
        to = header(&ok, "To"),
    );
    h.caller.send_to(ack.as_bytes(), h.proxy_addr).await?;
    h.caller.send_to(ok_to(&bye).as_bytes(), bye_src).await?;

    let records = h
        .cdr
        .wait_for_records(1, Duration::from_secs(10))
        .await
        .ok_or_else(|| anyhow!("no CDR"))?;
    assert!(
        hung_up.elapsed() >= CALLER_DELAY,
        "the CDR may only be written once the caller's BYE completed"
    );
    let record = &records[0];
    let answer = record.answer_time.expect("an answered call");
    let duration = record.end_time - answer;
    assert!(
        duration < chrono::Duration::milliseconds(800),
        "the call lasted from answer to bob's hangup, not until the caller's late BYE reply: {duration}"
    );
    assert!(duration >= chrono::Duration::zero());
    Ok(())
}
