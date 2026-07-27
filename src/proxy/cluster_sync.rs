use serde::Serialize;
use std::fmt;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::debug;

#[derive(Clone, Debug)]
pub struct AmiPeer {
    pub addr: String,
    pub ami_port: u16,
    pub ami_path: String,
    pub sip_addr: String,
}

impl AmiPeer {
    pub fn ami_url(&self, event_type: &str) -> String {
        format!(
            "http://{}:{}{}/cluster/event/{}",
            self.addr, self.ami_port, self.ami_path, event_type
        )
    }
    pub fn sip_socket_addr(&self) -> String {
        format!("{}:{}", self.addr, self.sip_addr)
    }
}

impl fmt::Display for AmiPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.addr, self.ami_port)
    }
}

#[derive(Clone)]
pub struct ClusterSync {
    client: reqwest::Client,
    peers: Vec<AmiPeer>,
}

impl ClusterSync {
    pub fn new(client: reqwest::Client, peers: Vec<AmiPeer>) -> Self {
        Self { client, peers }
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn peers(&self) -> &[AmiPeer] {
        &self.peers
    }

    /// Parallel fire-and-forget HTTP POST to all peers.
    pub fn broadcast<T: Serialize + Send + 'static>(&self, event_type: &str, body: &T) {
        if self.peers.is_empty() {
            return;
        }
        let client = self.client.clone();
        let peers = self.peers.clone();
        let et = event_type.to_string();
        let payload = match serde_json::to_value(body) {
            Ok(v) => v,
            Err(e) => {
                debug!("cluster_sync: serialize failed for {}: {}", et, e);
                return;
            }
        };
        let peer_count = peers.len();
        tokio::spawn(async move {
            let mut set = JoinSet::new();
            for p in peers {
                let url = p.ami_url(&et);
                let c = client.clone();
                let b = payload.clone();
                let etag = et.clone();
                set.spawn(async move {
                    let start = std::time::Instant::now();
                    match c.post(&url).json(&b).timeout(Duration::from_millis(1000)).send().await {
                        Ok(resp) => {
                            let status = resp.status();
                            debug!("cluster_sync: {} {} -> {} ({:?})", etag, p, status, start.elapsed());
                        }
                        Err(dur_err) => {
                            debug!("cluster_sync: {} {} failed: {}", etag, p, dur_err);
                        }
                    }
                });
            }
            while set.join_next().await.is_some() {}
            debug!("cluster_sync: {} broadcast to {} peers done", et, peer_count);
        });
    }
}
