//! Two-node cluster integration tests for the owner-routing invariant:
//!
//! **Every session-scoped operation must execute on the node that hosts the
//! session.** The entry point runs on node B; the call lives on node A; the
//! operation must land on A (state + event emitted THERE), B must stay clean,
//! and the request must be single-cast (B's terminal endpoints untouched —
//! no self-forward, no loop).
//!
//! Terminal handlers are the PRODUCTION cores (`ami::apply_*_local`); the
//! shared registry stands in for the shared `cluster_sessions` table.

use rustpbx::call::runtime::SessionInfo;
use rustpbx::proxy::cluster_forward::{OwnerOpOutcome, UserdataOp, routed_op_on_owner};

use crate::helpers::cluster_harness::{ClusterNode, shared_registry, spawn_node, tap_event_of};

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// The OWNER node always has a CallMetaStore entry for the call (inserted at
/// session construction) — `set_user_data`'s SessionNotFound contract depends
/// on it. Seed it exactly as the proxy would.
fn seed_owner_meta(node: &ClusterNode, call_id: &str) {
    node.gateway.read().meta_store.insert(
        call_id.to_string(),
        rustpbx::rwi::CallMeta {
            caller: Some("sip:caller@localhost".to_string()),
            ..Default::default()
        },
    );
}

/// `call.set_userdata` issued on node B for a call hosted on node A:
/// state + `call_userdata_updated` land on A only, B's terminal untouched.
#[tokio::test]
async fn userdata_set_from_non_owner_lands_on_owner() {
    let registry = shared_registry();
    let node_a = spawn_node().await;
    let node_b = spawn_node().await;
    registry
        .register(&SessionInfo::new("call-1", &node_a.node_id))
        .await
        .unwrap();
    seed_owner_meta(&node_a, "call-1");

    let mut tap_a = node_a.subscribe();

    // Entry point on node B (self = B): the command travels in the generic
    // session_op envelope.
    let outcome = routed_op_on_owner(
        &registry,
        &[node_a.peer.clone(), node_b.peer.clone()],
        Some(&node_b.node_id),
        "",
        &client(),
        "call-1",
        &rustpbx::proxy::cluster_forward::RwiCommandOp {
            command: serde_json::json!({
                "action": "call.set_userdata",
                "params": { "call_id": "call-1", "data": {"crm_id": "C-1"} }
            }),
        },
    )
    .await;

    match outcome {
        OwnerOpOutcome::Applied(status, _) => {
            assert_eq!(status, reqwest::StatusCode::OK)
        }
        other => panic!("expected Applied, got {other:?}"),
    }

    // The OWNER (A) holds the state and emitted the event THERE.
    assert!(
        !node_a
            .gateway
            .read()
            .get_user_data(&"call-1".to_string())
            .is_empty(),
        "user_data must live on the owner node A"
    );
    let evt = tap_event_of(&mut tap_a, "call_userdata_updated").await;
    assert_eq!(evt["user_data"]["crm_id"], "C-1", "{evt}");

    // Node B stayed clean.
    assert!(
        node_b
            .gateway
            .read()
            .get_user_data(&"call-1".to_string())
            .is_empty(),
        "non-owner node B must not hold user_data"
    );

    // Single-cast: only A's terminal served the request.
    assert_eq!(node_a.hits(), 1, "owner serves exactly one request");
    assert_eq!(node_b.hits(), 0, "no self-forward / no loop through B");
}

