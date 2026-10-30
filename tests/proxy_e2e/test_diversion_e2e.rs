//! Diversion / History-Info rendering on forwarded calls (RFC 5806 / RFC 7044).
//!
//! The diversion chain is seeded from the inbound INVITE's `Diversion`
//! header and re-rendered onto the trunk INVITE when the resolved policy
//! (global `[proxy]` flags with per-trunk overrides) enables it. Default is
//! off: interconnect peers only see the headers when they opt in.

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_helpers::pcmu_sdp;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::common::wait::wait_for_value;
use anyhow::Result;
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::proxy::routing::{DestConfig, MatchConditions, RouteAction, RouteRule, TrunkConfig};
use std::sync::Arc;
use std::time::Duration;

const CALLER_DIVERSION: &str = "<sip:origin@example.net>;reason=unconditional;counter=1";

struct Policy {
    global_diversion: bool,
    global_history: bool,
    trunk_diversion: Option<bool>,
    trunk_history: Option<bool>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            global_diversion: false,
            global_history: false,
            trunk_diversion: None,
            trunk_history: None,
        }
    }
}

/// alice dials `91300000000` with a `Diversion` header on her INVITE; the
/// route forwards to the `carrier` trunk whose peer is a bare TestUa. Returns
/// the Diversion / History-Info headers the carrier received.
async fn call_through_carrier(policy: Policy) -> Result<(Option<String>, Option<String>)> {
    let carrier_port = portpicker::pick_unused_port().expect("free port");
    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(portpicker::pick_unused_port().expect("free port")),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        media_proxy: MediaProxyMode::All,
        diversion_headers: policy.global_diversion,
        history_info: policy.global_history,
        ..Default::default()
    };
    config.trunks.insert(
        "carrier".to_string(),
        TrunkConfig {
            dest: format!("127.0.0.1:{carrier_port}"),
            diversion_headers: policy.trunk_diversion,
            history_info: policy.trunk_history,
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "to-carrier".to_string(),
        match_conditions: MatchConditions {
            to_user: Some("^913".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("forward".to_string()),
            dest: Some(DestConfig::Single("carrier".to_string())),
            ..Default::default()
        },
        ..Default::default()
    }]);
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let mut carrier = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "carrier".to_string(),
        password: String::new(),
        realm: "127.0.0.1".to_string(),
        local_port: carrier_port,
        proxy_addr: server.proxy_addr,
    });
    carrier.start().await?;
    let alice = Arc::new(server.create_ua("alice").await?);

    let invite_headers = vec![("Diversion".to_string(), CALLER_DIVERSION.to_string())];
    let alice_clone = alice.clone();
    let caller_handle = rustpbx::utils::spawn(async move {
        alice_clone
            .make_call_with_headers(
                "91300000000",
                Some(pcmu_sdp("127.0.0.1", 40000)),
                invite_headers,
            )
            .await
    });

    let carrier_id = wait_for_value(Duration::from_secs(5), || async {
        carrier
            .process_dialog_events()
            .await
            .ok()?
            .into_iter()
            .find_map(|event| match event {
                TestUaEvent::IncomingCall(id, _) => Some(id),
                _ => None,
            })
    })
    .await
    .map_err(|_| anyhow::anyhow!("carrier never got the INVITE"))?;

    carrier
        .send_ringing(&carrier_id, Some(pcmu_sdp("127.0.0.1", 40010)))
        .await?;
    carrier
        .answer_call(&carrier_id, Some(pcmu_sdp("127.0.0.1", 40010)))
        .await?;

    let caller_id = tokio::time::timeout(Duration::from_secs(8), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("alice's INVITE timed out"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;

    let diversion = carrier
        .incoming_invite_header(&carrier_id, "Diversion")
        .await;
    let history = carrier
        .incoming_invite_header(&carrier_id, "History-Info")
        .await;

    alice.hangup(&caller_id).await.ok();
    server.stop();
    Ok((diversion, history))
}

#[tokio::test]
async fn diversion_absent_when_disabled() -> Result<()> {
    let (diversion, history) = call_through_carrier(Policy::default()).await?;
    assert!(
        diversion.is_none(),
        "default policy must not render Diversion, got {diversion:?}"
    );
    assert!(
        history.is_none(),
        "default policy must not render History-Info, got {history:?}"
    );
    Ok(())
}

#[tokio::test]
async fn trunk_flags_render_diversion_and_history_info() -> Result<()> {
    let (diversion, history) = call_through_carrier(Policy {
        trunk_diversion: Some(true),
        trunk_history: Some(true),
        ..Default::default()
    })
    .await?;

    let diversion = diversion.expect("trunk flag on: Diversion must be rendered");
    assert!(
        diversion.contains("<sip:origin@example.net>;reason=unconditional;counter=1"),
        "re-rendered Diversion, got: {diversion}"
    );

    let history = history.expect("trunk flag on: History-Info must be rendered");
    assert!(
        history.contains("<sip:origin@example.net>;index=1"),
        "first hi-entry is the diverting party, got: {history}"
    );
    assert!(
        history.contains("index=1.1"),
        "final target closes the branch, got: {history}"
    );
    Ok(())
}

#[tokio::test]
async fn global_flags_render_when_trunk_unspecified() -> Result<()> {
    let (diversion, _) = call_through_carrier(Policy {
        global_diversion: true,
        global_history: false,
        ..Default::default()
    })
    .await?;
    let diversion = diversion.expect("global flag on: Diversion must be rendered");
    assert!(diversion.contains("reason=unconditional;counter=1"));
    Ok(())
}

/// Call-forward-on-busy: alice → bob, bob's phone rejects with 486, the
/// session chains to bob's `when_busy` target (charlie). Charlie's INVITE
/// carries the diversion hop with `reason=user-busy` and a History-Info
/// entry whose Reason cause is 486.
#[tokio::test]
async fn call_forward_on_busy_appends_diversion_hop() -> Result<()> {
    use rustpbx::call::user::SipUser;
    use rustpbx::config::MediaProxyMode;

    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(portpicker::pick_unused_port().expect("free port")),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        media_proxy: MediaProxyMode::All,
        diversion_headers: true,
        history_info: true,
        ..Default::default()
    };
    config.ensure_user = Some(false);

    let mut bob = SipUser {
        id: 2,
        username: "bob".to_string(),
        password: Some("password456".to_string()),
        enabled: true,
        realm: Some("127.0.0.1".to_string()),
        is_support_webrtc: false,
        voicemail_disabled: true,
        ..Default::default()
    };
    bob.call_forwarding_mode = Some("when_busy".to_string());
    bob.call_forwarding_destination = Some("charlie".to_string());

    let server = Arc::new(
        E2eTestServer::start_with_inject(
            config,
            E2eTestServerInject {
                users: vec![
                    SipUser {
                        id: 1,
                        username: "alice".to_string(),
                        password: Some("password123".to_string()),
                        enabled: true,
                        realm: Some("127.0.0.1".to_string()),
                        is_support_webrtc: false,
                        voicemail_disabled: true,
                        ..Default::default()
                    },
                    bob,
                    SipUser {
                        id: 3,
                        username: "charlie".to_string(),
                        password: Some("password789".to_string()),
                        enabled: true,
                        realm: Some("127.0.0.1".to_string()),
                        is_support_webrtc: false,
                        voicemail_disabled: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        )
        .await?,
    );

    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    let charlie = server.create_ua("charlie").await?;

    let caller_handle = rustpbx::utils::spawn({
        let alice = alice.clone();
        async move {
            alice
                .make_call("bob", Some(pcmu_sdp("127.0.0.1", 40000)))
                .await
        }
    });

    // Bob's phone rings and rejects with 486 Busy Here.
    let bob_id = wait_for_value(Duration::from_secs(5), || async {
        bob.process_dialog_events()
            .await
            .ok()?
            .into_iter()
            .find_map(|event| match event {
                TestUaEvent::IncomingCall(id, _) => Some(id),
                _ => None,
            })
    })
    .await
    .map_err(|_| anyhow::anyhow!("bob never got the INVITE"))?;
    bob.reject_call_with_reason(&bob_id, Some(486), None)
        .await?;

    // The session forwards to charlie with the busy diversion hop.
    let charlie_id = wait_for_value(Duration::from_secs(8), || async {
        charlie
            .process_dialog_events()
            .await
            .ok()?
            .into_iter()
            .find_map(|event| match event {
                TestUaEvent::IncomingCall(id, _) => Some(id),
                _ => None,
            })
    })
    .await
    .map_err(|_| anyhow::anyhow!("charlie never got the forwarded INVITE"))?;

    let diversion = charlie
        .incoming_invite_header(&charlie_id, "Diversion")
        .await
        .expect("forwarded INVITE must carry Diversion");
    assert!(
        diversion.contains("sip:bob@") && diversion.contains("reason=user-busy"),
        "diverting party is bob with reason=user-busy, got: {diversion}"
    );
    let history = charlie
        .incoming_invite_header(&charlie_id, "History-Info")
        .await
        .expect("forwarded INVITE must carry History-Info");
    assert!(
        history.contains("cause%3D486") || history.contains("cause=486"),
        "History-Info Reason carries the busy cause, got: {history}"
    );

    charlie
        .answer_call(&charlie_id, Some(pcmu_sdp("127.0.0.1", 40020)))
        .await?;
    let caller_id = tokio::time::timeout(Duration::from_secs(8), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("alice's INVITE timed out"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;

    alice.hangup(&caller_id).await.ok();
    server.stop();
    Ok(())
}
