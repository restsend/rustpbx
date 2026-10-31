//! crate::call root-module test suite (moved verbatim from mod.rs).
//! Sibling module so the implementation file stays focused; repo convention
//! is `<module>_test.rs`.

#![cfg(test)]

use super::*;

#[test]
fn sequential_attempt_preserves_registered_contact_and_transport() {
    let location = Location {
        aor: "sip:alice@192.0.2.10".try_into().unwrap(),
        registered_aor: Some("sip:alice@pbx.local".try_into().unwrap()),
        contact_raw: Some("<sip:alice@192.0.2.10>;ob".into()),
        path: Some(vec!["sip:edge@192.0.2.1;lr".try_into().unwrap()]),
        transport: Some(Transport::Wss),
        supports_webrtc: true,
        ..Default::default()
    };
    let mut attempt = DialAttempt::from(location.clone());
    assert!(attempt.caller.is_none());
    assert!(attempt.retry_policy.is_none());
    attempt.location.aor = "sip:rewritten@192.0.2.10".try_into().unwrap();
    let resolved = &attempt.location;
    assert_eq!(resolved.aor.user(), Some("rewritten"));
    assert_eq!(resolved.registered_aor, location.registered_aor);
    assert_eq!(resolved.contact_raw, location.contact_raw);
    assert_eq!(resolved.path, location.path);
    assert_eq!(resolved.transport, Some(Transport::Wss));
    assert!(resolved.supports_webrtc);
}

#[test]
fn location_routes_via_remote_home_proxy_matches_routing_rule() {
    fn sip_addr(addr: &str) -> SipAddr {
        SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from(addr).unwrap(),
        }
    }
    let local = sip_addr("10.0.0.1:5060");
    let remote = sip_addr("10.0.0.2:5060");

    // Cluster disabled: never remote (single-node keeps roles).
    let mut loc = Location {
        home_proxy: Some(remote.clone()),
        ..Default::default()
    };
    assert!(!loc.routes_via_remote_home_proxy(false, Some(&local)));

    // Remote home_proxy ≠ self → remote.
    assert!(loc.routes_via_remote_home_proxy(true, Some(&local)));
    // Home_proxy == self → local.
    loc.home_proxy = Some(local.clone());
    assert!(!loc.routes_via_remote_home_proxy(true, Some(&local)));

    // No registration home (external trunk) → not remote.
    loc.home_proxy = None;
    assert!(!loc.routes_via_remote_home_proxy(true, Some(&local)));

    // Unknown self identity: conservative — treat as remote (deliver via
    // the home node, which owns the role attribution).
    loc.home_proxy = Some(remote);
    assert!(loc.routes_via_remote_home_proxy(true, None));
}

#[test]
fn route_point_transfer_endpoint_preserves_distinct_prefix() {
    let endpoint = TransferEndpoint::parse("toivr:39230").expect("route point must parse");

    assert_eq!(endpoint.to_string(), "toivr:39230");
}

#[test]
fn realtime_transfer_endpoint_roundtrip() {
    let endpoint =
        TransferEndpoint::parse("realtime:support-bot").expect("realtime target must parse");
    assert_eq!(endpoint, TransferEndpoint::Realtime("support-bot".into()));
    assert_eq!(endpoint.kind(), "realtime");
    assert_eq!(endpoint.to_string(), "realtime:support-bot");

    // Case-insensitive prefix, trimmed value.
    let endpoint = TransferEndpoint::parse("  Realtime: Sales-Bot ").unwrap();
    assert_eq!(endpoint, TransferEndpoint::Realtime("Sales-Bot".into()));

    // Empty preset name is rejected (falls into the empty-name guard).
    assert!(TransferEndpoint::parse("realtime:").is_none());
    assert!(TransferEndpoint::parse("realtime:  ").is_none());

    // Plain strings still parse as URIs.
    assert_eq!(
        TransferEndpoint::parse("sip:1001@pbx.local"),
        Some(TransferEndpoint::Uri("sip:1001@pbx.local".into()))
    );
}

// ── QueuePlan::from_app_params ─────────────────────────────────────────

