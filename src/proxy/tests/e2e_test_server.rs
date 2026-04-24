//! E2E Test Server - Full PBX server with CDR capture for end-to-end testing

use super::cdr_capture::CdrCapture;
use super::rtp_utils::{RtpReceiver, RtpSender};
use super::test_ua::{TestUa, TestUaConfig};
use crate::call::user::SipUser;
use crate::config::{MediaProxyMode, ProxyConfig};
use crate::proxy::{
    active_call_registry::ActiveProxyCallRegistry,
    auth::AuthModule,
    call::CallModule,
    locator::MemoryLocator,
    registrar::RegistrarModule,
    server::{SipServerBuilder, SipServerRef},
    user::MemoryUserBackend,
};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// E2E Test Server with full capabilities
pub struct E2eTestServer {
    pub port: u16,
    pub proxy_addr: SocketAddr,
    pub server_ref: SipServerRef,
    pub cdr_capture: CdrCapture,
    pub registry: Arc<ActiveProxyCallRegistry>,
    pub media_proxy_mode: MediaProxyMode,
    cancel_token: CancellationToken,
    _server_handle: Option<tokio::task::JoinHandle<()>>,
}

impl E2eTestServer {
    /// Start a new E2E test server with specified MediaProxy mode
    pub async fn start_with_mode(mode: MediaProxyMode) -> Result<Self> {
        let port = portpicker::pick_unused_port().unwrap_or(15060);
        let proxy_addr = format!("127.0.0.1:{}", port).parse()?;

        let config = Arc::new(ProxyConfig {
            addr: "127.0.0.1".to_string(),
            udp_port: Some(port),
            tcp_port: None,
            tls_port: None,
            ws_port: None,
            useragent: Some("RustPBX-E2E-Test/0.1.0".to_string()),
            modules: Some(vec![
                "auth".to_string(),
                "registrar".to_string(),
                "call".to_string(),
            ]),
            media_proxy: mode,
            ensure_user: Some(false),
            ..Default::default()
        });

        // Create CDR capture
        let (cdr_capture, cdr_sender) = CdrCapture::new();

        // Create user backend with test users
        let user_backend = MemoryUserBackend::new(None);
        for user in Self::create_test_users() {
            user_backend.create_user(user).await?;
        }

        let locator = MemoryLocator::new();
        let cancel_token = CancellationToken::new();

        let mut builder = SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel_token.clone())
            .with_callrecord_sender(Some(cdr_sender));

        builder = builder
            .register_module("registrar", |inner, config| {
                Ok(Box::new(RegistrarModule::new(inner, config)))
            })
            .register_module("auth", |inner, _config| {
                Ok(Box::new(AuthModule::new(inner.clone(), inner.proxy_config.clone())))
            })
            .register_module("call", |inner, config| {
                Ok(Box::new(CallModule::new(config, inner)))
            });

        let server = Arc::new(builder.build().await?);
        let server_ref = server.get_inner();
        let registry = server_ref.active_call_registry.clone();

        // Start server in background
        let cancel_token_clone = cancel_token.clone();
        let _server_handle = Some(tokio::spawn(async move {
            tokio::select! {
                _ = cancel_token_clone.cancelled() => {
                    info!("E2E test server cancelled");
                }
                result = server.serve() => {
                    if let Err(e) = result {
                        warn!("E2E test server error: {:?}", e);
                    }
                }
            }
        }));

        // Wait for server to be ready
        sleep(Duration::from_millis(200)).await;

        info!(port, ?mode, "E2E test server started");

