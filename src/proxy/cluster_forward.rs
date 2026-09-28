//! Cluster command forwarding with session-registry routing (commerce).
//!
//! When a console/cc control request arrives on a node that does not host the
//! target session, the request must reach the owning node. The
//! [`SessionRegistry`](crate::call::runtime::SessionRegistry) answers
//! "which node owns call X"; this module turns that answer into a targeted
//! HTTP forward to the owner's AMI cluster endpoint, with a fan-out to all
//! peers as the fallback (e.g. when the registry record is missing because
//! the owning node crashed and the call was re-homed).

use crate::call::runtime::SessionRegistryRef;
use crate::config::ClusterPeer;

/// Timeout for a single peer HTTP forward.
const CLUSTER_FORWARD_TIMEOUT_SECS: u64 = 5;

/// The AMI cluster base path for a peer (scheme://host:port/path-prefix).
pub(crate) fn peer_ami_base(peer: &ClusterPeer, ami_path: &str) -> String {
    format!("http://{}:{}{}", peer.addr, peer.ami_port, ami_path)
}

/// Find the peer matching a registry `node_id` (`"addr:sip_port"`).
fn peer_for_node_id<'a>(peers: &'a [ClusterPeer], node_id: &str) -> Option<&'a ClusterPeer> {
    peers
        .iter()
        .find(|p| node_id == format!("{}:{}", p.addr, p.sip_port) || node_id == p.addr)
}

/// Issue a JSON request to one peer's AMI cluster endpoint.
pub(crate) async fn forward_json(
    client: &reqwest::Client,
    url: &str,
    method: reqwest::Method,
    body: Option<&serde_json::Value>,
) -> Option<(reqwest::StatusCode, serde_json::Value)> {
    let opts = crate::http_util::HttpFetchOptions::new()
        .with_timeout(std::time::Duration::from_secs(CLUSTER_FORWARD_TIMEOUT_SECS));
    let req = match method {
        reqwest::Method::GET => client.get(url),
        _ => {
            let mut r = client.request(method.clone(), url);
            if let Some(b) = body {
                r = r.json(b);
            }
            r
        }
    };
    match crate::http_util::execute_request(req, &opts.headers, opts.timeout).await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.json::<serde_json::Value>().await.ok()?;
            Some((status, body))
        }
        Err(_) => None,
    }
}

/// Dispatch a call command cluster-wide.
///
/// Strategy: session-registry `lookup_owner` first — one targeted request to
/// the owning node; if that fails (unknown owner, unreachable node, or the
/// owner no longer has the call) fall back to fanning out to every peer and
/// return the first response that is not a 404.
///
/// `payload` is the wire form of the console `CallCommandPayload` consumed by
/// the remote node's `/cluster/dispatch_command` endpoint.
///
/// `session_id` may be a proxy session id or a dialog Call-ID alias registered
/// via [`SessionInfo::dialog_alias`].
pub async fn dispatch_call_command(
    registry: &SessionRegistryRef,
    peers: &[ClusterPeer],
    ami_path: &str,
    client: &reqwest::Client,
    session_id: &str,
    payload: &serde_json::Value,
) -> Option<(reqwest::StatusCode, serde_json::Value)> {
    if peers.is_empty() {
        return None;
    }

    // Prefer canonical session id when `session_id` is a dialog alias.
    let forward_id = crate::call::runtime::resolve_owner_and_session(registry, session_id)
        .await
        .map(|(_, sid)| sid)
        .unwrap_or_else(|| session_id.to_string());

    let body = serde_json::json!({
        "session_id": forward_id,
        "payload": payload,
    });

    // 1. Targeted forward to the owning node.
    if let Some(owner) = registry.lookup_owner(session_id).await {
        if let Some(peer) = peer_for_node_id(peers, &owner) {
            let url = format!("{}/cluster/dispatch_command", peer_ami_base(peer, ami_path));
            if let Some(resp) = forward_json(client, &url, reqwest::Method::POST, Some(&body)).await
            {
                if resp.0 != reqwest::StatusCode::NOT_FOUND {
                    return Some(resp);
                }
            }
        }
    }

    // 2. Fan-out fallback: first non-404 response wins.
    let mut handles = Vec::new();
    for peer in peers {
        let url = format!("{}/cluster/dispatch_command", peer_ami_base(peer, ami_path));
        let client = client.clone();
        let body = body.clone();
        handles.push(tokio::spawn(async move {
            forward_json(&client, &url, reqwest::Method::POST, Some(&body)).await
        }));
    }
    for handle in handles {
        if let Ok(Some(resp)) = handle.await {
            if resp.0 != reqwest::StatusCode::NOT_FOUND {
                return Some(resp);
            }
        }
    }
    None
}

