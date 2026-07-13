//! IVR multi-key DTMF routing regression.
//!
//! For each digit 1/2/3, register ONLY the matching callee, then have a caller
//! dial the IVR and press that digit. Correct routing → the callee answers
//! (has_rtp_rx). A misparse would target an unregistered name and the call
//! would fail to connect. sipbot's `dtmf_flows` sends RFC4733 telephone-event
//! RTP, so this also exercises telephone-event reception → IVR `on_dtmf`.

mod helpers;

use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TestPbx, TestPbxInject};
use rustpbx::call::SipUser;
use rustpbx::config::{ProxyConfig, UserBackendConfig};
use rustpbx::proxy::routing::RouteRule;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;
use uuid::Uuid;

fn build_ivr_toml() -> String {
    r#"[ivr]
name = "multickey-ivr"
ivr_mode = "tree"

[ivr.root]
greeting_text = "Press 1 2 or 3"
timeout_ms = 15000
max_retries = 5

[[ivr.root.entries]]
key = "1"
action = { type = "transfer", target = "destA" }

[[ivr.root.entries]]
key = "2"
action = { type = "transfer", target = "destB" }

[[ivr.root.entries]]
key = "3"
action = { type = "transfer", target = "destC" }
"#
    .to_string()
}

#[tokio::test]
async fn test_ivr_multi_key_dtmf_routing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new("rustpbx=info").unwrap())
        .try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_ivr_dtmf_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let ivr_path = temp_dir.join("ivr.toml");
    std::fs::write(&ivr_path, build_ivr_toml()).unwrap();

    let route_toml = format!(
        r#"
name = "multickey-ivr-route"
priority = 100
app = "ivr"
auto_answer = true

[match]
"to.user" = "multickey"

[app_params]
file = "{}"
"#,
        ivr_path.display()
    );
    let routes: Vec<RouteRule> = vec![toml::from_str(&route_toml).expect("route")];

    let proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(sip_port),
        ensure_user: Some(false),
        user_backends: vec![UserBackendConfig::Memory {
            users: Some(vec![
                SipUser {
                    id: 0,
                    enabled: true,
                    username: "multickey".to_string(),
                    password: None,
                    realm: None,
                    allow_guest_calls: true,
                    ..Default::default()
                },
                SipUser {
                    id: 1,
                    enabled: true,
                    username: "destA".to_string(),
                    password: Some("p".to_string()),
                    realm: None,
                    allow_guest_calls: true,
                    ..Default::default()
                },
                SipUser {
                    id: 2,
                    enabled: true,
                    username: "destB".to_string(),
                    password: Some("p".to_string()),
                    realm: None,
                    allow_guest_calls: true,
                    ..Default::default()
                },
                SipUser {
                    id: 3,
                    enabled: true,
                    username: "destC".to_string(),
                    password: Some("p".to_string()),
                    realm: None,
                    allow_guest_calls: true,
                    ..Default::default()
                },
            ]),
        }],
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(
        sip_port,
        TestPbxInject {
            proxy_config: Some(proxy_config),
            routes: Some(routes),
            ..Default::default()
        },
    )
    .await;
    let domain = format!("127.0.0.1:{}", sip_port);

    // Each digit routes to a distinct target. For each scenario we register
    // ONLY the expected callee — a misroute to either other name (unregistered)
    // would fail to connect, so callee.has_rtp_rx proves correct digit parsing.
    for (digit, target_user) in [("1", "destA"), ("2", "destB"), ("3", "destC")] {
        let callee_port = portpicker::pick_unused_port().unwrap();
        let callee =
            TestUa::registered_callee(callee_port, 1, target_user, "p", &domain, &domain).await;
        sleep(Duration::from_millis(800)).await; // registration settle

        let caller_port = portpicker::pick_unused_port().unwrap();
        let target = format!("sip:multickey@127.0.0.1:{}", sip_port);
        let dtmf = format!("1.5s:{}", digit);
        let caller = TestUa::caller_with_dtmf(caller_port, "caller", target, &dtmf).await;
        info!(digit, target_user, "scenario: press digit, expect route");

        // answer → greeting → DTMF → transfer → callee answer → RTP
        let mut routed = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            if callee.has_rtp_rx() {
                routed = true;
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
        assert!(
            routed,
            "digit '{}' should route to registered callee {}; callee stats={}",
            digit, target_user, callee.rtp_stats_summary()
        );
        info!("digit '{}' routed to {}", digit, target_user);
        caller.stop();
        callee.stop();
        sleep(Duration::from_millis(400)).await;
    }

    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);
    info!("test_ivr_multi_key_dtmf_routing PASSED");
}
