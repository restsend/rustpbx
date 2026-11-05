//! Custom SDP attribute pass-through across the trunk B2BUA.
//!
//! Vendor-specific `a=` lines must survive the offer/answer rewrite in both
//! directions by default, and must be stripped when either the source or the
//! destination trunk sets `passthrough_custom_sdp_attrs = false` (AND
//! semantics). Covered here: anchored mode (SDP rebuilt by the local
//! PeerConnection — attrs are copied in explicitly), bypass mode (SDP is
//! relayed verbatim — attrs are stripped on opt-out), destination-trunk and
//! source-trunk gates.

use anyhow::Result;
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::proxy::routing::{DestConfig, MatchConditions, RouteAction, RouteRule, TrunkConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::rtp_utils::RtpReceiver;
use crate::common::test_helpers::make_sdp;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::common::wait::wait_for_value;

const CALLER_SESSION_ATTR: &str = "a=x-caller-tag:abc123";
const CALLER_MEDIA_ATTR: &str = "a=x-media-flag:on";
const CARRIER_ATTR: &str = "a=x-carrier-tag:xyz789";

/// Caller SDP with one session-level and one media-level custom attribute.
fn caller_sdp_with_custom_attrs(port: u16) -> String {
    let mut sdp = make_sdp(port);
    // Session-level: before the m-line.
    sdp = sdp.replace(
        "t=0 0\r\n",
        &format!("t=0 0\r\n{}\r\n", CALLER_SESSION_ATTR),
    );
    // Media-level: after the last rtpmap line.
    sdp = sdp.replace(
        "a=sendrecv\r\n",
        &format!("{}\r\na=sendrecv\r\n", CALLER_MEDIA_ATTR),
    );
    sdp
}

/// Carrier answer SDP with a session-level custom attribute.
fn carrier_sdp_with_custom_attr(port: u16) -> String {
    make_sdp(port).replace(
        "a=sendrecv\r\n",
        &format!("{}\r\na=sendrecv\r\n", CARRIER_ATTR),
    )
}

struct CallOutcome {
    /// SDP of the INVITE the carrier (trunk far-end) received.
    carrier_invite_sdp: String,
    /// Answer SDP the caller received in the 200 OK.
    caller_answer_sdp: Option<String>,
}

/// Drives alice → route rule → carrier_trunk → carrier UA and returns the
/// SDP observed at both ends. `media_proxy` selects anchored (All) vs bypass
/// (None); `source_disabled` / `dest_disabled` set the per-trunk gates.
async fn run_trunk_call(
    media_proxy: MediaProxyMode,
    dest_disabled: bool,
    source_disabled: bool,
) -> Result<CallOutcome> {
    let carrier_port = portpicker::pick_unused_port().unwrap_or(26200);

    let mut trunks = HashMap::new();
    trunks.insert(
        "carrier_trunk".to_string(),
        TrunkConfig {
            dest: format!("sip:127.0.0.1:{}", carrier_port),
            passthrough_custom_sdp_attrs: Some(!dest_disabled),
            inbound_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        },
    );
    // A dedicated inbound (source) trunk matching every local INVITE by IP.
    // The explicit `id` pins it ahead of `carrier_trunk` (also a 127.0.0.1
    // candidate via its dest) in the inbound-trunk trie, so the source gate
    // below is the one that merges into the call hints.
    trunks.insert(
        "source_trunk".to_string(),
        TrunkConfig {
            dest: "sip:127.0.0.1:1".to_string(),
            direction: Some(rustpbx::proxy::routing::TrunkDirection::Inbound),
            inbound_hosts: vec!["127.0.0.1".to_string()],
            passthrough_custom_sdp_attrs: Some(!source_disabled),
            id: Some(1),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "route_to_carrier".to_string(),
        priority: 1,
        match_conditions: MatchConditions {
            to_user: Some("^5100\\d{6}$".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("forward".to_string()),
            dest: Some(DestConfig::Single("carrier_trunk".to_string())),
            ..Default::default()
        },
        ..Default::default()
    }];

    let config = ProxyConfig {
        media_proxy,
        trunks,
        routes: Some(routes),
        ..Default::default()
    };

    // The acl module owns inbound source-trunk resolution
    // (`is_from_trunk_context`): without it, no TrunkContext is attached to
    // the INVITE and the source-trunk gate never merges into the call hints.
    let server = Arc::new(
        E2eTestServer::start_with_inject(
            config,
            E2eTestServerInject {
                register_acl: true,
                ..Default::default()
            },
        )
        .await?,
    );

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

    let caller_receiver = RtpReceiver::bind(0).await?;
    let carrier_receiver = RtpReceiver::bind(0).await?;

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp_with_custom_attrs(caller_receiver.port()?);
        async move { a.make_call("5100123456", Some(sdp)).await }
    });

    let (carrier_dialog_id, carrier_invite_sdp) = wait_for_value(Duration::from_secs(5), || async {
        match carrier.process_dialog_events().await {
            Ok(events) => events.into_iter().find_map(|event| match event {
                TestUaEvent::IncomingCall(id, sdp) => Some(Ok((id, sdp))),
                _ => None,
            }),
            Err(error) => Some(Err(error)),
        }
    })
    .await
    .expect("carrier should receive the forwarded call")?;
    let carrier_invite_sdp = carrier_invite_sdp.expect("forwarded call should contain SDP");
    carrier
        .answer_call(
            &carrier_dialog_id,
            Some(carrier_sdp_with_custom_attr(carrier_receiver.port()?)),
        )
        .await?;

    // Wait for the 200 OK to reach the caller.
    let dialog_id = tokio::time::timeout(Duration::from_secs(5), caller).await???;
    let caller_answer_sdp = Some(
        wait_for_value(Duration::from_secs(2), || {
            alice.get_negotiated_answer_sdp(&dialog_id)
        })
        .await
        .expect("caller should receive the 200 OK answer"),
    );
    alice.hangup(&dialog_id).await?;

    server.stop();
    Ok(CallOutcome {
        carrier_invite_sdp,
        caller_answer_sdp,
    })
}

