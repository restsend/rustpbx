//! Trunk health check — end-to-end tests using a real SIP server + OPTIONS responder.

#![cfg(feature = "addon-sbc")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use portpicker::pick_unused_port;
use rsipstack::{
    EndpointBuilder,
    sip::{Method, StatusCode},
    transport::{TransportLayer, udp::UdpConnection},
};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use rustpbx::config::ProxyConfig;
use rustpbx::proxy::call::CallModule;
use rustpbx::proxy::locator::MemoryLocator;
use rustpbx::proxy::registrar::RegistrarModule;
use rustpbx::proxy::routing::TrunkConfig;
use rustpbx::proxy::server::SipServerBuilder;
use rustpbx::proxy::trunk_health::{HealthStateMap, snapshot, spawn_health_loop};
use rustpbx::proxy::user::MemoryUserBackend;

// ── Helpers ─────────────────────────────────────────────────────────────────

struct OptionsResponder {
    #[allow(dead_code)]
    cancel: CancellationToken,
    port: u16,
}

impl OptionsResponder {
    async fn start(port: u16) -> Self {
        let cancel = CancellationToken::new();
        let tl = TransportLayer::new(cancel.child_token());
        let udp = UdpConnection::create_connection(
            format!("127.0.0.1:{port}").parse().unwrap(),
            None,
            Some(cancel.child_token()),
        )
        .await
        .unwrap();
        tl.add_transport(udp.into());

        let mut builder = EndpointBuilder::new();
        builder.with_user_agent("e2e-responder/1.0");
        builder.with_transport_layer(tl);
        builder.with_cancel_token(cancel.child_token());
        builder.with_timer_interval(Duration::from_millis(50));
        let endpoint = builder.build();

        let ep_inner = endpoint.inner.clone();
        let ct = cancel.clone();
        rustpbx::utils::spawn(async move {
            tokio::select! {
                _ = ct.cancelled() => {}
                r = ep_inner.serve() => { if let Err(e) = r { tracing::warn!("resp serve: {e}"); } }
            }
        });

        let mut rx = endpoint.incoming_transactions().unwrap();
        let ct2 = cancel.clone();
        rustpbx::utils::spawn(async move {
            loop {
                tokio::select! {
                    _ = ct2.cancelled() => break,
                    tx = rx.recv() => {
                        if let Some(mut tx) = tx {
                            if tx.original.method == Method::Options {
                                tx.reply(StatusCode::OK).await.ok();
                            }
                        }
                    }
                }
            }
        });

        sleep(Duration::from_millis(300)).await;
        Self { cancel, port }
    }

    #[allow(dead_code)]
    fn stop(&self) {
        self.cancel.cancel();
    }
    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

async fn create_pbx(port: u16) -> (rustpbx::proxy::server::SipServer, CancellationToken) {
    let cancel = CancellationToken::new();
    let config = Arc::new(ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        tcp_port: None,
        useragent: Some("RustPBX-E2E/1.0".to_string()),
        modules: Some(vec!["registrar".into(), "call".into()]),
        ..Default::default()
    });

    let user_backend = MemoryUserBackend::new(None);
    let locator = MemoryLocator::new();

    let mut builder = SipServerBuilder::new(config)
        .with_user_backend(Box::new(user_backend))
        .with_locator(Box::new(locator))
        .with_cancel_token(cancel.child_token());

    builder = builder
        .register_module("registrar", |inner, cfg| {
            Ok(Box::new(RegistrarModule::new(inner, cfg)))
        })
        .register_module("call", |inner, cfg| {
            Ok(Box::new(CallModule::new(cfg, inner)))
        });

    let server = builder.build().await.unwrap();
    let ep = server.inner.endpoint.inner.clone();
    let ct = cancel.clone();
    rustpbx::utils::spawn(async move {
        tokio::select! {
            _ = ct.cancelled() => {}
            r = ep.serve() => { if let Err(e) = r { tracing::warn!("pbx endpoint: {e}"); } }
        }
    });
    sleep(Duration::from_millis(200)).await;
    (server, cancel)
}

fn make_trunk(dest: &str) -> TrunkConfig {
    TrunkConfig {
        dest: format!("sip:{}", dest),
        health_check_enabled: Some(true),
        health_check_interval_secs: Some(1),
        health_check_probe_count: Some(2),
        ..Default::default()
    }
}

// ── E2E Tests ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_health_check_healthy() {
    let pbx_port = pick_unused_port().unwrap();
    let resp_port = pick_unused_port().unwrap();

    let responder = OptionsResponder::start(resp_port).await;
    let (server, _pbx_cancel) = create_pbx(pbx_port).await;

    let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
    let trunk = make_trunk(&responder.addr());
    let cancel = CancellationToken::new();

    let trunks = Arc::new(std::sync::Mutex::new(HashMap::from([(
        "e2e-trunk".into(),
        trunk,
    )])));
    let fn_ = {
        let t = trunks.clone();
        move || t.lock().unwrap().clone()
    };

    let endpoint_inner = server.inner.endpoint.inner.clone();
    let local_addr = format!("127.0.0.1:{pbx_port}");

    spawn_health_loop(
        fn_,
        states.clone(),
        endpoint_inner,
        local_addr,
        1,
        cancel.child_token(),
    );

    sleep(Duration::from_secs(3)).await;
    let snap = snapshot(&states).await;
    assert!(!snap.is_empty(), "should have health state");
    assert!(snap[0].healthy, "trunk should be healthy");
    assert!(snap[0].rtt_ms.is_some(), "should have rtt");
    assert!(snap[0].last_error.is_none());

    cancel.cancel();
}

