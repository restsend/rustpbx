#![cfg(feature = "addon-sbc")]

mod helpers;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::{Router, routing::post, Json};
use portpicker::pick_unused_port;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use helpers::sipbot_helper::TestUa;

use rustpbx::addons::sbc::jsonrpc::config::{
    SbcJsonRpcConfig, Rule, Upstream, ResponseConfig,
};
use rustpbx::addons::sbc::jsonrpc::inspector::SbcJsonRpcInspector;
use rustpbx::call::user::SipUser;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::call::CallModule;
use rustpbx::proxy::locator::MemoryLocator;
use rustpbx::proxy::registrar::RegistrarModule;
use rustpbx::proxy::server::SipServerBuilder;
use rustpbx::proxy::user::MemoryUserBackend;

fn create_proxy_config(port: u16) -> ProxyConfig {
    ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        tcp_port: None,
        useragent: Some("RustPBX-Test/0.1.0".to_string()),
        modules: Some(vec![
            "acl".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    }
}

async fn start_mock_upstream(port: u16, call_count: Arc<AtomicUsize>) -> CancellationToken {
    let cancel = CancellationToken::new();
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();

    let app = Router::new().route(
        "/jsonrpc",
        post(move |body: Json<Value>| {
            let cnt = call_count.clone();
            async move {
                cnt.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "result": "ok",
                    "echo": body.0,
                }))
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel_clone.cancelled().await })
            .await
            .ok();
    });

    sleep(Duration::from_millis(100)).await;
    cancel
}

#[tokio::test]
async fn test_sbc_jsonrpc_e2e_sipbot_rewrite() {
    let upstream_port = pick_unused_port().unwrap();
    let sip_port = pick_unused_port().unwrap();
    let bob_port = pick_unused_port().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    // ── Mock upstream HTTP ─────────────────────────────────────────────────
    let upstream_cancel = start_mock_upstream(upstream_port, call_count.clone()).await;

    // ── SBC inspector ──────────────────────────────────────────────────────
    let sbc_config = Arc::new(RwLock::new(Some(SbcJsonRpcConfig {
        enabled: true,
        timeout_ms: 5000,
        rules: vec![Rule {
            name: "e2e-rule".into(),
            enabled: true,
            when: "true".into(),
            match_group: None,
            extractors: vec![],
            upstream: Upstream {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{upstream_port}/jsonrpc"),
                headers: HashMap::new(),
                body: json!({"jsonrpc":"2.0","method":"route","id":1}).to_string(),
            },
            response: ResponseConfig {
                success_when: "true".into(),
                callee_rewrite: "".into(),
                caller_rewrite: "".into(),
                reject_status: 403,
                reject_reason: "".into(),
                passthrough_original_headers: true,
                inject_headers: vec![],
            },
        }],
    })));
    let inspector = SbcJsonRpcInspector::new(sbc_config.clone());

    // ── User backend with bob ──────────────────────────────────────────────
    let user_backend = MemoryUserBackend::new(None);
    user_backend
        .create_user(SipUser {
            id: 1,
            username: "bob".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    // ── Build SIP server ──────────────────────────────────────────────────
    let config = Arc::new(create_proxy_config(sip_port));
    let locator = MemoryLocator::new();
    let cancel_token = CancellationToken::new();

    let mut builder = SipServerBuilder::new(config.clone())
        .with_user_backend(Box::new(user_backend))
        .with_locator(Box::new(locator))
        .with_cancel_token(cancel_token.child_token())
        .with_dialplan_inspector(Box::new(inspector));

    builder = builder
        .register_module("registrar", |inner, config| {
            Ok(Box::new(RegistrarModule::new(inner, config)))
        })
        .register_module("call", |inner, config| {
            Ok(Box::new(CallModule::new(config, inner)))
        });

    let server = Arc::new(builder.build().await.unwrap());
    let server_clone = server.clone();
    tokio::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            tracing::warn!("SipServer serve error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(200)).await;

    // ── Register bob via sipbot ──────────────────────────────────────────
    let bob = TestUa::registered_callee(
        bob_port,
        2,
        "bob",
        "password",
        "127.0.0.1",
        &format!("127.0.0.1:{sip_port}"),
    )
    .await;
    sleep(Duration::from_millis(500)).await;

    // ── Alice calls bob through the PBX ──────────────────────────────────
    let _alice = TestUa::caller_with_target(
        pick_unused_port().unwrap(),
        "alice",
        format!("sip:bob@127.0.0.1:{sip_port}"),
    )
    .await;

    // ── Wait for call establishment ──────────────────────────────────────
    sleep(Duration::from_secs(5)).await;

    // ── Verify ───────────────────────────────────────────────────────────
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "upstream JSON-RPC should have been called"
    );
    assert!(bob.has_rtp_rx(), "bob should have received RTP (call established)");

    // ── Cleanup ──────────────────────────────────────────────────────────
    cancel_token.cancel();
    upstream_cancel.cancel();
}
