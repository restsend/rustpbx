//! Bidirectional custom-header passthrough through a destination trunk.
//!
//! The destination trunk's `header_passthrough` rule governs both the
//! caller's INVITE headers copied onto the trunk INVITE and the trunk's
//! 18x/200 headers copied back onto the caller's responses.

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::test_helpers::pcmu_sdp;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use anyhow::Result;
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::proxy::routing::{
    DestConfig, HeaderPassthrough, HeaderPassthroughMode, MatchConditions, RouteAction, RouteRule,
    TrunkConfig,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

struct Observed {
    /// Headers of the INVITE the carrier received.
    invite: Vec<(String, Option<String>)>,
    /// Header values alice saw on 1xx responses.
    provisional: Vec<(String, Vec<String>)>,
    /// Header values alice saw on the 200 OK.
    answer: Vec<(String, Vec<String>)>,
}

impl Observed {
    fn invite(&self, name: &str) -> Option<String> {
        self.invite.iter().find(|(n, _)| n == name).and_then(|(_, v)| v.clone())
    }

    fn provisional(&self, name: &str) -> Vec<String> {
        lookup(&self.provisional, name)
    }

    fn answer(&self, name: &str) -> Vec<String> {
        lookup(&self.answer, name)
    }
}

fn lookup(list: &[(String, Vec<String>)], name: &str) -> Vec<String> {
    list.iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn owned(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// alice dials `91300000000`, routed to a `carrier` trunk whose peer is a
/// bare TestUa. The carrier rings with a 183 + SDP and answers, each with the
/// given headers; returns what crossed in each direction.
async fn call_through_carrier(
    passthrough: Option<HeaderPassthrough>,
    invite_headers: &[(&str, &str)],
    ringing_headers: &[(&str, &str)],
    answer_headers: &[(&str, &str)],
    observe: &[&str],
) -> Result<Observed> {
    call_through_carrier_with_twins(
        passthrough,
        &[],
        invite_headers,
        ringing_headers,
        answer_headers,
        observe,
    )
    .await
}

/// Like [`call_through_carrier`], plus extra trunks (`twins`) that share the
/// carrier's destination address but carry their own rules.
async fn call_through_carrier_with_twins(
    passthrough: Option<HeaderPassthrough>,
    twins: &[(&str, Option<HeaderPassthrough>)],
    invite_headers: &[(&str, &str)],
    ringing_headers: &[(&str, &str)],
    answer_headers: &[(&str, &str)],
    observe: &[&str],
) -> Result<Observed> {
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
        ..Default::default()
    };
    config.trunks.insert(
        "carrier".to_string(),
        TrunkConfig {
            dest: format!("127.0.0.1:{carrier_port}"),
            header_passthrough: passthrough,
            ..Default::default()
        },
    );
    for (name, rule) in twins {
        config.trunks.insert(
            name.to_string(),
            TrunkConfig {
                dest: format!("127.0.0.1:{carrier_port}"),
                header_passthrough: rule.clone(),
                ..Default::default()
            },
        );
    }
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
    sleep(Duration::from_millis(100)).await;

    let alice_clone = alice.clone();
    let invite_headers = owned(invite_headers);
    let caller_handle = rustpbx::utils::spawn(async move {
        alice_clone
            .make_call_with_headers(
                "91300000000",
                Some(pcmu_sdp("127.0.0.1", 40000)),
                invite_headers,
            )
            .await
    });

    let mut carrier_dialog = None;
    for _ in 0..50 {
        for event in carrier.process_dialog_events().await? {
            if let TestUaEvent::IncomingCall(id, _) = event {
                carrier
                    .send_ringing_with_headers(
                        &id,
                        Some(pcmu_sdp("127.0.0.1", 40010)),
                        owned(ringing_headers),
                    )
                    .await?;
                carrier_dialog = Some(id);
                break;
            }
        }
        if carrier_dialog.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let carrier_id =
        carrier_dialog.ok_or_else(|| anyhow::anyhow!("carrier never got the INVITE"))?;

    sleep(Duration::from_millis(300)).await;
    carrier
        .answer_call_with_headers(
            &carrier_id,
            Some(pcmu_sdp("127.0.0.1", 40010)),
            owned(answer_headers),
        )
        .await?;

    let caller_id = tokio::time::timeout(Duration::from_secs(8), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("alice's INVITE timed out"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;
    sleep(Duration::from_millis(100)).await;

    let mut observed = Observed {
        invite: Vec::new(),
        provisional: Vec::new(),
        answer: Vec::new(),
    };
    for name in observe {
        observed.invite.push((
            name.to_string(),
            carrier.incoming_invite_header(&carrier_id, name).await,
        ));
        observed
            .provisional
            .push((name.to_string(), alice.provisional_header_values(name).await));
        observed.answer.push((
            name.to_string(),
            alice.answer_header_values(&caller_id, name).await,
        ));
    }

    alice.hangup(&caller_id).await.ok();
    server.stop();
    Ok(observed)
}

#[tokio::test]
async fn test_trunk_whitelist_applies_in_both_directions() -> Result<()> {
    let rule = HeaderPassthrough {
        mode: HeaderPassthroughMode::Whitelist,
        whitelist: vec!["P-Early-Media".into(), "X-*".into()],
        blacklist: vec![],
    };
    let seen = call_through_carrier(
        Some(rule),
        &[
            ("P-Early-Media", "supported,gated"),
            ("X-Account", "acct-1"),
            ("P-Charging-Vector", "icid-value=caller"),
        ],
        &[
            ("P-Early-Media", "sendrecv"),
            ("X-Carrier-Ref", "ring-1"),
            ("P-Charging-Vector", "icid-value=carrier"),
        ],
        &[("X-Carrier-Ref", "answer-1"), ("P-Charging-Vector", "icid-value=carrier")],
        &["P-Early-Media", "X-Account", "X-Carrier-Ref", "P-Charging-Vector"],
    )
    .await?;

    // Request: caller INVITE -> carrier INVITE.
    assert_eq!(seen.invite("P-Early-Media").as_deref(), Some("supported,gated"));
    assert_eq!(seen.invite("X-Account").as_deref(), Some("acct-1"));
    assert_eq!(seen.invite("P-Charging-Vector"), None);

    // Response: carrier 183 / 200 -> caller.
    assert_eq!(seen.provisional("P-Early-Media"), vec!["sendrecv".to_string()]);
    assert_eq!(seen.provisional("X-Carrier-Ref"), vec!["ring-1".to_string()]);
    assert!(seen.provisional("P-Charging-Vector").is_empty());
    assert_eq!(seen.answer("X-Carrier-Ref"), vec!["answer-1".to_string()]);
    assert!(seen.answer("P-Charging-Vector").is_empty());
    Ok(())
}

#[tokio::test]
async fn test_trunk_without_rule_copies_no_custom_headers() -> Result<()> {
    let seen = call_through_carrier(
        None,
        &[("X-Account", "acct-1")],
        &[("X-Carrier-Ref", "ring-1")],
        &[("X-Carrier-Ref", "answer-1")],
        &["X-Account", "X-Carrier-Ref"],
    )
    .await?;

    assert_eq!(seen.invite("X-Account"), None);
    assert!(seen.provisional("X-Carrier-Ref").is_empty());
    assert!(seen.answer("X-Carrier-Ref").is_empty());
    Ok(())
}

#[tokio::test]
async fn test_routed_trunk_rule_wins_over_trunks_sharing_its_address() -> Result<()> {
    // Same gateway, different trunks: the route picks `carrier`, so only its
    // rule may apply, never a twin's found by address.
    let rule = HeaderPassthrough {
        mode: HeaderPassthroughMode::Whitelist,
        whitelist: vec!["X-*".into()],
        blacklist: vec![],
    };
    let seen = call_through_carrier_with_twins(
        Some(rule),
        &[
            ("carrier_b", None),
            ("carrier_c", None),
            ("carrier_d", Some(HeaderPassthrough::none())),
            ("carrier_e", None),
        ],
        &[("X-Account", "acct-1")],
        &[],
        &[],
        &["X-Account"],
    )
    .await?;

    assert_eq!(seen.invite("X-Account").as_deref(), Some("acct-1"));
    Ok(())
}