#[test]
fn queue_plan_from_app_params_skill_group_passthrough() {
    let (plan, priority) = QueuePlan::from_app_params(&serde_json::json!({
        "queue": "skill-group:42",
        "priority": 5,
    }))
    .expect("skill-group target must parse");

    assert_eq!(priority, Some(5));
    // Regression guard: the parsed priority MUST land on the plan itself —
    // the factory copies `plan.priority` into `QueueConfig`, and every
    // FIFO-gate / shared-queue write derives from it. A revert to
    // "return it separately but drop it" silently re-hardcodes 0.
    assert_eq!(plan.priority, 5, "plan must carry the parsed priority");
    assert_eq!(plan.queue_name, "skill-group:42");
    let Some(DialStrategy::Sequential(agents)) = plan.dial_strategy else {
        panic!("expected sequential strategy");
    };
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].location.aor.to_string(), "skill-group:42");
    assert_eq!(agents[0].location.contact_raw.as_deref(), Some("skill-group:42"));
}

#[test]
fn queue_plan_from_app_params_sip_uri_passthrough() {
    let (plan, priority) = QueuePlan::from_app_params(&serde_json::json!({
        "queue": "sip:1001@pbx.local"
    }))
    .expect("sip target must parse");

    assert_eq!(priority, None);
    assert_eq!(
        plan.priority, 0,
        "absent priority must default the plan to 0"
    );
    assert_eq!(plan.queue_name, "sip:1001@pbx.local");
    let Some(DialStrategy::Sequential(agents)) = plan.dial_strategy else {
        panic!("expected sequential strategy");
    };
    assert_eq!(agents[0].location.aor.to_string(), "sip:1001@pbx.local");
}

#[test]
fn queue_plan_from_app_params_bare_name_maps_to_skill_group() {
    let (plan, _) = QueuePlan::from_app_params(&serde_json::json!({
        "queue": "sales"
    }))
    .expect("bare name must parse");

    // queue_name keeps the caller-supplied id; the dial target widens to
    // the skill-group scheme.
    assert_eq!(plan.queue_name, "sales");
    let Some(DialStrategy::Sequential(agents)) = plan.dial_strategy else {
        panic!("expected sequential strategy");
    };
    assert_eq!(agents[0].location.aor.to_string(), "skill-group:sales");
}

#[test]
fn queue_plan_from_app_params_rejects_missing_queue() {
    let err = QueuePlan::from_app_params(&serde_json::json!({})).unwrap_err();
    assert!(err.to_string().contains("queue"), "got: {err}");

    let err = QueuePlan::from_app_params(&serde_json::json!({"queue": "  " })).unwrap_err();
    assert!(err.to_string().contains("queue"), "got: {err}");
}

#[test]
fn queue_plan_from_app_params_ignores_out_of_range_priority() {
    let (_, priority) = QueuePlan::from_app_params(
        &serde_json::json!({"queue": "sg", "priority": 1_000_000_000_000_u64}),
    )
    .expect("must parse despite oversized priority");
    assert_eq!(
        priority, None,
        "priority beyond u32 must be dropped, not fatal"
    );
}

#[test]
fn queue_plan_from_app_params_accepts_numeric_and_string_priority() {
    let (_, numeric) = QueuePlan::from_app_params(&serde_json::json!({
        "queue": "sg", "priority": 3
    }))
    .expect("numeric priority must parse");
    assert_eq!(numeric, Some(3));

    // Producers that stringify app params (HashMap<String, String>) send
    // `"priority": "3"` — the value must survive, not silently drop.
    let (_, stringified) = QueuePlan::from_app_params(&serde_json::json!({
        "queue": "sg", "priority": "3"
    }))
    .expect("stringified priority must parse");
    assert_eq!(stringified, Some(3));

    let (_, garbage) = QueuePlan::from_app_params(&serde_json::json!({
        "queue": "sg", "priority": "high"
    }))
    .expect("must parse despite non-numeric priority");
    assert_eq!(
        garbage, None,
        "non-numeric priority must be dropped, not fatal"
    );
}

