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
        envelope(cid, "2026-09-29T07:00:03.002Z", "queue_left", json!({
            "agent_id": "489218", "queue_id": "queue-name", "reason": "connected"
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
    assert_eq!(tl.event_count(), 9);
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

// ── Option-A dequeue invariants (unit-level pinning) ─────────────────────

/// Catch-unwind helper returning the panic message.
fn contract_msg(tl: RwiTimeline, agent: &str) -> String {
    let result = std::panic::catch_unwind(move || tl.assert_queue_agent_contract(agent));
    let err = result.err().expect("contract must FAIL");
    err.downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn option_a_base(extra: serde_json::Value) -> Vec<serde_json::Value> {
    let mut events = vec![
        envelope("oa", "t0", "call_created", json!({})),
        envelope("oa", "t1", "queue_joined", json!({"queue_id": "q"})),
        envelope("oa", "t2", "call_ringing", json!({"leg_id": "l1", "agent_id": "a1"})),
        envelope("oa", "t3", "call_ringing", json!({"agent_id": "a1"})),
        envelope("oa", "t4", "queue_agent_offered", json!({"agent_id": "a1", "leg_id": "l1"})),
    ];
    if let serde_json::Value::Array(rows) = extra {
        for r in rows {
            events.push(envelope(
                "oa",
                r["t"].as_str().unwrap_or(""),
                r["type"].as_str().unwrap_or(""),
                r["payload"].clone(),
            ));
        }
    }
    events.push(envelope("oa", "t9", "call_answered", json!({"agent_id": "a1"})));
    events.push(envelope("oa", "t10", "call_hangup", json!({"agent_id": "a1"})));
    events
}

/// `queue_left{connected}` BEFORE `queue_agent_connected` (the assign-time
/// dequeue regression) must fail.
#[test]
fn queue_left_connected_before_connect_fails() {
    let events = option_a_base(json!([
        {"t": "t5", "type": "queue_left", "payload": {"reason": "connected", "agent_id": "a1"}},
        {"t": "t6", "type": "queue_agent_connected", "payload": {"agent_id": "a1", "leg_id": "l1"}},
    ]));
    let msg = contract_msg(RwiTimeline::from_events("oa", events), "a1");
    assert!(
        msg.contains("BEFORE queue_agent_connected"),
        "must flag assign-time dequeue: {msg}"
    );
}

/// A non-overflow `queue_left` inside the ringing window must fail.
#[test]
fn queue_left_abandoned_inside_ringing_window_fails() {
    let events = option_a_base(json!([
        {"t": "t5", "type": "queue_left", "payload": {"reason": "abandoned"}},
        {"t": "t6", "type": "queue_agent_connected", "payload": {"agent_id": "a1", "leg_id": "l1"}},
    ]));
    let msg = contract_msg(RwiTimeline::from_events("oa", events), "a1");
    assert!(
        msg.contains("inside the ringing window"),
        "must flag mid-ring leave: {msg}"
    );
}

/// An overflow stage switch (`queue_left{overflow}` mid-ring) is LEGAL and
/// must pass.
#[test]
fn queue_left_overflow_inside_ringing_window_passes() {
    let events = option_a_base(json!([
        {"t": "t5", "type": "queue_left", "payload": {"reason": "overflow"}},
        {"t": "t6", "type": "queue_agent_connected", "payload": {"agent_id": "a1", "leg_id": "l1"}},
        {"t": "t7", "type": "queue_left", "payload": {"reason": "connected"}},
    ]));
    // Two queue_left events (overflow + connected) violate the
    // exactly-one-terminal invariant — assert the SPECIFIC violation is
    // the count, not the Option-A window rule.
    let msg = contract_msg(RwiTimeline::from_events("oa", events), "a1");
    assert!(
        !msg.contains("inside the ringing window"),
        "overflow leave is legal mid-ring: {msg}"
    );
}

// ── queue* ↔ skill_group* parity (unit-level pinning) ────────────────────

fn parity_events() -> Vec<serde_json::Value> {
    vec![
        envelope("p", "t0", "queue_joined", json!({"queue_id": "q"})),
        envelope("p", "t1", "skill_group_call_joined", json!({"skill_group_id": "q", "reason": "waited", "queue_depth": 1})),
        envelope("p", "t2", "skill_group_candidates_found", json!({"candidates": ["a1"]})),
        envelope("p", "t3", "skill_group_agent_assigned", json!({"agent_id": "a1", "attempt": 1})),
        envelope("p", "t4", "queue_agent_offered", json!({"agent_id": "a1", "leg_id": "l1"})),
        envelope("p", "t5", "queue_agent_no_answer", json!({"agent_id": "a1", "attempt": 1, "leg_id": "l1"})),
        envelope("p", "t6", "skill_group_agent_no_answer", json!({"agent_id": "a1", "attempt": 1, "leg_id": "l1"})),
        envelope("p", "t7", "skill_group_agent_assigned", json!({"agent_id": "a1", "attempt": 2})),
        envelope("p", "t8", "queue_agent_offered", json!({"agent_id": "a1", "leg_id": "l2"})),
        envelope("p", "t9", "queue_agent_connected", json!({"agent_id": "a1", "leg_id": "l2"})),
        envelope("p", "t10", "skill_group_agent_connected", json!({"agent_id": "a1", "attempt": 2, "wait_secs": 8})),
        envelope("p", "t11", "queue_left", json!({"reason": "connected"})),
        envelope("p", "t12", "skill_group_call_left", json!({"reason": "connected", "wait_secs": 8, "skill_groups": ["q"]})),
    ]
}

#[test]
fn parity_passes_on_aligned_families() {
    RwiTimeline::from_events("p", parity_events()).assert_skill_group_parity(); // no panic
}

#[test]
fn parity_fails_when_a_family_event_goes_missing() {
    let mut events = parity_events();
    // Drop the skill_group side of the no-answer round.
    events.retain(|e| e["event_type"].as_str() != Some("skill_group_agent_no_answer"));
    let result = std::panic::catch_unwind(move || {
        RwiTimeline::from_events("p", events).assert_skill_group_parity()
    });
    let err = result.err().expect("parity must FAIL on a missing pair");
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(msg.contains("no-answer parity"), "msg: {msg}");
}

#[test]
fn parity_is_skipped_without_skill_group_events() {
    let events = vec![
        envelope("plain", "t0", "queue_joined", json!({})),
        envelope("plain", "t1", "queue_agent_offered", json!({"agent_id": "a1"})),
        envelope("plain", "t2", "queue_agent_connected", json!({"agent_id": "a1"})),
    ];
    RwiTimeline::from_events("plain", events).assert_skill_group_parity(); // no panic
}

// ── Closed-set assertion ──────────────────────────────────────────────────

#[test]
fn closed_set_rejects_unexpected_event_types() {
    let events = vec![
        envelope("c", "t0", "queue_joined", json!({})),
        envelope("c", "t1", "mystery_event", json!({})),
    ];
    let result = std::panic::catch_unwind(move || {
        RwiTimeline::from_events("c", events).assert_only_expected(&["queue_joined"])
    });
    assert!(result.is_err(), "closed set must FAIL on mystery_event");
}
