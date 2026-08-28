//! E2E Test Server - Full PBX server with CDR capture for end-to-end testing

use super::cdr_capture::CdrCapture;
use super::test_helpers;
use super::test_ua::{TestUa, TestUaConfig};
use anyhow::Result;
use rustpbx::call::user::SipUser;
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry;
use rustpbx::proxy::locator::MemoryLocator;
use rustpbx::proxy::proxy_call::session_hooks::CallSessionHook;
use rustpbx::proxy::server::{SipServerBuilder, SipServerRef};
use rustpbx::proxy::user::MemoryUserBackend;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Extra wiring to install when starting an [`E2eTestServer`] via
/// [`E2eTestServer::start_with_inject`].
pub struct E2eTestServerInject {
    /// SIP users created in the memory user backend. When non-empty these
    /// REPLACE the standard alice/bob/charlie users (useful when tests need
    /// their own caller/agent identities with distinct ids).
    pub users: Vec<SipUser>,
    /// Session hook installed via `with_session_hook`.
    pub session_hook: Option<Arc<dyn CallSessionHook>>,
    /// App-level agent registry installed after the standard modules
    /// (e.g. a `CcAgentRegistryAdapter` for skill-group queues).
    pub agent_registry: Option<Arc<dyn rustpbx::call::app::agent_registry::AgentRegistry>>,
    /// RWI gateway installed via `with_rwi_gateway`. When set, every app
    /// (IVR/queue) and session-level lifecycle event (`call_*`) is emitted
    /// through it — drive it with a webhook handler to capture full event
    /// sequences.
    pub rwi_gateway: Option<rustpbx::rwi::RwiGatewayRef>,
}

impl Default for E2eTestServerInject {
    fn default() -> Self {
        Self {
            users: Vec::new(),
            session_hook: None,
            agent_registry: None,
            rwi_gateway: None,
        }
    }
}

/// E2E Test Server with full capabilities
pub struct E2eTestServer {
    pub port: u16,
    pub proxy_addr: SocketAddr,
    pub server_ref: SipServerRef,
    pub cdr_capture: CdrCapture,
    pub registry: Arc<ActiveProxyCallRegistry>,
    pub media_proxy_mode: MediaProxyMode,
    cancel_token: CancellationToken,
    _server_abort: Option<tokio::task::AbortHandle>,
}

impl E2eTestServer {
    /// Shared build/serve/spawn core for every public constructor.
    async fn start_builder(
        port: u16,
        proxy_addr: SocketAddr,
        mode: MediaProxyMode,
        builder: SipServerBuilder,
        cdr_capture: CdrCapture,
        cancel_token: CancellationToken,
        label: &str,
    ) -> Result<Self> {
        let server = Arc::new(builder.build().await?);
        let server_ref = server.get_inner();
        let registry = server_ref.active_call_registry.clone();

        let cancel_token_clone = cancel_token.clone();
        let join_handle = rustpbx::utils::spawn(async move {
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
        });
        let _server_abort = Some(join_handle.abort_handle());

        // Wait for server to be ready
        sleep(Duration::from_millis(200)).await;

        info!(port, ?mode, "{label}");

        Ok(Self {
            port,
            proxy_addr,
            server_ref,
            cdr_capture,
            registry,
            media_proxy_mode: mode,
            cancel_token,
            _server_abort,
        })
    }

    /// Start a new E2E test server with specified MediaProxy mode
    pub async fn start_with_mode(mode: MediaProxyMode) -> Result<Self> {
        let mut proxy_config =
            test_helpers::test_proxy_config(portpicker::pick_unused_port().unwrap_or(15060));
        proxy_config.media_proxy = mode;
        proxy_config.ensure_user = Some(false);
        proxy_config.enable_latching = false;

        Self::start_with_config_and_inject(
            proxy_config,
            E2eTestServerInject::default(),
            "E2E test server started",
        )
        .await
    }

