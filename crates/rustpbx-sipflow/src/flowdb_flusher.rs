use anyhow::Result;
use dashmap::DashMap;
use flowdb::{Config as FlowDbConfig, Engine, Record};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Message delivered to a per-shard flusher thread.
pub(crate) enum FlushMsg {
    Record { path: PathBuf, record: Record },
    FlushSync { done: std::sync::mpsc::Sender<()> },
}

/// A cached engine entry, tagged with the last time it was touched so the LRU
/// evictor can pick the oldest victim.
struct CachedEngine {
    engine: Arc<Engine>,
    last_used: Instant,
}

/// Shared LRU cache of open FlowDB engines, keyed by absolute data directory.
///
/// Shared between the read path (queries) and the per-shard flusher threads,
/// so exactly one `Engine` exists per bucket directory. Eviction drops the
/// cache entry without an explicit `close()`: any `Arc` still held by an
/// in-flight flusher/query keeps the engine alive, and the last reference
/// cleanly stops its background maintenance thread.
pub(crate) struct EngineCache {
    engines: DashMap<PathBuf, CachedEngine>,
    max_open: usize,
    ttl_secs: Option<u64>,
    memtable_size_mb: usize,
    block_cache_capacity_mb: usize,
    wal_sync_mode: flowdb::SyncMode,
}

impl EngineCache {
    pub(crate) fn new(
        shards: usize,
        ttl_secs: Option<u64>,
        memtable_size_mb: usize,
        block_cache_capacity_mb: usize,
        wal_sync_mode: flowdb::SyncMode,
    ) -> Self {
        let max_open = (shards * 4).max(24);
        Self {
            engines: DashMap::new(),
            max_open,
            ttl_secs,
            memtable_size_mb,
            block_cache_capacity_mb,
            wal_sync_mode,
        }
    }

    /// Get-or-open the engine for `path`, updating its LRU stamp.
    ///
    /// When the cache exceeds the capacity, the least-recently-used engine is
    /// removed (without closing — surviving `Arc`s keep it alive until done).
    pub(crate) fn get_or_open(&self, path: &PathBuf) -> Result<Arc<Engine>> {
        // Fast path: cache hit.
        if let Some(mut entry) = self.engines.get_mut(path) {
            entry.last_used = Instant::now();
            return Ok(entry.engine.clone());
        }

        // Slow path: open a new engine outside the lock to avoid blocking
        // other callers on directory creation / WAL replay.
        let new_engine = Self::open_engine_at(
            path,
            self.ttl_secs,
            self.memtable_size_mb,
            self.block_cache_capacity_mb,
            self.wal_sync_mode,
        )?;

        // Re-check: another thread may have raced us.
        if let Some(mut entry) = self.engines.get_mut(path) {
            entry.last_used = Instant::now();
            return Ok(entry.engine.clone());
        }
        self.engines.insert(
            path.clone(),
            CachedEngine {
                engine: new_engine.clone(),
                last_used: Instant::now(),
            },
        );

        // Evict the LRU engine (excluding the one just opened) if over capacity.
        if self.engines.len() > self.max_open {
            let victim = self
                .engines
                .iter()
                .filter(|entry| entry.key().as_path() != path.as_path())
                .min_by_key(|entry| entry.value().last_used)
                .map(|entry| entry.key().clone());
            if let Some(victim_path) = victim {
                self.engines.remove(&victim_path);
            }
        }

        Ok(new_engine)
    }

    fn open_engine_at(
        path: &PathBuf,
        ttl_secs: Option<u64>,
        memtable_size_mb: usize,
        block_cache_capacity_mb: usize,
        wal_sync_mode: flowdb::SyncMode,
    ) -> Result<Arc<Engine>> {
        std::fs::create_dir_all(path)?;
        let config = FlowDbConfig {
            data_dir: path.clone(),
            default_ttl_secs: ttl_secs,
            memtable_size_mb,
            block_cache_capacity_mb,
            auto_background: true,
            wal_sync_mode,
            ..Default::default()
        };
        let engine = Engine::open(config)?;
        Ok(Arc::new(engine))
    }

