//! F4 repro (acceptance 2026-11-16 case 3101bb84d47f02e6):
//! agent outbound -> customer answers -> single-step BLIND transfer to the
//! facade step-IVR (88880002). The facade answers the first step with a
//! `voip_bridge` TTS room carrying `return_app=ivr&return_target=<ivr>`
//! (its announcement step); the bridge closes when the announcement ends
//! ("Bridge WS closed remotely" in the acceptance log). Contract: the step
//! tree must CONTINUE (the provider is asked for the next step); the
//! customer must NOT be hung up at the bridge close.
//!
//! Defect: `handle_start_return_app`'s `caller_alive` guard conflates "the
//! transferor's caller leg was retired by the outbound hand-off" with "the
//! caller died while suspended" — it drains the pending hangup onto the
//! SURVIVING customer and skips the return app entirely, so the second
//! `/ivr/step` call never happens and the customer is dropped right after
//! the announcement.

use anyhow::Result;
use futures::StreamExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;

use rustpbx::call::user::SipUser;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::routing::{MatchConditions, RouteAction, RouteRule};

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent, create_test_sdp};

const IVR_NAME: &str = "f4-step-ivr";
const IVR_NUMBER: &str = "8888";
const BRIDGE_HOLD_MS: u64 = 1500;

// ── step provider mock ────────────────────────────────────────────────────

struct StepProviderMock {
    url: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl StepProviderMock {
    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

async fn start_step_provider(script: Vec<serde_json::Value>) -> StepProviderMock {
    let idx = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let script = Arc::new(script);

    let idx_step = idx.clone();
    let script_step = script.clone();
    let requests_step = requests.clone();
    let app = axum::Router::new()
        .route(
            "/ivr/step",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let idx = idx_step.clone();
                let script = script_step.clone();
                let requests = requests_step.clone();
                async move {
                    requests.lock().unwrap().push(body);
                    let i = idx.fetch_add(1, Ordering::SeqCst);
                    let action = script
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "hangup"}));
                    axum::Json(action)
                }
            }),
        )
        .route(
            "/ivr/step/start",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        )
        .route(
            "/ivr/step/end",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    StepProviderMock {
        url: format!("http://127.0.0.1:{port}/ivr/step"),
        requests,
    }
}

// ── WS TTS bridge mock (the facade's announcement bridge) ────────────────

async fn start_ws_bridge(hold_ms: u64) -> (u16, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let conns = Arc::new(AtomicUsize::new(0));
    let c = conns.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            c.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                    eprintln!("[f4-mock] ws bridge accepted");
                    // Drain frames, then close — the facade ends the bridge
                    // when the announcement audio finishes.
                    let _ = tokio::time::timeout(Duration::from_millis(hold_ms), async {
                        while let Some(Ok(_)) = ws.next().await {}
                    })
                    .await;
                    eprintln!("[f4-mock] ws bridge closing (hold elapsed)");
                    let _ = ws.close(None).await;
                    eprintln!("[f4-mock] ws bridge close sent");
                }
            });
        }
    });
    (port, conns)
}

// ── harness ───────────────────────────────────────────────────────────────

fn step_ivr_toml(provider_url: &str) -> String {
    format!(
        r#"[ivr]
name = "{IVR_NAME}"
ivr_mode = "step"

[ivr.provider]
url = "{provider_url}"
max_retries = 2
retry_delay_ms = 200
timeout_secs = 5
"#
    )
}

async fn start_harness(ivr_dir: &Path, ivr_file: &Path) -> Result<E2eTestServer> {
    let mut proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(0),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;
    // Anchored media — the acceptance topology (WSS agent leg <-> trunk leg)
    // always anchors; a direct-relay harness never creates per-leg media
    // objects and the voip_bridge has nothing to play into.
    proxy_config.media_proxy = rustpbx::config::MediaProxyMode::All;
    // return_target resolves through the ivr dir by name.
    proxy_config.ivr_dir = Some(ivr_dir.to_string_lossy().to_string());
    proxy_config.routes = Some(vec![RouteRule {
        name: "route_to_f4_step_ivr".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(IVR_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({
                "file": ivr_file.to_string_lossy(),
            })),
            ..Default::default()
        },
        ..Default::default()
    }]);

    let users = vec![
        SipUser {
            id: 1,
            username: "alice".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 2,
            username: "bob".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
    ];

    E2eTestServer::start_with_inject(
        proxy_config,
        E2eTestServerInject {
            users,
            ..Default::default()
        },
    )
    .await
}

async fn make_registered_ua(server: &E2eTestServer, username: &str) -> Result<TestUa> {
    let mut ua = TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(28400),
        proxy_addr: server.proxy_addr,
    });
    ua.start().await?;
    ua.register().await?;
    sleep(Duration::from_millis(300)).await;
    Ok(ua)
}

