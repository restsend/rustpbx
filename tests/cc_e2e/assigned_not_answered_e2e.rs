//! E2E: assigned-but-not-answered (分配未接起) — the 486 auto-reject loop
//! incident.
//!
//! Scenario (production drift): the agent's client is on 小休 (break) but the
//! server's presence view drifted to `idle` (cross-node release losing the
//! pre-ring capture). ACD dispatches, the client auto-rejects with 486.
//!
//! Pins the full contract on the REAL adapter + registry + alerting plane:
//!
//! 1. the durable history reconstruction restores the client-declared break
//!    (server presence re-aligned, audited);
//! 2. the instant `assigned_not_answered` alert reaches the RWI webhook with
//!    the presence-mismatch marker and critical severity;
//! 3. a durable `cc_alert_history` row is persisted for the audit trail;
//! 4. the cooldown dedup suppresses a second firing within the window;
//! 5. the rejecting agent is not a dispatch candidate again (loop dead).

use std::sync::Arc;
use std::time::Duration;

use rustpbx::addons::cc::acd::config::AcdConfig;
use rustpbx::addons::cc::acd::engine::AcdEngine;
use rustpbx::addons::cc::agent::{AgentRegistry, AgentStatus};
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::alerting::InstantAlertSink;
use rustpbx::addons::cc::models::cc_alert_history;
use rustpbx::call::app::agent_registry::{
    AgentRegistry as _, AssignFailReason, AssignFailedAlert,
};
use rustpbx::config::LocatorWebhookConfig;
use rustpbx::rwi::{
    RwiGateway, RwiGatewayRef,
    webhook::{WEBHOOK_CHANNEL_SIZE, start_rwi_webhook_handler},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use sea_orm_migration::MigratorTrait;

use crate::common::webhook_capture::WebhookCapture;

async fn e2e_db() -> sea_orm::DatabaseConnection {
    let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();
    db
}

fn webhook_config(url: String) -> LocatorWebhookConfig {
    LocatorWebhookConfig {
        url,
        events: vec![],
        headers: None,
        timeout_ms: Some(5000),
        retries: None,
        track_queue_latency: None,
    }
}

/// The production drift, replayed: agent goes on 小休, the queue's ring
/// OVERWRITES the break — and the node-local restore capture is LOST
/// (release on another node / owner restart). This is the exact shape the
/// durable reconstruction heals.
async fn ring_over_break_without_capture(registry: &Arc<AgentRegistry>, agent: &str, call: &str) {
    registry.register(agent.into(), vec![], 1).await.unwrap();
    registry
        .update_status(agent, AgentStatus::Away("lunch".into()))
        .await
        .unwrap();
    // try_reserve overwrites the break; NO capture_resume_status follows —
    // the cross-node / post-restart view.
    assert!(registry.try_reserve_agent(agent, call.into()).await);
}

#[tokio::test]
async fn assigned_not_answered_drift_heals_alerts_and_audits() {
    let _ = tracing_subscriber::fmt::try_init();
    let capture = WebhookCapture::start().await;
    let webhook_tx =
        start_rwi_webhook_handler(webhook_config(capture.url.clone()), WEBHOOK_CHANNEL_SIZE);
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(webhook_tx);
        gw
    }));

    let db = e2e_db().await;
    let registry = Arc::new(AgentRegistry::with_db(db.clone()));
    ring_over_break_without_capture(&registry, "ag-e2e", "call-486").await;

    let engine = Arc::new(AcdEngine::new(AcdConfig::default()));
    let adapter = Arc::new(
        CcAgentRegistryAdapter::new(registry.clone(), engine, "localhost").with_instant_alerts(
            InstantAlertSink {
                gateway: Some(gateway),
                recent_alerts: Some(Default::default()),
                cooldowns: Some(Default::default()),
                webhook_url: String::new(),
                cooldown_secs: 60,
                enabled: true,
                db: Some(db.clone()),
            },
        ),
    );

    // ── 1. The reject reconciles the drifted presence back to the break ──
    let outcome = adapter
        .note_agent_rejected("ag-e2e", "call-486", None)
        .await;
    assert_eq!(
        outcome.restored_break.as_deref(),
        Some("away:lunch"),
        "the durable reconstruction must heal the drifted view"
    );
    assert!(outcome.state_mismatch());
    let agent = registry.get_agent("ag-e2e").await.unwrap();
    assert!(matches!(agent.status, AgentStatus::Away(ref d) if d == "lunch"));

    // ── 2. The instant alert reaches the RWI webhook (critical + mismatch) ──
    // The queue app fires this through the adapter's sink.
    adapter
        .emit_assign_failed_alert(&AssignFailedAlert {
            call_id: "call-486".into(),
            queue_id: "sg-e2e".into(),
            agent_id: "ag-e2e".into(),
            attempt: 1,
            reason: AssignFailReason::Rejected,
            sip_status: Some(486),
            outcome: Some(outcome.clone()),
        })
        .await;

    let payload = {
        let mut found = None;
        for _ in 0..50 {
            let got = capture.received.lock().unwrap();
            if let Some(v) = got
                .iter()
                .find(|v| {
                    v["event_type"].as_str() == Some("queue_alert")
                        && v["event"]["alert_type"].as_str() == Some("assigned_not_answered")
                })
                .cloned()
            {
                found = Some(v);
                break;
            }
            drop(got);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        found.expect("assigned_not_answered alert must reach the RWI webhook")
    };
    assert_eq!(payload["event"]["queue_id"], "sg-e2e");
    assert_eq!(payload["event"]["severity"], "critical");
    assert!(
        payload["event"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("PRESENCE MISMATCH")),
        "the alert must call out the drift, got {}",
        payload["event"]["message"]
    );

    // ── 3. Durable audit row in cc_alert_history ──
    let rows = cc_alert_history::Entity::find()
        .filter(cc_alert_history::Column::AlertType.eq("assigned_not_answered"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one persisted alert row");
    assert_eq!(rows[0].severity, "critical");
    assert_eq!(rows[0].queue_id, "sg-e2e");

    // ── 4. Cooldown dedup: a second firing inside the window is suppressed ──
    adapter
        .emit_assign_failed_alert(&AssignFailedAlert {
            call_id: "call-486-b".into(),
            queue_id: "sg-e2e".into(),
            agent_id: "ag-e2e".into(),
            attempt: 2,
            reason: AssignFailReason::Rejected,
            sip_status: Some(486),
            outcome: Some(outcome),
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rows = cc_alert_history::Entity::find()
        .filter(cc_alert_history::Column::AlertType.eq("assigned_not_answered"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "cooldown must suppress the duplicate");

    // ── 5. The loop is dead: the restored break is not a dispatch candidate ──
    let candidates = registry.find_candidates(&[]).await;
    assert!(
        candidates.iter().all(|a| a.agent_id != "ag-e2e"),
        "the rejecting agent must not be re-dispatched"
    );
}

/// The plain (non-drifted) reject: no mismatch marker, warning severity —
/// proving the escalation is driven by evidence, not fired unconditionally.
#[tokio::test]
async fn assigned_not_answered_plain_reject_stays_warning() {
    let db = e2e_db().await;
    let registry = Arc::new(AgentRegistry::with_db(db.clone()));
    registry.register("ag-2".into(), vec![], 1).await.unwrap();
    registry.update_status("ag-2", AgentStatus::Idle).await.unwrap();
    assert!(registry.try_reserve_agent("ag-2", "call-x".into()).await);

    let engine = Arc::new(AcdEngine::new(AcdConfig::default()));
    let adapter = Arc::new(
        CcAgentRegistryAdapter::new(registry.clone(), engine, "localhost").with_instant_alerts(
            InstantAlertSink {
                gateway: None,
                recent_alerts: Some(Default::default()),
                cooldowns: Some(Default::default()),
                webhook_url: String::new(),
                cooldown_secs: 60,
                enabled: true,
                db: Some(db.clone()),
            },
        ),
    );

    let outcome = adapter.note_agent_rejected("ag-2", "call-x", None).await;
    assert!(outcome.restored_break.is_none(), "no drift evidence");
    assert_eq!(outcome.cooldown_secs, Some(30), "reject cooldown floor");

    adapter
        .emit_assign_failed_alert(&AssignFailedAlert {
            call_id: "call-x".into(),
            queue_id: "sg-e2e".into(),
            agent_id: "ag-2".into(),
            attempt: 1,
            reason: AssignFailReason::Rejected,
            sip_status: Some(486),
            outcome: Some(outcome),
        })
        .await;
    // Read back from the DB (the durable audit trail).
    let records = cc_alert_history::Entity::find()
        .filter(cc_alert_history::Column::AlertType.eq("assigned_not_answered"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].severity, "warning");
}
