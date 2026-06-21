use std::sync::Arc;
use std::time::{Duration, Instant};

use rsipstack::transport::SipAddr;
use tokio::sync::Mutex;

/// Soft TTL after which an entry is eligible for opportunistic eviction.
///
/// Entries are normally removed by the explicit `remove` call on the WS
/// close path. If a client disconnects ungracefully (network drop, crash)
/// the close frame is never sent, so without this TTL the entry would stay
/// forever. Eviction is lazy (triggered on `register` once the soft cap is
/// hit) so there is no background task and the registry stays cheap.
const ENTRY_TTL: Duration = Duration::from_secs(300);

/// Soft cap that triggers eviction of TTL-expired entries inside `register`.
/// Generously above realistic concurrent authenticating-client counts so the
/// eviction path is essentially never exercised in normal operation.
const SOFT_CAP: usize = 256;

pub struct PreAuthRegistry {
    map: Mutex<Vec<(SipAddr, String, Instant)>>,
}

impl PreAuthRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::new(Vec::new()),
        })
    }

    pub async fn register(&self, addr: SipAddr, agent_id: String) {
        let mut map = self.map.lock().await;
        // Opportunistic GC: when the table grows past the soft cap, evict
        // expired entries before inserting. Keeps memory bounded for the
        // pathological case of many ungraceful WS disconnects.
        if map.len() >= SOFT_CAP {
            let now = Instant::now();
            map.retain(|(_, _, ts)| now.duration_since(*ts) < ENTRY_TTL);
        }
        map.retain(|(a, _, _)| !addrs_equal(a, &addr));
        map.push((addr, agent_id, Instant::now()));
    }

    pub async fn lookup(&self, addr: &SipAddr) -> Option<String> {
        let map = self.map.lock().await;
        for (a, agent, _) in map.iter() {
            if addrs_equal(a, addr) {
                return Some(agent.clone());
            }
        }
        None
    }

    pub async fn remove(&self, addr: &SipAddr) {
        let mut map = self.map.lock().await;
        map.retain(|(a, _, _)| !addrs_equal(a, addr));
    }
}

fn addrs_equal(a: &SipAddr, b: &SipAddr) -> bool {
    a.r#type == b.r#type && a.addr.host == b.addr.host && a.addr.port == b.addr.port
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr_for(port: u16) -> SipAddr {
        // SipAddr: From<SocketAddr> exists in rsipstack; using it keeps the
        // test free of internal layout assumptions.
        SipAddr::from(
            format!("127.0.0.1:{}", port)
                .parse::<SocketAddr>()
                .expect("valid socket addr"),
        )
    }

    #[tokio::test]
    async fn register_evicts_expired_entries_when_above_soft_cap() {
        // Regression: a client that disconnects ungracefully never calls
        // `remove`, so its entry used to stay forever. Verify that pushing
        // past the soft cap evicts TTL-expired entries.
        let registry = PreAuthRegistry::new();

        // Pre-populate with back-dated entries that look expired.
        {
            let mut map = registry.map.lock().await;
            for i in 0..SOFT_CAP {
                map.push((
                    addr_for(40_000 + i as u16),
                    format!("stale-agent-{i}"),
                    Instant::now() - ENTRY_TTL - Duration::from_secs(1),
                ));
            }
            assert!(map.len() >= SOFT_CAP);
        }

        // Inserting one more fresh entry must sweep the stale ones.
        registry
            .register(addr_for(50_000), "fresh-agent".to_string())
            .await;

        let map = registry.map.lock().await;
        assert_eq!(
            map.len(),
            1,
            "stale entries should have been evicted, remaining: {}",
            map.len()
        );
        assert_eq!(map[0].1, "fresh-agent");
    }

    #[tokio::test]
    async fn register_replaces_existing_entry_for_same_addr() {
        let registry = PreAuthRegistry::new();
        registry
            .register(addr_for(40_000), "agent-a".to_string())
            .await;
        registry
            .register(addr_for(40_000), "agent-b".to_string())
            .await;
        let map = registry.map.lock().await;
        assert_eq!(map.len(), 1);
        assert_eq!(map[0].1, "agent-b");
    }

    #[tokio::test]
    async fn lookup_and_remove_round_trip() {
        let registry = PreAuthRegistry::new();
        let a = addr_for(40_001);
        registry.register(a.clone(), "agent-x".to_string()).await;
        assert_eq!(
            registry.lookup(&a).await.as_deref(),
            Some("agent-x"),
            "entry must be visible immediately after register"
        );
        registry.remove(&a).await;
        assert!(
            registry.lookup(&a).await.is_none(),
            "entry must be gone after remove"
        );
    }
}