async fn answer_next_incoming(
    ua: &TestUa,
    sdp: &str,
    secs: u64,
) -> Result<rsipstack::dialog::DialogId> {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        for event in ua.process_dialog_events().await? {
            if let TestUaEvent::IncomingCall(id, _) = event {
                ua.answer_call(&id, Some(sdp.to_string())).await?;
                return Ok(id);
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("no incoming call within {secs}s")
}

#[tokio::test]
async fn blind_transfer_to_step_ivr_bridge_close_must_continue_the_tree() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // ── script: announcement bridge (return_ivr_resume) -> hangup ────
    let (ws_port, ws_conns) = start_ws_bridge(BRIDGE_HOLD_MS).await;
    let step0 = serde_json::json!({
        "type": "voip_bridge",
        "create_room_uri": format!("ws://127.0.0.1:{ws_port}/tts"),
        "return_app": "ivr",
        "return_target": IVR_NAME,
        "timeout_ms": 60000,
    });
    let step1 = serde_json::json!({"type": "hangup"});
    let provider = start_step_provider(vec![step0, step1]).await;

    let ivr_dir = std::env::temp_dir().join(format!(
        "f4-step-ivr-{}",
        portpicker::pick_unused_port().unwrap_or(43200)
    ));
    std::fs::create_dir_all(&ivr_dir)?;
    let ivr_file = ivr_dir.join(format!("{IVR_NAME}.toml"));
    std::fs::write(&ivr_file, step_ivr_toml(&provider.url))?;

    let server = start_harness(&ivr_dir, &ivr_file).await?;
    let alice = make_registered_ua(&server, "alice").await?;
    let bob = make_registered_ua(&server, "bob").await?;
    let sdp = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30500),
        false,
    );
    // G729 answer for the customer leg — alice (PCMU) and bob (G729)
    // transcode, which materializes the per-leg media objects the bridge
    // plays into (a pure PCMU/PCMU direct-relay pair has none).
    let bob_sdp = format!(
        "v=0\r\no=testua 1 1 IN IP4 127.0.0.1\r\ns=Test Call\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 18\r\na=rtpmap:18 G729/8000\r\na=sendrecv\r\n",
        portpicker::pick_unused_port().unwrap_or(30550)
    );

    // ── agent outbound -> customer answers ───────────────────────────
    let dial = rustpbx::utils::spawn({
        let a = alice.clone();
        let sdp = sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await.unwrap() }
    });
    let bob_dialog = answer_next_incoming(&bob, &bob_sdp, 10).await?;
    let alice_dialog = dial.await.expect("agent dial must complete");
    sleep(Duration::from_millis(500)).await;
    bob.set_answer_sdp(&bob_dialog, &bob_sdp).await;

    // ── single-step blind transfer to the step IVR ───────────────────
    let target = format!("sip:{IVR_NUMBER}@{}", server.proxy_addr);
    assert_eq!(alice.send_refer(&alice_dialog, &target).await?, 202);

    // The real client BYEs its own leg once the transfer's final NOTIFY
    // reports success (the acceptance log's "Removing leg from session
    // leg_id=caller") — mirror that so the CALLER-TRANSFEROR leg is gone
    // before the announcement bridge closes.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut refer_ok = false;
    while !refer_ok && std::time::Instant::now() < deadline {
        for event in alice.process_dialog_events().await? {
            if let TestUaEvent::ReferNotify(_, body, state) = event {
                if state.starts_with("terminated") && body.contains("200") {
                    refer_ok = true;
                }
            }
        }
        if !refer_ok {
            sleep(Duration::from_millis(50)).await;
        }
    }
    assert!(refer_ok, "the transferor must get the final NOTIFY 200");
    alice.hangup(&alice_dialog).await?;
    sleep(Duration::from_millis(300)).await;

    // the announcement bridge must be entered
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while ws_conns.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        ws_conns.load(Ordering::SeqCst),
        1,
        "the announcement bridge must connect"
    );
    assert_eq!(provider.request_count(), 1, "step 1 asked once");

    // ── wait for the bridge close, then for the resumed step ─────────
    // (bridge closes at ~BRIDGE_HOLD_MS; the resumed step must follow)
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while provider.request_count() < 2 && std::time::Instant::now() < deadline {
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        provider.request_count() >= 2,
        "the step tree must CONTINUE after the announcement bridge closes \
         (the facade is asked for the next step); got {} request(s) — the \
         return-app path hung up the surviving customer instead",
        provider.request_count()
    );

    // ── the customer ends via the TREE (step 2 = hangup), not the ────
    // ── return-app pending-hangup cascade ────────────────────────────
    let mut bob_terminated = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while !bob_terminated && std::time::Instant::now() < deadline {
        for event in bob.process_dialog_events().await? {
            if matches!(event, TestUaEvent::CallTerminated(_)) {
                bob_terminated = true;
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        bob_terminated,
        "the customer must be released by the resumed tree's hangup step"
    );

    server.stop();
    Ok(())
}
