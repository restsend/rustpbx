//! E2E: IVR publish-new-version regression.
//!
//! Validates that after overwriting the IVR TOML (simulating "publish new
//! version"), subsequent calls pick up the new config. The IVR runtime reads
//! the TOML from disk on every call (`sip_session.rs:360`), so no route reload
//! is required — this test pins that behavior so a future caching change can't
//! silently break "publish new version".
//!
//! Scenario (observed via which registered sipbot receives RTP):
//!   v1 TOML: press 1 -> transfer to "destA"  (destA rings, destB silent)
//!   v2 TOML: press 1 -> transfer to "destB"  (destB rings, destA was silent)
//!
//! Run: cargo test --test ivr_publish_version_e2e_test -- --nocapture

mod helpers;

use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TestPbx, TestPbxInject};
use rustpbx::call::SipUser;
use rustpbx::config::{ProxyConfig, UserBackendConfig};
use rustpbx::proxy::routing::RouteRule;
use std::time::Duration;

fn ivr_toml(transfer_target: &str) -> String {
    format!(
        r#"[ivr]
name = "publish-version-test"
ivr_mode = "tree"

[ivr.root]
greeting = "config/sounds/hello_pcmu.wav"
timeout_ms = 8000
max_retries = 1

[[ivr.root.entries]]
key = "1"
action = {{ type = "transfer", target = "{tgt}" }}
"#,
        tgt = transfer_target
    )
}

fn build_ivr_route(ivr_path: &str) -> Vec<RouteRule> {
    let s = format!(
        r#"
name = "ivr-publish-test"
priority = 100
app = "ivr"
auto_answer = true

[match]
"to.user" = "ivr"

[app_params]
file = "{}"
"#,
        ivr_path
    );
    vec![toml::from_str(&s).expect("route")]
}

fn sip_user(username: &str) -> SipUser {
    SipUser {
        id: 0,
        enabled: true,
        username: username.to_string(),
        password: Some("pass".to_string()),
        realm: None,
        allow_guest_calls: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn test_publish_v1_then_v2_uses_latest_toml() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("sip port");
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_ivr_pub_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).expect("temp dir");
    let ivr_path = temp_dir.join("ivr-publish.toml");
    let ivr_path_str = ivr_path.to_string_lossy().to_string();

    // ── v1: press 1 -> transfer to "destA" ──────────────────────────────
    std::fs::write(&ivr_path, ivr_toml("destA")).expect("write v1 toml");

    let proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(sip_port),
        ensure_user: Some(false),
        // ivr = guest (no password); destA/destB = registered with "pass".
        user_backends: vec![UserBackendConfig::Memory {
            users: Some(vec![
                SipUser {
                    id: 0,
                    enabled: true,
                    username: "ivr".to_string(),
                    password: None,
                    realm: None,
                    allow_guest_calls: true,
                    ..Default::default()
                },
                sip_user("destA"),
                sip_user("destB"),
            ]),
        }],
        ..Default::default()
    };

    let inject = TestPbxInject {
        proxy_config: Some(proxy_config),
        routes: Some(build_ivr_route(&ivr_path_str)),
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(sip_port, inject).await;

    let domain = format!("127.0.0.1:{}", sip_port);
    let proxy_addr = format!("127.0.0.1:{}", sip_port);

    // Register both dest sipbots up front (they need valid credentials).
    let dest_a_port = portpicker::pick_unused_port().expect("destA port");
    let dest_b_port = portpicker::pick_unused_port().expect("destB port");
    let dest_a =
        TestUa::registered_callee(dest_a_port, 2, "destA", "pass", &domain, &proxy_addr).await;
    let dest_b =
        TestUa::registered_callee(dest_b_port, 2, "destB", "pass", &domain, &proxy_addr).await;
    // Let registrations settle.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ── Call #1 against v1 TOML -> destA should answer ──────────────────
    let caller1_port = portpicker::pick_unused_port().expect("caller1 port");
    let target = format!("sip:ivr@127.0.0.1:{}", sip_port);
    let caller1 = TestUa::caller_with_dtmf(caller1_port, "caller1", target.clone(), "2s:1").await;
    tokio::time::sleep(Duration::from_secs(12)).await;

    assert!(
        dest_a.has_rtp_rx(),
        "v1: destA should have RX RTP (TOML transfers to destA). stats: {}",
        dest_a.rtp_stats_summary()
    );
    assert!(
        !dest_b.has_rtp_rx(),
        "v1: destB should have NO RTP (TOML targets destA)"
    );
    tracing::info!("v1 OK: destA received the transferred call");
    caller1.stop();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // ── Publish v2: overwrite TOML -> press 1 -> transfer to "destB" ─────
    std::fs::write(&ivr_path, ivr_toml("destB")).expect("write v2 toml");
    tracing::info!("Published v2 TOML (transfer target -> destB)");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Call #2 against v2 TOML -> destB should answer ──────────────────
    let caller2_port = portpicker::pick_unused_port().expect("caller2 port");
    let caller2 = TestUa::caller_with_dtmf(caller2_port, "caller2", target, "2s:1").await;
    tokio::time::sleep(Duration::from_secs(12)).await;

    assert!(
        dest_b.has_rtp_rx(),
        "v2: destB should have RX RTP after publish (TOML now transfers to destB). stats: {}",
        dest_b.rtp_stats_summary()
    );
    tracing::info!("v2 OK: destB received the transferred call after publish");

    dest_a.stop();
    dest_b.stop();
    caller2.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);
}