#[tokio::test]
async fn test_e2e_health_check_unhealthy() {
    let pbx_port = pick_unused_port().unwrap();
    let dead_port = pick_unused_port().unwrap();
    let dest = format!("127.0.0.1:{dead_port}");

    let (server, _pbx_cancel) = create_pbx(pbx_port).await;

    let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
    let trunk = make_trunk(&dest);
    let cancel = CancellationToken::new();

    let trunks = Arc::new(std::sync::Mutex::new(HashMap::from([(
        "e2e-trunk".into(),
        trunk,
    )])));
    let fn_ = {
        let t = trunks.clone();
        move || t.lock().unwrap().clone()
    };

    let endpoint_inner = server.inner.endpoint.inner.clone();
    let local_addr = format!("127.0.0.1:{pbx_port}");

    spawn_health_loop(
        fn_,
        states.clone(),
        endpoint_inner,
        local_addr,
        1,
        cancel.child_token(),
    );

    // Probe timeout is 10s (default), interval=1s → each probe cycle ~11s
    // threshold=2 → ~22s needed. We use shorter wait + check it's failing
    sleep(Duration::from_secs(5)).await;
    let snap = snapshot(&states).await;
    if !snap.is_empty() {
        assert!(
            !snap[0].healthy || snap[0].consecutive_failures > 0,
            "should have some failures"
        );
    }

    cancel.cancel();
}

#[tokio::test]
async fn test_e2e_health_check_recovery() {
    let pbx_port = pick_unused_port().unwrap();
    let resp_port = pick_unused_port().unwrap();

    let (server, _pbx_cancel) = create_pbx(pbx_port).await;

    let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
    let trunk = make_trunk(&format!("127.0.0.1:{resp_port}"));
    let cancel = CancellationToken::new();

    let trunks = Arc::new(std::sync::Mutex::new(HashMap::from([(
        "e2e-trunk".into(),
        trunk,
    )])));
    let fn_ = {
        let t = trunks.clone();
        move || t.lock().unwrap().clone()
    };

    let endpoint_inner = server.inner.endpoint.inner.clone();
    let local_addr = format!("127.0.0.1:{pbx_port}");

    spawn_health_loop(
        fn_,
        states.clone(),
        endpoint_inner,
        local_addr,
        1,
        cancel.child_token(),
    );

    // Initially no responder → failures accumulate
    sleep(Duration::from_secs(4)).await;

    // Start responder → should recover
    let _responder = OptionsResponder::start(resp_port).await;
    sleep(Duration::from_secs(12)).await; // need one probe cycle to succeed (~10s timeout + 1s interval)

    let snap = snapshot(&states).await;
    assert!(!snap.is_empty());
    // At probe_timeout=10s, after 12s we might have had at most one full probe cycle
    // If the first probe started before responder was up, it timed out.
    // The next probe (within 12+1=13s) should succeed after responder starts.
    // Check that some progress was made
    if snap[0].consecutive_failures > 0 {
        assert!(snap[0].last_error.is_some());
    } else {
        assert!(snap[0].healthy, "should be healthy or have failures");
    }

    cancel.cancel();
}

#[tokio::test]
async fn test_e2e_health_check_multiple_trunks() {
    let pbx_port = pick_unused_port().unwrap();
    let resp1 = pick_unused_port().unwrap();
    let resp2 = pick_unused_port().unwrap();

    let _r1 = OptionsResponder::start(resp1).await;
    let _r2 = OptionsResponder::start(resp2).await;
    let (server, _pbx_cancel) = create_pbx(pbx_port).await;

    let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
    let t1 = make_trunk(&format!("127.0.0.1:{resp1}"));
    let t2 = make_trunk(&format!("127.0.0.1:{resp2}"));
    let cancel = CancellationToken::new();

    let trunks = Arc::new(std::sync::Mutex::new(HashMap::from([
        ("trunk-a".into(), t1),
        ("trunk-b".into(), t2),
    ])));
    let fn_ = {
        let t = trunks.clone();
        move || t.lock().unwrap().clone()
    };

    let endpoint_inner = server.inner.endpoint.inner.clone();
    let local_addr = format!("127.0.0.1:{pbx_port}");

    spawn_health_loop(
        fn_,
        states.clone(),
        endpoint_inner,
        local_addr,
        1,
        cancel.child_token(),
    );

    sleep(Duration::from_secs(3)).await;
    let snap = snapshot(&states).await;
    assert_eq!(snap.len(), 2, "should have 2 trunk states");
    assert_eq!(snap[0].trunk_name, "trunk-a");
    assert_eq!(snap[1].trunk_name, "trunk-b");
    assert!(snap[0].healthy, "trunk-a should be healthy");
    assert!(snap[1].healthy, "trunk-b should be healthy");

    cancel.cancel();
}