#[test]
fn clamp_priority_u32_to_i32_saturates_without_sign_flip() {
    assert_eq!(super::clamp_priority_u32_to_i32(0), 0);
    assert_eq!(super::clamp_priority_u32_to_i32(5), 5);
    assert_eq!(super::clamp_priority_u32_to_i32(i32::MAX as u32), i32::MAX);
    // A plain `as i32` cast would wrap this to a NEGATIVE priority and
    // invert the ordering — saturation is the contract.
    assert_eq!(
        super::clamp_priority_u32_to_i32(u32::MAX),
        i32::MAX,
        "u32::MAX must saturate at i32::MAX, never wrap negative"
    );
    assert!(super::clamp_priority_u32_to_i32(u32::MAX) > 0);
}

#[test]
fn transfer_target_kind_classifies_prefixes() {
    assert_eq!(
        transfer_target_kind("queue:sales?target=skillgroup:sg_1"),
        Some("queue".to_string())
    );
    assert_eq!(
        transfer_target_kind("ivr:main-ivr"),
        Some("ivr".to_string())
    );
    assert_eq!(
        transfer_target_kind("toivr:8000"),
        Some("route_point".to_string())
    );
    assert_eq!(
        transfer_target_kind("voicemail:1001"),
        Some("voicemail".to_string())
    );
    assert_eq!(
        transfer_target_kind("conference:room-1"),
        Some("conference".to_string())
    );
    assert_eq!(
        transfer_target_kind("bridge:ws://127.0.0.1:9100?samplerate=8000"),
        Some("bridge".to_string())
    );
    assert_eq!(
        transfer_target_kind("voip_bridge:ws://127.0.0.1:9100"),
        Some("bridge".to_string())
    );
    // Bare numbers / SIP URIs dial as plain SIP targets.
    assert_eq!(transfer_target_kind("8000"), Some("sip".to_string()));
    assert_eq!(
        transfer_target_kind("sip:1001@rustpbx.com"),
        Some("sip".to_string())
    );
    // Prefix matching is case-insensitive like TransferEndpoint::parse.
    assert_eq!(
        transfer_target_kind("QUEUE:sales"),
        Some("queue".to_string())
    );
    // Empty / prefix-only targets are unclassifiable.
    assert_eq!(transfer_target_kind(""), None);
    assert_eq!(transfer_target_kind("   "), None);
    assert_eq!(transfer_target_kind("queue:"), None);
}

fn minimal_request() -> rsipstack::sip::Request {
    let uri = rsipstack::sip::Uri {
        scheme: Some(rsipstack::sip::Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: "1001".into(),
            password: None,
        }),
        host_with_port: rsipstack::sip::HostWithPort {
            host: "pbx.local".parse().unwrap(),
            port: None,
        },
        params: vec![],
        headers: vec![],
    };
    rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri,
        version: rsipstack::sip::Version::V2,
        headers: Default::default(),
        body: vec![],
    }
}

// ── Dialplan::voicemail_enabled ────────────────────────────────────────

#[test]
fn new_dialplan_has_voicemail_enabled_false_by_default() {
    // Voicemail routing must be explicitly enabled by the proxy layer after
    // looking up the callee.  Until then the flag is false so that calls
    // without a resolvable callee extension are never silently routed to
    // voicemail.
    let dp = Dialplan::new(
        "sess-001".into(),
        minimal_request(),
        DialDirection::Internal,
    );
    assert!(
        !dp.voicemail_enabled,
        "new Dialplan must default voicemail_enabled to false"
    );
}

#[test]
fn dialplan_voicemail_enabled_can_be_set() {
    let mut dp = Dialplan::new(
        "sess-002".into(),
        minimal_request(),
        DialDirection::Internal,
    );
    dp.voicemail_enabled = true;
    assert!(dp.voicemail_enabled);
}

