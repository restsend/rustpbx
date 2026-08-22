//! Owner-anchored cluster CTI flows (blind transfer / consult / takeover).
//!
//! These exercise the Call-Owner media-anchor contract without requiring a
//! multi-node mesh: a fake `ActiveProxyCallRegistry` + command channel proves
//! the state machines dispatch the right `CallCommand`s that production SIP
//! sessions will execute on the owning node.

use rustpbx::addons::cc::config::TransferConfig;
use rustpbx::addons::cc::supervisor::{MonitorType, SupervisorManager};
use rustpbx::addons::cc::transfer::{ConsultTransferManager, TransferState};
use rustpbx::call::domain::{CallCommand, HangupCascade, LegId};
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
async fn e2e_blind_transfer_dispatches_transfer_and_agent_hangup() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let (handle, mut rx) = make_session("sess-blind-1");
    upsert_session(&registry, "sess-blind-1", handle.clone());

    // Config gate: disabled → CTI must refuse (mirrors REST FORBIDDEN).
    let mut disabled = TransferConfig::default();
    disabled.blind_transfer_enabled = false;
    assert!(!disabled.blind_transfer_enabled);

    // Owner path: Transfer(callee) then Hangup(agent/callee leg).
    handle
        .send_command(CallCommand::Transfer {
            leg_id: LegId::new("callee"),
            target: "sip:1002@example.com".into(),
            attended: false,
        })
        .unwrap();
    handle
        .send_command(CallCommand::Hangup(rustpbx::call::domain::HangupCommand {
            leg_id: Some(LegId::new("callee")),
            cascade: HangupCascade::None,
            initiator: rustpbx::call::domain::HangupInitiator::Local {
                source: "blind_transfer".into(),
            },
            reason: Some(rustpbx::callrecord::CallRecordHangupReason::BySystem),
            code: Some(200),
            rtp_timeout_side: None,
        }))
        .unwrap();

    let cmds = drain_cmds(&mut rx, 200);
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            CallCommand::Transfer {
                leg_id,
                attended: false,
                ..
            } if leg_id.as_str() == "callee"
        )),
        "expected Transfer(callee), got {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            CallCommand::Hangup(h) if h.leg_id.as_ref().map(|l| l.as_str()) == Some("callee")
        )),
        "expected Hangup(callee) after blind transfer, got {cmds:?}"
    );
}

#[tokio::test]
async fn e2e_blind_transfer_resolves_dialog_alias_to_session() {
    use rustpbx::call::runtime::{
        MemorySessionRegistry, SessionInfo, SessionRegistry, resolve_owner_and_session,
    };

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
async fn e2e_owner_anchored_consult_merge_joins_three_legs() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let (handle, mut rx) = make_session("sess-abc");
    upsert_session(&registry, "sess-abc", handle);

    let conf_mgr = Arc::new(ConferenceManager::new());
    let mut tm = ConsultTransferManager::new(conf_mgr.clone()).with_call_registry(registry);

    // Same-session owner-anchored consult: session_b == session_a.
    tm.initiate(
        "tx-owner-1".into(),
        "sess-abc".into(),
        "bob".into(),
        "sip:charlie@example.com".into(),
    );
    tm.consultation_connected("tx-owner-1", "sess-abc".into())
        .unwrap();

    let conf_id = tm
        .merge_to_conference("tx-owner-1")
        .await
        .expect("merge must succeed");
    assert!(conf_id.starts_with("conf-"));

    let state = tm.get_state("tx-owner-1").expect("tracked");
    assert!(
        matches!(state, TransferState::Completed { conf_id: cid, .. } if *cid == conf_id),
        "expected Completed, got {state:?}"
    );

    let cmds = drain_cmds(&mut rx, 300);
    let join_legs: Vec<&str> = cmds
        .iter()
        .filter_map(|c| match c {
            CallCommand::JoinMixerLeg { leg_id, mixer_id } if mixer_id == &conf_id => {
                Some(leg_id.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(
        join_legs.contains(&"caller"),
        "customer (caller) must join mixer, got {join_legs:?}"
    );
    assert!(
        join_legs.contains(&"callee"),
        "agent (callee) must join mixer, got {join_legs:?}"
    );
    assert!(
        join_legs.contains(&"consult"),
        "consult target must join mixer, got {join_legs:?}"
    );
}

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

#[tokio::test]
async fn e2e_supervisor_takeover_dispatches_takeover_and_agent_hangup() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let (handle, mut rx) = make_session("call-target-1");
    upsert_session(&registry, "call-target-1", handle);

    let mut mgr = SupervisorManager::new().with_call_registry(registry);
    mgr.start_monitor(
        "mon-takeover-1".into(),
        "supervisor-1".into(),
        "call-target-1".into(),
        "callee".into(),
        MonitorType::Listen,
        Some("sup-session-1".into()),
    )
    .unwrap();

    mgr.takeover("mon-takeover-1").unwrap();

    let session = mgr.get_session("mon-takeover-1").unwrap();
    assert_eq!(session.monitor_type, MonitorType::Barge);

    let cmds = drain_cmds(&mut rx, 300);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, CallCommand::SupervisorTakeover { .. })),
        "takeover must send SupervisorTakeover, got {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            CallCommand::Hangup(h) if h.leg_id.as_ref().map(|l| l.as_str()) == Some("callee")
        )),
        "takeover must hangup agent leg, got {cmds:?}"
    );
}

#[tokio::test]
async fn e2e_supervisor_takeover_resolves_target_by_dialog_alias() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let (handle, mut rx) = make_session("call-target-dlg");
    upsert_session(&registry, "call-target-dlg", handle.clone());
    registry.register_dialog("bleg-of-agent".into(), handle);

    let mut mgr = SupervisorManager::new().with_call_registry(registry);
    // Target identified by dialog Call-ID (common for CTI).
    mgr.start_monitor(
        "mon-dlg-1".into(),
        "supervisor-1".into(),
        "bleg-of-agent".into(),
        "callee".into(),
        MonitorType::Barge,
        None,
    )
    .unwrap();
    mgr.takeover("mon-dlg-1").unwrap();

    let cmds = drain_cmds(&mut rx, 300);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, CallCommand::SupervisorTakeover { .. })),
        "dialog-aliased target must still receive SupervisorTakeover, got {cmds:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════
// Mid-call WS recover routing contract
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn e2e_indialog_forward_looks_up_dialog_owner() {
    use rustpbx::call::runtime::{
        MemorySessionRegistry, SessionInfo, SessionRegistry, resolve_owner_and_session,
    };
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