/// Forward arbitrary JSON to the owning node's AMI relative path.
///
/// Core has no knowledge of addon endpoints: callers (e.g. the CC addon)
/// supply `ami_relative_path` such as `"cluster/cc_owner_op"` and the full
/// request body. Owner is resolved via session registry (dialog alias OK);
/// if the targeted peer returns 404, fans out to remaining peers.
pub async fn dispatch_to_owner(
    registry: &SessionRegistryRef,
    peers: &[ClusterPeer],
    ami_path: &str,
    client: &reqwest::Client,
    session_or_dialog_id: &str,
    ami_relative_path: &str,
    body: &serde_json::Value,
) -> Option<(reqwest::StatusCode, serde_json::Value)> {
    if peers.is_empty() {
        return None;
    }

    let owner = crate::call::runtime::resolve_owner_and_session(registry, session_or_dialog_id)
        .await
        .map(|(o, _)| o)
        .or(registry.lookup_owner(session_or_dialog_id).await)?;

    let rel = ami_relative_path.trim_start_matches('/');

    if let Some(peer) = peer_for_node_id(peers, &owner) {
        let url = format!("{}/{}", peer_ami_base(peer, ami_path), rel);
        if let Some(resp) = forward_json(client, &url, reqwest::Method::POST, Some(body)).await {
            if resp.0 != reqwest::StatusCode::NOT_FOUND {
                return Some(resp);
            }
        }
    }

    // Fan-out fallback
    let mut handles = Vec::new();
    for peer in peers {
        let url = format!("{}/{}", peer_ami_base(peer, ami_path), rel);
        let client = client.clone();
        let body = body.clone();
        handles.push(tokio::spawn(async move {
            forward_json(&client, &url, reqwest::Method::POST, Some(&body)).await
        }));
    }
    for handle in handles {
        if let Ok(Some(resp)) = handle.await {
            if resp.0 != reqwest::StatusCode::NOT_FOUND {
                return Some(resp);
            }
        }
    }
    None
}

/// Forward raw in-dialog SIP (BYE/INFO/…) to the dialog owner when this node
/// has no matching dialog. Body is the serialized SIP request bytes / text.
pub async fn dispatch_indialog_sip(
    registry: &SessionRegistryRef,
    peers: &[ClusterPeer],
    ami_path: &str,
    client: &reqwest::Client,
    dialog_call_id: &str,
    sip_message: &str,
) -> Option<(reqwest::StatusCode, serde_json::Value)> {
    if peers.is_empty() {
        return None;
    }
    let owner = registry.lookup_owner(dialog_call_id).await?;
    let peer = peer_for_node_id(peers, &owner)?;
    let url = format!("{}/cluster/forward_sip", peer_ami_base(peer, ami_path));
    let body = serde_json::json!({
        "dialog_call_id": dialog_call_id,
        "message": sip_message,
    });
    forward_json(client, &url, reqwest::Method::POST, Some(&body)).await
}

/// Fetch a session snapshot cluster-wide (console "show call"). Same
/// owner-first strategy as [`dispatch_call_command`].
pub async fn query_session(
    registry: &SessionRegistryRef,
    peers: &[ClusterPeer],
    ami_path: &str,
    client: &reqwest::Client,
    session_id: &str,
) -> Option<(reqwest::StatusCode, serde_json::Value)> {
    if peers.is_empty() {
        return None;
    }

    if let Some(owner) = registry.lookup_owner(session_id).await {
        if let Some(peer) = peer_for_node_id(peers, &owner) {
            let url = format!(
                "{}/cluster/show_session/{}",
                peer_ami_base(peer, ami_path),
                session_id
            );
            if let Some(resp) = forward_json(client, &url, reqwest::Method::GET, None).await {
                if resp.0 != reqwest::StatusCode::NOT_FOUND {
                    return Some(resp);
                }
            }
        }
    }

    let mut handles = Vec::new();
    for peer in peers {
        let url = format!(
            "{}/cluster/show_session/{}",
            peer_ami_base(peer, ami_path),
            session_id
        );
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            forward_json(&client, &url, reqwest::Method::GET, None).await
        }));
    }
    for handle in handles {
        if let Ok(Some(resp)) = handle.await {
            if resp.0 != reqwest::StatusCode::NOT_FOUND {
                return Some(resp);
            }
        }
    }
    None
}

