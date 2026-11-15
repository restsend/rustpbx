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
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let (server, _) = create_test_server_with_rwi_gateway(
        ProxyConfig::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
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
    let dialplan = Dialplan::new(local_id.into(), request, DialDirection::Inbound)
        .with_caller("sip:alice@example.test".parse().unwrap());
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
