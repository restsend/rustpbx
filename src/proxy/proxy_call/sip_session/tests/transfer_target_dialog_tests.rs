//! REFER-Replaces takeover: the `QueryTransferTargetDialog` answer carries
//! the dialog the TARGET UA faces plus the cluster-hop flag — the flag is
//! what tells the takeover node to resolve the real dialog cross-node
//! (`cluster_forward::query_transfer_target_dialog_on_peers`) instead of
//! stamping a hop dialog the target UA can never match (production
//! 2026-11-15: cross-node consult, hop-dialog stamp → 486 busy at the
//! target → room fallback instead of the pure REFER-Replaces chain).

use super::*;

/// Build a session shaped like the consult leg owner: caller leg (customer)
/// + a callee leg toward `target_user`, optionally stamped as a cluster hop
/// (`remote_registered`), with a live dialog on the callee leg.
async fn consult_session_with_target_leg(
    target_user: &str,
    remote_registered: bool,
) -> SipSession {
    consult_session_with_setup(target_user, remote_registered, None, None, None).await
}

/// `cluster_peer` seeds the server's cluster view; the two contexts mirror
/// the two takeover rides (dialplan extension = takeover-node stamp; cookie
/// extension = delegated takeover on the transferee-home node).
async fn consult_session_with_setup(
    target_user: &str,
    remote_registered: bool,
    cluster_peer: Option<std::net::IpAddr>,
    dialplan_ctx: Option<crate::call::ReferReplacesContext>,
    cookie_ctx: Option<crate::call::ReferReplacesContext>,
) -> SipSession {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let (mut server, _) = create_test_server_with_rwi_gateway(
        ProxyConfig::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    if let Some(peer) = cluster_peer {
        Arc::get_mut(&mut server)
            .expect("unique server")
            .cluster_peer_ips = vec![peer];
    }
    let mut request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "example.test",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("server dialog");
    let local_id = "aabbccddeeff0011";
    let mut dialplan = Dialplan::new(local_id.into(), request, DialDirection::Inbound)
        .with_caller("sip:alice@example.test".parse().unwrap());
    if let Some(ctx) = dialplan_ctx {
        dialplan.extensions.insert(ctx);
    }
    let context = CallContext {
        session_id: local_id.into(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "alice".into(),
        original_callee: "sip:bob@example.test".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    if let Some(ctx) = cookie_ctx {
        context.cookie.insert_extension(ctx);
    }
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        dialog,
        false,
    );

    let mut caller = Leg::new(LegId::from("caller"));
    caller.state = LegState::Connected;
    session.legs.insert(LegId::from("caller"), caller);

    // A live dialog for the callee leg (the consult INVITE the target UA
    // answered). The caller dialog's identity stands in for it.
    let callee_dialog = session
        .caller_dialog
        .as_ref()
        .expect("caller dialog")
        .clone();
    let mut callee = Leg::new(LegId::from("callee"));
    callee.state = LegState::Connected;
    callee.endpoint = Some(format!("sip:{target_user}@example.test"));
    callee.remote_registered = remote_registered;
    session.legs.insert(LegId::from("callee"), callee);
    session.legs.set_dialog(LegId::from("callee"), Dialog::Invite(callee_dialog));
    session
}

/// Same-node consult: the answer is the local dialog and the hop flag is
/// clean — the takeover stamps it verbatim (RFC 3891 matching at the target
/// UA hits).
#[tokio::test]
async fn transfer_target_dialog_local_leg_is_not_marked_as_hop() {
    let mut session = consult_session_with_target_leg("1004", false).await;
    let (reply, rx) = tokio::sync::oneshot::channel();
    assert!(
        session
            .execute_command(
                CallCommand::QueryTransferTargetDialog {
                    target_user: "1004".into(),
                    reply,
                },
                None,
            )
            .await
            .success
    );
    let info = rx.await.unwrap().expect("dialog resolved");
    assert!(!info.remote_registered, "local leg must not be a hop");
    assert!(info.endpoint_matched, "endpoint match must be flagged");
    assert_eq!(info.call_id, session.caller_dialog.as_ref().unwrap().id().call_id);
}

/// Cross-node consult: the callee leg was dialed through the peer's home
/// proxy — the answer MUST carry `remote_registered` so the takeover node
/// queries the peer for the dialog the target UA actually holds instead of
/// stamping this hop dialog.
#[tokio::test]
async fn transfer_target_dialog_cluster_hop_leg_is_marked() {
    let mut session =
        consult_session_with_target_leg("1004", true).await;
    // The consult session carries the logical call's root (queue-call root
    // adopted via `initialize_agent_consultation` in production).
    session.meta.root_session_id = Some("root-call-1".into());
    let (reply, rx) = tokio::sync::oneshot::channel();
    assert!(
        session
            .execute_command(
                CallCommand::QueryTransferTargetDialog {
                    target_user: "1004".into(),
                    reply,
                },
                None,
            )
            .await
            .success
    );
    let info = rx.await.unwrap().expect("dialog resolved");
    assert!(info.remote_registered, "hop leg must be flagged");
    assert!(info.endpoint_matched, "endpoint match must be flagged");
    // The root binding lets the peer filter foreign logical calls out.
    assert_eq!(info.root_session_id, "root-call-1");
}

/// An endpoint that does not match the dialed target resolves nothing (the
/// callee fallback still answers for the classic consult shape, but only
/// from the `callee` slot).
#[tokio::test]
async fn transfer_target_dialog_unknown_user_falls_back_to_callee_leg() {
    let mut session = consult_session_with_target_leg("1004", false).await;
    let (reply, rx) = tokio::sync::oneshot::channel();
    assert!(
        session
            .execute_command(
                CallCommand::QueryTransferTargetDialog {
                    target_user: "9999".into(),
                    reply,
                },
                None,
            )
            .await
            .success
    );
    let info = rx.await.unwrap().expect("callee fallback");
    assert_eq!(
        info.call_id,
        session.caller_dialog.as_ref().unwrap().id().call_id
    );
    // The fallback answer is NOT endpoint-matched — peer scans must reject
    // it (an unrelated session on the same root would poison the stamp).
    assert!(!info.endpoint_matched, "fallback must not claim a match");
}


// ── Delegated takeover: cookie-carried context on the home-proxy hop ───────

fn delegated_location() -> crate::call::Location {
    let mut loc = crate::call::Location::default();
    loc.aor = "sip:1002@example.test".parse().unwrap();
    loc.registered_aor = Some("sip:1002@peer.example.test:35061".parse().unwrap());
    loc.home_proxy = Some(rsipstack::transport::SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: "peer.example.test:35061".try_into().unwrap(),
    });
    loc
}

fn replaces_headers(option: &rsipstack::dialog::invitation::InviteOption) -> Vec<String> {
    option
        .headers
        .iter()
        .flat_map(|hs| hs.iter())
        .filter(|h| format!("{:?}", h.name()).to_lowercase().contains("replaces"))
        .map(|h| h.value().to_string())
        .collect()
}

/// Delegated takeover (the gate rode the context on the TRANSACTION cookie —
/// the dialplan does not exist at delegation time): the home-proxy hop
/// carries the ORIGINAL Replaces verbatim so the peer's gate consumes it and
/// runs the local takeover. Production 2026-11-15: the stamping block read
/// the dialplan only, the delegated hop lost the header, the peer gate saw a
/// plain INVITE, the busy target answered 486 and the whole transfer
/// degraded to the conference-room fallback (noise + seconds-long switch).
#[tokio::test]
async fn delegated_takeover_keeps_replaces_on_the_home_proxy_hop() {
    let mut session = consult_session_with_setup(
        "1002",
        false,
        Some("192.0.2.44".parse().unwrap()),
        None,
        Some(crate::call::ReferReplacesContext {
            header: "replaced-call@h;to-tag=ttt;from-tag=fff".to_string(),
            delegate_via_home_proxy: true,
        }),
    )
    .await;

    let (option, callee_uri, _call_id, routed_via_home_proxy) = session
        .build_target_invite_option(&delegated_location(), None, None)
        .await
        .expect("invite option");
    assert_eq!(
        replaces_headers(&option),
        vec!["replaced-call@h;to-tag=ttt;from-tag=fff".to_string()],
        "the delegated hop must carry the ORIGINAL Replaces verbatim"
    );
    assert!(routed_via_home_proxy, "delegation routes via the home proxy");
    assert!(
        callee_uri.host_with_port.to_string().contains("peer.example.test"),
        "delegation must keep the hop on the peer's home proxy, got {callee_uri}"
    );
}

/// Same-node takeover (context in the dialplan, no delegation flag): the
/// replacement leg SKIPS home-proxy routing (direct to the target
/// registration) and carries the re-stamped Replaces for RFC 3891.
#[tokio::test]
async fn direct_takeover_carries_replaces_and_skips_home_proxy() {
    let mut session = consult_session_with_setup(
        "1002",
        false,
        Some("192.0.2.44".parse().unwrap()),
        Some(crate::call::ReferReplacesContext {
            header: "real-call@h;to-tag=rrr;from-tag=sss".to_string(),
            delegate_via_home_proxy: false,
        }),
        None,
    )
    .await;

    let (option, callee_uri, _call_id, routed_via_home_proxy) = session
        .build_target_invite_option(&delegated_location(), None, None)
        .await
        .expect("invite option");
    assert_eq!(
        replaces_headers(&option),
        vec!["real-call@h;to-tag=rrr;from-tag=sss".to_string()],
        "the direct replacement leg carries the re-stamped Replaces"
    );
    assert!(
        !routed_via_home_proxy,
        "REFER-Replaces replacement legs skip home-proxy routing"
    );
    assert_eq!(
        callee_uri.host_with_port.to_string(),
        "example.test",
        "direct replacement dials the target AOR; the locator resolves the registration at dial time"
    );
}

/// No takeover context: a plain dial through the same peer-registered
/// target keeps home-proxy routing and carries NO Replaces.
#[tokio::test]
async fn plain_dial_to_peer_registered_target_has_no_replaces() {
    let mut session = consult_session_with_setup(
        "1002",
        false,
        Some("192.0.2.44".parse().unwrap()),
        None,
        None,
    )
    .await;

    let (option, _callee_uri, _call_id, routed_via_home_proxy) = session
        .build_target_invite_option(&delegated_location(), None, None)
        .await
        .expect("invite option");

    assert!(replaces_headers(&option).is_empty());
    assert!(routed_via_home_proxy);
}


/// SELF-DISPATCH single-step transfer (production 2026-11-15): the customer
/// device is itself a registered agent, the queue hands the call BACK to it,
/// and the LegConnected endpoint stamp pins agent_id on the CALLER (customer)
/// leg. resolve_cc_agent_leg must NEVER return that leg for an inbound
/// session — releasing it BYE'd the customer and the whole transferred call
/// dropped; the DISPATCHED agent leg is the transferor.
#[tokio::test]
async fn self_dispatch_resolution_never_releases_the_customer_leg() {
    let mut session = consult_session_with_target_leg("1003", false).await;
    // Queue-dispatch shape: the helper builds the session INBOUND already;
    // the caller leg is the customer dialog (stamped with the served agent
    // id — the endpoint resolves to a registered agent) and the DISPATCHED
    // agent leg is a dynamic leg with purpose=Agent.
    {
        let mut caller = session.legs.get_mut(&LegId::from("caller")).unwrap();
        caller.agent_id = Some("1003".to_string());
    }
    let mut dispatched = Leg::new(LegId::from("agent-dispatch"));
    dispatched.state = LegState::Connected;
    dispatched.endpoint = Some("sip:1003@example.test".into());
    dispatched.agent_id = Some("1003".to_string());
    dispatched.purpose = Some(crate::call::domain::LegPurpose::Agent);
    session.legs.insert(LegId::from("agent-dispatch"), dispatched);

    let agent_leg = session
        .resolve_cc_agent_leg(None, None)
        .expect("agent leg must resolve");
    assert_ne!(
        agent_leg,
        LegId::from("caller"),
        "an inbound session's customer leg must never resolve as the transferor"
    );
    assert_eq!(agent_leg, LegId::from("agent-dispatch"));
}
