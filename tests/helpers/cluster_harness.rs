// tests/helpers/cluster_harness.rs
//
// Two-node cluster test harness for the "session operations execute on the
// owner node" invariant.
//
// Each node = a REAL `RwiGateway` plus an axum AMI server exposing the
// TERMINAL cluster endpoints (`cluster/{set,get}_userdata`,
// `cluster/{set,get}_var`) bound to that node's own gateway.  Terminal
// handlers are the production cores (`rustpbx::handler::ami::apply_*_local`)
// — they never consult the registry and never forward, so a forwarded request
// cannot loop.
//
// Nodes share one `SessionRegistry`, which is the single source of owner
// location — mirroring the production topology (every node sees the same
// `cluster_sessions` table).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{Json, Router, extract::State, routing::post};
use tokio::net::TcpListener;
use tokio::time::timeout;

use rustpbx::call::runtime::SessionRegistryRef;
use rustpbx::config::ClusterPeer;
use rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry;
use rustpbx::proxy::cluster_forward::SessionOpEnvelope;
use rustpbx::rwi::{
    processor::{execute_session_op_local, CommandError},
    EventCacheEntry, RwiCommandProcessor, RwiGateway, RwiGatewayRef,
};

type NodeState = (RwiGatewayRef, Arc<AtomicUsize>);

/// TERMINAL: the production envelope endpoint bound to THIS node's own
/// processor.  `execute_session_op_local` contains no routing — a forwarded
/// request stops here (loop-safety invariant ①).
async fn terminal_session_op(
    State(state): State<NodeState>,
    Json(body): Json<SessionOpEnvelope>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    state.1.fetch_add(1, Ordering::SeqCst);
    let command: rustpbx::rwi::RwiCommandPayload =
        match serde_json::from_value(body.command.clone()) {
            Ok(c) => c,
            Err(e) => {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("invalid command: {e}") })),
                )
            }
        };
    let processor = RwiCommandProcessor::new(
        Arc::new(ActiveProxyCallRegistry::new()),
        state.0.clone(),
        Arc::new(rustpbx::call::runtime::ConferenceManager::new()),
    );
    match execute_session_op_local(&processor, &body.session_id, body.hops, command).await {
        Ok(result) => {
            let result = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({ "ok": true, "result": result })),
            )
        }
        Err(e) => {
            let status = match &e {
                CommandError::CallNotFound(_) => axum::http::StatusCode::NOT_FOUND,
                CommandError::CommandFailed(msg) if msg.contains("hop limit") => {
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                }
                CommandError::CommandFailed(_) => axum::http::StatusCode::BAD_REQUEST,
            };
            (status, Json(serde_json::json!({ "ok": false, "error": e.to_string() })))
        }
    }
}

/// A spawned cluster node.
pub struct ClusterNode {
    /// Peer descriptor to hand to the OTHER node's routing helper.
    pub peer: ClusterPeer,
    /// This node's registry node id (`"addr:sip_port"`).
    pub node_id: String,
    pub gateway: RwiGatewayRef,
    hits: Arc<AtomicUsize>,
}

impl ClusterNode {
    /// How many terminal requests this node has served.
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// HTTP base of this node's terminal AMI server.
    pub fn http_base(&self) -> String {
        format!("http://{}:{}", self.peer.addr, self.peer.ami_port)
    }

    /// Subscribe to this node's event tap (enriched, same stream as SSE).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EventCacheEntry> {
        self.gateway.read().subscribe_events()
    }
}

/// Spawn one cluster node: gateway + terminal AMI server on an OS port.
pub async fn spawn_node() -> ClusterNode {
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new(RwiGateway::new()));
    let hits = Arc::new(AtomicUsize::new(0));

    let app: Router = Router::new()
        .route(
            "/cluster/session_op",
            post(terminal_session_op).get(terminal_session_op),
        )
        .with_state((gateway.clone(), hits.clone()));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    ClusterNode {
        peer: ClusterPeer {
            addr: "127.0.0.1".to_string(),
            sip_port: port,
            ami_port: port,
        },
        node_id: format!("127.0.0.1:{port}"),
        gateway,
        hits,
    }
}

/// Shared registry both nodes resolve owners against (in-process stand-in for
/// the shared `cluster_sessions` table).
pub fn shared_registry() -> SessionRegistryRef {
    rustpbx::call::runtime::MemorySessionRegistry::new(
        "shared-test-registry",
        Duration::from_secs(60),
        Duration::from_secs(3600),
    )
    .into_ref()
}

/// Drain the tap until an event of `event_type` arrives; return its payload.
pub async fn tap_event_of(
    tap: &mut tokio::sync::broadcast::Receiver<EventCacheEntry>,
    event_type: &str,
) -> serde_json::Value {
    loop {
        let entry = timeout(Duration::from_secs(5), tap.recv())
            .await
            .expect("timeout waiting for tap event")
            .expect("tap closed");
        if entry.event.event_type == event_type {
            return entry.event.payload.clone();
        }
    }
}