/// Default (no trunk gate): custom attributes cross the B2BUA in both
/// directions in anchored mode, at their original SDP level, and standard
/// attributes stay intact.
#[tokio::test]
async fn anchored_passes_custom_attrs_through_both_directions() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let outcome = run_trunk_call(MediaProxyMode::All, false, false).await?;

    let invite = &outcome.carrier_invite_sdp;
    assert!(
        invite.contains(CALLER_SESSION_ATTR),
        "session-level custom attr must reach the trunk offer:\n{invite}"
    );
    assert!(
        invite.contains(CALLER_MEDIA_ATTR),
        "media-level custom attr must reach the trunk offer:\n{invite}"
    );
    // Placement: session-level attr stays in the session header block.
    let session_block = invite.split("m=audio").next().unwrap();
    assert!(
        session_block.contains(CALLER_SESSION_ATTR),
        "session attr must stay session-level:\n{invite}"
    );
    assert!(
        invite.contains("a=rtpmap:0 PCMU/8000"),
        "standard attributes must survive the rewrite:\n{invite}"
    );

    let answer = outcome
        .caller_answer_sdp
        .expect("caller should receive the 200 OK answer");
    assert!(
        answer.contains(CARRIER_ATTR),
        "carrier custom attr must reach the caller answer:\n{answer}"
    );
    assert!(
        answer.contains("a=rtpmap:0 PCMU/8000"),
        "standard attributes must survive the answer rewrite:\n{answer}"
    );
    Ok(())
}

/// Destination trunk gate off: the trunk offer carries no caller custom
/// attrs, and the caller answer carries no carrier custom attrs.
#[tokio::test]
async fn destination_trunk_disabled_strips_custom_attrs() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let outcome = run_trunk_call(MediaProxyMode::All, true, false).await?;

    let invite = &outcome.carrier_invite_sdp;
    assert!(
        !invite.contains("x-caller-tag"),
        "caller session attr must be stripped towards the trunk:\n{invite}"
    );
    assert!(
        !invite.contains("x-media-flag"),
        "caller media attr must be stripped towards the trunk:\n{invite}"
    );
    assert!(
        invite.contains("a=rtpmap:0 PCMU/8000"),
        "standard attributes must survive stripping:\n{invite}"
    );

    let answer = outcome
        .caller_answer_sdp
        .expect("caller should receive the 200 OK answer");
    assert!(
        !answer.contains("x-carrier-tag"),
        "carrier attr must be stripped towards the caller:\n{answer}"
    );
    assert!(
        answer.contains("a=rtpmap:0 PCMU/8000"),
        "standard attributes must survive the answer rewrite:\n{answer}"
    );
    Ok(())
}

/// Source trunk gate off (destination default): AND semantics — the call is
/// stripped in both directions even though the destination trunk allows it.
#[tokio::test]
async fn source_trunk_disabled_strips_custom_attrs() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let outcome = run_trunk_call(MediaProxyMode::All, false, true).await?;

    let invite = &outcome.carrier_invite_sdp;
    assert!(
        !invite.contains("x-caller-tag"),
        "source trunk off must strip caller attrs towards the trunk:\n{invite}"
    );
    let answer = outcome
        .caller_answer_sdp
        .expect("caller should receive the 200 OK answer");
    assert!(
        !answer.contains("x-carrier-tag"),
        "source trunk off must strip carrier attrs towards the caller:\n{answer}"
    );
    Ok(())
}

/// Bypass mode relays SDP verbatim: custom attrs flow through by default.
#[tokio::test]
async fn bypass_passes_custom_attrs_through_by_default() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let outcome = run_trunk_call(MediaProxyMode::None, false, false).await?;

    let invite = &outcome.carrier_invite_sdp;
    assert!(
        invite.contains(CALLER_SESSION_ATTR),
        "bypass offer must carry the caller session attr verbatim:\n{invite}"
    );
    assert!(
        invite.contains(CALLER_MEDIA_ATTR),
        "bypass offer must carry the caller media attr verbatim:\n{invite}"
    );
    let answer = outcome
        .caller_answer_sdp
        .expect("caller should receive the 200 OK answer");
    assert!(
        answer.contains(CARRIER_ATTR),
        "bypass answer must carry the carrier attr verbatim:\n{answer}"
    );
    Ok(())
}

/// Bypass mode + destination trunk gate off: both relay directions stripped.
#[tokio::test]
async fn bypass_destination_disabled_strips_custom_attrs() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let outcome = run_trunk_call(MediaProxyMode::None, true, false).await?;

    let invite = &outcome.carrier_invite_sdp;
    assert!(
        !invite.contains("x-caller-tag"),
        "bypass offer must strip caller attrs when the trunk opts out:\n{invite}"
    );
    assert!(
        !invite.contains("x-media-flag"),
        "bypass offer must strip caller media attrs when the trunk opts out:\n{invite}"
    );
    let answer = outcome
        .caller_answer_sdp
        .expect("caller should receive the 200 OK answer");
    assert!(
        !answer.contains("x-carrier-tag"),
        "bypass answer must strip carrier attrs when the trunk opts out:\n{answer}"
    );
    assert!(
        answer.contains("a=rtpmap:0 PCMU/8000"),
        "standard attributes must survive bypass stripping:\n{answer}"
    );
    Ok(())
}
