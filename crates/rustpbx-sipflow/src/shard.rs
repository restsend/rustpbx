use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use chrono::{DateTime, Datelike, Local, Timelike};

use crate::config::SipFlowSubdirs;

/// Router mode: the currently active bucket is a legacy single-file bucket
/// (`Single`) or a sharded bucket with `shard-*` subdirs (`Multi`).
pub const MODE_SINGLE: u8 = 1;
pub const MODE_MULTI: u8 = 2;

/// Shared write-routing state. `record()` reads it to pick a shard; worker
/// threads / bucket rotation update it when they rotate into a new bucket.
///
/// This is the single source of truth for "which bucket are we in, is it
/// sharded, and which shard does a call go to" — shared by both the SQLite and
/// FlowDB backends so routing behaves identically.
pub struct RouterState {
    mode: AtomicU8,
    pub shards: usize,
    /// FNV-1a of the active bucket subdir; used to detect when the bucket
    /// changes (hour/day boundary) so `current_layout` only pays for a disk
    /// scan on change.
    bucket_key: AtomicU64,
}

impl RouterState {
    pub fn new(mode: u8, shards: usize) -> Self {
        Self {
            mode: AtomicU8::new(mode),
            shards,
            bucket_key: AtomicU64::new(0),
        }
    }

    pub fn mode(&self) -> u8 {
        self.mode.load(Ordering::Relaxed)
    }

    pub fn set_mode(&self, mode: u8) {
        self.mode.store(mode, Ordering::Relaxed);
    }

    /// Pick the shard pipeline for a call: worker 0 while the active bucket is
    /// a legacy single-file bucket, otherwise `fnv1a(call_id) % shards`.
    /// `shards <= 1` always routes to 0.
    pub fn route_index(&self, call_id: &str) -> usize {
        if self.mode() == MODE_SINGLE || self.shards <= 1 {
            0
        } else {
            (fnv1a(call_id) as usize) % self.shards
        }
    }

    /// Layout of the active bucket, re-detected from disk when the bucket
    /// subdir changes (hour/day boundary). Lock-free on the hot path: a cheap
    /// FNV-1a hash of the subdir is compared against a cached value; only a
    /// subdir change pays for `detect_bucket_layout`. `shards <= 1` always
    /// reports [`BucketLayout::Single`].
    pub fn current_layout(&self, subdir: &str, base: &Path) -> BucketLayout {
        let key = fnv1a(subdir);
        if self.bucket_key.load(Ordering::Relaxed) == key {
            return BucketLayout::from_mode(self.mode());
        }
        let layout = if self.shards <= 1 {
            BucketLayout::Single
        } else {
            detect_bucket_layout(base)
        };
        self.bucket_key.store(key, Ordering::Relaxed);
        self.set_mode(layout.mode());
        layout
    }
}

/// Stable FNV-1a 64-bit hash used to route a call_id to a shard.
///
/// Deliberately not `std`'s `DefaultHasher` (SipHash keys are not guaranteed
/// stable across Rust releases); FNV-1a is trivial and version-stable so a
/// call keeps landing in the same shard across restarts.
pub fn fnv1a(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in input.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Layout of a bucket directory: sharded (has `shard-*` subdirs) or a legacy
/// single-file bucket (`sipflow.db`/`data.raw` at the bucket root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketLayout {
    Single,
    Multi,
}

impl BucketLayout {
    pub fn mode(self) -> u8 {
        match self {
            BucketLayout::Single => MODE_SINGLE,
            BucketLayout::Multi => MODE_MULTI,
        }
    }

    pub fn from_mode(mode: u8) -> Self {
        if mode == MODE_MULTI {
            BucketLayout::Multi
        } else {
            BucketLayout::Single
        }
    }
}

/// Decide a bucket's layout from its on-disk state:
/// - contains `shard-*` subdirs → `Multi`
/// - missing or empty           → `Multi` (new directories get sharded)
/// - has files but no `shard-*` → `Single` (legacy layout)
pub fn detect_bucket_layout(dir: &Path) -> BucketLayout {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return BucketLayout::Multi;
    };
    let mut any = false;
    for entry in rd.flatten() {
        any = true;
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() && entry.file_name().to_string_lossy().starts_with("shard-") {
            return BucketLayout::Multi;
        }
    }
    if any {
        BucketLayout::Single
    } else {
        BucketLayout::Multi
    }
}

/// Bucket subdirectory string for `dt`, mirroring the write-side rotation.
pub fn bucket_subdir(subdirs: &SipFlowSubdirs, dt: DateTime<Local>) -> String {
    match subdirs {
        SipFlowSubdirs::Hourly => format!(
            "{:04}{:02}{:02}/{:02}",
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour()
        ),
        SipFlowSubdirs::Daily => format!("{:04}{:02}{:02}", dt.year(), dt.month(), dt.day()),
        SipFlowSubdirs::None => String::new(),
    }
}

/// The active bucket directory for `dt` under `root`.
pub fn active_bucket_dir(root: &Path, subdirs: &SipFlowSubdirs, dt: DateTime<Local>) -> PathBuf {
    root.join(bucket_subdir(subdirs, dt))
}

/// Expand a discovered bucket dir into the legacy dir plus its `shard-*`
/// subdirs, so reads cover old single-file buckets and any shard count.
pub fn bucket_query_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![dir.to_path_buf()];
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut shards: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("shard-"))
        })
        .collect();
    shards.sort();
    out.extend(shards);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fnva_stable_and_distinct() {
        let h1 = fnv1a("call-abc");
        assert_eq!(fnv1a("call-abc"), h1, "hash must be stable across calls");
        let mut seen = std::collections::HashSet::new();
        for i in 0..1000u32 {
            assert!(seen.insert(fnv1a(&format!("call-{i}"))), "no dup for {i}");
        }
    }

    #[test]
    fn test_detect_bucket_layout() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // Missing dir → Multi (new dirs get sharded).
        assert_eq!(
            detect_bucket_layout(&base.join("nope")),
            BucketLayout::Multi
        );

        // Empty dir → Multi.
        std::fs::create_dir_all(base.join("empty")).unwrap();
        assert_eq!(
            detect_bucket_layout(&base.join("empty")),
            BucketLayout::Multi
        );

        // Legacy files, no shard-* → Single.
        let legacy = base.join("legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("sipflow.db"), b"x").unwrap();
        assert_eq!(detect_bucket_layout(&legacy), BucketLayout::Single);

        // shard-* subdir present → Multi (even alongside legacy files).
        let mixed = base.join("mixed");
        std::fs::create_dir_all(&mixed).unwrap();
        std::fs::write(mixed.join("sipflow.db"), b"x").unwrap();
        std::fs::create_dir_all(mixed.join("shard-0")).unwrap();
        assert_eq!(detect_bucket_layout(&mixed), BucketLayout::Multi);
    }

    #[test]
    fn test_bucket_query_dirs_expands_shards() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        // Legacy-only dir → just itself.
        assert_eq!(bucket_query_dirs(base), vec![base.to_path_buf()]);

        // With shards → [base, shard-0, shard-1, ...] sorted.
        std::fs::create_dir_all(base.join("shard-2")).unwrap();
        std::fs::create_dir_all(base.join("shard-0")).unwrap();
        std::fs::create_dir_all(base.join("shard-1")).unwrap();
        std::fs::write(base.join("sipflow.db"), b"x").unwrap();
        let dirs = bucket_query_dirs(base);
        assert_eq!(
            dirs,
            vec![
                base.to_path_buf(),
                base.join("shard-0"),
                base.join("shard-1"),
                base.join("shard-2"),
            ]
        );
    }
}
