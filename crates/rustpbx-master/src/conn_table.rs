//! Connection ownership table: `(ConnKind, peer) → generation`, bounded +
//! leak-proof (FlowTable discipline).
//!
//! Fed by worker `ConnUp`/`ConnDown` reports. Two consumers:
//! * accept-time affinity — an fd-forwarded SIP TCP/TLS connection from a
//!   peer whose live connection already lives in generation N routes to N,
//!   not to whatever generation is "current";
//! * cross-gen relay routing — a `RelayToConn` frame (dialog generation →
//!   connection generation) resolves its target here.
//!
//! `conn_id → key` is a secondary index so `ConnDown` (which only carries
//! the id the worker minted) can evict exactly one entry.

use rustpbx_master_ipc::ConnKind;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Full-table sweeps are throttled to one per this many lookups: the
/// per-entry TTL check already returns `None` for expired entries on every
/// lookup, so the sweep only reclaims memory and must never put an O(entries)
/// walk on the relay hot path.
const SWEEP_INTERVAL: u64 = 1024;

#[derive(Debug, Default)]
pub struct Metrics {
    pub ups: AtomicU64,
    pub downs: AtomicU64,
    pub purge_removes: AtomicU64,
    pub cap_evicts: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    generation: u32,
    seen: Instant,
}

pub struct ConnTable {
    map: Mutex<HashMap<(u8, SocketAddr), Entry>>,
    /// conn_id → table key, for id-only eviction.
    ids: Mutex<HashMap<u64, (u8, SocketAddr)>>,
    max: usize,
    ttl: Duration,
    metrics: Arc<Metrics>,
    lookups: AtomicU64,
}

