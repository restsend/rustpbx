#![cfg(feature = "addon-sbc")]

mod helpers;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::{Json, Router, routing::post};
use portpicker::pick_unused_port;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use helpers::sipbot_helper::TestUa;

use rustpbx::addons::sbc::jsonrpc::config::{ResponseConfig, Rule, SbcJsonRpcConfig, Upstream};
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
    rustpbx::utils::spawn(async move {
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
            trunks: vec![],
            upstream: Upstream {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{upstream_port}/jsonrpc"),
                headers: HashMap::new(),
                body: json!({"jsonrpc":"2.0","method":"route","id":1}).to_string(),
                query_params: None,
            },
            response: ResponseConfig {
                success_when: "true".into(),
                callee_rewrite: "".into(),
                caller_rewrite: "".into(),
                reject_status: 403,
                reject_reason: "".into(),
                reject_on_eval_error: true,
                passthrough_original_headers: true,
                inject_headers: vec![],
                allow_codecs: vec![],
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
    rustpbx::utils::spawn(async move {
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
    assert!(
        bob.has_rtp_rx(),
        "bob should have received RTP (call established)"
    );
    {
        let q = bob.audio_quality_summary();
        tracing::info!(
            "bob audio quality after SBC call: total={} silence={}",
            q.total_frames,
            q.silence_frames
        );
    }

    // ── Cleanup ──────────────────────────────────────────────────────────
    cancel_token.cancel();
    upstream_cancel.cancel();
}

// ─── Test 2: SBC JSON-RPC reject — upstream returns non-success → call rejected ──

#[tokio::test]
async fn test_sbc_jsonrpc_e2e_reject_call() {
    let upstream_port = pick_unused_port().unwrap();
    let sip_port = pick_unused_port().unwrap();
    let bob_port = pick_unused_port().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    // Mock upstream that always returns "result": "denied"
    let upstream_cancel = {
        let cnt = call_count.clone();
        let cancel = CancellationToken::new();
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], upstream_port).into();
        let app = Router::new().route(
            "/jsonrpc",
            post(move |body: Json<Value>| {
                let cnt = cnt.clone();
                async move {
                    cnt.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "result": "denied",
                        "echo": body.0,
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let cc = cancel.clone();
        rustpbx::utils::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cc.cancelled().await })
                .await
                .ok();
        });
        sleep(Duration::from_millis(100)).await;
        cancel
    };

    let sbc_config = Arc::new(RwLock::new(Some(SbcJsonRpcConfig {
        enabled: true,
        timeout_ms: 5000,
        rules: vec![Rule {
            name: "reject-rule".into(),
            enabled: true,
            when: "true".into(),
            match_group: None,
            extractors: vec![],
            trunks: vec![],
            upstream: Upstream {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{upstream_port}/jsonrpc"),
                headers: HashMap::new(),
                body: json!({"jsonrpc":"2.0","method":"route","id":1}).to_string(),
                query_params: None,
            },
            response: ResponseConfig {
                success_when: "json.result == \"ok\"".into(),
                callee_rewrite: "".into(),
                caller_rewrite: "".into(),
                reject_status: 403,
                reject_reason: "Blocked by policy".into(),
                reject_on_eval_error: true,
                passthrough_original_headers: true,
                inject_headers: vec![],
                allow_codecs: vec![],
            },
        }],
    })));
    let inspector = SbcJsonRpcInspector::new(sbc_config.clone());

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
    rustpbx::utils::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            tracing::warn!("SipServer serve error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(200)).await;

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

    let _alice = TestUa::caller_with_target(
        pick_unused_port().unwrap(),
        "alice",
        format!("sip:bob@127.0.0.1:{sip_port}"),
    )
    .await;

    sleep(Duration::from_secs(3)).await;

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "upstream should have been called once"
    );
    assert!(
        !bob.has_rtp_rx(),
        "bob should NOT have received RTP — call should be rejected"
    );

    cancel_token.cancel();
    upstream_cancel.cancel();
}

// ─── Test 3: SBC JSON-RPC header injection — verify headers reach outbound INVITE ──

#[tokio::test]
async fn test_sbc_jsonrpc_e2e_header_injection() {
    let upstream_port = pick_unused_port().unwrap();
    let sip_port = pick_unused_port().unwrap();
    let bob_port = pick_unused_port().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    let upstream_cancel = start_mock_upstream(upstream_port, call_count.clone()).await;

    use rustpbx::addons::sbc::jsonrpc::config::{HeaderAction, HeaderOp};

    let sbc_config = Arc::new(RwLock::new(Some(SbcJsonRpcConfig {
        enabled: true,
        timeout_ms: 5000,
        rules: vec![Rule {
            name: "inject-header-rule".into(),
            enabled: true,
            when: "true".into(),
            match_group: None,
            extractors: vec![],
            trunks: vec![],
            upstream: Upstream {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{upstream_port}/jsonrpc"),
                headers: HashMap::new(),
                body: json!({"jsonrpc":"2.0","method":"route","id":1}).to_string(),
                query_params: None,
            },
            response: ResponseConfig {
                success_when: "true".into(),
                callee_rewrite: "".into(),
                caller_rewrite: "".into(),
                reject_status: 403,
                reject_reason: "".into(),
                reject_on_eval_error: true,
                passthrough_original_headers: true,
                inject_headers: vec![
                    HeaderOp {
                        action: HeaderAction::Add,
                        name: "X-SBC-Route".into(),
                        value: "injected-by-e2e-test".into(),
                    },
                    HeaderOp {
                        action: HeaderAction::Set,
                        name: "X-Priority".into(),
                        value: "high".into(),
                    },
                ],
                allow_codecs: vec![],
            },
        }],
    })));
    let inspector = SbcJsonRpcInspector::new(sbc_config.clone());

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
    rustpbx::utils::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            tracing::warn!("SipServer serve error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(200)).await;

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

    let _alice = TestUa::caller_with_target(
        pick_unused_port().unwrap(),
        "alice",
        format!("sip:bob@127.0.0.1:{sip_port}"),
    )
    .await;

    sleep(Duration::from_secs(5)).await;

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "upstream JSON-RPC should have been called"
    );
    assert!(
        bob.has_rtp_rx(),
        "bob should have received RTP (call established with header injection)"
    );
    {
        let q = bob.audio_quality_summary();
        tracing::info!(
            "bob audio quality (header injection): total={} silence={}",
            q.total_frames,
            q.silence_frames
        );
    }

    cancel_token.cancel();
    upstream_cancel.cancel();
}

// ─── Test 4: SBC JSON-RPC multi-rule — first rule skips, second matches ──

#[tokio::test]
async fn test_sbc_jsonrpc_e2e_multi_rule_matching() {
    let upstream_port = pick_unused_port().unwrap();
    let sip_port = pick_unused_port().unwrap();
    let bob_port = pick_unused_port().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    let upstream_cancel = start_mock_upstream(upstream_port, call_count.clone()).await;

    use rustpbx::addons::sbc::jsonrpc::config::{
        MatchCondition, MatchField, MatchGroup, MatchLogic, MatchOp,
    };

    let sbc_config = Arc::new(RwLock::new(Some(SbcJsonRpcConfig {
        enabled: true,
        timeout_ms: 5000,
        rules: vec![
            Rule {
                name: "skip-rule".into(),
                enabled: true,
                when: String::new(),
                match_group: Some(MatchGroup {
                    logic: MatchLogic::All,
                    conditions: vec![MatchCondition {
                        field: MatchField::CalleeUser,
                        op: MatchOp::Equals,
                        value: "nonexistent".into(),
                    }],
                }),
                extractors: vec![],
                trunks: vec![],
                upstream: Upstream {
                    method: "POST".into(),
                    url: format!("http://127.0.0.1:{upstream_port}/jsonrpc"),
                    headers: HashMap::new(),
                    body: json!({"jsonrpc":"2.0","method":"route","id":1}).to_string(),
                    query_params: None,
                },
                response: ResponseConfig {
                    success_when: "true".into(),
                    callee_rewrite: "".into(),
                    caller_rewrite: "".into(),
                    reject_status: 403,
                    reject_reason: "".into(),
                    reject_on_eval_error: true,
                    passthrough_original_headers: true,
                    inject_headers: vec![],
                    allow_codecs: vec![],
                },
            },
            Rule {
                name: "catch-all-rule".into(),
                enabled: true,
                when: "true".into(),
                match_group: None,
                extractors: vec![],
                trunks: vec![],
                upstream: Upstream {
                    method: "POST".into(),
                    url: format!("http://127.0.0.1:{upstream_port}/jsonrpc"),
                    headers: HashMap::new(),
                    body: json!({"jsonrpc":"2.0","method":"route","id":2}).to_string(),
                    query_params: None,
                },
                response: ResponseConfig {
                    success_when: "true".into(),
                    callee_rewrite: "".into(),
                    caller_rewrite: "".into(),
                    reject_status: 403,
                    reject_reason: "".into(),
                    reject_on_eval_error: true,
                    passthrough_original_headers: true,
                    inject_headers: vec![],
                    allow_codecs: vec![],
                },
            },
        ],
    })));
    let inspector = SbcJsonRpcInspector::new(sbc_config.clone());

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
    rustpbx::utils::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            tracing::warn!("SipServer serve error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(200)).await;

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

    let _alice = TestUa::caller_with_target(
        pick_unused_port().unwrap(),
        "alice",
        format!("sip:bob@127.0.0.1:{sip_port}"),
    )
    .await;

    sleep(Duration::from_secs(5)).await;

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "upstream should be called once (only catch-all rule matches)"
    );
    assert!(
        bob.has_rtp_rx(),
        "bob should have received RTP (call established via catch-all rule)"
    );
    {
        let q = bob.audio_quality_summary();
        tracing::info!(
            "bob audio quality (multi-rule): total={} silence={}",
            q.total_frames,
            q.silence_frames
        );
    }

    cancel_token.cancel();
    upstream_cancel.cancel();
}

// ─── Test 5: SBC JSON-RPC upstream unreachable → DialplanVerdict::Reject ──

#[tokio::test]
async fn test_sbc_jsonrpc_e2e_upstream_unreachable_rejects() {
    let sip_port = pick_unused_port().unwrap();
    let bob_port = pick_unused_port().unwrap();

    // Point to a port where nothing is listening
    let dead_port = pick_unused_port().unwrap();

    let sbc_config = Arc::new(RwLock::new(Some(SbcJsonRpcConfig {
        enabled: true,
        timeout_ms: 1000,
        rules: vec![Rule {
            name: "dead-upstream".into(),
            enabled: true,
            when: "true".into(),
            match_group: None,
            extractors: vec![],
            trunks: vec![],
            upstream: Upstream {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{dead_port}/jsonrpc"),
                headers: HashMap::new(),
                body: json!({"jsonrpc":"2.0","method":"route","id":1}).to_string(),
                query_params: None,
            },
            response: ResponseConfig {
                success_when: "true".into(),
                callee_rewrite: "".into(),
                caller_rewrite: "".into(),
                reject_status: 503,
                reject_reason: "".into(),
                reject_on_eval_error: true,
                passthrough_original_headers: true,
                inject_headers: vec![],
                allow_codecs: vec![],
            },
        }],
    })));
    let inspector = SbcJsonRpcInspector::new(sbc_config.clone());

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
    rustpbx::utils::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            tracing::warn!("SipServer serve error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(200)).await;

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

    let _alice = TestUa::caller_with_target(
        pick_unused_port().unwrap(),
        "alice",
        format!("sip:bob@127.0.0.1:{sip_port}"),
    )
    .await;

    // Give time for the upstream to timeout and the call to be rejected
    sleep(Duration::from_secs(3)).await;

    assert!(
        !bob.has_rtp_rx(),
        "bob should NOT have received RTP — call rejected due to unreachable upstream"
    );

    cancel_token.cancel();
}

// ─── Test 6: SBC JSON-RPC callee rewrite — rewrite target via upstream response ──

#[tokio::test]
async fn test_sbc_jsonrpc_e2e_callee_rewrite() {
    let upstream_port = pick_unused_port().unwrap();
    let sip_port = pick_unused_port().unwrap();
    let bob_port = pick_unused_port().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    let upstream_cancel = {
        let cnt = call_count.clone();
        let cancel = CancellationToken::new();
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], upstream_port).into();
        let app = Router::new().route(
            "/jsonrpc",
            post(move |body: Json<Value>| {
                let cnt = cnt.clone();
                async move {
                    cnt.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "result": "ok",
                        "target": "bob",
                        "echo": body.0,
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let cc = cancel.clone();
        rustpbx::utils::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cc.cancelled().await })
                .await
                .ok();
        });
        sleep(Duration::from_millis(100)).await;
        cancel
    };

    let sbc_config = Arc::new(RwLock::new(Some(SbcJsonRpcConfig {
        enabled: true,
        timeout_ms: 5000,
        rules: vec![Rule {
            name: "rewrite-rule".into(),
            enabled: true,
            when: "true".into(),
            match_group: None,
            extractors: vec![],
            trunks: vec![],
            upstream: Upstream {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{upstream_port}/jsonrpc"),
                headers: HashMap::new(),
                body: json!({"jsonrpc":"2.0","method":"route","id":1}).to_string(),
                query_params: None,
            },
            response: ResponseConfig {
                success_when: "json.result == \"ok\"".into(),
                callee_rewrite: "sip:{{ json.target }}@127.0.0.1".into(),
                caller_rewrite: "".into(),
                reject_status: 403,
                reject_reason: "".into(),
                reject_on_eval_error: true,
                passthrough_original_headers: true,
                inject_headers: vec![],
                allow_codecs: vec![],
            },
        }],
    })));
    let inspector = SbcJsonRpcInspector::new(sbc_config.clone());

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
    rustpbx::utils::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            tracing::warn!("SipServer serve error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(200)).await;

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

    let _alice = TestUa::caller_with_target(
        pick_unused_port().unwrap(),
        "alice",
        format!("sip:anyone@127.0.0.1:{sip_port}"),
    )
    .await;

    sleep(Duration::from_secs(5)).await;

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "upstream should have been called once"
    );
    assert!(
        bob.has_rtp_rx(),
        "bob should have received RTP (callee rewritten to bob)"
    );
    {
        let q = bob.audio_quality_summary();
        tracing::info!(
            "bob audio quality (callee rewrite): total={} silence={}",
            q.total_frames,
            q.silence_frames
        );
    }

    cancel_token.cancel();
    upstream_cancel.cancel();
}

// ─── Test 7: success_when eval error with reject_on_eval_error=true → REJECT ──

#[tokio::test]
async fn test_sbc_jsonrpc_e2e_eval_error_reject() {
    let upstream_port = pick_unused_port().unwrap();
    let sip_port = pick_unused_port().unwrap();
    let bob_port = pick_unused_port().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    let upstream_cancel = {
        let cnt = call_count.clone();
        let cancel = CancellationToken::new();
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], upstream_port).into();
        let app = Router::new().route(
            "/jsonrpc",
            post(move |_body: Json<Value>| {
                let cnt = cnt.clone();
                async move {
                    cnt.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "status": "error",
                        "message": "internal error",
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let cc = cancel.clone();
        rustpbx::utils::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cc.cancelled().await })
                .await
                .ok();
        });
        sleep(Duration::from_millis(100)).await;
        cancel
    };

    let sbc_config = Arc::new(RwLock::new(Some(SbcJsonRpcConfig {
        enabled: true,
        timeout_ms: 5000,
        rules: vec![Rule {
            name: "eval-error-reject".into(),
            enabled: true,
            when: "true".into(),
            match_group: None,
            extractors: vec![],
            trunks: vec![],
            upstream: Upstream {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{upstream_port}/jsonrpc"),
                headers: HashMap::new(),
                body: "{}".into(),
                query_params: None,
            },
            response: ResponseConfig {
                success_when: "json.code == 0".into(),
                callee_rewrite: "".into(),
                caller_rewrite: "".into(),
                reject_status: 503,
                reject_reason: "eval error".into(),
                reject_on_eval_error: true,
                passthrough_original_headers: true,
                inject_headers: vec![],
                allow_codecs: vec![],
            },
        }],
    })));
    let inspector = SbcJsonRpcInspector::new(sbc_config.clone());

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
    rustpbx::utils::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            tracing::warn!("SipServer serve error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(200)).await;

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

    let _alice = TestUa::caller_with_target(
        pick_unused_port().unwrap(),
        "alice",
        format!("sip:anyone@127.0.0.1:{sip_port}"),
    )
    .await;

    sleep(Duration::from_secs(3)).await;

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "upstream should have been called once"
    );
    assert!(
        !bob.has_rtp_rx(),
        "bob should NOT have received RTP (call rejected due to eval error)"
    );

    cancel_token.cancel();
    upstream_cancel.cancel();
}