#[test]
fn dialplan_voicemail_enabled_independent_of_call_forwarding() {
    // voicemail_enabled and call_forwarding are orthogonal: a call can have
    // forwarding configured AND voicemail enabled simultaneously.
    let mut dp = Dialplan::new(
        "sess-003".into(),
        minimal_request(),
        DialDirection::Internal,
    );
    dp.voicemail_enabled = true;
    dp.call_forwarding = Some(CallForwardingConfig {
        mode: CallForwardingMode::WhenNoAnswer,
        endpoint: TransferEndpoint::Uri("sip:2001@pbx.local".into()),
        timeout: std::time::Duration::from_secs(20),
    });

    assert!(dp.voicemail_enabled);
    assert!(dp.call_forwarding.is_some());
}

// ── DialplanFlow::registered_owner ─────────────────────────────────────

fn registration(aor: &str, user: &str) -> Location {
    Location {
        registered_username: Some(user.into()),
        registered_realm: Some("pbx.local".into()),
        ..make_location(aor, false)
    }
}

#[test]
fn registered_owner_is_the_user_whose_phones_ring() {
    // Two phones of one user: that user's voicemail may answer a failure.
    let phones = DialplanFlow::Targets(DialStrategy::Parallel(vec![
        registration("sip:1002@10.0.0.2:5060", "1002"),
        registration("sip:1002@10.0.0.3:5060", "1002"),
    ]));
    assert_eq!(
        phones.registered_owner(),
        Some(("1002".to_string(), Some("pbx.local".to_string())))
    );
}

#[test]
fn registered_owner_none_unless_every_target_is_that_users_phone() {
    // A trunk target carries no registration, even when the dialed number
    // is a local user: its failure must not reach that user's voicemail.
    let trunk = DialplanFlow::Targets(DialStrategy::Sequential(vec![DialAttempt::from(
        make_location("sip:1002@198.51.100.1:5060", false),
    )]));
    assert_eq!(trunk.registered_owner(), None);

    let mixed = DialplanFlow::Targets(DialStrategy::Sequential(vec![
        DialAttempt::from(registration("sip:1002@10.0.0.2:5060", "1002")),
        DialAttempt::from(make_location("sip:1002@198.51.100.1:5060", false)),
    ]));
    assert_eq!(mixed.registered_owner(), None);

    let two_users = DialplanFlow::Targets(DialStrategy::Parallel(vec![
        registration("sip:1002@10.0.0.2:5060", "1002"),
        registration("sip:1003@10.0.0.4:5060", "1003"),
    ]));
    assert_eq!(two_users.registered_owner(), None);

    let nothing = DialplanFlow::Targets(DialStrategy::Sequential(vec![]));
    assert_eq!(nothing.registered_owner(), None);
}

// ── DialplanFlow::all_webrtc_target ────────────────────────────────────────

fn make_location(aor: &str, supports_webrtc: bool) -> Location {
    Location {
        aor: aor.try_into().unwrap(),
        supports_webrtc,
        ..Default::default()
    }
}

#[test]
fn empty_targets_returns_false_for_all_webrtc() {
    // Empty targets should return false, not true (vacuous truth bug fix)
    let flow = DialplanFlow::Targets(DialStrategy::Sequential(vec![]));
    assert!(
        !flow.all_webrtc_target(),
        "empty targets should return false"
    );
}

#[test]
fn single_webrtc_target_returns_true() {
    let flow = DialplanFlow::Targets(DialStrategy::Sequential(vec![make_location(
        "sip:webrtc@example.com",
        true,
    )].into_iter().map(Into::into).collect()));
    assert!(flow.all_webrtc_target());
}

#[test]
fn single_non_webrtc_target_returns_false() {
    let flow = DialplanFlow::Targets(DialStrategy::Sequential(vec![make_location(
        "sip:trunk@example.com",
        false,
    )].into_iter().map(Into::into).collect()));
    assert!(!flow.all_webrtc_target());
}

#[test]
fn mixed_targets_return_false() {
    let flow = DialplanFlow::Targets(DialStrategy::Sequential(vec![
        make_location("sip:webrtc@example.com", true),
        make_location("sip:trunk@example.com", false),
    ].into_iter().map(Into::into).collect()));
    assert!(
        !flow.all_webrtc_target(),
        "mixed targets should return false"
    );
}