impl ConnTable {
    pub fn new(max: usize, ttl: Duration, metrics: Arc<Metrics>) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            ids: Mutex::new(HashMap::new()),
            max,
            ttl,
            metrics,
            lookups: AtomicU64::new(0),
        }
    }

    /// Record/refresh the owner of a live connection. Overwrites any prior
    /// entry for the same key (newest wins — same-NAT-ip peers share the
    /// key exactly like the UDP FlowTable) and drops the superseded
    /// conn_id's index entry, so a stale ConnDown can never evict the
    /// replacement.
    pub fn upsert(&self, kind: ConnKind, peer: SocketAddr, conn_id: u64, generation: u32) {
        let key = (kind.as_u8(), peer);
        {
            let mut ids = self.lock_ids();
            // Keep at most one id per key: the newest conn wins the key, so
            // any other id pointing at it (the replaced connection) is dead.
            ids.retain(|id, k| *id == conn_id || *k != key);
            ids.insert(conn_id, key);
        }
        let mut map = self.lock_map();
        self.sweep_locked(&mut map);
        if map.len() >= self.max {
            // Bound: drop the oldest entry (FlowTable eviction discipline).
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.seen)
                .map(|(k, _)| *k)
            {
                map.remove(&oldest);
                self.metrics.cap_evicts.fetch_add(1, Ordering::Relaxed);
            }
        }
        map.insert(key, Entry { generation, seen: Instant::now() });
        self.metrics.ups.fetch_add(1, Ordering::Relaxed);
    }

    /// Affinity/relay lookup. The per-entry TTL check returns `None` for
    /// expired entries immediately; the full-table sweep is throttled
    /// (see [`SWEEP_INTERVAL`]) so the hot path stays O(1).
    pub fn owner(&self, kind: ConnKind, peer: &SocketAddr) -> Option<u32> {
        let mut map = self.lock_map();
        let n = self.lookups.fetch_add(1, Ordering::Relaxed);
        if n % SWEEP_INTERVAL == 0 {
            self.sweep_locked(&mut map);
        }
        let entry = map.get_mut(&(kind.as_u8(), *peer))?;
        if entry.seen.elapsed() > self.ttl {
            return None;
        }
        entry.seen = Instant::now();
        Some(entry.generation)
    }

    /// Force a full-table TTL sweep (tests and admin paths).
    pub fn sweep_now(&self) {
        let mut map = self.lock_map();
        self.sweep_locked(&mut map);
    }

    /// Evict by the worker-minted conn_id; returns the evicted key so the
    /// caller (relay hub) can fan `ConnClosed` out to virtual holders.
    pub fn remove_by_conn_id(&self, conn_id: u64) -> Option<(ConnKind, SocketAddr)> {
        let key = self.lock_ids().remove(&conn_id)?;
        let removed = self.lock_map().remove(&key);
        if removed.is_some() {
            self.metrics.downs.fetch_add(1, Ordering::Relaxed);
        }
        ConnKind::from_u8(key.0).map(|k| (k, key.1))
    }

    /// Worker/generation exit: purge everything it owned. Returns the keys
    /// (for ConnClosed fan-out to virtual holders).
    pub fn purge_generation(&self, generation: u32) -> Vec<(ConnKind, SocketAddr)> {
        let mut map = self.lock_map();
        let stale: Vec<(u8, SocketAddr)> = map
            .iter()
            .filter(|(_, e)| e.generation == generation)
            .map(|(k, _)| *k)
            .collect();
        for key in &stale {
            map.remove(key);
        }
        {
            let mut ids = self.lock_ids();
            ids.retain(|_, key| !stale.contains(key));
        }
        let purged = stale.len();
        if purged > 0 {
            self.metrics
                .purge_removes
                .fetch_add(purged as u64, Ordering::Relaxed);
        }
        stale
            .into_iter()
            .filter_map(|(b, peer)| ConnKind::from_u8(b).map(|k| (k, peer)))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.lock_map().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn sweep_locked(&self, map: &mut HashMap<(u8, SocketAddr), Entry>) {
        let cutoff = Instant::now() - self.ttl;
        let stale: Vec<(u8, SocketAddr)> = map
            .iter()
            .filter(|(_, e)| e.seen <= cutoff)
            .map(|(k, _)| *k)
            .collect();
        if stale.is_empty() {
            return;
        }
        for key in &stale {
            map.remove(key);
        }
        let mut ids = self.lock_ids();
        ids.retain(|_, key| map.contains_key(key));
    }

    fn lock_map(&self) -> std::sync::MutexGuard<'_, HashMap<(u8, SocketAddr), Entry>> {
        self.map.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_ids(&self) -> std::sync::MutexGuard<'_, HashMap<u64, (u8, SocketAddr)>> {
        self.ids.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn peer(p: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::from([10, 0, 0, 1]), p)
    }

    fn table() -> ConnTable {
        ConnTable::new(
            100,
            Duration::from_secs(60),
            Arc::new(Metrics::default()),
        )
    }

    #[test]
    fn upsert_lookup_and_down() {
        let t = table();
        assert_eq!(t.owner(ConnKind::Ws, &peer(1)), None);
        t.upsert(ConnKind::Ws, peer(1), 7, 3);
        assert_eq!(t.owner(ConnKind::Ws, &peer(1)), Some(3));
        // Kind is part of the key: a Ws entry must not answer a Wss lookup.
        assert_eq!(t.owner(ConnKind::Wss, &peer(1)), None);
        // ConnDown by id evicts exactly that entry and reports the key.
        let key = t.remove_by_conn_id(7).expect("evicted");
        assert_eq!(key, (ConnKind::Ws, peer(1)));
        assert_eq!(t.owner(ConnKind::Ws, &peer(1)), None);
        assert_eq!(t.remove_by_conn_id(7), None);
    }

    #[test]
    fn reupsert_same_peer_replaces_and_keeps_id_index_consistent() {
        let t = table();
        t.upsert(ConnKind::Ws, peer(1), 7, 3);
        // Worker restarted (new gen) — same peer reconnects with a new id.
        t.upsert(ConnKind::Ws, peer(1), 8, 4);
        assert_eq!(t.owner(ConnKind::Ws, &peer(1)), Some(4));
        // Old id's down is a no-op (its key mapping was replaced).
        assert_eq!(t.remove_by_conn_id(7), None);
        // New id's down still evicts.
        assert_eq!(t.remove_by_conn_id(8), Some((ConnKind::Ws, peer(1))));
    }

    #[test]
    fn purge_generation_returns_keys_and_scopes_correctly() {
        let t = table();
        t.upsert(ConnKind::Ws, peer(1), 1, 1);
        t.upsert(ConnKind::SipTcp, peer(2), 2, 1);
        t.upsert(ConnKind::Wss, peer(3), 3, 2);
        let mut keys = t.purge_generation(1);
        keys.sort_by_key(|(_, p)| p.port());
        assert_eq!(
            keys,
            vec![(ConnKind::Ws, peer(1)), (ConnKind::SipTcp, peer(2))]
        );
        assert_eq!(t.owner(ConnKind::Wss, &peer(3)), Some(2));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn ttl_expiry_returns_none_and_sweep_reclaims() {
        let t = ConnTable::new(100, Duration::from_millis(0), Arc::new(Metrics::default()));
        t.upsert(ConnKind::Ws, peer(1), 1, 1);
        // The per-entry TTL check answers None immediately (O(1) hot path)…
        assert_eq!(t.owner(ConnKind::Ws, &peer(1)), None);
        // …while the (throttled) sweep reclaims the memory.
        t.sweep_now();
        assert_eq!(t.len(), 0, "sweep must reclaim expired entries");
    }

    #[test]
    fn cap_evicts_oldest_and_stays_bounded() {
        let t = ConnTable::new(3, Duration::from_secs(60), Arc::new(Metrics::default()));
        for i in 1..=5u16 {
            t.upsert(ConnKind::Ws, peer(i), i as u64, 1);
        }
        assert!(t.len() <= 3, "cap exceeded: {}", t.len());
        assert!(t.metrics.cap_evicts.load(Ordering::Relaxed) >= 1);
        // The newest entries survive; the evicted ones are the oldest.
        assert_eq!(t.owner(ConnKind::Ws, &peer(5)), Some(1));
    }
}
