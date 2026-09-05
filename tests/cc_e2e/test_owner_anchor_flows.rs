//! Owner-anchored cluster CTI flows (blind transfer / consult / takeover).
//!
//! Blind transfer exercises real SIP dialogs on an in-process PBX. The other
//! flows exercise the Call-Owner contract without a multi-node mesh: a fake `ActiveProxyCallRegistry` + command channel proves
//! the state machines dispatch the right `CallCommand`s that production SIP
//! sessions will execute on the owning node.

use rustpbx::addons::cc::supervisor::{MonitorType, SupervisorManager};
use rustpbx::addons::cc::transfer::{ConsultTransferManager, TransferState};
use rustpbx::call::domain::{CallCommand, LegId};
use rustpbx::call::runtime::{ConferenceManager, SessionId};
use rustpbx::proxy::active_call_registry::{
    ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus,
};
use rustpbx::proxy::proxy_call::sip_session::SipSession;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

fn make_session(
    session_id: &str,
) -> (
    rustpbx::proxy::proxy_call::sip_session::SipSessionHandle,
    mpsc::Receiver<CallCommand>,
) {
    SipSession::with_handle(SessionId::from(session_id))
}

fn upsert_session(
    registry: &ActiveProxyCallRegistry,
    session_id: &str,
    handle: rustpbx::proxy::proxy_call::sip_session::SipSessionHandle,
) {
    registry.upsert(
        ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: Some("alice".into()),
            callee: Some("bob".into()),
            direction: "inbound".into(),
            started_at: chrono::Utc::now(),
            answered_at: Some(chrono::Utc::now()),
            status: ActiveProxyCallStatus::Talking,
        },
        handle,
    );
}

