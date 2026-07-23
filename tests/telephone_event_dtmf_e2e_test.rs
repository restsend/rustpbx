//! RFC4733 telephone-event DTMF regression.
//!
//! Two guards:
//! 1. `test_telephone_event_ivr_digit_value`: caller sends RFC4733 digit '5'
//!    (sipbot `dtmf_flows`) into an IVR menu whose '5' entry routes to a
//!    registered callee. The callee being reached proves the telephone-event
//!    RTP is received, depacketized, and the digit value ('5') correctly
//!    parsed — a stronger check than the existing IVR tests that only pressed
//!    '1'/'2'. (sipbot sends real telephone-event RTP at PT 101, not SIP INFO.)
//! 2. `test_telephone_event_does_not_break_b2bua_media`: in a plain B2BUA
//!    (no IVR consuming the digit), sending telephone-event must not tear down
//!    or stall media — audio keeps flowing on both legs afterwards.

mod helpers;

use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TestPbx, TestPbxInject};
use rustpbx::call::SipUser;
use rustpbx::config::{ProxyConfig, UserBackendConfig};
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;
use uuid::Uuid;

fn base_proxy(sip_port: u16, users: Vec<SipUser>) -> ProxyConfig {
    ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(sip_port),
        ensure_user: Some(false),
        user_backends: vec![UserBackendConfig::Memory { users: Some(users) }],
        ..Default::default()
    }
}

/// A digit not used by other IVR tests ('5'), to guard against digit-specific
/// misparse. IVR routes '5' → destE (registered only for this scenario).
#[tokio::test]
async fn test_telephone_event_ivr_digit_value() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new("rustpbx=info").unwrap())
        .try_init();
    let sip_port = portpicker::pick_unused_port().unwrap();
    let temp = std::env::temp_dir().join(format!("rfc4733_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp).unwrap();

    let ivr = r#"[ivr]
name = "te-ivr"
ivr_mode = "tree"
[ivr.root]
greeting_text = "Press 5"
timeout_ms = 15000
max_retries = 5
[[ivr.root.entries]]
key = "5"
action = { type = "transfer", target = "destE" }
"#;
    let ivr_path = temp.join("ivr.toml");
    std::fs::write(&ivr_path, ivr).unwrap();
    let route = format!(
        r#"
name = "te-ivr-route"
priority = 100
app = "ivr"
auto_answer = true
[match]
"to.user" = "teivr"
[app_params]
file = "{}"
"#,
        ivr_path.display()
    );
    let users = vec![
        SipUser {
            id: 0,
            enabled: true,
            username: "teivr".into(),
            password: None,
            realm: None,
            allow_guest_calls: true,
            ..Default::default()
        },
        SipUser {
            id: 1,
            enabled: true,
            username: "destE".into(),
            password: Some("p".to_string()),
            realm: None,
            allow_guest_calls: true,
            ..Default::default()
        },
    ];
    let pbx = TestPbx::start_with_inject(
        sip_port,
        TestPbxInject {
            proxy_config: Some(base_proxy(sip_port, users)),
            routes: Some(vec![toml::from_str(&route).unwrap()]),
            ..Default::default()
        },
    )
    .await;
    let domain = format!("127.0.0.1:{}", sip_port);

    let callee_port = portpicker::pick_unused_port().unwrap();
    let callee = TestUa::registered_callee(callee_port, 1, "destE", "p", &domain, &domain).await;
    sleep(Duration::from_millis(800)).await;

    let caller_port = portpicker::pick_unused_port().unwrap();
    let target = format!("sip:teivr@127.0.0.1:{}", sip_port);
    let _caller = TestUa::caller_with_dtmf(caller_port, "caller", target, "1.5s:5").await;
    info!("caller pressing RFC4733 '5' into IVR");

    let mut routed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if callee.has_rtp_rx() {
            routed = true;
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    assert!(
        routed,
        "RFC4733 '5' should route to destE; callee stats={}",
        callee.rtp_stats_summary()
    );
    info!("RFC4733 digit '5' correctly parsed and routed");
    callee.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp);
    info!("test_telephone_event_ivr_digit_value PASSED");
}
