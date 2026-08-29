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
}
