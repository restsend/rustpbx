//! Node-level registration directory (Registry-in-Master).
//!
//! Memory-locator deployments have per-generation registries: a registration
//! accepted on gen1 is invisible to gen2, so a flip-window dispatch cannot
//! resolve the agent at all. Workers report accepted bindings here
//! (`RegistryUp`); the dispatching generation consults this table when its
//! LOCAL locator misses (`RegistryLookup`) and the relay delivers to the
//! generation that owns the registering connection.
//!
//! Cleanup paths (each bounded):
//! * conn death — `ConnDown` evicts by the (kind, peer) destination key
//! * expiry — per-entry check on lookup (lazy remove); no hot-path sweep
//! * generation exit — `purge_generation`
//! * deregister — `RegistryUp` with `expires_secs == 0` removes the keys

use rustpbx_master_ipc::ConnKind;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Metrics {
    pub ups: AtomicU64,
    pub lookups: AtomicU64,
    pub lookup_hits: AtomicU64,
    pub removes: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: u8,
    peer: SocketAddr,
    expires: Instant,
    owner_gen: u32,
}

pub struct RegistryTable {
    map: Mutex<HashMap<String, Entry>>,
    max: usize,
    metrics: Arc<Metrics>,
}

impl RegistryTable {
    pub fn new(max: usize, metrics: Arc<Metrics>) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max,
            metrics,
        }
    }

    /// Insert/refresh one binding under every key. `expires_secs == 0`
    /// removes the keys (deregistration). Keys alias the same entry — the
    /// registrar reports both the contact URI and the canonical AoR.
    pub fn upsert(
        &self,
        keys: &[String],
        kind: ConnKind,
        peer: Option<SocketAddr>,
        expires_secs: u64,
        owner_gen: u32,
    ) {
        let mut map = self.lock();
        if expires_secs == 0 || peer.is_none() {
            for key in keys {
                if map.remove(key).is_some() {
                    self.metrics.removes.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        }
        let expires = Instant::now() + Duration::from_secs(expires_secs);
        for key in keys {
            if map.len() >= self.max && !map.contains_key(key) {
                // Bounded: drop one expired/oldest entry to make room.
                if let Some(oldest) = map.iter().min_by_key(|(_, e)| e.expires).map(|(k, _)| k.clone()) {
                    map.remove(&oldest);
                }
            }
            map.insert(
                key.clone(),
                Entry { kind: kind.as_u8(), peer: peer.unwrap(), expires, owner_gen: owner_gen },
            );
        }
        self.metrics.ups.fetch_add(1, Ordering::Relaxed);
    }

    /// Directory query: per-entry expiry check (expired entries are lazily
    /// removed), O(1) on the hot path.
    pub fn lookup(&self, aor: &str) -> Option<(ConnKind, SocketAddr)> {
        self.metrics.lookups.fetch_add(1, Ordering::Relaxed);
        let mut map = self.lock();
        let entry = map.get(aor)?;
        if entry.expires <= Instant::now() {
            map.remove(aor);
            self.metrics.removes.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.metrics.lookup_hits.fetch_add(1, Ordering::Relaxed);
        ConnKind::from_u8(entry.kind).map(|k| (k, entry.peer))
    }

    /// A connection died: evict every binding whose destination rides it.
    /// Returns the number of removed entries.
    pub fn remove_by_conn(&self, kind: ConnKind, peer: &SocketAddr) -> usize {
        let mut map = self.lock();
        let before = map.len();
        map.retain(|_, e| !(e.kind == kind.as_u8() && e.peer == *peer));
        let removed = before - map.len();
        if removed > 0 {
            self.metrics.removes.fetch_add(removed as u64, Ordering::Relaxed);
        }
        removed
    }

    /// Generation exit: purge everything it reported.
    pub fn purge_generation(&self, generation: u32) -> usize {
        let mut map = self.lock();
        let before = map.len();
        map.retain(|_, e| e.owner_gen != generation);
        let purged = before - map.len();
        if purged > 0 {
            self.metrics.removes.fetch_add(purged as u64, Ordering::Relaxed);
        }
        purged
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test-only: insert an already-expired entry.
    #[cfg(test)]
    fn upsert_expired(
        &self,
        keys: &[String],
        kind: ConnKind,
        peer: Option<SocketAddr>,
        owner_gen: u32,
    ) {
        let mut map = self.lock();
        for key in keys {
            map.insert(
                key.clone(),
                Entry { kind: kind.as_u8(), peer: peer.unwrap(), expires: Instant::now(), owner_gen: owner_gen },
            );
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.map.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn peer(p: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::from([10, 0, 0, 1]), p)
    }

    fn table() -> RegistryTable {
        RegistryTable::new(100, Arc::new(Metrics::default()))
    }

    fn keys(aor: &str) -> Vec<String> {
        vec![format!("sip:{aor}"), aor.to_string()]
    }

    #[test]
    fn upsert_lookup_and_deregister() {
        let t = table();
        t.upsert(&keys("echo@probe"), ConnKind::Ws, Some(peer(1)), 300, 1);
        assert_eq!(
            t.lookup("echo@probe"),
            Some((ConnKind::Ws, peer(1))),
            "every reported key resolves"
        );
        assert_eq!(
            t.lookup("sip:echo@probe"),
            Some((ConnKind::Ws, peer(1)))
        );
        // Deregister via expires=0 removes ALL keys of the binding.
        t.upsert(&keys("echo@probe"), ConnKind::Ws, None, 0, 1);
        assert_eq!(t.lookup("echo@probe"), None);
        assert_eq!(t.lookup("sip:echo@probe"), None);
    }

    #[test]
    fn expired_entry_is_lazy_removed() {
        let t = RegistryTable::new(100, Arc::new(Metrics::default()));
        // Already-expired entry (test helper).
        t.upsert_expired(&["gone".to_string()], ConnKind::Ws, Some(peer(2)), 1);
        assert_eq!(t.lookup("gone"), None, "expired entry must not resolve");
        assert_eq!(t.len(), 0, "expired entry lazily reclaimed");
    }

    #[test]
    fn conn_death_evicts_by_destination() {
        let t = table();
        t.upsert(&keys("a"), ConnKind::Ws, Some(peer(1)), 300, 1);
        t.upsert(&keys("b"), ConnKind::Ws, Some(peer(2)), 300, 1);
        t.upsert(&keys("c"), ConnKind::SipTcp, Some(peer(1)), 300, 1);
        // Both aliased keys of the binding are removed together.
        assert_eq!(t.remove_by_conn(ConnKind::Ws, &peer(1)), 2);
        assert_eq!(t.lookup("a"), None);
        assert_eq!(t.lookup("b"), Some((ConnKind::Ws, peer(2))));
        assert_eq!(t.lookup("c"), Some((ConnKind::SipTcp, peer(1))));
    }

    #[test]
    fn purge_generation_scopes() {
        let t = table();
        t.upsert(&keys("g1"), ConnKind::Ws, Some(peer(1)), 300, 1);
        t.upsert(&keys("g2"), ConnKind::Wss, Some(peer(2)), 300, 2);
        assert_eq!(t.purge_generation(1), 2);
        assert_eq!(t.lookup("g1"), None);
        assert_eq!(t.lookup("g2"), Some((ConnKind::Wss, peer(2))));
    }
}