#[test]
fn all_webrtc_targets_return_true() {
    let flow = DialplanFlow::Targets(DialStrategy::Sequential(vec![
        make_location("sip:a@example.com", true),
        make_location("sip:b@example.com", true),
    ].into_iter().map(Into::into).collect()));
    assert!(flow.all_webrtc_target());
}

#[test]
fn queue_with_empty_dial_strategy_and_empty_next_returns_false() {
    // This is the bug case: Queue with empty dial_strategy and empty next
    // was incorrectly returning true due to vacuous truth
    let queue_plan = QueuePlan {
        accept_immediately: false,
        passthrough_ringback: false,
        hold: None,
        dial_strategy: None,
        ..Default::default()
    };
    let flow = DialplanFlow::Queue {
        plan: queue_plan,
        next: Box::new(DialplanFlow::Targets(DialStrategy::Sequential(vec![]))),
    };
    assert!(
        !flow.all_webrtc_target(),
        "queue with empty targets should return false"
    );
}

#[test]
fn queue_with_webrtc_targets_in_dial_strategy_returns_true() {
    let queue_plan = QueuePlan {
        accept_immediately: false,
        passthrough_ringback: false,
        hold: None,
        dial_strategy: Some(DialStrategy::Sequential(vec![make_location(
            "sip:webrtc@example.com",
            true,
        )].into_iter().map(Into::into).collect())),
        ..Default::default()
    };
    let flow = DialplanFlow::Queue {
        plan: queue_plan,
        next: Box::new(DialplanFlow::Targets(DialStrategy::Sequential(vec![]))),
    };
    assert!(
        flow.all_webrtc_target(),
        "queue with webrtc dial_strategy should return true"
    );
}

#[test]
fn queue_with_non_webrtc_targets_in_dial_strategy_returns_false() {
    let queue_plan = QueuePlan {
        accept_immediately: false,
        passthrough_ringback: false,
        hold: None,
        dial_strategy: Some(DialStrategy::Sequential(vec![make_location(
            "sip:trunk@example.com",
            false,
        )].into_iter().map(Into::into).collect())),
        ..Default::default()
    };
    let flow = DialplanFlow::Queue {
        plan: queue_plan,
        next: Box::new(DialplanFlow::Targets(DialStrategy::Sequential(vec![]))),
    };
    assert!(
        !flow.all_webrtc_target(),
        "queue with non-webrtc dial_strategy should return false"
    );
}

#[test]
fn queue_fallback_to_next_when_dial_strategy_is_empty() {
    // When dial_strategy is empty/None, should check next
    let queue_plan = QueuePlan {
        accept_immediately: false,
        passthrough_ringback: false,
        hold: None,
        dial_strategy: Some(DialStrategy::Sequential(vec![])),
        ..Default::default()
    };
    let flow = DialplanFlow::Queue {
        plan: queue_plan,
        next: Box::new(DialplanFlow::Targets(DialStrategy::Sequential(vec![
            make_location("sip:webrtc@example.com", true),
        ].into_iter().map(Into::into).collect()))),
    };
    assert!(
        flow.all_webrtc_target(),
        "should fallback to next which has webrtc target"
    );
}

#[test]
fn application_flow_returns_false() {
    let flow = DialplanFlow::Application {
        app_name: "voicemail".to_string(),
        app_params: None,
        auto_answer: false,
    };
    assert!(!flow.all_webrtc_target());
}

// ── Header passthrough ──────────────────────────────────────────────────

fn custom_header(name: &str, value: &str) -> rsipstack::sip::Header {
    rsipstack::sip::Header::Other(name.to_string(), value.to_string())
}

#[test]
fn new_dialplan_header_passthrough_none_by_default() {
    let dp = Dialplan::new(
        "sess-001".into(),
        minimal_request(),
        DialDirection::Internal,
    );
    assert!(dp.header_passthrough.is_none());
}