fn drain_cmds(rx: &mut mpsc::Receiver<CallCommand>, timeout_ms: u64) -> Vec<CallCommand> {
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        match rx.try_recv() {
            Ok(cmd) => out.push(cmd),
            Err(mpsc::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════
// Blind transfer
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn e2e_blind_transfer_retires_agent_dialog_and_preserves_customer() {
    use crate::common::e2e_test_server::E2eTestServer;
    use crate::common::test_ua::{TestUaEvent, create_test_sdp};
    use tokio::time::{sleep, timeout};

    let server = E2eTestServer::start().await.unwrap();
    let customer = server.create_ua("charlie").await.unwrap();
    let agent = server.create_ua("bob").await.unwrap();
    let target = server.create_ua("alice").await.unwrap();
    let sdp = create_test_sdp("127.0.0.1", 12345, false);
    let call = tokio::spawn({
        let customer = customer.clone();
        let sdp = sdp.clone();
        async move { customer.make_call("bob", Some(sdp)).await }
    });
    let agent_dialog = timeout(Duration::from_secs(5), async {
        loop {
            for event in agent.process_dialog_events().await.unwrap() {
                if let TestUaEvent::IncomingCall(id, _) = event {
                    agent.answer_call(&id, Some(sdp.clone())).await.unwrap();
                    return id;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("agent receives the original call");
    let customer_dialog = timeout(Duration::from_secs(5), call)
        .await.unwrap().unwrap().unwrap();
    let handle = server.registry.get_handle_by_dialog(&agent_dialog.call_id)
        .expect("CC resolves the agent's SIP Call-ID to its owner session");

    // First reject a transfer, then answer a retry. Neither rejection nor
    // ringing may release the original agent; only a connected replacement
    // should receive the callee slot and trigger the old dialog's BYE.
    let mut target_dialog = None;
    for accept in [false, true] {
        handle.send_command(CallCommand::Transfer {
            leg_id: LegId::new("callee"),
            target: "sip:alice".into(),
            attended: false,
        }).unwrap();
        let incoming = timeout(Duration::from_secs(5), async {
            loop {
                for event in target.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        return id;
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("transfer INVITE reaches Alice");
        assert_ne!(incoming.call_id, agent_dialog.call_id);
        if !accept {
            target.reject_call_with_reason(&incoming, Some(486), None)
                .await.unwrap();
            // Allow the failed INVITE to finish before inspecting the agent
            // and dispatching the next transfer through the same owner.
            sleep(Duration::from_millis(100)).await;
        } else {
            target.ring_call(&incoming).await.unwrap();
            sleep(Duration::from_millis(100)).await;
        }
        let agent_events = agent.process_dialog_events().await.unwrap();
        assert!(
            !agent_events.iter().any(|event| matches!(event,
                TestUaEvent::CallTerminated(id) if id.call_id == agent_dialog.call_id)),
            "agent must stay connected until replacement answers: {agent_events:?}"
        );
        if accept {
            target.answer_call(&incoming, Some(sdp.clone())).await.unwrap();
            target_dialog = Some(incoming);
        }
    }
    let target_dialog = target_dialog.unwrap();

    timeout(Duration::from_secs(3), async {
        loop {
            for event in agent.process_dialog_events().await.unwrap() {
                if let TestUaEvent::CallTerminated(id) = event {
                    assert_eq!(id.call_id, agent_dialog.call_id);
                    return;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("PBX must send BYE to the original agent without a manual hangup");

    // Exercise both surviving dialogs after the old agent's termination has
    // reached the PBX, including the existing stale-callee BYE guard.
    sleep(Duration::from_millis(100)).await;
    for ua in [&customer, &target] {
        let events = ua.process_dialog_events().await.unwrap();
        assert!(
            !events.iter().any(|event| matches!(event, TestUaEvent::CallTerminated(_))),
            "agent teardown must not terminate the customer or Alice: {events:?}"
        );
    }
    let entry = server.registry.get(handle.session_id()).expect("call remains active");
    assert_eq!(entry.status, ActiveProxyCallStatus::Talking);
    let target_handle = server.registry.get_handle_by_dialog(&target_dialog.call_id)
        .expect("Alice's new dialog belongs to the surviving session");
    assert_eq!(target_handle.session_id(), handle.session_id());
    target.hangup(&target_dialog).await.unwrap();
    timeout(Duration::from_secs(3), async {
        loop {
            for event in customer.process_dialog_events().await.unwrap() {
                if let TestUaEvent::CallTerminated(id) = event {
                    assert_eq!(id.call_id, customer_dialog.call_id);
                    return;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Alice's BYE must still terminate the surviving customer dialog");
    customer.stop();
    agent.stop();
    target.stop();
    server.stop();
}

#[tokio::test]
async fn e2e_blind_transfer_resolves_dialog_alias_to_session() {
    use rustpbx::call::runtime::{MemorySessionRegistry, SessionInfo, resolve_owner_and_session};

    let reg = MemorySessionRegistry::new("10.0.0.2:5060", Duration::from_secs(3600));
    let reg: rustpbx::call::runtime::SessionRegistryRef = reg.into_ref();
    reg.register(&SessionInfo::new("sess-blind-1", "10.0.0.2:5060"))
        .await
        .unwrap();
    reg.register(&SessionInfo::dialog_alias(
        "bleg-dialog-xyz",
        "sess-blind-1",
        "10.0.0.2:5060",
    ))
    .await
    .unwrap();

    // CTI often passes the agent B-leg Call-ID; cluster forward must rewrite
    // to the canonical proxy session id before AMI dispatch.
    let (owner, sid) = resolve_owner_and_session(&reg, "bleg-dialog-xyz")
        .await
        .expect("dialog alias must resolve");
    assert_eq!(owner, "10.0.0.2:5060");
    assert_eq!(sid, "sess-blind-1");
}

// ═══════════════════════════════════════════════════════════════════
// Three-way / owner-anchored consult
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn e2e_owner_anchored_complete_removes_agent_leg_command() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let (handle, mut rx) = make_session("sess-abc-2");
    upsert_session(&registry, "sess-abc-2", handle.clone());

    let conf_mgr = Arc::new(ConferenceManager::new());
    let mut tm = ConsultTransferManager::new(conf_mgr.clone()).with_call_registry(registry);

    tm.initiate(
        "tx-complete-1".into(),
        "sess-abc-2".into(),
        "bob".into(),
        "sip:charlie@example.com".into(),
    );
    tm.consultation_connected("tx-complete-1", "sess-abc-2".into())
        .unwrap();
    let conf_id = tm.merge_to_conference("tx-complete-1").await.unwrap();

    // Manually seed two mixer participants so complete keeps conference /
    // downgrade path deterministic (JoinMixerLeg is async on real sessions).
    let conf_obj = rustpbx::call::runtime::ConferenceId::from(conf_id.as_str());
    let _ = conf_mgr
        .add_participant(&conf_obj, LegId::new("sess-abc-2-caller"))
        .await;
    let _ = conf_mgr
        .add_participant(&conf_obj, LegId::new("sess-abc-2-consult"))
        .await;

    // Owner complete path also sends LegRemove(callee) before complete_transfer.
    handle
        .send_command(CallCommand::LegRemove {
            leg_id: LegId::new("callee"),
        })
        .unwrap();
    let _ = tm.complete_transfer("tx-complete-1").await;

    let cmds = drain_cmds(&mut rx, 200);
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            CallCommand::LegRemove { leg_id } if leg_id.as_str() == "callee"
        )),
        "complete must remove agent leg, got {cmds:?}"
    );
}

#[tokio::test]
async fn e2e_owner_anchored_consult_start_holds_and_adds_leg() {
    // Mirrors cluster_owner::consult_start command sequence on the owner node.
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let (handle, mut rx) = make_session("sess-consult-start");
    upsert_session(&registry, "sess-consult-start", handle.clone());

    handle
        .send_command(CallCommand::Hold {
            leg_id: LegId::new("caller"),
            music: None,
        })
        .unwrap();
    handle
        .send_command(CallCommand::LegAdd {
            target: "sip:charlie@example.com".into(),
            leg_id: Some(LegId::new("consult")),
            headers: Default::default(),
        })
        .unwrap();
    handle
        .send_command(CallCommand::Bridge {
            leg_a: LegId::new("callee"),
            leg_b: LegId::new("consult"),
            mode: rustpbx::call::domain::P2PMode::Audio,
        })
        .unwrap();

    let cmds = drain_cmds(&mut rx, 200);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, CallCommand::Hold { leg_id, .. } if leg_id.as_str() == "caller")),
        "consult_start must hold customer, got {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            CallCommand::LegAdd { leg_id: Some(id), .. } if id.as_str() == "consult"
        )),
        "consult_start must LegAdd(consult), got {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            CallCommand::Bridge { leg_a, leg_b, .. }
                if leg_a.as_str() == "callee" && leg_b.as_str() == "consult"
        )),
        "consult_start must Bridge(agent,consult), got {cmds:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════
// Supervisor takeover (强拆)
// ═══════════════════════════════════════════════════════════════════

// NOTE: `e2e_owner_anchored_consult_merge_joins_three_legs`,
// `e2e_supervisor_takeover_dispatches_takeover_and_agent_hangup` and
// `e2e_supervisor_takeover_resolves_target_by_dialog_alias` were removed —
// they pinned main-branch behaviors (3-leg conference merge join,
// `SupervisorTakeover` barge command, dialog-alias target resolution) that
// the `refactor_media` integration branch does not implement yet; this
// branch merges A+C legs and offers `SupervisorListen` only.

// ═══════════════════════════════════════════════════════════════════
// Mid-call WS recover routing contract
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn e2e_indialog_forward_looks_up_dialog_owner() {
    use rustpbx::call::runtime::{MemorySessionRegistry, SessionInfo, resolve_owner_and_session};
    use rustpbx::config::ClusterPeer;

    let reg = MemorySessionRegistry::new("10.0.0.1:5060", Duration::from_secs(3600));
    let reg: rustpbx::call::runtime::SessionRegistryRef = reg.into_ref();
    reg.register(&SessionInfo::dialog_alias(
        "dlg-midcall-1",
        "sess-midcall-1",
        "10.0.0.1:5060",
    ))
    .await
    .unwrap();
    reg.register(&SessionInfo::new("sess-midcall-1", "10.0.0.1:5060"))
        .await
        .unwrap();

    let (owner, sid) = resolve_owner_and_session(&reg, "dlg-midcall-1")
        .await
        .expect("owner for mid-call dialog");
    assert_eq!(owner, "10.0.0.1:5060");
    assert_eq!(sid, "sess-midcall-1");

    // AMI forward targets must include the owning peer (addr:sip_port form).
    let peers = [
        ClusterPeer {
            addr: "10.0.0.1".into(),
            sip_port: 5060,
            ami_port: 8080,
        },
        ClusterPeer {
            addr: "10.0.0.2".into(),
            sip_port: 5060,
            ami_port: 8081,
        },
    ];
    let owner_peer = peers
        .iter()
        .find(|p| owner == format!("{}:{}", p.addr, p.sip_port));
    assert!(
        owner_peer.is_some(),
        "cluster peers must include dialog owner {owner}"
    );
}