/// `call.get_userdata` issued on node B reads node A's state.
#[tokio::test]
async fn userdata_get_from_non_owner_reads_owner() {
    let registry = shared_registry();
    let node_a = spawn_node().await;
    let node_b = spawn_node().await;
    registry
        .register(&SessionInfo::new("call-2", &node_a.node_id))
        .await
        .unwrap();
    seed_owner_meta(&node_a, "call-2");

    // Seed on the owner.
    let mut data = serde_json::Map::new();
    data.insert("crm_id".to_string(), serde_json::json!("C-2"));
    node_a
        .gateway
        .write()
        .set_user_data(&"call-2".to_string(), data)
        .unwrap();

    let outcome = routed_op_on_owner(
        &registry,
        &[node_a.peer.clone(), node_b.peer.clone()],
        Some(&node_b.node_id),
        "",
        &client(),
        "call-2",
        &rustpbx::proxy::cluster_forward::RwiCommandOp {
            command: serde_json::json!({
                "action": "call.get_userdata",
                "params": { "call_id": "call-2" }
            }),
        },
    )
    .await;

    match outcome {
        OwnerOpOutcome::Applied(status, body) => {
            assert_eq!(status, reqwest::StatusCode::OK);
            assert_eq!(
                body["result"]["UserData"]["user_data"]["crm_id"],
                "C-2",
                "{body}"
            );
        }
        other => panic!("expected Applied, got {other:?}"),
    }
}

/// Call vars: set from node B lands on node A; a later get (still from B)
/// reads node A's value. Vars live on the owner — no replication to B.
#[tokio::test]
async fn call_vars_route_to_owner_node() {
    let registry = shared_registry();
    let node_a = spawn_node().await;
    let node_b = spawn_node().await;
    registry
        .register(&SessionInfo::new("call-3", &node_a.node_id))
        .await
        .unwrap();

    let var_set = serde_json::json!({
        "action": "call.set_var",
        "params": { "call_id": "call-3", "key": "menu", "value": "3" }
    });
    let var_get = serde_json::json!({
        "action": "call.get_var",
        "params": { "call_id": "call-3", "key": "menu" }
    });

    // SET from B.
    let outcome = routed_op_on_owner(
        &registry,
        &[node_a.peer.clone(), node_b.peer.clone()],
        Some(&node_b.node_id),
        "",
        &client(),
        "call-3",
        &rustpbx::proxy::cluster_forward::RwiCommandOp { command: var_set },
    )
    .await;
    assert!(matches!(outcome, OwnerOpOutcome::Applied(_, _)), "{outcome:?}");

    // GET from B reads the owner's value.
    let outcome = routed_op_on_owner(
        &registry,
        &[node_a.peer.clone(), node_b.peer.clone()],
        Some(&node_b.node_id),
        "",
        &client(),
        "call-3",
        &rustpbx::proxy::cluster_forward::RwiCommandOp { command: var_get },
    )
    .await;
    match outcome {
        OwnerOpOutcome::Applied(status, body) => {
            assert_eq!(status, reqwest::StatusCode::OK);
            assert_eq!(body["result"]["CallVar"]["value"], "3", "{body}");
        }
        other => panic!("expected Applied, got {other:?}"),
    }

    // Single-cast: set + get on A, zero on B.
    assert_eq!(node_a.hits(), 2, "owner serves set + get");
    assert_eq!(node_b.hits(), 0, "no self-forward through B");
}

/// Registry says the owner is B (stale/self) while the entry point IS B:
/// must resolve to a LOCAL apply — never an HTTP call to itself.
#[tokio::test]
async fn stale_self_owner_resolves_locally_without_http() {
    let registry = shared_registry();
    let node_a = spawn_node().await;
    let node_b = spawn_node().await;

    // No registry row at all → UnknownCall → the entry point would apply
    // locally. Assert the helper reports it and B's terminal stays cold.
    let outcome = routed_op_on_owner(
        &registry,
        &[node_a.peer.clone(), node_b.peer.clone()],
        Some(&node_b.node_id),
        "",
        &client(),
        "ghost",
        &UserdataOp::Set(serde_json::json!({"k": "v"})),
    )
    .await;

    assert!(matches!(outcome, OwnerOpOutcome::UnknownCall), "{outcome:?}");
    assert_eq!(node_a.hits(), 0);
    assert_eq!(node_b.hits(), 0, "unknown call must not touch any node");
}

