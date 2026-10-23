//! Per-trunk `nat_fix` override, end to end.
//!
//! The trunk peer is a raw UDP socket so its Via and Contact can lie: both
//! claim a "NATed" port that differs from the socket's real port. A second
//! socket (the trap) listens on that claimed port. Once the dialog is up, the
//! proxy's in-dialog request (BYE towards a calling trunk, ACK towards a
//! called trunk) shows where the proxy believes the trunk's Contact is: the
//! real port means the Contact was NAT-fixed, the trap means it was not.
//!
//! Every SIP peer here shares 127.0.0.1, so a trunk override applies to every
//! loopback source; the registered UAs send truthful Contacts, so only the
//! trunk peer is affected.

use crate::common::test_helpers;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use anyhow::{Result, anyhow};
use rustpbx::config::MediaProxyMode;
use rustpbx::proxy::routing::{DestConfig, MatchConditions, RouteAction, RouteRule, TrunkConfig};
use rustpbx::proxy::{
    acl::AclModule, auth::AuthModule, call::CallModule, locator::MemoryLocator,
    registrar::RegistrarModule, server::SipServerBuilder, user::MemoryUserBackend,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

/// Where the proxy sent its in-dialog request to the trunk.
#[derive(Debug, PartialEq, Eq)]
enum Landed {
    /// The trunk's real socket: the Contact was NAT-fixed.
    Real,
    /// The port the Contact claimed: the Contact was left alone.
    Claimed,
}

struct Harness {
    proxy_addr: SocketAddr,
    /// The trunk peer's real socket.
    trunk: UdpSocket,
    /// Listens on the port the trunk's Via/Contact claim.
    trap: UdpSocket,
    cancel: CancellationToken,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Harness {
    async fn start(global_nat_fix: bool, trunk_nat_fix: Option<bool>) -> Result<Self> {
        let trunk = UdpSocket::bind("127.0.0.1:0").await?;
        let trap = UdpSocket::bind("127.0.0.1:0").await?;
        let trunk_port = trunk.local_addr()?.port();

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
        config.nat_fix = global_nat_fix;
        config.trunks.insert(
            "carrier".to_string(),
            TrunkConfig {
                dest: format!("sip:127.0.0.1:{trunk_port}"),
                inbound_hosts: vec!["127.0.0.1".to_string()],
                nat_fix: trunk_nat_fix,
                ..Default::default()
            },
        );
        config.routes = Some(vec![RouteRule {
            name: "to-carrier".to_string(),
            match_conditions: MatchConditions {
                to_user: Some("^913".to_string()),
                ..Default::default()
            },
            action: RouteAction {
                action: Some("forward".to_string()),
                dest: Some(DestConfig::Single("carrier".to_string())),
                ..Default::default()
            },
            ..Default::default()
        }]);

        let user_backend = MemoryUserBackend::new(None);
        for user in test_helpers::standard_test_users() {
            user_backend.create_user(user).await?;
        }
        let cancel = CancellationToken::new();
        let server = SipServerBuilder::new(Arc::new(config))
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(MemoryLocator::new()))
            .with_cancel_token(cancel.clone())
            .register_module("acl", |inner, config| AclModule::create(inner, config))
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
        sleep(Duration::from_millis(200)).await;

        Ok(Self {
            proxy_addr: format!("127.0.0.1:{port}").parse()?,
            trunk,
            trap,
            cancel,
        })
    }

    fn claimed_port(&self) -> u16 {
        self.trap.local_addr().unwrap().port()
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

    /// Wait for a SIP message on the trunk socket matching `pred`.
    async fn recv_on_trunk(&self, pred: impl Fn(&str) -> bool) -> Result<(String, SocketAddr)> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = vec![0u8; 65535];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (n, src) = tokio::time::timeout(left, self.trunk.recv_from(&mut buf))
                .await
                .map_err(|_| anyhow!("timed out waiting on the trunk socket"))??;
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            if pred(&text) {
                return Ok((text, src));
            }
        }
    }

    /// Wait for an in-dialog `method` request on either socket, answering a
    /// BYE with 200 so the proxy stops retransmitting.
    async fn landed(&self, method: &str, call_id: &str) -> Result<Landed> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut real_buf = vec![0u8; 65535];
        let mut trap_buf = vec![0u8; 65535];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (landed, text, src, socket) = tokio::select! {
                r = self.trunk.recv_from(&mut real_buf) => {
                    let (n, src) = r?;
                    (Landed::Real, String::from_utf8_lossy(&real_buf[..n]).to_string(), src, &self.trunk)
                }
                r = self.trap.recv_from(&mut trap_buf) => {
                    let (n, src) = r?;
                    (Landed::Claimed, String::from_utf8_lossy(&trap_buf[..n]).to_string(), src, &self.trap)
                }
                _ = sleep(left) => return Err(anyhow!("no {method} reached either socket")),
            };
            if !text.starts_with(&format!("{method} ")) || header(&text, "Call-ID") != call_id {
                continue;
            }
            if method == "BYE" {
                let ok = response(&text, "200 OK", None, None);
                socket.send_to(ok.as_bytes(), src).await?;
            }
            return Ok(landed);
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

/// A response to `req` echoing its Via/From/To/Call-ID/CSeq, with an optional
/// To-tag and an optional Contact plus SDP body.
fn response(
    req: &str,
    status: &str,
    to_tag: Option<&str>,
    contact_sdp: Option<(&str, &str)>,
) -> String {
    let mut out = format!("SIP/2.0 {status}\r\n");
    for line in req.lines() {
        let name = line.split(':').next().unwrap_or_default().trim();
        if name.eq_ignore_ascii_case("Via") {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    let to = header(req, "To");
    let to = match to_tag {
        Some(tag) if !to.contains(";tag=") => format!("{to};tag={tag}"),
        _ => to,
    };
    out.push_str(&format!("From: {}\r\n", header(req, "From")));
    out.push_str(&format!("To: {to}\r\n"));
    out.push_str(&format!("Call-ID: {}\r\n", header(req, "Call-ID")));
    out.push_str(&format!("CSeq: {}\r\n", header(req, "CSeq")));
    match contact_sdp {
        Some((contact, sdp)) => {
            out.push_str(&format!("Contact: <{contact}>\r\n"));
            out.push_str("Content-Type: application/sdp\r\n");
            out.push_str(&format!("Content-Length: {}\r\n\r\n{sdp}", sdp.len()));
        }
        None => out.push_str("Content-Length: 0\r\n\r\n"),
    }
    out
}

/// The trunk calls bob. Its INVITE's Via and Contact claim the trap port, so
/// the proxy adds `received`/`rport`; bob then hangs up and the proxy's BYE
/// follows the caller's (possibly fixed) Contact.
async fn trunk_calls_bob(global_nat_fix: bool, trunk_nat_fix: Option<bool>) -> Result<Landed> {
    let h = Harness::start(global_nat_fix, trunk_nat_fix).await?;
    let bob = h.ua("bob", "password456").await?;
    sleep(Duration::from_millis(100)).await;

    let claimed = h.claimed_port();
    let call_id = format!("nat-in-{claimed}@test");
    let sdp = test_helpers::pcmu_sdp("127.0.0.1", 40000);
    let invite = format!(
        "INVITE sip:bob@{proxy} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{claimed};branch=z9hG4bK-natin;rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:15550001@127.0.0.1>;tag=natin\r\n\
         To: <sip:bob@127.0.0.1>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:15550001@127.0.0.1:{claimed}>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {len}\r\n\r\n{sdp}",
        proxy = h.proxy_addr,
        len = sdp.len(),
    );
    h.trunk.send_to(invite.as_bytes(), h.proxy_addr).await?;

    // bob answers whatever arrives.
    let bob_answer = {
        let bob = bob.clone();
        tokio::spawn(async move {
            for _ in 0..100 {
                for event in bob.process_dialog_events().await? {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        let answer = test_helpers::pcmu_sdp("127.0.0.1", 40010);
                        bob.answer_call(&id, Some(answer)).await?;
                        return Ok(id);
                    }
                }
                sleep(Duration::from_millis(50)).await;
            }
            Err(anyhow!("bob never received the INVITE"))
        })
    };

    let (ok, _) = h
        .recv_on_trunk(|m| m.starts_with("SIP/2.0 200") && m.contains("INVITE"))
        .await?;
    let bob_dialog = bob_answer.await??;

    let ack = format!(
        "ACK {target} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{claimed};branch=z9hG4bK-natin-ack;rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:15550001@127.0.0.1>;tag=natin\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n",
        target = angle_uri(&header(&ok, "Contact")),
        to = header(&ok, "To"),
    );
    h.trunk.send_to(ack.as_bytes(), h.proxy_addr).await?;
    sleep(Duration::from_millis(300)).await;

    bob.hangup(&bob_dialog).await?;
    h.landed("BYE", &call_id).await
}

/// alice calls out through the trunk. The trunk's 200 OK carries a Contact
/// claiming the trap port; the proxy's ACK follows that (possibly fixed)
/// Contact.
async fn alice_calls_trunk(global_nat_fix: bool, trunk_nat_fix: Option<bool>) -> Result<Landed> {
    let h = Harness::start(global_nat_fix, trunk_nat_fix).await?;
    let alice = h.ua("alice", "password123").await?;
    sleep(Duration::from_millis(100)).await;

    let call = {
        let alice = alice.clone();
        tokio::spawn(async move {
            let offer = test_helpers::pcmu_sdp("127.0.0.1", 40020);
            alice.make_call("91300000000", Some(offer)).await
        })
    };

    let (invite, proxy) = h.recv_on_trunk(|m| m.starts_with("INVITE ")).await?;
    let call_id = header(&invite, "Call-ID");
    let contact = format!("sip:carrier@127.0.0.1:{}", h.claimed_port());
    let sdp = test_helpers::pcmu_sdp("127.0.0.1", 40030);
    let ok = response(&invite, "200 OK", Some("natout"), Some((&contact, &sdp)));
    h.trunk.send_to(ok.as_bytes(), proxy).await?;

    let landed = h.landed("ACK", &call_id).await;
    call.abort();
    landed
}

#[tokio::test]
async fn trunk_caller_nat_fixed_when_trunk_enables_it() -> Result<()> {
    assert_eq!(trunk_calls_bob(false, Some(true)).await?, Landed::Real);
    Ok(())
}

#[tokio::test]
async fn trunk_caller_not_fixed_when_trunk_disables_it() -> Result<()> {
    assert_eq!(trunk_calls_bob(true, Some(false)).await?, Landed::Claimed);
    Ok(())
}

#[tokio::test]
async fn trunk_caller_follows_global_without_override() -> Result<()> {
    assert_eq!(trunk_calls_bob(false, None).await?, Landed::Claimed);
    assert_eq!(trunk_calls_bob(true, None).await?, Landed::Real);
    Ok(())
}

#[tokio::test]
async fn trunk_callee_nat_fixed_when_trunk_enables_it() -> Result<()> {
    assert_eq!(alice_calls_trunk(false, Some(true)).await?, Landed::Real);
    Ok(())
}

#[tokio::test]
async fn trunk_callee_not_fixed_when_trunk_disables_it() -> Result<()> {
    assert_eq!(alice_calls_trunk(true, Some(false)).await?, Landed::Claimed);
    Ok(())
}

#[tokio::test]
async fn trunk_callee_follows_global_without_override() -> Result<()> {
    assert_eq!(alice_calls_trunk(false, None).await?, Landed::Claimed);
    assert_eq!(alice_calls_trunk(true, None).await?, Landed::Real);
    Ok(())
}
