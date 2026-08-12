use std::time::Instant;

use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database};
use sea_orm_migration::MigratorTrait;

use rustpbx::addons::cc::CcAddonState;
use rustpbx::addons::cc::agent::AgentStatus;
use rustpbx::rwi::proto::RwiEvent;

/// Test that AgentStateChanged, AgentRegistered, AgentUnregistered events
/// are emitted correctly for ALL status transitions.
#[tokio::test]
async fn test_agent_events_on_transitions() {
    let _ = tracing_subscriber::fmt::try_init();

    // Setup in-memory database
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect sqlite memory");
    rustpbx::models::migration::Migrator::up(&db, None)
        .await
        .expect("core migrations");
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .expect("cc migrations");

    // Create extension + agent + endpoint
    let extension_number = "bot";
    let _extension = rustpbx::models::extension::ActiveModel {
        extension: Set(extension_number.to_string()),
        sip_password: Set(Some("testpass".to_string())),
        login_disabled: Set(false),
        voicemail_disabled: Set(false),
        allow_guest_calls: Set(false),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create extension");

    let agent_id = "agent001";
    let _agent = rustpbx::addons::cc::models::cc_agent::ActiveModel {
        agent_id: Set(agent_id.to_string()),
        display_name: Set(Some("Test Agent".to_string())),
        primary_endpoint: Set(Some(extension_number.to_string())),
        skills: Set(serde_json::json!([])),
        max_concurrency: Set(1),
        is_active: Set(true),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create agent");

    let _endpoint = rustpbx::addons::cc::models::cc_agent_endpoint::ActiveModel {
        agent_id: Set(agent_id.to_string()),
        endpoint_type: Set("extension".to_string()),
        endpoint_value: Set(extension_number.to_string()),
        priority: Set(1),
        is_active: Set(true),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create endpoint");

    // Create CC addon state
    let cc_state = CcAddonState::with_db(db.clone());
    cc_state
        .agent_registry
        .load_from_db()
        .await
        .expect("load agents");

    // Set up event channel for verification
    let (test_tx, mut test_rx) = tokio::sync::mpsc::unbounded_channel::<RwiEvent>();
    cc_state.agent_registry.set_event_tx(test_tx);

    // Verify initial state
    assert_eq!(
        cc_state.agent_registry.list_agents().await[0]
            .status
            .to_string(),
        "offline"
    );

    // ── Test 1: Offline → Idle via update_status ──────────────────────
    // This simulates what the bridge does on SIP register
    cc_state
        .agent_registry
        .update_status(agent_id, AgentStatus::Idle)
        .await
        .expect("update to idle");

    // Should receive AgentStateChanged
    let event = test_rx
        .try_recv()
        .expect("should have AgentStateChanged event");
    assert_eq!(
        event.event_type, "agent_state_changed",
        "event type should be agent_state_changed"
    );
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["agent_id"].as_str(), Some(agent_id));
    assert_eq!(payload["from_status"].as_str(), Some("offline"));
    assert_eq!(payload["to_status"].as_str(), Some("idle"));

    // ── Test 2: Idle → Ringing (incoming call) ────────────────────────
    cc_state
        .agent_registry
        .update_status(
            agent_id,
            AgentStatus::Ringing {
                call_id: "call-1".into(),
                since: Instant::now(),
            },
        )
        .await
        .expect("update to ringing");

    let event = test_rx.try_recv().expect("should have Ringing event");
    assert_eq!(event.event_type, "agent_state_changed");
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["from_status"].as_str(), Some("idle"));
    assert_eq!(payload["to_status"].as_str(), Some("ringing"));
    assert_eq!(payload["call_id"].as_str(), Some("call-1"));

    // ── Test 3: Ringing → Busy (answering call) ───────────────────────
    cc_state
        .agent_registry
        .update_status(
            agent_id,
            AgentStatus::Busy {
                call_id: "call-1".into(),
                since: Instant::now(),
            },
        )
        .await
        .expect("update to busy");

    let event = test_rx.try_recv().expect("should have Busy event");
    assert_eq!(event.event_type, "agent_state_changed");
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["from_status"].as_str(), Some("ringing"));
    assert_eq!(payload["to_status"].as_str(), Some("busy"));
    assert_eq!(payload["call_id"].as_str(), Some("call-1"));

    // ── Test 4: Busy → Wrapup (hanging up) ────────────────────────────
    cc_state
        .agent_registry
        .update_status(
            agent_id,
            AgentStatus::Wrapup {
                call_id: "call-1".into(),
                since: Instant::now(),
            },
        )
        .await
        .expect("update to wrapup");

    let event = test_rx.try_recv().expect("should have Wrapup event");
    assert_eq!(event.event_type, "agent_state_changed");
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["from_status"].as_str(), Some("busy"));
    assert_eq!(payload["to_status"].as_str(), Some("wrapup"));
    assert_eq!(payload["call_id"].as_str(), Some("call-1"));

    // ── Test 5: Wrapup → Idle (after-call work done) ──────────────────
    cc_state
        .agent_registry
        .update_status(agent_id, AgentStatus::Idle)
        .await
        .expect("update to idle from wrapup");

    let event = test_rx
        .try_recv()
        .expect("should have Idle from wrapup event");
    assert_eq!(event.event_type, "agent_state_changed");
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["from_status"].as_str(), Some("wrapup"));
    assert_eq!(payload["to_status"].as_str(), Some("idle"));
    assert_eq!(payload["call_id"].as_str(), Some("call-1"));

    // ── Test 6: Idle → Busy (outbound call, agent is already idle) ────
    let (test_tx3, mut test_rx3) = tokio::sync::mpsc::unbounded_channel();
    cc_state.agent_registry.set_event_tx(test_tx3);
    cc_state
        .agent_registry
        .update_status(
            agent_id,
            AgentStatus::Busy {
                call_id: "outbound-call".into(),
                since: Instant::now(),
            },
        )
        .await
        .expect("update to busy from idle");

    let event = test_rx3
        .try_recv()
        .expect("should have Busy event from idle");
    assert_eq!(event.event_type, "agent_state_changed");
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["from_status"].as_str(), Some("idle"));
    assert_eq!(payload["to_status"].as_str(), Some("busy"));
    assert_eq!(payload["call_id"].as_str(), Some("outbound-call"));

    // Clean up: back to offline via wrapup
    let (test_tx4, mut test_rx4) = tokio::sync::mpsc::unbounded_channel();
    cc_state.agent_registry.set_event_tx(test_tx4);
    cc_state
        .agent_registry
        .update_status(
            agent_id,
            AgentStatus::Wrapup {
                call_id: "outbound-call".into(),
                since: Instant::now(),
            },
        )
        .await
        .unwrap();
    let _ = test_rx4.try_recv();
    cc_state
        .agent_registry
        .update_status(agent_id, AgentStatus::Offline)
        .await
        .unwrap();
    let _ = test_rx4.try_recv();

    // ── Test 7: Idle → Offline (SIP unregister, go idle first) ────────
    let (test_tx5, mut test_rx5) = tokio::sync::mpsc::unbounded_channel();
    cc_state.agent_registry.set_event_tx(test_tx5);
    cc_state
        .agent_registry
        .update_status(agent_id, AgentStatus::Idle)
        .await
        .expect("back to idle");
    let _ = test_rx5.try_recv(); // offline→idle
    cc_state
        .agent_registry
        .update_status(agent_id, AgentStatus::Offline)
        .await
        .expect("update to offline from idle");

    let event = test_rx5.try_recv().expect("should have Offline event");
    assert_eq!(event.event_type, "agent_state_changed");
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["from_status"].as_str(), Some("idle"));
    assert_eq!(payload["to_status"].as_str(), Some("offline"));

    // ── Test 7: Direct AgentRegistered event on new agent registration ─
    let cc_state2 = CcAddonState::with_db(db.clone());
    let (test_tx2, mut test_rx2) = tokio::sync::mpsc::unbounded_channel::<RwiEvent>();
    cc_state2.agent_registry.set_event_tx(test_tx2);

    // Register a NEW agent (simulates adding from console)
    cc_state2
        .agent_registry
        .register("new-agent".to_string(), vec![], 1)
        .await
        .expect("register agent");

    // Should receive AgentRegistered
    let event = test_rx2
        .try_recv()
        .expect("should have AgentRegistered event");
    assert_eq!(event.event_type, "agent_registered");
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["agent_id"].as_str(), Some("new-agent"));

    // Should also receive AgentStateChanged
    let event = test_rx2
        .try_recv()
        .expect("should have AgentStateChanged after register");
    assert_eq!(event.event_type, "agent_state_changed");
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["from_status"].as_str(), Some("offline"));
    assert_eq!(payload["to_status"].as_str(), Some("idle"));

    // ── Test 8: Unregister agent ──────────────────────────────────────
    cc_state2
        .agent_registry
        .unregister("new-agent")
        .await
        .expect("unregister agent");

    // Should receive AgentUnregistered
    let event = test_rx2
        .try_recv()
        .expect("should have AgentUnregistered event");
    assert_eq!(event.event_type, "agent_unregistered");
    let payload = event.payload.as_object().expect("payload object");
    assert_eq!(payload["agent_id"].as_str(), Some("new-agent"));

    // Should also receive AgentStateChanged
    let event = test_rx2
        .try_recv()
        .expect("should have AgentStateChanged after unregister");
    assert_eq!(event.event_type, "agent_state_changed");

    eprintln!("All event tests passed!");
}