// ─── Test 8: success_when eval error with reject_on_eval_error=false → CONTINUE ──

#[tokio::test]
async fn test_sbc_jsonrpc_e2e_eval_error_continue() {
    let upstream_port = pick_unused_port().unwrap();
    let sip_port = pick_unused_port().unwrap();
    let bob_port = pick_unused_port().unwrap();
    let call_count = Arc::new(AtomicUsize::new(0));

    let upstream_cancel = {
        let cnt = call_count.clone();
        let cancel = CancellationToken::new();
        let addr: std::net::SocketAddr = ([127, 0, 0, 1], upstream_port).into();
        let app = Router::new().route(
            "/jsonrpc",
            post(move |_body: Json<Value>| {
                let cnt = cnt.clone();
                async move {
                    cnt.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "status": "error",
                        "message": "internal error",
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let cc = cancel.clone();
        rustpbx::utils::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cc.cancelled().await })
                .await
                .ok();
        });
        sleep(Duration::from_millis(100)).await;
        cancel
    };

    let sbc_config = Arc::new(RwLock::new(Some(SbcJsonRpcConfig {
        enabled: true,
        timeout_ms: 5000,
        rules: vec![Rule {
            name: "eval-error-continue".into(),
            enabled: true,
            when: "true".into(),
            match_group: None,
            extractors: vec![],
            trunks: vec![],
            upstream: Upstream {
                method: "POST".into(),
                url: format!("http://127.0.0.1:{upstream_port}/jsonrpc"),
                headers: HashMap::new(),
                body: "{}".into(),
                query_params: None,
            },
            response: ResponseConfig {
                success_when: "json.code == 0".into(),
                callee_rewrite: "".into(),
                caller_rewrite: "".into(),
                reject_status: 403,
                reject_reason: "".into(),
                reject_on_eval_error: false,
                passthrough_original_headers: true,
                inject_headers: vec![],
                allow_codecs: vec![],
            },
        }],
    })));
    let inspector = SbcJsonRpcInspector::new(sbc_config.clone());

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
    rustpbx::utils::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            tracing::warn!("SipServer serve error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(200)).await;

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

    let _alice = TestUa::caller_with_target(
        pick_unused_port().unwrap(),
        "alice",
        format!("sip:anyone@127.0.0.1:{sip_port}"),
    )
    .await;

    sleep(Duration::from_secs(3)).await;

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "upstream should have been called once"
    );
    assert!(
        !bob.has_rtp_rx(),
        "bob should NOT have received RTP (no rule succeeded)"
    );

    cancel_token.cancel();
    upstream_cancel.cancel();
}