    /// Start an E2E test server with the presence module registered
    /// (in addition to auth, registrar, call).
    pub async fn start_with_presence(mode: MediaProxyMode) -> Result<Self> {
        let port = portpicker::pick_unused_port().unwrap_or(15060);
        let proxy_addr = format!("127.0.0.1:{}", port).parse()?;

        let mut proxy_config = test_helpers::test_proxy_config_with_presence(port);
        proxy_config.media_proxy = mode;
        proxy_config.ensure_user = Some(false);
        proxy_config.enable_latching = false;

        let mode = proxy_config.media_proxy;
        let (builder, cdr_capture, cancel_token) =
            Self::base_builder(proxy_config, &E2eTestServerInject::default()).await?;
        let builder = test_helpers::register_modules_with_presence(builder);
        Self::start_builder(
            port,
            proxy_addr,
            mode,
            builder,
            cdr_capture,
            cancel_token,
            "E2E test server with presence started",
        )
        .await
    }

    /// Start with a custom ProxyConfig, allowing injection of trunks, routes, etc.
    pub async fn start_with_config(proxy_config: ProxyConfig) -> Result<Self> {
        Self::start_with_config_and_inject(
            proxy_config,
            E2eTestServerInject::default(),
            "E2E test server started with custom config",
        )
        .await
    }

    /// Start with a custom ProxyConfig PLUS test-specific wiring (custom
    /// users, session hook, agent registry).
    pub async fn start_with_inject(
        proxy_config: ProxyConfig,
        inject: E2eTestServerInject,
    ) -> Result<Self> {
        Self::start_with_config_and_inject(
            proxy_config,
            inject,
            "E2E test server started with custom config",
        )
        .await
    }

    async fn start_with_config_and_inject(
        mut proxy_config: ProxyConfig,
        inject: E2eTestServerInject,
        label: &str,
    ) -> Result<Self> {
        let port = portpicker::pick_unused_port().unwrap_or(15060);
        let proxy_addr = format!("127.0.0.1:{}", port).parse()?;

        let base = test_helpers::test_proxy_config(port);
        proxy_config.addr = base.addr;
        proxy_config.udp_port = base.udp_port;
        proxy_config.tcp_port = base.tcp_port;
        proxy_config.tls_port = base.tls_port;
        proxy_config.ws_port = base.ws_port;
        proxy_config.useragent = base.useragent;
        proxy_config.modules = base.modules;
        proxy_config.ensure_user = Some(false);
        proxy_config.enable_latching = false;

        let mode = proxy_config.media_proxy;
        let (builder, cdr_capture, cancel_token) =
            Self::base_builder(proxy_config, &inject).await?;
        let builder = test_helpers::register_standard_modules(builder);
        let builder = match inject.agent_registry {
            Some(registry) => builder.with_agent_registry(registry),
            None => builder,
        };

        Self::start_builder(
            port,
            proxy_addr,
            mode,
            builder,
            cdr_capture,
            cancel_token,
            label,
        )
        .await
    }

    /// Create the builder base: config arc, CDR capture, user backend,
    /// locator, cancel token, optional session hook. The caller registers
    /// the modules (standard / presence) and any agent registry.
    async fn base_builder(
        proxy_config: ProxyConfig,
        inject: &E2eTestServerInject,
    ) -> Result<(SipServerBuilder, CdrCapture, CancellationToken)> {
        let config = Arc::new(proxy_config);

        // Create CDR capture
        let (cdr_capture, cdr_sender) = CdrCapture::new();

        // Create user backend with test users
        let user_backend = MemoryUserBackend::new(None);
        let users = if inject.users.is_empty() {
            test_helpers::standard_test_users()
        } else {
            inject.users.clone()
        };
        for user in users {
            user_backend.create_user(user).await?;
        }

        let locator = MemoryLocator::new();
        let cancel_token = CancellationToken::new();

        let mut builder = SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel_token.clone())
            .with_callrecord_sender(Some(cdr_sender));
        if let Some(hook) = &inject.session_hook {
            builder = builder.with_session_hook(hook.clone());
        }
        if let Some(gateway) = &inject.rwi_gateway {
            builder = builder.with_rwi_gateway(gateway.clone());
        }
        Ok((builder, cdr_capture, cancel_token))
    }

    /// Start with default settings (Auto mode)
    pub async fn start() -> Result<Self> {
        Self::start_with_mode(MediaProxyMode::Auto).await
    }

    /// Create standard test users
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
            webrtc: false,
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
    ) -> Vec<rustpbx::proxy::active_call_registry::ActiveProxyCallEntry> {
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
        if let Some(abort) = self._server_abort.take() {
            abort.abort();
        }
    }
}