#[test]
fn diversion_chain_seeds_from_request_and_appends() {
    use crate::call::diversion::DiversionReason;

    let mut request = minimal_request();
    request.headers.push(rsipstack::sip::headers::make_header(
        "Diversion",
        "<sip:origin@example.com>;reason=user-busy;counter=1".to_string(),
    ));
    let dp = Dialplan::new("sess-002".into(), request.clone(), DialDirection::Inbound)
        .with_diversion_chain_from_request(&request);
    assert_eq!(dp.diversion_chain.len(), 1);
    assert_eq!(
        dp.diversion_chain[0].reason,
        Some(DiversionReason::UserBusy)
    );

    // Forwarding appends hops with a continuing counter; renders as a
    // single Diversion header.
    let mut dp = dp;
    dp.push_diversion("sip:1002@pbx.local", Some(DiversionReason::Unconditional));
    assert_eq!(dp.diversion_chain.len(), 2);
    assert_eq!(dp.diversion_chain[1].counter, 2);

    let header = crate::call::diversion::diversion_header(&dp.diversion_chain).unwrap();
    let value = header.value();
    assert!(value.contains("<sip:origin@example.com>;reason=user-busy;counter=1"));
    assert!(value.contains("<sip:1002@pbx.local>;reason=unconditional;counter=2"));
}


#[test]
fn is_custom_header_means_unparsed_x_or_p_header() {
    use rsipstack::sip::Header;
    assert!(Dialplan::is_custom_header(&custom_header("X-Smart2Agent", "v")));
    assert!(Dialplan::is_custom_header(&custom_header("x-referred-id", "v")));
    assert!(Dialplan::is_custom_header(&custom_header("P-Early-Media", "sendrecv")));
    // Unprefixed or standard headers are never custom.
    assert!(!Dialplan::is_custom_header(&custom_header("Allow-Events", "talk")));
    assert!(!Dialplan::is_custom_header(&custom_header("Diversion", "<sip:a@b>")));
    assert!(!Dialplan::is_custom_header(&custom_header("Call-ID", "v")));
    assert!(!Dialplan::is_custom_header(&Header::Via("SIP/2.0/UDP h:5060".into())));
    assert!(!Dialplan::is_custom_header(&Header::Require("100rel".into())));
    assert!(!Dialplan::is_custom_header(&Header::PAssertedIdentity(
        "<sip:1001@pbx>".into()
    )));
}

#[test]
fn select_passthrough_headers_all_rule_skips_standard_and_dedupes() {
    use crate::proxy::routing::HeaderPassthrough;
    let original = vec![
        custom_header("X-Smart2Agent", "agent"),
        custom_header("X-SmartParams", "params"),
        custom_header("X-Referred-Id", "ref"),
        custom_header("From", "sip:a@b"), // standard -> never forwarded
        rsipstack::sip::Header::Via("SIP/2.0/UDP h:5060".into()),
    ];
    let existing = vec![custom_header("X-Referred-Id", "already-there")];
    let rule = HeaderPassthrough::all();

    let selected = Dialplan::select_passthrough_headers(&original, &existing, &rule);

    let names: Vec<String> = selected.iter().map(|h| h.name().to_string()).collect();
    assert!(names.contains(&"X-Smart2Agent".to_string()));
    assert!(names.contains(&"X-SmartParams".to_string()));
    // Duplicate name already present -> skipped
    assert!(!names.contains(&"X-Referred-Id".to_string()));
    // Standard SIP headers always excluded
    assert!(!names.iter().any(|n| n.eq_ignore_ascii_case("from")));
    assert!(!names.iter().any(|n| n.eq_ignore_ascii_case("via")));
}

