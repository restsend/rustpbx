//! A trunk peer that is a raw UDP socket, for tests that need exact control
//! over the caller's SIP: withholding an ACK, delaying a BYE reply, or
//! sending requests a full UA would not (a BYE on an early dialog). The
//! server routes the trunk's calls to registered users and records CDRs.

use crate::common::cdr_capture::CdrCapture;
use crate::common::test_helpers;
use crate::common::test_ua::{TestUa, TestUaConfig};
use anyhow::Result;
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

pub(crate) struct Harness {
    pub(crate) proxy_addr: SocketAddr,
    pub(crate) caller: UdpSocket,
    pub(crate) cdr: CdrCapture,
    cancel: CancellationToken,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Harness {
    pub(crate) async fn start() -> Result<Self> {
        Self::start_with_trunk(|_| {}).await
    }

    /// `start`, with the caller's trunk adjusted by `configure`.
    pub(crate) async fn start_with_trunk(configure: impl FnOnce(&mut TrunkConfig)) -> Result<Self> {
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
        let mut trunk = TrunkConfig {
            dest: format!("sip:127.0.0.1:{caller_port}"),
            inbound_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        };
        configure(&mut trunk);
        config.trunks.insert("carrier".to_string(), trunk);

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

    pub(crate) async fn ua(&self, username: &str, password: &str) -> Result<Arc<TestUa>> {
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
    pub(crate) async fn collect(&self, window: Duration) -> Result<Vec<(String, SocketAddr)>> {
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
pub(crate) fn header(msg: &str, name: &str) -> String {
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
pub(crate) fn angle_uri(value: &str) -> String {
    value
        .split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(uri, _)| uri.to_string())
        .unwrap_or_else(|| value.to_string())
}

/// 200 OK to a request, echoing its Via/From/To/Call-ID/CSeq.
pub(crate) fn ok_to(req: &str) -> String {
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
