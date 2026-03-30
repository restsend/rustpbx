// tests/helpers/test_server.rs
//
// `TestPbx` — a minimal in-process RustPBX server that combines:
//   * a real SipServer bound to a random UDP port
//   * an Axum HTTP server with the `/rwi` WebSocket endpoint
//
// This is used by E2E tests that need a real SIP stack (so that sipbot UAs can
// actually exchange SIP/SDP with the PBX).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Extension,
    extract::{Query, ws::WebSocketUpgrade},
    http::HeaderMap,
    routing::get,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use rustpbx::{
    config::ProxyConfig,
    proxy::{
        active_call_registry::ActiveProxyCallRegistry,
        server::{SipServerBuilder, SipServerRef},
    },
    rwi::{
        RwiAuth, RwiAuthRef, RwiGateway, RwiGatewayRef,
        auth::{RwiConfig, RwiTokenConfig},
        handler::rwi_ws_handler,
    },
};

pub const TEST_TOKEN: &str = "e2e-test-token";

/// A running in-process PBX with a real SIP stack and an RWI WebSocket endpoint.
pub struct TestPbx {
    /// Base WebSocket URL for connecting RWI clients, e.g. `ws://127.0.0.1:<port>/rwi/v1`.
    pub rwi_url: String,
    /// SIP port this server is listening on (UDP).
    #[allow(dead_code)]
    pub sip_port: u16,
    /// `127.0.0.1` IP where the SIP server is bound.
    #[allow(dead_code)]
    pub sip_addr: String,
    /// RWI gateway — can be used to inject events in tests.
    #[allow(dead_code)]
    pub gateway: RwiGatewayRef,
    /// Shared call registry (same instance as in the SipServer).
    #[allow(dead_code)]
    pub registry: Arc<ActiveProxyCallRegistry>,
    /// Cancellation token — cancel to shut everything down.
    pub cancel_token: CancellationToken,
}

impl TestPbx {
    /// Start a TestPbx bound to the given `sip_port`.
    ///
    /// Use `portpicker::pick_unused_port().unwrap()` to choose ports.
    pub async fn start(sip_port: u16) -> Self {
        let cancel_token = CancellationToken::new();

        // ── Build SipServer ──────────────────────────────────────────────────
        let proxy_config = Arc::new(ProxyConfig {
            addr: "127.0.0.1".to_string(),
            udp_port: Some(sip_port),
            enable_latching: true,
            ..Default::default()
        });

        let sip_server = SipServerBuilder::new(proxy_config.clone())
            .with_cancel_token(cancel_token.child_token())
            .build()
            .await
            .expect("SipServer build failed");

        let sip_server_ref: SipServerRef = sip_server.get_inner();
        let registry = sip_server_ref.active_call_registry.clone();

        // Spawn the SIP serving loop
        {
            let ct = cancel_token.child_token();
            tokio::spawn(async move {
                tokio::select! {
                    _ = ct.cancelled() => {}
                    res = sip_server.serve() => {
                        if let Err(e) = res {
                            tracing::error!("SipServer serve error: {e:?}");
                        }
                    }
                }
            });
        }

        // ── Build RWI components ─────────────────────────────────────────────
        let rwi_config = RwiConfig {
            enabled: true,
            tokens: vec![RwiTokenConfig {
                token: TEST_TOKEN.to_string(),
                scopes: vec!["call.control".to_string()],
            }],
            ..Default::default()
        };
        let auth: RwiAuthRef = Arc::new(tokio::sync::RwLock::new(RwiAuth::new(&rwi_config)));
        let gateway: RwiGatewayRef = Arc::new(tokio::sync::RwLock::new(RwiGateway::new()));

        // ── Build Axum router with RWI endpoint ──────────────────────────────
        let auth_c = auth.clone();
        let gw_c = gateway.clone();
        let reg_c = registry.clone();
        let srv_c = Some(sip_server_ref.clone());

        let router = axum::Router::new().route(
            "/rwi/v1",
            get(
                move |client_addr: rustpbx::handler::middleware::clientaddr::ClientAddr,
                      ws: WebSocketUpgrade,
                      Query(params): Query<HashMap<String, String>>,
                      headers: HeaderMap| {
                    let a = auth_c.clone();
                    let g = gw_c.clone();
                    let r = reg_c.clone();
                    let s = srv_c.clone();
                    async move {
                        rwi_ws_handler(
                            client_addr,
                            ws,
                            Query(params),
                            Extension(a),
                            Extension(g),
                            Extension(r),
                            Extension(s),
                            headers,
                        )
                        .await
                    }
                },
            ),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = listener.local_addr().unwrap().port();

        let ct = cancel_token.child_token();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { ct.cancelled().await })
                .await
                .unwrap();
        });

        let rwi_url = format!("ws://127.0.0.1:{}/rwi/v1", http_port);

        Self {
            rwi_url,
            sip_port,
            sip_addr: "127.0.0.1".to_string(),
            gateway,
            registry,
            cancel_token,
        }
    }

    /// Return the SIP address string: `127.0.0.1:<sip_port>`.
    #[allow(dead_code)]
    pub fn sip_host(&self) -> String {
        format!("{}:{}", self.sip_addr, self.sip_port)
    }

    /// Shut down the server.
    #[allow(dead_code)]
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

impl Drop for TestPbx {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
