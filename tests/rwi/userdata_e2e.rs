//! Single-node end-to-end assertions for session user data.
//!
//! Pins the single-node contract: the session and the RWI gateway live on the
//! same node, so a `call.set_userdata` for a live call must NEVER fail with
//! "call not found", and once set, every subsequent call-scoped event must
//! carry the `user_data` object (gateway enrichment).
//!
//! Events are observed on the gateway event tap — the same enriched stream
//! the outbound SSE interface consumes. Commands run through the REAL
//! WebSocket handler (auth → processor → gateway).
//!
//! Cluster regressions are covered by `proxy::cluster_forward` unit tests
//! (locate-owner outcomes, loop-safety) — this file is the baseline proof
//! that the set → emit → enrich chain works end to end on one node.

use std::time::Duration;

use tokio::time::timeout;

use crate::helpers::ws_harness::{connect, req, send_recv_matching, start_test_server};

/// Read the next enriched event from the gateway event tap.
async fn next_tap_event(
    tap: &mut tokio::sync::broadcast::Receiver<rustpbx::rwi::EventCacheEntry>,
) -> serde_json::Value {
    loop {
        let entry = timeout(Duration::from_secs(5), tap.recv())
            .await
            .expect("timeout waiting for event tap")
            .expect("event tap closed");
        return entry.event.payload.clone();
    }
}

/// Full chain on one node: set over the real WS → `call_userdata_updated`
/// dispatched → a subsequent call-scoped event enriched with `user_data` →
/// get round-trip.
#[tokio::test]
async fn single_node_set_userdata_then_events_carry_it() {
    let (url, gw, _registry) = start_test_server().await;
    let mut tap = gw.read().subscribe_events();
    let mut ws = connect(&url).await;

    // Simulate the live call the proxy would have created: CallMeta in the
    // store (event context enrichment).
    gw.read().meta_store.insert(
        "call-1".to_string(),
        rustpbx::rwi::CallMeta {
            caller: Some("sip:alice@localhost".to_string()),
            callee: Some("sip:4000@localhost".to_string()),
            ..Default::default()
        },
    );

    // Set user data over the real RWI WebSocket — must succeed: on a single
    // node the session and the gateway share one meta_store.
    let (set_id, set_req) = req(
        "call.set_userdata",
        serde_json::json!({"call_id": "call-1", "data": {"crm_id": "C-1"}}),
    );
    let v = send_recv_matching(&mut ws, &set_req, &set_id).await;
    assert_eq!(v["status"], "success", "set_userdata must succeed on a live call: {v}");

    // The set announces itself with the full new value.
    let updated = next_tap_event(&mut tap).await;
    assert_eq!(updated["event_type"], "call_userdata_updated", "{updated}");
    assert_eq!(updated["user_data"]["crm_id"], "C-1", "{updated}");

    // A subsequent call-scoped event (as the media stack would emit) must
    // carry the user data via gateway enrichment.
    gw.read().send_to_owner(&rustpbx::rwi::Dtmf {
        call_id: "call-1".into(),
        digit: "5".into(),
        leg_id: None,
        extra: None,
    });
    let dtmf = next_tap_event(&mut tap).await;
    assert_eq!(dtmf["event_type"], "dtmf", "{dtmf}");
    assert_eq!(dtmf["user_data"]["crm_id"], "C-1", "dtmf must carry user_data: {dtmf}");
    // Context enrichment from the CallMeta rides along too.
    assert_eq!(dtmf["caller"], "sip:alice@localhost", "{dtmf}");

    // Round-trip: read it back over the WS.
    let (get_id, get_req) = req("call.get_userdata", serde_json::json!({"call_id": "call-1"}));
    let v = send_recv_matching(&mut ws, &get_req, &get_id).await;
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(v["data"]["user_data"]["crm_id"], "C-1", "{v}");

    ws.close(None).await.unwrap();
}

/// A set for a call that does not exist must fail explicitly (not silently).
#[tokio::test]
async fn single_node_set_userdata_unknown_call_fails() {
    let (url, _gw, _registry) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (id, json) = req(
        "call.set_userdata",
        serde_json::json!({"call_id": "ghost", "data": {"k": "v"}}),
    );
    let v = send_recv_matching(&mut ws, &json, &id).await;
    assert_eq!(v["status"], "error", "{v}");
    assert_eq!(v["type"], "command_failed", "{v}");
    assert!(
        v["error"].as_str().unwrap_or("").contains("ghost"),
        "error must mention the call id: {v}"
    );

    ws.close(None).await.unwrap();
}

/// After the call finishes (`call_finished`), the user data is gone — the
/// cleanup contract that bounds user_data to the call's lifetime.
#[tokio::test]
async fn single_node_call_finished_clears_userdata() {
    let (url, gw, _registry) = start_test_server().await;
    let mut ws = connect(&url).await;

    gw.read().meta_store.insert(
        "call-2".to_string(),
        rustpbx::rwi::CallMeta {
            caller: Some("sip:bob@localhost".to_string()),
            ..Default::default()
        },
    );

    let (set_id, set_req) = req(
        "call.set_userdata",
        serde_json::json!({"call_id": "call-2", "data": {"crm_id": "C-2"}}),
    );
    let v = send_recv_matching(&mut ws, &set_req, &set_id).await;
    assert_eq!(v["status"], "success", "{v}");

    // The call record completed (guard drop) → cleanup.
    gw.write().call_finished(&"call-2".to_string());

    let (get_id, get_req) = req("call.get_userdata", serde_json::json!({"call_id": "call-2"}));
    let v = send_recv_matching(&mut ws, &get_req, &get_id).await;
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(
        v["data"]["user_data"],
        serde_json::json!({}),
        "user_data must be empty after call_finished: {v}"
    );

    ws.close(None).await.unwrap();
}

/// Single-node call vars over the real WS: set → get round-trip, then the
/// `call_finished` cleanup contract (vars are bounded to the call lifetime,
/// same as user_data).
#[tokio::test]
async fn single_node_call_vars_roundtrip_and_cleanup() {
    let (url, gw, _registry) = start_test_server().await;
    let mut ws = connect(&url).await;

    gw.read().meta_store.insert(
        "call-5".to_string(),
        rustpbx::rwi::CallMeta {
            caller: Some("sip:carol@localhost".to_string()),
            ..Default::default()
        },
    );

    let (set_id, set_req) = req(
        "call.set_var",
        serde_json::json!({"call_id": "call-5", "key": "menu", "value": "3"}),
    );
    let v = send_recv_matching(&mut ws, &set_req, &set_id).await;
    assert_eq!(v["status"], "success", "{v}");

    let (get_id, get_req) = req(
        "call.get_var",
        serde_json::json!({"call_id": "call-5", "key": "menu"}),
    );
    let v = send_recv_matching(&mut ws, &get_req, &get_id).await;
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(v["data"]["key"], "menu", "{v}");
    assert_eq!(v["data"]["value"], "3", "{v}");

    // The call record completed (guard drop) → cleanup.
    gw.write().call_finished(&"call-5".to_string());

    let (get_id2, get_req2) = req(
        "call.get_var",
        serde_json::json!({"call_id": "call-5", "key": "menu"}),
    );
    let v = send_recv_matching(&mut ws, &get_req2, &get_id2).await;
    assert_eq!(v["status"], "success", "{v}");
    assert!(
        v["data"]["value"].is_null(),
        "var must be gone after call_finished: {v}"
    );

    ws.close(None).await.unwrap();
}
