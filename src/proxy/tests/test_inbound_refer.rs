//! Inbound REFER in-session execution.
//!
//! Blind REFERs (no Replaces) must execute inside the original session —
//! `execute_inbound_refer_in_session` dispatches a `CallCommand::Transfer`
//! to the session that received the REFER so the call stays one session and
//! one CDR. These tests cover the dispatch contract and the config switch;
//! the B-leg swap itself (success retires the old leg, failure keeps it) is
//! the blind-transfer B2BUA path already exercised by the sip_session
//! transfer tests.

use crate::call::domain::{CallCommand, LegId};
use crate::config::ProxyConfig;
use crate::proxy::call::CallModule;
use crate::proxy::tests::common::{create_test_request, create_test_server};
use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;
use rsipstack::dialog::DialogId;
use rsipstack::sip::Method;

fn refer_test_dialplan() -> crate::call::Dialplan {
    crate::call::Dialplan::new(
        "refer-test-session".to_string(),
        create_test_request(Method::Invite, "alice", None, "rustpbx.com", None),
        crate::call::DialDirection::Internal,
    )
}

#[tokio::test]
async fn inbound_refer_in_session_dispatches_transfer_command() {
    let (mut session, handle, mut cmd_rx) = build_session_with_cmd_rx(refer_test_dialplan()).await;
    let dialog_id = session
        .caller_dialog
        .as_ref()
        .map(|d| d.id())
        .expect("UAS test session must have a caller dialog");

    // Drive the session command loop: QueryLegByDialog is executed for real
    // (resolving the transferor leg from the caller dialog), while the
    // dispatched Transfer command is captured instead of executed — running
    // the B-leg dial would hit the network and block the test.
    let driver = tokio::spawn(async move {
        let mut captured = None;
        while let Some(command) = cmd_rx.recv().await {
            match command {
                CallCommand::Transfer {
                    leg_id,
                    target,
                    attended,
                } => {
                    captured = Some((leg_id, target, attended));
                    break;
                }
                other => {
                    let _ = session.execute_command(other, None).await;
                }
            }
        }
        captured
    });

    let dispatched = CallModule::execute_inbound_refer_in_session(
        &handle,
        &DialogId::from(dialog_id.clone()),
        "sip:2001@rustpbx.com",
    )
    .await
    .expect("dispatch must not error");
    assert!(dispatched, "known dialog must dispatch in-session");

    drop(handle);
    let captured = tokio::time::timeout(std::time::Duration::from_secs(5), driver)
        .await
        .expect("driver must terminate after the Transfer command")
        .expect("driver task panicked");
    let (leg_id, target, attended) = captured.expect("Transfer command must reach the session");
    // The REFER arrived on the caller dialog → the transferor leg is "caller".
    assert_eq!(leg_id.as_str(), "caller");
    assert_eq!(target, "sip:2001@rustpbx.com");
    assert!(!attended);
}

#[tokio::test]
async fn inbound_refer_in_session_falls_through_for_unknown_dialog() {
    let (_session, handle, _cmd_rx) = build_session_with_cmd_rx(refer_test_dialplan()).await;

    let unknown = DialogId::try_from((
        &create_test_request(Method::Info, "bob", None, "rustpbx.com", None),
        rsipstack::transaction::key::TransactionRole::Server,
    ))
    .expect("dialog id must parse");

    let dispatched = CallModule::execute_inbound_refer_in_session(
        &handle,
        &unknown,
        "sip:2001@rustpbx.com",
    )
    .await
    .expect("fallthrough must not error");
    assert!(
        !dispatched,
        "unknown dialog must fall through to the raw originate"
    );
}

#[tokio::test]
async fn inbound_refer_in_session_falls_through_when_session_loop_is_gone() {
    let (session, handle, cmd_rx) = build_session_with_cmd_rx(refer_test_dialplan()).await;
    let dialog_id = session
        .caller_dialog
        .as_ref()
        .map(|d| d.id())
        .expect("UAS test session must have a caller dialog");
    // Drop the receiver: the session loop is gone, commands fail to send.
    drop(cmd_rx);

    let dispatched = CallModule::execute_inbound_refer_in_session(
        &handle,
        &DialogId::from(dialog_id),
        "sip:2001@rustpbx.com",
    )
    .await
    .expect("dispatch failure surfaces as fallthrough, not an error");
    assert!(!dispatched, "dead session loop must fall through");
}

#[test]
fn inbound_refer_in_session_defaults_enabled() {
    let config = ProxyConfig::default();
    assert!(
        config.inbound_refer_in_session,
        "blind inbound REFERs must execute in-session by default"
    );
}

#[test]
fn transfer_command_shape_matches_dispatch() {
    // Guard the wire shape consumed by handle_transfer: a blind (attended:
    // false) transfer with the raw Refer-To target.
    let command = CallCommand::Transfer {
        leg_id: LegId::from("callee"),
        target: "sip:2001@rustpbx.com".to_string(),
        attended: false,
    };
    let CallCommand::Transfer {
        leg_id,
        target,
        attended,
    } = command
    else {
        panic!("expected Transfer");
    };
    assert_eq!(leg_id.as_str(), "callee");
    assert_eq!(target, "sip:2001@rustpbx.com");
    assert!(!attended);
}
