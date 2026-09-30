//! Self-tests for `tests/common/rwi_timeline.rs`.
//! The contract helper is exercised on synthetic webhook envelopes so its
//! pass/fail behavior is pinned WITHOUT needing the addon-cc e2e suites.

use crate::common::rwi_timeline::RwiTimeline;
use serde_json::json;

/// Build a webhook envelope like the real receiver captures:
/// `{"call_id", "event_type", "timestamp", "event": {payload}}`.
fn envelope(
    call_id: &str,
    ts: &str,
    event_type: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    json!({
        "call_id": call_id,
        "event_type": event_type,
        "timestamp": ts,
        "event": payload,
    })
}

/// A clean queue→agent timeline must pass the contract.
#[test]
fn clean_queue_agent_timeline_passes() {
    let cid = "clean-call";
    let events = vec![
        envelope(cid, "2026-09-29T07:00:00.100Z", "call_created", json!({"callee":"bob"})),
        envelope(cid, "2026-09-29T07:00:00.200Z", "queue_joined", json!({"queue_id":"queue-name"})),
        envelope(cid, "2026-09-29T07:00:01.000Z", "call_ringing", json!({
            "leg_id": "leg-1", "agent_id": "489218", "agent_name": "T-G2", "queue_id": "queue-name"
        })),
        envelope(cid, "2026-09-29T07:00:01.001Z", "call_ringing", json!({
            "agent_id": "489218", "agent_name": "T-G2", "queue_id": "queue-name"
        })),
        envelope(cid, "2026-09-29T07:00:01.002Z", "queue_agent_offered", json!({
            "agent_id": "489218", "queue_id": "queue-name"
        })),
        envelope(cid, "2026-09-29T07:00:03.000Z", "queue_agent_connected", json!({
            "agent_id": "489218", "queue_id": "queue-name"
        })),
        envelope(cid, "2026-09-29T07:00:03.004Z", "call_answered", json!({
            "agent_id": "489218", "agent_name": "T-G2", "queue_id": "queue-name"
        })),
        envelope(cid, "2026-09-29T07:02:00.000Z", "call_hangup", json!({
            "agent_id": "489218", "queue_id": "queue-name"
        })),
    ];
    let tl = RwiTimeline::from_events(cid, events);
    tl.assert_queue_agent_contract("489218"); // must not panic
    assert_eq!(tl.event_count(), 8);
}

/// The production-42 failure mode: both ringing events missing agent_id →
/// the contract must fail.
#[test]
fn ringings_without_agent_fail() {
    let cid = "broken-call";
    let events = vec![
        envelope(cid, "t0", "call_created", json!({})),
        envelope(cid, "t1", "queue_joined", json!({"queue_id":"queue-name"})),
        envelope(cid, "t2", "call_ringing", json!({"leg_id": "leg-1"})),
        envelope(cid, "t3", "call_ringing", json!({"queue_id": "queue-name"})),
        envelope(cid, "t4", "queue_agent_offered", json!({"agent_id": "489218"})),
        envelope(cid, "t5", "queue_agent_connected", json!({"agent_id": "489218"})),
        envelope(cid, "t6", "call_answered", json!({"agent_id": "489218"})),
        envelope(cid, "t7", "call_hangup", json!({"agent_id": "489218"})),
    ];
    let tl = RwiTimeline::from_events(cid, events);
    let result = std::panic::catch_unwind(move || tl.assert_queue_agent_contract("489218"));
    let err = result.err().expect("contract must FAIL for unattributed ringings");
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(msg.contains("call_ringing (leg-level)"), "msg: {msg}");
    assert!(msg.contains("call_ringing (session-level)"), "msg: {msg}");
}