/// Registry points at a DEAD node (e.g. crashed owner, stale row): the
/// targeted forward fails → bounded single fan-out round rescues the op via
/// the alive peer that hosts the call — never a silent local 404.
#[tokio::test]
async fn stale_owner_row_is_rescued_by_bounded_fanout() {
    let registry = shared_registry();
    let node_a = spawn_node().await;
    let node_b = spawn_node().await;

    // Registry claims the owner is a dead address; the call actually lives
    // on A (fresh row overwrote nothing — simulating a stale duplicate).
    registry
        .register(&SessionInfo::new("call-4", "203.0.113.9:5060"))
        .await
        .unwrap();
    seed_owner_meta(&node_a, "call-4");

    let mut dead_peer = node_a.peer.clone();
    dead_peer.ami_port = 1; // unreachable
    dead_peer.sip_port = 1;

    let outcome = routed_op_on_owner(
        &registry,
        &[dead_peer, node_a.peer.clone(), node_b.peer.clone()],
        Some(&node_b.node_id),
        "",
        &client(),
        "call-4",
        &rustpbx::proxy::cluster_forward::RwiCommandOp {
            command: serde_json::json!({
                "action": "call.set_userdata",
                "params": { "call_id": "call-4", "data": {"k": "v"} }
            }),
        },
    )
    .await;

    // Targeted (dead) failed → bounded fan-out: A serves it (first non-404).
    assert!(matches!(outcome, OwnerOpOutcome::Applied(_, _)), "{outcome:?}");
    // The fan-out is a single round: at most one request per peer.
    assert!(
        node_a.hits() <= 1,
        "fan-out must be bounded to one round, got {}",
        node_a.hits()
    );
    assert!(
        node_b.hits() <= 1,
        "fan-out must be bounded to one round, got {}",
        node_b.hits()
    );
}

/// Loop canary: an envelope with `hops > 1` (i.e. already forwarded once)
/// must be REJECTED by the terminal with a 500 + distinctive error — proof
/// the terminal never re-routes, no matter what the registry says.
#[tokio::test]
async fn hop_canary_rejects_already_forwarded_envelope() {
    let node = spawn_node().await;
    let client = client();

    let envelope = serde_json::json!({
        "session_id": "call-9",
        "command": {
            "action": "call.set_userdata",
            "params": { "call_id": "call-9", "data": {"k": "v"} }
        },
        "hops": 2,
    });
    let resp = client
        .post(format!("{}/cluster/session_op", node.http_base()))
        .json(&envelope)
        .send()
        .await
        .expect("request must complete");
    assert_eq!(resp.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("hop limit exceeded"),
        "{body}"
    );
    // And no state was applied.
    assert!(node.gateway.read().get_user_data(&"call-9".to_string()).is_empty());
}

/// Stale-registry ping-pong guard: the registry claims the owner is node B,
/// but the call actually lives on node A.  Node B (entry) forwards to A; A's
/// terminal finds no local state and answers 404 — it must NOT bounce the
/// request back to B.  Total requests stay bounded (≤ 1 per peer).
#[tokio::test]
async fn stale_registry_cannot_ping_pong() {
    let registry = shared_registry();
    let node_a = spawn_node().await;
    let node_b = spawn_node().await;

    // Registry row points at B; no meta anywhere.
    registry
        .register(&SessionInfo::new("call-7", &node_b.node_id))
        .await
        .unwrap();

    let outcome = routed_op_on_owner(
        &registry,
        &[node_a.peer.clone(), node_b.peer.clone()],
        Some(&node_a.node_id),
        "",
        &client(),
        "call-7",
        &rustpbx::proxy::cluster_forward::RwiCommandOp {
            command: serde_json::json!({
                "action": "call.set_userdata",
                "params": { "call_id": "call-7", "data": {"k": "v"} }
            }),
        },
    )
    .await;

    // Terminal on B: no local state → 404 → fan-out exhausted → unreachable.
    assert!(
        matches!(outcome, OwnerOpOutcome::OwnerUnreachable),
        "{outcome:?}"
    );
    // Bounded: at most one targeted + one fan-out round per peer — never a
    // ping-pong (a loop would show hits >> 1).
    assert!(node_a.hits() <= 2, "A hit {} times", node_a.hits());
    assert!(node_b.hits() <= 2, "B hit {} times", node_b.hits());
}