#[test]
fn select_passthrough_headers_whitelist_and_blacklist() {
    use crate::proxy::routing::{HeaderPassthrough, HeaderPassthroughMode};
    let original = vec![
        custom_header("X-Smart2Agent", "a"),
        custom_header("X-SmartBridgeType", "b"),
        custom_header("X-Token", "secret"),
    ];

    let whitelist = HeaderPassthrough {
        mode: HeaderPassthroughMode::Whitelist,
        whitelist: vec!["x-smart2agent".into()],
        blacklist: vec![],
    };
    let sel = Dialplan::select_passthrough_headers(&original, &[], &whitelist);
    assert_eq!(sel.len(), 1);
    assert_eq!(sel[0].name(), "X-Smart2Agent");

    let blacklist = HeaderPassthrough {
        mode: HeaderPassthroughMode::Blacklist,
        whitelist: vec![],
        blacklist: vec!["x-token".into()],
    };
    let sel = Dialplan::select_passthrough_headers(&original, &[], &blacklist);
    let names: Vec<String> = sel.iter().map(|h| h.name().to_string()).collect();
    assert!(names.contains(&"X-Smart2Agent".to_string()));
    assert!(!names.iter().any(|n| n.eq_ignore_ascii_case("x-token")));
}

#[test]
fn select_passthrough_headers_copies_only_custom_headers() {
    use crate::proxy::routing::HeaderPassthrough;
    let headers = vec![
        custom_header("P-Early-Media", "sendrecv"),
        custom_header("X-Carrier-Ref", "r-1"),
        custom_header("Allow-Events", "talk"),
        custom_header("Diversion", "<sip:a@b>"),
        rsipstack::sip::Header::Require("100rel".into()),
        rsipstack::sip::Header::RSeq("1".into()),
        rsipstack::sip::Header::UserToUser("abc;encoding=hex".into()),
    ];

    let sel = Dialplan::select_passthrough_headers(&headers, &[], &HeaderPassthrough::all());

    let names: Vec<String> = sel.iter().map(|h| h.name().to_string()).collect();
    assert_eq!(names, vec!["P-Early-Media", "X-Carrier-Ref"]);
}

/// The per-trunk `x_only` mode (CCF-facing dial-out metadata trunks):
/// `X-` headers ride, other custom headers do not. Case-insensitive on
/// the `x-` prefix.
#[test]
fn select_passthrough_headers_x_only_rule() {
    use crate::proxy::routing::HeaderPassthrough;
    let original = vec![
        custom_header("X-Aid", "agent-1"),
        custom_header("x-callright", "domestic"),
        custom_header("X-taskid", "t-42"),
        custom_header("P-Asserted-Identity", "<sip:02188886666@pbx>"),
        custom_header("XToken", "no-dash-stays"),
    ];
    let rule = HeaderPassthrough::x_custom();

    assert!(rule.allows("X-Aid"));
    assert!(rule.allows("x-callright"));
    assert!(!rule.allows("P-Asserted-Identity"));
    assert!(
        !rule.allows("XToken"),
        "bare X without dash is not x- prefixed"
    );

    let sel = Dialplan::select_passthrough_headers(&original, &[], &rule);
    let names: Vec<String> = sel.iter().map(|h| h.name().to_string()).collect();
    assert_eq!(
        names.len(),
        3,
        "exactly the three x- headers ride: {names:?}"
    );
    assert!(names.contains(&"X-Aid".to_string()));
    assert!(names.contains(&"x-callright".to_string()));
    assert!(names.contains(&"X-taskid".to_string()));
    assert!(
        !names
            .iter()
            .any(|n| n.eq_ignore_ascii_case("p-asserted-identity"))
    );
    assert!(!names.iter().any(|n| n.eq_ignore_ascii_case("xtoken")));
}