        Ok(Self {
            port,
            proxy_addr,
            server_ref,
            cdr_capture,
            registry,
            media_proxy_mode: mode,
            cancel_token,
            _server_handle,
        })
    }

    /// Start with a custom ProxyConfig, allowing injection of trunks, routes, etc.
    pub async fn start_with_config(mut proxy_config: ProxyConfig) -> Result<Self> {
        let port = portpicker::pick_unused_port().unwrap_or(15060);
        let proxy_addr = format!("127.0.0.1:{}", port).parse()?;

        proxy_config.addr = "127.0.0.1".to_string();
        proxy_config.udp_port = Some(port);
        proxy_config.tcp_port = None;
        proxy_config.tls_port = None;
        proxy_config.ws_port = None;
        proxy_config.useragent = Some("RustPBX-E2E-Test/0.1.0".to_string());
        proxy_config.modules = Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]);
        proxy_config.ensure_user = Some(false);

        let config = Arc::new(proxy_config);
        let mode = config.media_proxy;

        // Create CDR capture
        let (cdr_capture, cdr_sender) = CdrCapture::new();

        // Create user backend with test users
        let user_backend = MemoryUserBackend::new(None);
        for user in Self::create_test_users() {
            user_backend.create_user(user).await?;
        }

        let locator = MemoryLocator::new();
        let cancel_token = CancellationToken::new();

        let mut builder = SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel_token.clone())
            .with_callrecord_sender(Some(cdr_sender));

        builder = builder
            .register_module("registrar", |inner, config| {
                Ok(Box::new(RegistrarModule::new(inner, config)))
            })
            .register_module("auth", |inner, _config| {
                Ok(Box::new(AuthModule::new(inner.clone(), inner.proxy_config.clone())))
            })
            .register_module("call", |inner, config| {
                Ok(Box::new(CallModule::new(config, inner)))
            });

        let server = Arc::new(builder.build().await?);
        let server_ref = server.get_inner();
        let registry = server_ref.active_call_registry.clone();

        // Start server in background
        let cancel_token_clone = cancel_token.clone();
        let _server_handle = Some(tokio::spawn(async move {
            tokio::select! {
                _ = cancel_token_clone.cancelled() => {
                    info!("E2E test server cancelled");
                }
                result = server.serve() => {
                    if let Err(e) = result {
                        warn!("E2E test server error: {:?}", e);
                    }
                }
            }
        }));

        // Wait for server to be ready
        sleep(Duration::from_millis(200)).await;

        info!(port, ?mode, "E2E test server started with custom config");

        Ok(Self {
            port,
            proxy_addr,
            server_ref,
            cdr_capture,
            registry,
            media_proxy_mode: mode,
            cancel_token,
            _server_handle,
        })
    }

    /// Start with default settings (Auto mode)
    pub async fn start() -> Result<Self> {
        Self::start_with_mode(MediaProxyMode::Auto).await
    }

    /// Create standard test users
    fn create_test_users() -> Vec<SipUser> {
        vec![
            SipUser {
                id: 1,
                username: "alice".to_string(),
                password: Some("password123".to_string()),
                enabled: true,
                realm: Some("127.0.0.1".to_string()),
                is_support_webrtc: true,
                ..Default::default()
            },
            SipUser {
                id: 2,
                username: "bob".to_string(),
                password: Some("password456".to_string()),
                enabled: true,
                realm: Some("127.0.0.1".to_string()),
                is_support_webrtc: false,
                ..Default::default()
            },
            SipUser {
                id: 3,
                username: "charlie".to_string(),
                password: Some("password789".to_string()),
                enabled: true,
                realm: Some("127.0.0.1".to_string()),
                is_support_webrtc: true,
                ..Default::default()
            },
        ]
    }

    /// Create a TestUa for a user
    pub async fn create_ua(&self, username: &str) -> Result<TestUa> {
        let password = match username {
            "alice" => "password123",
            "bob" => "password456",
            "charlie" => "password789",
            _ => "password",
        };

        let local_port = portpicker::pick_unused_port().unwrap_or(25000);

        let config = TestUaConfig {
            username: username.to_string(),
            password: password.to_string(),
            realm: "127.0.0.1".to_string(),
            local_port,
            proxy_addr: self.proxy_addr,
        };

        let mut ua = TestUa::new(config);
        ua.start().await?;
        ua.register().await?;

        info!(username, port = local_port, "TestUa created and registered");
        Ok(ua)
    }

    /// Get active calls from registry
    pub fn get_active_calls(
        &self,
    ) -> Vec<crate::proxy::active_call_registry::ActiveProxyCallEntry> {
        self.registry.list_recent(100)
    }

    /// Wait for call to appear in registry
    pub async fn wait_for_active_call(&self, timeout: Duration) -> Option<String> {
        let start = tokio::time::Instant::now();

        while start.elapsed() < timeout {
            let calls = self.get_active_calls();
            if let Some(call) = calls.first() {
                return Some(call.session_id.clone());
            }
            sleep(Duration::from_millis(100)).await;
        }

        None
    }

    /// Stop the server
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