/// Result of asking the session registry where a call lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerLocation {
    /// The registry has no record of the call — it does not exist anywhere.
    Unknown,
    /// The registry backend could not be queried (DB outage).  Callers must
    /// NOT degrade this into "call not found": the call may be alive on a
    /// peer whose record cannot be read right now.
    Unavailable(String),
    /// `node_id` of the node hosting the call.
    Found(String),
}

/// Ask the registry which node hosts `call_id`.
pub async fn locate_owner(registry: &SessionRegistryRef, call_id: &str) -> OwnerLocation {
    match registry.lookup_owner_checked(call_id).await {
        Ok(Some(node)) => OwnerLocation::Found(node),
        Ok(None) => OwnerLocation::Unknown,
        Err(e) => OwnerLocation::Unavailable(e.to_string()),
    }
}

/// Outcome of an operation routed to the session's owner node.
#[derive(Debug)]
pub enum OwnerOpOutcome {
    /// The registry says THIS node hosts the session — apply locally (the
    /// local apply is the loop terminator; if the record is stale the local
    /// apply simply answers 404).
    ApplyLocally,
    /// The owner node answered — pass the (status, body) through.
    Applied(reqwest::StatusCode, serde_json::Value),
    /// The registry has no record of the call: it does not exist anywhere.
    UnknownCall,
    /// The registry backend failed: the owner's location is unknowable right
    /// now.  Retryable — the call may well be alive on a peer.
    RegistryUnavailable(String),
    /// The owner was resolved but could not be reached (or answered 404).
    OwnerUnreachable,
}

// ── Owner-routed operation contract ────────────────────────────────────────
//
// CONTRACT: every operation that touches session-scoped state (user_data,
// call vars, per-session commands, …) MUST execute on the node that hosts the
// session.  Entry points either apply locally (session hosted here), or
// locate the owner via the session registry and single-cast to that node's
// TERMINAL `cluster/*` endpoint — which applies locally and never
// re-resolves (invariant: a forwarded request cannot loop).  A locate
// failure must surface as a retryable error (503), never as a misleading
// local "call not found".
//
// Routed entry points: userdata set/get, call-var set/get, console call
// commands.  Known NOT routed (by design, fail loud on the wrong node):
// attach, list_calls, session/call resume (node-local event cache).

/// An operation that can be routed to the node hosting its session.
pub trait OwnerRoutedOp {
    /// Terminal AMI-cluster relative path on the owner node.
    fn relative_path(&self) -> &'static str;
    /// Wire body for the terminal endpoint.
    fn body(&self, session_id: &str) -> serde_json::Value;
}

/// The userdata operation to execute on the owner node.
#[derive(Debug, Clone)]
pub enum UserdataOp {
    Set(serde_json::Value),
    Get,
}

impl OwnerRoutedOp for UserdataOp {
    fn relative_path(&self) -> &'static str {
        match self {
            UserdataOp::Set(_) => "cluster/set_userdata",
            UserdataOp::Get => "cluster/get_userdata",
        }
    }

    fn body(&self, session_id: &str) -> serde_json::Value {
        match self {
            UserdataOp::Set(data) => serde_json::json!({ "session_id": session_id, "data": data }),
            UserdataOp::Get => serde_json::json!({ "session_id": session_id }),
        }
    }
}

/// An RWI command routed to the node hosting its session — THE generic
/// envelope for the command plane.  The wire body matches the terminal
/// `/cluster/session_op` endpoint (`ami.rs`): `{session_id, command, hops}`;
/// `hops: 1` is stamped by the router and is a forwarding-loop canary (the
/// terminal rejects anything above 1).
#[derive(Debug, Clone)]
pub struct RwiCommandOp {
    /// The command, serialized in the `RwiCommandPayload` wire format
    /// (`{"action": …, "params": …}`).
    pub command: serde_json::Value,
}

/// Wire envelope of `/cluster/session_op` (terminal side).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionOpEnvelope {
    pub session_id: String,
    /// `RwiCommandPayload` JSON (`{"action": …, "params": …}`).
    pub command: serde_json::Value,
    /// Forwarding-loop canary: stamped to 1 by the router; the terminal
    /// rejects anything above 1 with a 500.
    #[serde(default)]
    pub hops: u8,
}