/// The production-43 failure mode: only the LEG-level ringing missing the
/// agent (session-level enriched) → the contract must fail on that one.
#[test]
fn leg_ringing_missing_agent_fails_even_when_session_level_has_it() {
    let cid = "leg-gap-call";
    let events = vec![
        envelope(cid, "t0", "call_created", json!({})),
        envelope(cid, "t1", "queue_joined", json!({"queue_id":"queue-name"})),
        envelope(cid, "t2", "call_ringing", json!({"leg_id": "leg-1"})),
        envelope(cid, "t3", "call_ringing", json!({"agent_id": "489218"})),
        envelope(cid, "t4", "queue_agent_offered", json!({"agent_id": "489218"})),
        envelope(cid, "t5", "queue_agent_connected", json!({"agent_id": "489218"})),
        envelope(cid, "t6", "call_answered", json!({"agent_id": "489218"})),
        envelope(cid, "t7", "call_hangup", json!({"agent_id": "489218"})),
    ];
    let tl = RwiTimeline::from_events(cid, events);
    let result = std::panic::catch_unwind(move || tl.assert_queue_agent_contract("489218"));
    let err = result.err().expect("leg-level gap must FAIL the contract");
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(msg.contains("call_ringing (leg-level)"), "msg: {msg}");
    assert!(!msg.contains("call_ringing (session-level)"), "msg: {msg}");
}

/// Duplicated session-level `call_answered` (an earlier incident class) must
/// fail, as must a leg-scoped one.
#[test]
fn duplicated_or_leg_scoped_answered_fails() {
    let cid = "dup-answered-call";
    let base = |answer_payload: serde_json::Value| {
        vec![
            envelope(cid, "t0", "call_created", json!({})),
            envelope(cid, "t1", "queue_joined", json!({})),
            envelope(cid, "t2", "call_ringing", json!({"leg_id": "leg-1", "agent_id": "489218"})),
            envelope(cid, "t3", "call_ringing", json!({"agent_id": "489218"})),
            envelope(cid, "t4", "queue_agent_offered", json!({"agent_id": "489218"})),
            envelope(cid, "t5", "queue_agent_connected", json!({"agent_id": "489218"})),
            envelope(cid, "t6", "call_answered", answer_payload),
            envelope(cid, "t7", "call_hangup", json!({"agent_id": "489218"})),
        ]
    };
    // Exactly-one: a second answered fails.
    let mut events = base(json!({"agent_id": "489218"}));
    events.push(envelope(cid, "t8", "call_answered", json!({"agent_id": "489218"})));
    let tl = RwiTimeline::from_events(cid, events);
    let result = std::panic::catch_unwind(move || tl.assert_queue_agent_contract("489218"));
    assert!(result.is_err(), "duplicate call_answered must fail");

    // Session-scoped: a leg-scoped answered fails.
    let events = base(json!({"leg_id": "leg-1", "agent_id": "489218"}));
    let tl = RwiTimeline::from_events(cid, events);
    let result = std::panic::catch_unwind(move || tl.assert_queue_agent_contract("489218"));
    let err = result.err().expect("leg-scoped call_answered must fail");
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(msg.contains("session-scoped"), "msg: {msg}");
}

/// Ordering violations must be caught: ringing after offered.
#[test]
fn ringing_after_offered_fails() {
    let cid = "order-call";
    let events = vec![
        envelope(cid, "t0", "call_created", json!({})),
        envelope(cid, "t1", "queue_joined", json!({})),
        envelope(cid, "t2", "queue_agent_offered", json!({"agent_id": "489218"})),
        envelope(cid, "t3", "call_ringing", json!({"leg_id": "leg-1", "agent_id": "489218"})),
        envelope(cid, "t4", "call_ringing", json!({"agent_id": "489218"})),
        envelope(cid, "t5", "queue_agent_connected", json!({"agent_id": "489218"})),
        envelope(cid, "t6", "call_answered", json!({"agent_id": "489218"})),
        envelope(cid, "t7", "call_hangup", json!({"agent_id": "489218"})),
    ];
    let tl = RwiTimeline::from_events(cid, events);
    let result = std::panic::catch_unwind(move || tl.assert_queue_agent_contract("489218"));
    let err = result.err().expect("ringing after offered must fail");
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(msg.contains("AFTER queue_agent_offered"), "msg: {msg}");
}