impl Drop for E2eTestServer {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

/// Enhanced TestUa with RTP capabilities
pub struct E2eTestUa {
    pub ua: TestUa,
    pub rtp_receiver: Option<RtpReceiver>,
    pub rtp_sender: Option<RtpSender>,
    pub rtp_port: Option<u16>,
}

impl E2eTestUa {
    /// Create and setup E2E TestUa with RTP receiver
    pub async fn new_with_rtp(ua: TestUa) -> Result<Self> {
        // Create RTP receiver on a random port
        let rtp_receiver = RtpReceiver::bind(0).await?;
        let rtp_port = rtp_receiver.port()?;

        Ok(Self {
            ua,
            rtp_receiver: Some(rtp_receiver),
            rtp_sender: None,
            rtp_port: Some(rtp_port),
        })
    }

    /// Start RTP receiving
    pub fn start_receiving(&mut self) -> Result<()> {
        if let Some(ref receiver) = self.rtp_receiver {
            receiver.start_receiving();
            info!("RTP receiver started");
        }
        Ok(())
    }

    /// Setup RTP sender
    pub async fn setup_sender(&mut self) -> Result<()> {
        self.rtp_sender = Some(RtpSender::bind().await?);
        Ok(())
    }

    /// Get SDP with correct RTP port
    pub fn get_sdp_with_rtp_port(&self, base_sdp: &str) -> String {
        let port = self.rtp_port.unwrap_or(5004);

        // Replace media port in SDP
        base_sdp
            .replace(&format!("m=audio {} ", 5004), &format!("m=audio {} ", port))
            .replace(
                &format!("m=audio {} ", 12345),
                &format!("m=audio {} ", port),
            )
    }

    /// Get RTP stats
    pub async fn get_rtp_stats(&self) -> Option<super::rtp_utils::RtpStats> {
        if let Some(ref receiver) = self.rtp_receiver {
            Some(receiver.get_stats().await)
        } else {
            None
        }
    }

    /// Send RTP packets to target
    pub async fn send_rtp_to(
        &self,
        target: SocketAddr,
        packets: Vec<super::rtp_utils::RtpPacket>,
        interval_ms: u64,
    ) -> Result<()> {
        if let Some(ref sender) = self.rtp_sender {
            sender.send_sequence(target, packets, interval_ms).await?;
        }
        Ok(())
    }
}

/// Call scenario builder for complex test scenarios
pub struct CallScenario {
    server: Arc<E2eTestServer>,
    caller: Option<TestUa>,
    callee: Option<TestUa>,
}

impl CallScenario {
    pub fn new(server: Arc<E2eTestServer>) -> Self {
        Self {
            server,
            caller: None,
            callee: None,
        }
    }

    pub async fn with_caller(mut self, username: &str) -> Result<Self> {
        self.caller = Some(self.server.create_ua(username).await?);
        Ok(self)
    }

    pub async fn with_callee(mut self, username: &str) -> Result<Self> {
        self.callee = Some(self.server.create_ua(username).await?);
        Ok(self)
    }

    /// Execute the call scenario
    pub async fn execute(&mut self) -> Result<&str> {
        // Implementation depends on specific scenario
        // This is a placeholder for the pattern
        Ok("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_e2e_server_start() {
        let server = E2eTestServer::start().await;
        assert!(server.is_ok());

        let server = server.unwrap();
        assert!(server.port > 0);

        // Cleanup
        server.stop();
    }

    #[tokio::test]
    async fn test_create_ua() {
        let server = E2eTestServer::start().await.unwrap();

        let ua = server.create_ua("alice").await;
        assert!(ua.is_ok());

        server.stop();
    }
}