/// Mirrors the refer -> PBX -> WebRTC agent scenario: the incoming refer
/// INVITE carries the X-Smart* / X-Referred-* headers, and a registered
/// (internal) agent destination resolves to the "all" passthrough rule.
#[test]
fn refer_smart_headers_forwarded_to_registered_agent() {
    use crate::proxy::routing::HeaderPassthrough;

    let original = vec![
        custom_header("X-Smart2Agent", "strategyCode:|callId:abc|taskId:def"),
        custom_header("X-SmartParams", "nameListId:1|siteId:1|planId:9"),
        custom_header("X-SmartBridgeType", "refer"),
        custom_header("X-Referred-Id", "abc"),
        custom_header("X-Referred-To", "sip:187@x"),
        custom_header("X-Referred-From", "sip:101@y"),
        rsipstack::sip::Header::From("sip:187@172.25.225.2:15060".into()),
        rsipstack::sip::Header::To("sip:test0807@172.25.225.2:15060".into()),
        rsipstack::sip::Header::CallId("ref-abc".into()),
        rsipstack::sip::Header::CSeq("1 INVITE".into()),
    ];
    // The INVITE to the agent also carries the agent's own headers
    // (e.g. X-Agent/X-Token from its registration Location) plus standard
    // routing headers that must never be duplicated/forwarded.
    let existing = vec![
        custom_header("X-Agent", "test0807"),
        custom_header("Max-Forwards", "69"),
        rsipstack::sip::Header::Contact("sip:pbx@host".into()),
    ];

    // Internal destination (registered agent) -> All rule.
    let rule = HeaderPassthrough::all();
    let selected = Dialplan::select_passthrough_headers(&original, &existing, &rule);

    let names: Vec<String> = selected.iter().map(|h| h.name().to_string()).collect();
    for want in [
        "X-Smart2Agent",
        "X-SmartParams",
        "X-SmartBridgeType",
        "X-Referred-Id",
        "X-Referred-To",
        "X-Referred-From",
    ] {
        assert!(
            names.contains(&want.to_string()),
            "{want} should be forwarded to the registered agent"
        );
    }
    // Standard SIP headers are never forwarded; existing names are not duplicated.
    for excluded in [
        "From",
        "To",
        "Call-ID",
        "CSeq",
        "Contact",
        "Max-Forwards",
        "X-Agent",
    ] {
        assert!(
            !names.iter().any(|n| n.eq_ignore_ascii_case(excluded)),
            "{excluded} must not be forwarded"
        );
    }
}

fn queue_plan_with_hold(hold: Option<QueueHoldConfig>) -> QueuePlan {
    QueuePlan {
        hold,
        ..Default::default()
    }
}

#[test]
fn queue_hold_default_global_fills_when_queue_has_none() {
    let mut plan = queue_plan_with_hold(None);
    apply_queue_hold_music_default(&mut plan, Some("sounds/moh/kzmusic.wav".into()));
    let hold = plan.hold.expect("global default must materialize hold");
    assert_eq!(hold.audio_file.as_deref(), Some("sounds/moh/kzmusic.wav"));
    assert!(hold.loop_playback, "default hold loops");
}

#[test]
fn queue_hold_default_per_queue_config_wins() {
    let mut plan = queue_plan_with_hold(Some(
        QueueHoldConfig::default().with_audio_file("sounds/queue-own.wav".into()),
    ));
    apply_queue_hold_music_default(&mut plan, Some("sounds/moh/kzmusic.wav".into()));
    let hold = plan.hold.expect("per-queue hold must survive");
    assert_eq!(hold.audio_file.as_deref(), Some("sounds/queue-own.wav"));
}

#[test]
fn queue_hold_default_fills_hold_without_file_and_keeps_loop_setting() {
    let mut plan = queue_plan_with_hold(Some(QueueHoldConfig {
        audio_file: None,
        loop_playback: false,
    }));
    apply_queue_hold_music_default(&mut plan, Some("sounds/moh/kzmusic.wav".into()));
    let hold = plan.hold.expect("hold segment must stay Some");
    assert_eq!(hold.audio_file.as_deref(), Some("sounds/moh/kzmusic.wav"));
    assert!(!hold.loop_playback, "existing loop setting must be kept");
}

#[test]
fn queue_hold_default_no_global_leaves_plan_unchanged() {
    let mut empty = queue_plan_with_hold(None);
    apply_queue_hold_music_default(&mut empty, None);
    assert!(
        empty.hold.is_none(),
        "no global -> stays unset (route queues stay silent)"
    );

    let mut builtin = queue_plan_with_hold(Some(
        QueueHoldConfig::default().with_audio_file("sounds/phone-calling.wav".into()),
    ));
    apply_queue_hold_music_default(&mut builtin, None);
    assert_eq!(
        builtin.hold.and_then(|h| h.audio_file),
        Some("sounds/phone-calling.wav".to_string()),
        "no global -> factory built-in default path unaffected"
    );
}