    /// Write a batch of records grouped by their bucket path.
    fn flush_batch(&self, batch: &mut Vec<(PathBuf, Record)>) {
        if batch.is_empty() {
            return;
        }
        let mut groups: HashMap<PathBuf, Vec<Record>> = HashMap::new();
        for (path, record) in batch.drain(..) {
            groups.entry(path).or_default().push(record);
        }
        for (path, records) in groups {
            match self.get_or_open(&path) {
                Ok(engine) => {
                    if let Err(e) = engine.write_batch_sync(records) {
                        tracing::warn!(
                            "flowdb write_batch_sync failed for {}: {e}",
                            path.display()
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "flowdb engine open failed for {}: {e}; dropping batch",
                        path.display()
                    );
                }
            }
        }
    }

    /// Close every cached engine. Callers must ensure no flusher is active.
    fn close_all(&self) {
        let keys: Vec<PathBuf> = self.engines.iter().map(|e| e.key().clone()).collect();
        for key in keys {
            if let Some((_, entry)) = self.engines.remove(&key) {
                let _ = entry.engine.close();
            }
        }
    }

    /// Snapshot of every open engine (used to flush memtables on explicit flush).
    pub(crate) fn all_engines(&self) -> Vec<Arc<Engine>> {
        self.engines.iter().map(|e| e.value().engine.clone()).collect()
    }
}

impl Drop for EngineCache {
    fn drop(&mut self) {
        self.close_all();
    }
}

/// A per-shard flusher thread that owns a bounded channel of records.
///
/// `record()` pushes onto the channel; when it is full the sender blocks —
/// that is the intended backpressure that bounds memory under overload. The
/// flusher accumulates into a batch and writes it to the shard's engines when
/// the count or interval threshold is hit.
pub(crate) struct FlowDbFlusher {
    sender: Option<SyncSender<FlushMsg>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FlowDbFlusher {
    pub(crate) fn new(
        flush_count: usize,
        flush_interval_secs: u64,
        engines: Arc<EngineCache>,
    ) -> Self {
        let capacity = (flush_count * 4).max(1024);
        let (tx, rx) = sync_channel(capacity);
        let handle = thread::Builder::new()
            .name("flowdb-flusher".into())
            .spawn(move || {
                run(
                    rx,
                    flush_count,
                    Duration::from_secs(flush_interval_secs),
                    engines,
                )
            })
            .expect("failed to spawn flowdb flusher");
        Self {
            sender: Some(tx),
            handle: Some(handle),
        }
    }

    pub(crate) fn sender(&self) -> SyncSender<FlushMsg> {
        self.sender
            .as_ref()
            .expect("flowdb flusher sender taken")
            .clone()
    }
}

impl Drop for FlowDbFlusher {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let _ = sender.send(FlushMsg::FlushSync { done: done_tx });
            let _ = done_rx.recv_timeout(Duration::from_secs(30));
            drop(sender); // disconnect so the thread's recv() errors out
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(
    rx: Receiver<FlushMsg>,
    flush_count: usize,
    flush_interval: Duration,
    engines: Arc<EngineCache>,
) {
    let mut batch: Vec<(PathBuf, Record)> = Vec::with_capacity(flush_count.max(1024));
    let mut last_flush = Instant::now();
    // A zero interval means "no time trigger"; still poll the channel so the
    // thread isn't a busy loop.
    let recv_timeout = if flush_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        flush_interval
    };

    loop {
        match rx.recv_timeout(recv_timeout) {
            Ok(msg) => match msg {
                FlushMsg::Record { path, record } => {
                    batch.push((path, record));
                    if batch.len() >= flush_count || last_flush.elapsed() >= flush_interval {
                        engines.flush_batch(&mut batch);
                        last_flush = Instant::now();
                    }
                }
                FlushMsg::FlushSync { done } => {
                    engines.flush_batch(&mut batch);
                    last_flush = Instant::now();
                    let _ = done.send(());
                }
            },
            Err(RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    engines.flush_batch(&mut batch);
                    last_flush = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                engines.flush_batch(&mut batch);
                break;
            }
        }
    }
}