impl OwnerRoutedOp for RwiCommandOp {
    fn relative_path(&self) -> &'static str {
        "cluster/session_op"
    }

    fn body(&self, session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": session_id,
            "command": self.command,
            "hops": 1,
        })
    }
}

/// A console call command to execute on the owner node (wire shape consumed
/// by the owner's terminal `/cluster/dispatch_command`).
#[derive(Debug, Clone)]
pub struct ConsoleCommandOp {
    pub payload: serde_json::Value,
}

impl OwnerRoutedOp for ConsoleCommandOp {
    fn relative_path(&self) -> &'static str {
        "cluster/dispatch_command"
    }

    fn body(&self, session_id: &str) -> serde_json::Value {
        serde_json::json!({ "session_id": session_id, "payload": self.payload })
    }
}

/// Execute a session-userdata operation on the node that hosts `session_id`.
///
/// Loop-safety invariants (keep all four):
/// 1. **Single hop** — only entry points (REST handler, RWI command
///    processor) call this.  The internal `cluster/{set,get}_userdata`
///    endpoints apply locally and never re-resolve, so a forwarded request
///    always terminates.
/// 2. **No self-forward** — when the registry resolves the owner to this
///    node (`self_node_id`), returns [`OwnerOpOutcome::ApplyLocally`] instead
///    of HTTP-calling ourselves.  The local apply terminates: success, or a
///    plain 404 when the registry record was stale.
/// 3. **Replication never loops** — the owner's `user_data_sync` hook
///    broadcasts to peers, and peers apply via `apply_remote_user_data`
///    which neither re-broadcasts nor emits events.
/// 4. **Bounded fallback** — the fan-out below is a fixed single round over
///    the configured peer list, never recursive.
pub async fn userdata_op_on_owner(
    registry: &SessionRegistryRef,
    peers: &[ClusterPeer],
    self_node_id: Option<&str>,
    ami_path: &str,
    client: &reqwest::Client,
    session_id: &str,
    op: &UserdataOp,
) -> OwnerOpOutcome {
    routed_op_on_owner(
        registry,
        peers,
        self_node_id,
        ami_path,
        client,
        session_id,
        op,
    )
    .await
}

