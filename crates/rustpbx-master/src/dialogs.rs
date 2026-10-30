//! Dialog ownership table: `call_id → generation`, bounded + leak-proof.

use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub generation: u32,
    pub created: Instant,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub inserts: AtomicU64,
    pub down_removes: AtomicU64,
    pub purge_removes: AtomicU64,
    pub reconcile_removes: AtomicU64,
    pub reconcile_inserts: AtomicU64,
    pub cap_rejects: AtomicU64,
}

pub struct DialogTable {
    map: DashMap<String, Entry>,
    max_entries: usize,
    metrics: Arc<Metrics>,
}

#[derive(Debug, thiserror::Error)]
#[error("dialog table full")]
pub struct TableFull;

impl DialogTable {
    pub fn new(max_entries: usize, metrics: Arc<Metrics>) -> Self {
        Self {
            map: DashMap::with_capacity(max_entries.min(1 << 20)),
            max_entries,
            metrics,
        }
    }

    pub fn metrics(&self) -> &Arc<Metrics> { &self.metrics }

    pub fn upsert(&self, call_id: &str, generation: u32) -> Result<bool, TableFull> {
        if let Some(mut e) = self.map.get_mut(call_id) {
            if e.generation != generation {
                e.generation = generation;
            }
            return Ok(true);
        }
        if self.map.len() >= self.max_entries {
            self.metrics.cap_rejects.fetch_add(1, Ordering::Relaxed);
            return Err(TableFull);
        }
        self.map.insert(call_id.to_string(), Entry { generation, created: Instant::now() });
        self.metrics.inserts.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    pub fn remove(&self, call_id: &str) {
        if self.map.remove(call_id).is_some() {
            self.metrics.down_removes.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn owner(&self, call_id: &str) -> Option<u32> {
        self.map.get(call_id).map(|e| e.generation)
    }

    pub fn len(&self) -> usize { self.map.len() }
    pub fn is_empty(&self) -> bool { self.map.is_empty() }

    pub fn purge_generation(&self, generation: u32) -> usize {
        let before = self.map.len();
        self.map.retain(|_, e| e.generation != generation);
        let purged = before - self.map.len();
        if purged > 0 {
            self.metrics.purge_removes.fetch_add(purged as u64, Ordering::Relaxed);
        }
        purged
    }

    /// Full-snapshot reconciliation: prune stale entries owned by `generation`
    /// that are absent from the snapshot, re-insert snapshot ids missing from
    /// the table.
    pub fn reconcile_exact<I: Iterator<Item = String>>(&self, generation: u32, live: I) -> (usize, usize) {
        let live_set: std::collections::HashSet<String> = live.collect();
        let mut reinserted = 0usize;
        for call_id in &live_set {
            if !self.map.contains_key(call_id) {
                if self.map.len() >= self.max_entries {
                    self.metrics.cap_rejects.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                self.map.insert(call_id.clone(), Entry { generation, created: Instant::now() });
                reinserted += 1;
            }
        }
        let before = self.map.len();
        self.map.retain(|k, e| e.generation != generation || live_set.contains(k));
        let pruned = before - self.map.len();
        if reinserted > 0 { self.metrics.reconcile_inserts.fetch_add(reinserted as u64, Ordering::Relaxed); }
        if pruned > 0 { self.metrics.reconcile_removes.fetch_add(pruned as u64, Ordering::Relaxed); }
        (pruned, reinserted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(cap: usize) -> DialogTable {
        DialogTable::new(cap, Arc::new(Metrics::default()))
    }

    #[test]
    fn upsert_and_lookup() {
        let t = table(10);
        assert!(t.upsert("call-a", 1).unwrap());
        assert_eq!(t.owner("call-a"), Some(1));
        assert_eq!(t.owner("call-b"), None);
        assert_eq!(t.len(), 1);
        assert!(t.upsert("call-a", 1).unwrap());
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn dialog_down_removes() {
        let t = table(10);
        t.upsert("call-a", 1).unwrap();
        t.remove("call-a");
        assert_eq!(t.owner("call-a"), None);
        assert_eq!(t.metrics().down_removes.load(Ordering::Relaxed), 1);
        t.remove("never-existed");
        assert_eq!(t.metrics().down_removes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn hard_cap_rejects_instead_of_oom() {
        let t = table(3);
        for i in 0..3 { t.upsert(&format!("call-{i}"), 1).unwrap(); }
        assert!(t.upsert("call-3", 1).is_err());
        assert_eq!(t.len(), 3);
        assert_eq!(t.metrics().cap_rejects.load(Ordering::Relaxed), 1);
        assert!(t.upsert("call-0", 1).unwrap());
    }

    #[test]
    fn purge_generation_only_touches_that_generation() {
        let t = table(10);
        t.upsert("g1-a", 1).unwrap();
        t.upsert("g1-b", 1).unwrap();
        t.upsert("g2-a", 2).unwrap();
        assert_eq!(t.purge_generation(1), 2);
        assert_eq!(t.owner("g1-a"), None);
        assert_eq!(t.owner("g2-a"), Some(2));
        assert_eq!(t.purge_generation(1), 0);
    }

    #[test]
    fn reconcile_prunes_and_reinserts() {
        let t = table(10);
        t.upsert("live-1", 1).unwrap();
        t.upsert("stale", 1).unwrap();
        let (pruned, reinserted) = t.reconcile_exact(
            1,
            vec!["live-1".to_string(), "missed-up".to_string()].into_iter(),
        );
        assert_eq!(pruned, 1);
        assert_eq!(reinserted, 1);
        assert_eq!(t.owner("stale"), None);
        assert_eq!(t.owner("missed-up"), Some(1));
        t.upsert("g2-x", 2).unwrap();
        t.reconcile_exact(1, std::iter::empty());
        assert_eq!(t.owner("g2-x"), Some(2));
    }

    #[test]
    fn reconcile_respects_cap() {
        let t = table(2);
        t.upsert("a", 1).unwrap();
        t.upsert("b", 1).unwrap();
        let (_, _) = t.reconcile_exact(1, vec!["a".into(), "x".into(), "y".into(), "z".into()].into_iter());
        assert!(t.len() <= 2);
        assert!(t.metrics().cap_rejects.load(Ordering::Relaxed) >= 1);
    }
}