/// Locate the session's owner and execute `op` there (see the contract above).
pub async fn routed_op_on_owner(
    registry: &SessionRegistryRef,
    peers: &[ClusterPeer],
    self_node_id: Option<&str>,
    ami_path: &str,
    client: &reqwest::Client,
    session_id: &str,
    op: &impl OwnerRoutedOp,
) -> OwnerOpOutcome {
    if peers.is_empty() {
        // No peers → nothing to locate; the caller applies locally.
        return OwnerOpOutcome::UnknownCall;
    }

    match locate_owner(registry, session_id).await {
        OwnerLocation::Unknown => OwnerOpOutcome::UnknownCall,
        OwnerLocation::Unavailable(e) => OwnerOpOutcome::RegistryUnavailable(e),
        OwnerLocation::Found(owner) => {
            // Invariant 2: never HTTP ourselves — apply locally instead.
            if self_node_id.is_some_and(|self_id| self_id == owner) {
                return OwnerOpOutcome::ApplyLocally;
            }

            // 1. Targeted single-cast to the owning node.
            if let Some(peer) = peer_for_node_id(peers, &owner) {
                let url = format!(
                    "{}/{}",
                    peer_ami_base(peer, ami_path),
                    op.relative_path()
                );
                if let Some((status, body)) =
                    forward_json(client, &url, reqwest::Method::POST, Some(&op.body(session_id)))
                        .await
                {
                    if status != reqwest::StatusCode::NOT_FOUND {
                        return OwnerOpOutcome::Applied(status, body);
                    }
                }
            }

            // 2. Fan-out fallback (registry stale / node_id mismatch): first
            //    non-404 response wins.  Bounded: one round, no recursion.
            let mut handles = Vec::new();
            for peer in peers {
                let url = format!(
                    "{}/{}",
                    peer_ami_base(peer, ami_path),
                    op.relative_path()
                );
                let client = client.clone();
                let body = op.body(session_id);
                handles.push(tokio::spawn(async move {
                    forward_json(&client, &url, reqwest::Method::POST, Some(&body)).await
                }));
            }
            for handle in handles {
                if let Ok(Some((status, body))) = handle.await
                    && status != reqwest::StatusCode::NOT_FOUND
                {
                    return OwnerOpOutcome::Applied(status, body);
                }
            }
            OwnerOpOutcome::OwnerUnreachable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(addr: &str, sip_port: u16, ami_port: u16) -> ClusterPeer {
        ClusterPeer {
            addr: addr.to_string(),
            sip_port,
            ami_port,
        }
    }

    // ── userdata_op_on_owner: locate → single-cast → loop safety ───────────

    /// Registry double that can simulate an outage.
    struct MockRegistry {
        plan: MockPlan,
    }

    #[derive(Clone)]
    enum MockPlan {
        /// Registry row pointing at this node id.
        Row(String),
        /// No record — the call does not exist per the registry.
        NoRow,
        /// Backend outage.
        Down,
    }

    impl MockRegistry {
        fn new(plan: MockPlan) -> SessionRegistryRef {
            std::sync::Arc::new(Self { plan })
        }
    }

    #[async_trait::async_trait]
    impl crate::call::runtime::SessionRegistry for MockRegistry {
        async fn register(
            &self,
            _info: &crate::call::runtime::SessionInfo,
        ) -> Result<(), crate::call::runtime::RegistryError> {
            Ok(())
        }
        async fn unregister(&self, _call_id: &str) -> Result<(), crate::call::runtime::RegistryError> {
            Ok(())
        }
        async fn heartbeat_node(
            &self,
            _node_id: &str,
            _live_call_ids: &[String],
        ) -> Result<(), crate::call::runtime::RegistryError> {
            Ok(())
        }
        async fn lookup_owner(&self, call_id: &str) -> Option<String> {
            self.lookup_owner_checked(call_id).await.ok().flatten()
        }
        async fn lookup_owner_checked(
            &self,
            _call_id: &str,
        ) -> Result<Option<String>, crate::call::runtime::RegistryError> {
            match &self.plan {
                MockPlan::Row(node) => Ok(Some(node.clone())),
                MockPlan::NoRow => Ok(None),
                MockPlan::Down => Err(crate::call::runtime::RegistryError::Unavailable(
                    "db down".to_string(),
                )),
            }
        }
        async fn lookup(
            &self,
            _call_id: &str,
        ) -> Option<crate::call::runtime::SessionInfo> {
            None
        }
    }

    /// Spin a peer AMI server that captures `cluster/{set,get}_userdata`
    /// requests and answers `status`. Returns (peer, rx counter, hit count).
    async fn spawn_peer(
        status: axum::http::StatusCode,
    ) -> (
        ClusterPeer,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use axum::{Json, Router, extract::State, routing::post};

        type Shared = (
            tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
            axum::http::StatusCode,
        );

        async fn capture(
            State(state): State<Shared>,
            Json(body): Json<serde_json::Value>,
        ) -> (axum::http::StatusCode, Json<serde_json::Value>) {
            let (tx, hits, status) = state;
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = tx.send(body);
            (
                status,
                Json(serde_json::json!({"message": "User data updated", "data": {}})),
            )
        }

        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        let app: Router = Router::new()
            .route("/cluster/set_userdata", post(capture))
            .route("/cluster/get_userdata", post(capture))
            .route("/cluster/set_var", post(capture))
            .route("/cluster/get_var", post(capture))
            .route("/cluster/dispatch_command", post(capture))
            .route("/cluster/session_op", post(capture))
            .with_state((tx, hits.clone(), status));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (peer("127.0.0.1", port, port), rx, hits)
    }

    #[test]
    fn peer_for_node_id_matches_addr_port_form() {
        let peers = vec![peer("10.0.0.2", 5060, 8081), peer("10.0.0.3", 5060, 8082)];
        assert_eq!(
            peer_for_node_id(&peers, "10.0.0.3:5060").map(|p| p.ami_port),
            Some(8082)
        );
        assert_eq!(
            peer_for_node_id(&peers, "10.0.0.2:5060").map(|p| p.addr.clone()),
            Some("10.0.0.2".to_string())
        );
        assert!(peer_for_node_id(&peers, "10.9.9.9:5060").is_none());
    }

    /// `dispatch_to_owner` must POST the body to the peer owning the session
    /// (resolved via the session registry) — this is the path the console's
    /// user-data forwarding uses when the REST request lands on a non-owner
    /// node.
    #[tokio::test]
    async fn dispatch_to_owner_posts_to_owning_peer() {
        use crate::call::runtime::{MemorySessionRegistry, SessionInfo};
        use axum::{Json, Router, extract::State, routing::post};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        async fn capture(
            State(tx): State<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            let _ = tx.send(body);
            // Must return a JSON body: `forward_json` parses the response.
            Json(serde_json::json!({"ok": true}))
        }
        let app: Router = Router::new()
            .route("/cluster/set_userdata", post(capture))
            .with_state(tx);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let registry: crate::call::runtime::SessionRegistryRef = MemorySessionRegistry::new(
            "127.0.0.1:5060",
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(3600),
        );
        registry
            .register(&SessionInfo::new("sess-1", "127.0.0.1:5060"))
            .await
            .unwrap();

        let peers = vec![peer("127.0.0.1", 5060, port)];
        let body = serde_json::json!({"session_id":"sess-1","data":{"crm_id":"C-1"}});
        let resp = dispatch_to_owner(
            &registry,
            &peers,
            "",
            &reqwest::Client::new(),
            "sess-1",
            "cluster/set_userdata",
            &body,
        )
        .await;
        assert_eq!(resp.as_ref().map(|(s, _)| s.as_u16()), Some(200));
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("dispatch must reach the owning peer")
            .expect("body captured");
        assert_eq!(got["session_id"], "sess-1");
        assert_eq!(got["data"]["crm_id"], "C-1");
    }

    fn count(hits: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> usize {
        hits.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Owner resolved → EXACTLY ONE targeted POST to the owner peer; the
    /// bystander peer is never touched.
    #[tokio::test]
    async fn userdata_op_single_casts_to_owner_only() {
        let (owner_peer, mut owner_rx, owner_hits) =
            spawn_peer(axum::http::StatusCode::OK).await;
        let (bystander_peer, _rx, bystander_hits) =
            spawn_peer(axum::http::StatusCode::OK).await;

        let node_id = format!("{}:{}", owner_peer.addr, owner_peer.sip_port);
        let registry = MockRegistry::new(MockPlan::Row(node_id.clone()));
        let peers = vec![owner_peer.clone(), bystander_peer];

        let outcome = userdata_op_on_owner(
            &registry,
            &peers,
            Some("this-node:5060"),
            "",
            &reqwest::Client::new(),
            "call-9",
            &UserdataOp::Set(serde_json::json!({"crm_id": "C-9"})),
        )
        .await;

        match outcome {
            OwnerOpOutcome::Applied(status, _) => {
                assert_eq!(status, reqwest::StatusCode::OK)
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert_eq!(count(&owner_hits), 1, "owner must receive exactly one request");
        assert_eq!(count(&bystander_hits), 0, "bystander must never be contacted");
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            owner_rx.recv(),
        )
        .await
        .expect("owner must capture the body")
        .expect("body");
        assert_eq!(body["session_id"], "call-9");
        assert_eq!(body["data"]["crm_id"], "C-9");
    }

    /// Invariant 2: registry resolves the owner to THIS node → ApplyLocally,
    /// zero HTTP (a self-HTTP would risk a loop).
    #[tokio::test]
    async fn userdata_op_self_owner_never_self_forwards() {
        let (peer, _rx, hits) = spawn_peer(axum::http::StatusCode::OK).await;
        let self_node_id = format!("{}:{}", peer.addr, peer.sip_port);
        let registry = MockRegistry::new(MockPlan::Row(self_node_id.clone()));

        let outcome = userdata_op_on_owner(
            &registry,
            &[peer.clone()],
            Some(&self_node_id),
            "",
            &reqwest::Client::new(),
            "call-9",
            &UserdataOp::Get,
        )
        .await;

        assert!(matches!(outcome, OwnerOpOutcome::ApplyLocally), "{outcome:?}");
        assert_eq!(count(&hits), 0, "must not HTTP to itself");
    }

    /// Registry has no record → UnknownCall, zero HTTP (the caller answers a
    /// plain 404; the call does not exist anywhere).
    #[tokio::test]
    async fn userdata_op_unknown_call_short_circuits() {
        let (peer, _rx, hits) = spawn_peer(axum::http::StatusCode::OK).await;
        let registry = MockRegistry::new(MockPlan::NoRow);

        let outcome = userdata_op_on_owner(
            &registry,
            &[peer],
            Some("this-node:5060"),
            "",
            &reqwest::Client::new(),
            "gone",
            &UserdataOp::Set(serde_json::json!({})),
        )
        .await;

        assert!(matches!(outcome, OwnerOpOutcome::UnknownCall), "{outcome:?}");
        assert_eq!(count(&hits), 0);
    }

    /// Registry outage → RegistryUnavailable (retryable 503 upstream), never
    /// a silent "not found": the call may be alive on a peer.
    #[tokio::test]
    async fn userdata_op_registry_outage_is_reported() {
        let (peer, _rx, hits) = spawn_peer(axum::http::StatusCode::OK).await;
        let registry = MockRegistry::new(MockPlan::Down);

        let outcome = userdata_op_on_owner(
            &registry,
            &[peer],
            Some("this-node:5060"),
            "",
            &reqwest::Client::new(),
            "call-9",
            &UserdataOp::Get,
        )
        .await;

        assert!(
            matches!(outcome, OwnerOpOutcome::RegistryUnavailable(ref e) if e.contains("db down")),
            "{outcome:?}"
        );
        assert_eq!(count(&hits), 0);
    }

    /// Owner node id unknown to the peer list → bounded single fan-out round;
    /// the first non-404 answer wins.
    #[tokio::test]
    async fn userdata_op_fanout_fallback_when_owner_peer_unknown() {
        let (peer_a, mut rx_a, hits_a) = spawn_peer(axum::http::StatusCode::OK).await;
        let (peer_b, _rx_b, hits_b) = spawn_peer(axum::http::StatusCode::OK).await;
        let registry = MockRegistry::new(MockPlan::Row("203.0.113.9:5060".to_string()));

        let outcome = userdata_op_on_owner(
            &registry,
            &[peer_a, peer_b],
            Some("this-node:5060"),
            "",
            &reqwest::Client::new(),
            "call-9",
            &UserdataOp::Set(serde_json::json!({"k": "v"})),
        )
        .await;

        assert!(matches!(outcome, OwnerOpOutcome::Applied(_, _)), "{outcome:?}");
        // Bounded: at most ONE request per peer (a single fan-out round,
        // never a retry loop). Under full-suite concurrency a peer connect
        // can transiently fail, so assert the per-peer cap, not the total.
        assert!(
            count(&hits_a) <= 1,
            "fan-out must be bounded to one round, got {} for peer A",
            count(&hits_a)
        );
        assert!(
            count(&hits_b) <= 1,
            "fan-out must be bounded to one round, got {} for peer B",
            count(&hits_b)
        );
        let body = tokio::time::timeout(std::time::Duration::from_secs(2), rx_a.recv())
            .await
            .expect("one peer must have been hit")
            .expect("body");
        assert_eq!(body["session_id"], "call-9");
    }

    /// Every peer answering 404 → OwnerUnreachable, and the fan-out is
    /// bounded (exactly one request per peer — never a loop).
    #[tokio::test]
    async fn userdata_op_all_peers_404_is_bounded() {
        let (peer_a, _rx_a, hits_a) = spawn_peer(axum::http::StatusCode::NOT_FOUND).await;
        let (peer_b, _rx_b, hits_b) = spawn_peer(axum::http::StatusCode::NOT_FOUND).await;
        let registry = MockRegistry::new(MockPlan::Row("203.0.113.9:5060".to_string()));

        let outcome = userdata_op_on_owner(
            &registry,
            &[peer_a, peer_b],
            Some("this-node:5060"),
            "",
            &reqwest::Client::new(),
            "call-9",
            &UserdataOp::Get,
        )
        .await;

        assert!(matches!(outcome, OwnerOpOutcome::OwnerUnreachable), "{outcome:?}");
        // Bounded: at most one request per peer — never a loop.
        assert!(count(&hits_a) <= 1, "peer A hit {} times", count(&hits_a));
        assert!(count(&hits_b) <= 1, "peer B hit {} times", count(&hits_b));
    }

    /// `locate_owner` maps registry outcomes 1:1.
    #[tokio::test]
    async fn locate_owner_maps_registry_outcomes() {
        assert_eq!(
            locate_owner(&MockRegistry::new(MockPlan::Row("n1:5060".to_string())), "c").await,
            OwnerLocation::Found("n1:5060".to_string())
        );
        assert_eq!(
            locate_owner(&MockRegistry::new(MockPlan::NoRow), "c").await,
            OwnerLocation::Unknown
        );
        assert!(
            matches!(
                locate_owner(&MockRegistry::new(MockPlan::Down), "c").await,
                OwnerLocation::Unavailable(_)
            ),
            "outage must surface as Unavailable"
        );
    }

    // ── The same matrix for CallVarOp / ConsoleCommandOp — the generalized
    //    primitive must hold the invariants for EVERY routed op. ────────────

    /// call vars (via the generic command envelope): owner resolved →
    /// single-cast with the `{session_id, command, hops}` wire shape, and the
    /// router stamps the hop canary.
    #[tokio::test]
    async fn session_op_single_casts_to_owner_only() {
        let (owner_peer, mut owner_rx, owner_hits) =
            spawn_peer(axum::http::StatusCode::OK).await;

        let node_id = format!("{}:{}", owner_peer.addr, owner_peer.sip_port);
        let registry = MockRegistry::new(MockPlan::Row(node_id.clone()));

        let outcome = routed_op_on_owner(
            &registry,
            &[owner_peer.clone()],
            Some("this-node:5060"),
            "",
            &reqwest::Client::new(),
            "call-9",
            &RwiCommandOp {
                command: serde_json::json!({
                    "action": "call.set_var",
                    "params": { "call_id": "call-9", "key": "menu", "value": "3" }
                }),
            },
        )
        .await;

        assert!(matches!(outcome, OwnerOpOutcome::Applied(_, _)), "{outcome:?}");
        assert_eq!(count(&owner_hits), 1);
        let body = tokio::time::timeout(std::time::Duration::from_secs(2), owner_rx.recv())
            .await
            .expect("owner must capture the body")
            .expect("body");
        assert_eq!(body["session_id"], "call-9");
        assert_eq!(body["command"]["action"], "call.set_var");
        assert_eq!(body["command"]["params"]["key"], "menu");
        assert_eq!(body["hops"], 1, "router must stamp the hop canary");
    }

    /// call vars (via the generic command envelope): owner == self →
    /// ApplyLocally, zero HTTP (no self-loop).
    #[tokio::test]
    async fn session_op_self_owner_never_self_forwards() {
        let (peer, _rx, hits) = spawn_peer(axum::http::StatusCode::OK).await;
        let self_node_id = format!("{}:{}", peer.addr, peer.sip_port);
        let registry = MockRegistry::new(MockPlan::Row(self_node_id.clone()));

        let outcome = routed_op_on_owner(
            &registry,
            &[peer],
            Some(&self_node_id),
            "",
            &reqwest::Client::new(),
            "call-9",
            &RwiCommandOp {
                command: serde_json::json!({
                    "action": "call.get_var",
                    "params": { "call_id": "call-9", "key": "menu" }
                }),
            },
        )
        .await;

        assert!(matches!(outcome, OwnerOpOutcome::ApplyLocally), "{outcome:?}");
        assert_eq!(count(&hits), 0);
    }

    /// console commands: owner resolved → single-cast with the
    /// `{session_id, payload}` wire shape the terminal handler consumes.
    #[tokio::test]
    async fn command_op_single_casts_to_owner_only() {
        let (owner_peer, mut owner_rx, owner_hits) =
            spawn_peer(axum::http::StatusCode::OK).await;

        let node_id = format!("{}:{}", owner_peer.addr, owner_peer.sip_port);
        let registry = MockRegistry::new(MockPlan::Row(node_id.clone()));

        let outcome = routed_op_on_owner(
            &registry,
            &[owner_peer.clone()],
            Some("this-node:5060"),
            "",
            &reqwest::Client::new(),
            "call-9",
            &ConsoleCommandOp {
                payload: serde_json::json!({ "action": "hangup", "reason": "busy" }),
            },
        )
        .await;

        assert!(matches!(outcome, OwnerOpOutcome::Applied(_, _)), "{outcome:?}");
        assert_eq!(count(&owner_hits), 1);
        let body = tokio::time::timeout(std::time::Duration::from_secs(2), owner_rx.recv())
            .await
            .expect("owner must capture the body")
            .expect("body");
        assert_eq!(body["session_id"], "call-9");
        assert_eq!(body["payload"]["action"], "hangup");
    }

    /// console commands: registry outage → RegistryUnavailable (retryable),
    /// zero HTTP.
    #[tokio::test]
    async fn command_op_registry_outage_is_reported() {
        let (peer, _rx, hits) = spawn_peer(axum::http::StatusCode::OK).await;
        let registry = MockRegistry::new(MockPlan::Down);

        let outcome = routed_op_on_owner(
            &registry,
            &[peer],
            Some("this-node:5060"),
            "",
            &reqwest::Client::new(),
            "call-9",
            &ConsoleCommandOp {
                payload: serde_json::json!({ "action": "hangup" }),
            },
        )
        .await;

        assert!(
            matches!(outcome, OwnerOpOutcome::RegistryUnavailable(_)),
            "{outcome:?}"
        );
        assert_eq!(count(&hits), 0);
    }
}
