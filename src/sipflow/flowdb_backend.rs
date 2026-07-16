use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Datelike, Local, Timelike};
use flowdb::{Config as FlowDbConfig, Engine, Record, ScanRange};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::SipFlowSubdirs;
use crate::sipflow::flowdb_codec::{
    SIP_PREFIX, decode_rtp_value, decode_sip_value, encode_rtp_value, encode_sip_value,
    make_rtp_key, make_sip_key, rtp_call_leg_prefix, rtp_call_prefix, sip_call_prefix,
};
use crate::sipflow::rtp_stats::{MediaStatsAccumulator, parse_rtp_stats_header};
use crate::sipflow::wav_utils::{
    LegPayloadTypeMap, PayloadTypeMap, build_payload_type_map, build_payload_type_map_by_leg,
    generate_wav_to_writer,
};
use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMediaStats, SipFlowMsgType};

/// Upper bound on simultaneously open FlowDB engines.
///
/// Each `Engine` owns file descriptors and spawns a background maintenance
/// thread, so we cannot keep every bucket's engine open forever. When this
/// limit is exceeded the least-recently-used engine is flushed and closed.
/// Engines still referenced by an in-flight query (via the cloned `Arc`)
/// survive until the query completes — only the cache entry is dropped.
const MAX_OPEN_ENGINES: usize = 24;

/// A cached engine entry, tagged with the last time it was touched so the LRU
/// evictor can pick the oldest victim.
struct CachedEngine {
    engine: Arc<Engine>,
    last_used: Instant,
}

pub struct FlowDbBackend {
    /// Root directory that all bucket sub-directories live under.
    base_dir: PathBuf,
    subdirs: SipFlowSubdirs,

    ttl_secs: Option<u64>,
    ttl_micros: Option<i64>,
    memtable_size_mb: usize,
    block_cache_capacity_mb: usize,

    /// LRU cache of open engines, keyed by absolute data directory.
    engines: Mutex<HashMap<PathBuf, CachedEngine>>,

    counter: AtomicU64,
    /// Per-bucket pending batches. The key matches the key used in `engines`,
    /// i.e. the absolute data directory of the bucket.
    batches: Mutex<HashMap<PathBuf, Vec<Record>>>,
    flush_count: usize,
    flush_interval: std::time::Duration,
    last_flush: Mutex<Instant>,
}

impl FlowDbBackend {
    pub fn new(
        base_dir: impl Into<PathBuf>,
        subdirs: SipFlowSubdirs,
        ttl_secs: Option<u64>,
        memtable_size_mb: usize,
        block_cache_capacity_mb: usize,
        flush_count: usize,
        flush_interval_secs: u64,
    ) -> Result<Self> {
        let base_dir = base_dir.into();
        std::fs::create_dir_all(&base_dir)?;

        let ttl_micros = ttl_secs.map(|s| s as i64 * 1_000_000);

        Ok(Self {
            base_dir,
            subdirs,
            ttl_secs,
            ttl_micros,
            memtable_size_mb,
            block_cache_capacity_mb,
            engines: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            batches: Mutex::new(HashMap::new()),
            flush_count,
            flush_interval: std::time::Duration::from_secs(flush_interval_secs),
            last_flush: Mutex::new(Instant::now()),
        })
    }

    /// Sub-directory name (relative to `base_dir`) for the given local time.
    /// Returns an empty string for `SipFlowSubdirs::None`, meaning data lives
    /// directly in `base_dir`.
    fn subdir_name_for_dt(&self, dt: DateTime<Local>) -> String {
        match self.subdirs {
            SipFlowSubdirs::None => String::new(),
            SipFlowSubdirs::Daily => format!("{:04}{:02}{:02}", dt.year(), dt.month(), dt.day()),
            SipFlowSubdirs::Hourly => format!(
                "{:04}{:02}{:02}/{:02}",
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour()
            ),
        }
    }

    /// Absolute data directory for the bucket containing `dt`.
    fn bucket_path_for_dt(&self, dt: DateTime<Local>) -> PathBuf {
        let subdir = self.subdir_name_for_dt(dt);
        if subdir.is_empty() {
            self.base_dir.clone()
        } else {
            self.base_dir.join(subdir)
        }
    }

    /// All bucket directories that could contain data in `[start, end]`.
    ///
    /// Discovery is filesystem-based: bucket directories named for one day
    /// may contain data timestamped on other days, so out-of-range buckets
    /// are also returned (after the in-range ones). Callers must skip
    /// buckets that do not actually contain FlowDB data (see
    /// [`has_flowdb_data`]) — a bucket may belong to the SQLite engine.
    fn bucket_paths_in_range(&self, start: DateTime<Local>, end: DateTime<Local>) -> Vec<PathBuf> {
        crate::sipflow::storage::discover_data_dirs(&self.base_dir, &self.subdirs, start, end)
    }

    /// Open a brand-new Engine at `path`. Caller must ensure the parent
    /// directory exists (this function creates `path` itself).
    fn open_engine_at(path: &PathBuf, subdirs_cfg: &SubdirsTuning) -> Result<Arc<Engine>> {
        std::fs::create_dir_all(path)?;
        let config = FlowDbConfig {
            data_dir: path.clone(),
            default_ttl_secs: subdirs_cfg.ttl_secs,
            memtable_size_mb: subdirs_cfg.memtable_size_mb,
            block_cache_capacity_mb: subdirs_cfg.block_cache_capacity_mb,
            auto_background: true,
            ..Default::default()
        };
        let engine = Engine::open(config)?;
        Ok(Arc::new(engine))
    }

    /// Get-or-open the engine for `path`, updating its LRU stamp.
    ///
    /// When the cache exceeds [`MAX_OPEN_ENGINES`], the least-recently-used
    /// engine is flushed (so its batch is durable) and removed from the
    /// cache. If the bucket still has a pending batch the engine is kept
    /// alive — we never evict an engine that has unflushed data.
    fn engine_for_bucket(&self, path: &PathBuf) -> Result<Arc<Engine>> {
        // Fast path: cache hit.
        {
            let mut engines = self.engines.lock().unwrap();
            if let Some(entry) = engines.get_mut(path) {
                entry.last_used = Instant::now();
                return Ok(entry.engine.clone());
            }
        }

        // Slow path: open a new engine outside the lock to avoid blocking
        // other callers on directory creation / WAL replay.
        let tuning = SubdirsTuning {
            ttl_secs: self.ttl_secs,
            memtable_size_mb: self.memtable_size_mb,
            block_cache_capacity_mb: self.block_cache_capacity_mb,
        };
        let new_engine = Self::open_engine_at(path, &tuning)?;

        let evicted = {
            let mut engines = self.engines.lock().unwrap();
            // Re-check: another thread may have raced us.
            if let Some(entry) = engines.get_mut(path) {
                entry.last_used = Instant::now();
                return Ok(entry.engine.clone());
            }
            engines.insert(
                path.clone(),
                CachedEngine {
                    engine: new_engine.clone(),
                    last_used: Instant::now(),
                },
            );

            // Evict if over capacity.
            if engines.len() > MAX_OPEN_ENGINES {
                let batches = self.batches.lock().unwrap();
                // Pick the LRU engine whose bucket has no pending batch.
                let victim = engines
                    .iter()
                    .filter(|(p, _)| p.as_path() != path.as_path() && !batches.contains_key(*p))
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(p, _)| p.clone());
                drop(batches);
                if let Some(victim_path) = victim {
                    if let Some(entry) = engines.remove(&victim_path) {
                        drop(engines);
                        // Close outside the lock; ignore errors — data is
                        // still recoverable from the WAL on next open.
                        let _ = entry.engine.close();
                        Some(entry)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };

        let _ = evicted; // already closed above
        Ok(new_engine)
    }

    fn next_counter(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    fn make_expire_at(&self, ts: i64) -> i64 {
        match self.ttl_micros {
            Some(ttl) => ts + ttl,
            None => i64::MAX,
        }
    }

    /// Total number of records waiting across all buckets.
    fn total_pending(&self) -> usize {
        self.batches.lock().unwrap().values().map(|v| v.len()).sum()
    }

    fn should_flush(&self) -> bool {
        let pending = self.total_pending();
        if pending >= self.flush_count {
            return true;
        }
        if pending > 0 && self.last_flush.lock().unwrap().elapsed() >= self.flush_interval {
            return true;
        }
        false
    }

    /// Flush every pending batch to its bucket's engine.
    ///
    /// Lock order: `batches` is drained and released *before* we touch the
    /// `engines` cache, so there is no risk of deadlock with
    /// [`engine_for_bucket`].
    fn flush_all_batches(&self) -> Result<()> {
        let drained: HashMap<PathBuf, Vec<Record>> = {
            let mut batches = self.batches.lock().unwrap();
            if batches.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *batches)
        };

        for (path, mut records) in drained {
            if records.is_empty() {
                continue;
            }
            // Flush even if the bucket directory has been removed from disk
            // — engine_for_bucket recreates it. If the engine cannot be
            // opened, re-queue the records so they are not lost.
            match self.engine_for_bucket(&path) {
                Ok(engine) => {
                    if let Err(e) = engine.write_batch_sync(records) {
                        tracing::warn!(
                            "flowdb write_batch_sync failed for {}: {e}",
                            path.display()
                        );
                        self.batches.lock().unwrap().insert(path, vec![]);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "flowdb engine open failed for {}: {e}; re-queueing batch",
                        path.display()
                    );
                    records.clear();
                    self.batches.lock().unwrap().insert(path, records);
                }
            }
        }
        *self.last_flush.lock().unwrap() = Instant::now();
        Ok(())
    }

    fn scan_sip_flow_in_range(&self, start_ts: i64, end_ts: i64) -> Result<Vec<SipFlowItem>> {
        let buckets = self
            .bucket_paths_in_range(datetime_from_micros(start_ts), datetime_from_micros(end_ts));

        let mut items = Vec::new();
        for path in buckets {
            if !has_flowdb_data(&path) {
                continue;
            }
            let engine = match self.engine_for_bucket(&path) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        "flowdb: skipping bucket {} during sip scan: {e}",
                        path.display()
                    );
                    continue;
                }
            };
            let iter = engine.scan(ScanRange::prefix_time_range(SIP_PREFIX, start_ts, end_ts))?;
            for result in iter {
                let Ok(rec) = result else {
                    continue;
                };
                let Ok((src, dst, payload)) = decode_sip_value(&rec.value) else {
                    continue;
                };
                items.push(SipFlowItem {
                    timestamp: rec.ts as u64,
                    seq: 0,
                    leg: None,
                    msg_type: SipFlowMsgType::Sip,
                    src_addr: src,
                    dst_addr: dst,
                    payload: Bytes::from(payload),
                });
            }
        }

        items.sort_by_key(|i| i.timestamp);
        Ok(items)
    }

    fn scan_rtp_packets(
        &self,
        call_id: &str,
        start_ts: i64,
        end_ts: i64,
        leg_filter: Option<i32>,
    ) -> Result<Vec<(i32, u64, Vec<u8>)>> {
        let prefix = match leg_filter {
            Some(leg) => rtp_call_leg_prefix(call_id, leg),
            None => rtp_call_prefix(call_id),
        };
        let buckets = self
            .bucket_paths_in_range(datetime_from_micros(start_ts), datetime_from_micros(end_ts));

        let mut packets = Vec::new();
        for path in buckets {
            if !has_flowdb_data(&path) {
                continue;
            }
            let engine = match self.engine_for_bucket(&path) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        "flowdb: skipping bucket {} during rtp scan: {e}",
                        path.display()
                    );
                    continue;
                }
            };
            let iter = engine.scan_prefix_time_range(&prefix, start_ts, end_ts)?;
            for result in iter {
                let Ok(rec) = result else {
                    continue;
                };
                let Ok((leg, _src, payload)) = decode_rtp_value(&rec.value) else {
                    continue;
                };
                packets.push((leg, rec.ts as u64, payload));
            }
        }

        packets.sort_by_key(|p| p.1);
        Ok(packets)
    }

    fn scan_media_sources(
        &self,
        call_id: &str,
        start_ts: i64,
        end_ts: i64,
        leg_filter: Option<i32>,
    ) -> Result<HashMap<i32, Vec<String>>> {
        let prefix = match leg_filter {
            Some(leg) => rtp_call_leg_prefix(call_id, leg),
            None => rtp_call_prefix(call_id),
        };
        let buckets = self
            .bucket_paths_in_range(datetime_from_micros(start_ts), datetime_from_micros(end_ts));

        let mut leg_sources: HashMap<i32, Vec<String>> = HashMap::new();
        let mut seen: std::collections::HashSet<(i32, String)> = std::collections::HashSet::new();

        for path in buckets {
            if !has_flowdb_data(&path) {
                continue;
            }
            let engine = match self.engine_for_bucket(&path) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let iter = engine.scan_prefix_time_range(&prefix, start_ts, end_ts)?;
            for result in iter {
                let Ok(rec) = result else {
                    continue;
                };
                let Ok((leg, src, _payload)) = decode_rtp_value(&rec.value) else {
                    continue;
                };
                if seen.insert((leg, src.clone())) {
                    leg_sources.entry(leg).or_default().push(src);
                }
            }
        }

        Ok(leg_sources)
    }

    fn build_payload_maps(
        &self,
        call_id: &str,
        start_ts: i64,
        end_ts: i64,
        leg_filter: Option<i32>,
    ) -> (PayloadTypeMap, LegPayloadTypeMap) {
        let leg_sources = self
            .scan_media_sources(call_id, start_ts, end_ts, leg_filter)
            .unwrap_or_default();
        let flow = self
            .scan_sip_flow_in_range(start_ts, end_ts)
            .unwrap_or_default();
        let payload_map = build_payload_type_map(&flow);
        let leg_payload_map = build_payload_type_map_by_leg(&flow, &leg_sources);
        (payload_map, leg_payload_map)
    }

    fn generate_wav(
        &self,
        call_id: &str,
        start_ts: i64,
        end_ts: i64,
        leg_filter: Option<i32>,
    ) -> Result<Vec<u8>> {
        let packets = self.scan_rtp_packets(call_id, start_ts, end_ts, leg_filter)?;
        if packets.is_empty() {
            return Ok(Vec::new());
        }
        let (payload_map, leg_payload_map) =
            self.build_payload_maps(call_id, start_ts, end_ts, leg_filter);
        let mut cursor = std::io::Cursor::new(Vec::new());
        generate_wav_to_writer(&packets, &payload_map, &leg_payload_map, true, &mut cursor)?;
        Ok(cursor.into_inner())
    }

    /// Flush every cached engine and clear the cache on drop.
    fn shutdown_cached_engines(&self) {
        let engines = std::mem::take(&mut *self.engines.lock().unwrap());
        for (_, entry) in engines {
            let _ = entry.engine.close();
        }
    }
}

/// Engine hint: whether `path` looks like a FlowDB bucket.
///
/// Bucket discovery is shared with the SQLite engine, so a candidate
/// directory may contain SQLite data (`sipflow.db` + `data.raw`) instead of
/// FlowDB data. Opening a FlowDB engine there would pollute the directory
/// with WAL/SST subdirectories, so query paths must check this first.
fn has_flowdb_data(path: &std::path::Path) -> bool {
    path.join("WAL").is_dir() || path.join("SST").is_dir()
}

/// Configuration bundle passed to `Engine::open` for a new bucket.
struct SubdirsTuning {
    ttl_secs: Option<u64>,
    memtable_size_mb: usize,
    block_cache_capacity_mb: usize,
}

/// Convert a microsecond timestamp into a `DateTime<Local>`.
///
/// Used to derive bucket directories from absolute packet timestamps during
/// range queries. Falls back to "now" if the timestamp is invalid for the
/// local time zone (extremely unlikely in practice).
fn datetime_from_micros(ts: i64) -> DateTime<Local> {
    chrono::TimeZone::timestamp_micros(&Local, ts)
        .single()
        .unwrap_or_else(Local::now)
}

impl Drop for FlowDbBackend {
    fn drop(&mut self) {
        // Best-effort: flush anything pending, then close all engines so
        // background maintenance threads exit and file handles are released.
        let _ = self.flush_all_batches();
        self.shutdown_cached_engines();
    }
}

#[async_trait]
impl SipFlowBackend for FlowDbBackend {
    fn record(&self, call_id: &str, item: SipFlowItem) -> Result<()> {
        if call_id.is_empty() {
            return Ok(());
        }

        let ts = item.timestamp as i64;
        let expire_at = self.make_expire_at(ts);
        let counter = self.next_counter();

        let record = match item.msg_type {
            SipFlowMsgType::Sip => {
                let key = make_sip_key(call_id, counter);
                let value = encode_sip_value(&item.src_addr, &item.dst_addr, &item.payload);
                Record {
                    key: key.into(),
                    ts,
                    expire_at,
                    value,
                }
            }
            SipFlowMsgType::Rtp => {
                let leg = item.leg.unwrap_or(0);
                let key = make_rtp_key(call_id, leg, counter);
                let value = encode_rtp_value(leg, &item.src_addr, &item.payload);
                Record {
                    key: key.into(),
                    ts,
                    expire_at,
                    value,
                }
            }
        };

        let bucket_path = self.bucket_path_for_dt(Local::now());
        self.batches
            .lock()
            .unwrap()
            .entry(bucket_path)
            .or_default()
            .push(record);

        if self.should_flush() {
            self.flush_all_batches()?;
        }
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        self.flush_all_batches()?;
        // Also flush each open engine's memtable so SSTs are durable.
        let engines: Vec<Arc<Engine>> = self
            .engines
            .lock()
            .unwrap()
            .values()
            .map(|e| e.engine.clone())
            .collect();
        for engine in engines {
            let _ = engine.flush();
        }
        Ok(())
    }

    async fn query_flow(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();
        let prefix = sip_call_prefix(call_id);
        let buckets = self.bucket_paths_in_range(start_time, end_time);

        let mut items = Vec::new();
        for path in buckets {
            if !has_flowdb_data(&path) {
                continue;
            }
            let engine = match self.engine_for_bucket(&path) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        "flowdb: skipping bucket {} during query_flow: {e}",
                        path.display()
                    );
                    continue;
                }
            };
            let iter = engine.scan_prefix_time_range(&prefix, start_ts, end_ts)?;
            for result in iter {
                let Ok(rec) = result else {
                    continue;
                };
                let Ok((src, dst, payload)) = decode_sip_value(&rec.value) else {
                    continue;
                };
                items.push(SipFlowItem {
                    timestamp: rec.ts as u64,
                    seq: 0,
                    leg: None,
                    msg_type: SipFlowMsgType::Sip,
                    src_addr: src,
                    dst_addr: dst,
                    payload: Bytes::from(payload),
                });
            }
        }

        items.sort_by_key(|i| i.timestamp);
        Ok(items)
    }

    async fn query_media_stats(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowMediaStats>> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();
        let prefix = rtp_call_prefix(call_id);
        let buckets = self.bucket_paths_in_range(start_time, end_time);

        let mut accumulators: HashMap<(i32, String, Option<u32>), MediaStatsAccumulator> =
            HashMap::new();

        for path in buckets {
            if !has_flowdb_data(&path) {
                continue;
            }
            let engine = match self.engine_for_bucket(&path) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let iter = engine.scan_prefix_time_range(&prefix, start_ts, end_ts)?;
            for result in iter {
                let Ok(rec) = result else {
                    continue;
                };
                let Ok((leg, src, payload)) = decode_rtp_value(&rec.value) else {
                    continue;
                };
                let header = parse_rtp_stats_header(&payload);
                let ssrc = header.map(|h| h.ssrc);
                let key = (leg, src.clone(), ssrc);

                accumulators
                    .entry(key)
                    .or_insert_with(|| MediaStatsAccumulator::new(leg, src, ssrc))
                    .observe(rec.ts as u64, header);
            }
        }

        Ok(accumulators
            .into_values()
            .map(|acc| acc.into_stats())
            .collect())
    }

    async fn query_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();
        self.generate_wav(call_id, start_ts, end_ts, None)
    }

    async fn query_media_stream(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<Vec<u8>> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();
        self.generate_wav(call_id, start_ts, end_ts, stream_leg)
    }

    async fn generate_wav_file(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<tempfile::NamedTempFile> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();

        let packets = self.scan_rtp_packets(call_id, start_ts, end_ts, stream_leg)?;
        if packets.is_empty() {
            return Err(anyhow::anyhow!("No media packets found"));
        }

        let (payload_map, leg_payload_map) =
            self.build_payload_maps(call_id, start_ts, end_ts, stream_leg);

        let mut file = tempfile::NamedTempFile::new()?;
        generate_wav_to_writer(&packets, &payload_map, &leg_payload_map, true, &mut file)?;
        std::io::Write::flush(&mut file)?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local_dt_from_micros(ts: i64) -> DateTime<Local> {
        Local
            .timestamp_micros(ts)
            .single()
            .expect("valid local datetime")
    }

    fn make_sip_item(ts_micros: u64, call_id: &str) -> SipFlowItem {
        let payload = format!(
            "INVITE sip:test@example.com SIP/2.0\r\nCall-ID: {}\r\n",
            call_id
        );
        SipFlowItem {
            timestamp: ts_micros,
            seq: 0,
            leg: None,
            msg_type: SipFlowMsgType::Sip,
            src_addr: "127.0.0.1:5060".to_string(),
            dst_addr: "127.0.0.2:5060".to_string(),
            payload: Bytes::from(payload),
        }
    }

    fn make_rtp_item(ts_micros: u64, leg: i32, src: &str) -> SipFlowItem {
        let mut payload = vec![0x80u8, 0x00]; // RTP v2, PCMU
        payload.extend_from_slice(&1000u16.to_be_bytes()); // sequence
        payload.extend_from_slice(&160u32.to_be_bytes()); // timestamp
        payload.extend_from_slice(&1u32.to_be_bytes()); // ssrc
        payload.extend_from_slice(&vec![0x7Fu8; 160]); // PCMU payload

        SipFlowItem {
            timestamp: ts_micros,
            seq: 0,
            leg: Some(leg),
            msg_type: SipFlowMsgType::Rtp,
            src_addr: src.to_string(),
            dst_addr: String::new(),
            payload: Bytes::from(payload),
        }
    }

    fn make_rtp_item_with_seq(ts_micros: u64, leg: i32, src: &str, seq: u16) -> SipFlowItem {
        let mut payload = vec![0x80u8, 0x00]; // RTP v2, PCMU
        payload.extend_from_slice(&seq.to_be_bytes()); // sequence
        payload.extend_from_slice(&160u32.to_be_bytes()); // timestamp
        payload.extend_from_slice(&1u32.to_be_bytes()); // ssrc
        payload.extend_from_slice(&vec![0x7Fu8; 160]); // PCMU payload

        SipFlowItem {
            timestamp: ts_micros,
            seq: 0,
            leg: Some(leg),
            msg_type: SipFlowMsgType::Rtp,
            src_addr: src.to_string(),
            dst_addr: String::new(),
            payload: Bytes::from(payload),
        }
    }

    #[tokio::test]
    async fn test_flowdb_record_and_query_flow() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
        let call_id = "test-flow-1";
        let base = chrono::Utc::now().timestamp_micros();
        let t0 = (base + 1_000) as u64;
        let t1 = (base + 2_000) as u64;
        let t2 = (base + 3_000) as u64;

        backend.record(call_id, make_sip_item(t0, call_id)).unwrap();
        backend.record(call_id, make_sip_item(t1, call_id)).unwrap();
        backend.record(call_id, make_sip_item(t2, call_id)).unwrap();
        backend.flush().await.unwrap();

        let items = backend
            .query_flow(
                call_id,
                local_dt_from_micros(base),
                local_dt_from_micros(base + 10_000),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].timestamp, t0);
        assert_eq!(items[1].timestamp, t1);
        assert_eq!(items[2].timestamp, t2);
        assert!(items[0].payload.starts_with(b"INVITE"));
    }

    #[tokio::test]
    async fn test_flowdb_query_flow_time_range() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
        let call_id = "test-flow-range";
        let base = chrono::Utc::now().timestamp_micros();
        let t0 = (base + 1_000) as u64;
        let t1 = (base + 2_000) as u64;
        let t2 = (base + 3_000) as u64;

        backend.record(call_id, make_sip_item(t0, call_id)).unwrap();
        backend.record(call_id, make_sip_item(t1, call_id)).unwrap();
        backend.record(call_id, make_sip_item(t2, call_id)).unwrap();
        backend.flush().await.unwrap();

        let items = backend
            .query_flow(
                call_id,
                local_dt_from_micros(t1 as i64),
                local_dt_from_micros(t1 as i64),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].timestamp, t1);
    }

    #[tokio::test]
    async fn test_flowdb_record_rtp_and_query_media() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
        let call_id = "test-rtp-1";
        let base = chrono::Utc::now().timestamp_micros();

        for i in 0..5u64 {
            let ts = (base + i as i64 * 20_000) as u64;
            backend
                .record(call_id, make_rtp_item(ts, 0, "127.0.0.1:4000"))
                .unwrap();
        }
        backend.flush().await.unwrap();

        let wav = backend
            .query_media(
                call_id,
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 200_000),
            )
            .await
            .unwrap();

        assert!(!wav.is_empty());
        assert_eq!(&wav[..4], b"RIFF");
    }

    #[tokio::test]
    async fn test_flowdb_query_media_stats() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
        let call_id = "test-stats-1";
        let base = chrono::Utc::now().timestamp_micros();

        let mut seq = 1000u16;
        for i in 0..10u64 {
            let ts = (base + i as i64 * 20_000) as u64;
            let item = make_rtp_item_with_seq(ts, 0, "127.0.0.1:4000", seq);
            seq = seq.wrapping_add(1);
            backend.record(call_id, item).unwrap();
        }
        backend.flush().await.unwrap();

        let stats = backend
            .query_media_stats(
                call_id,
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 500_000),
            )
            .await
            .unwrap();

        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].leg, 0);
        assert_eq!(stats[0].packet_count, 10);
        assert_eq!(stats[0].lost_packets, 0);
    }

    #[tokio::test]
    async fn test_flowdb_query_media_stats_with_loss() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
        let call_id = "test-stats-loss";
        let base = chrono::Utc::now().timestamp_micros();

        // Write packets with a gap: seq 1000, then 1002 (skip 1001)
        for (i, seq) in [1000u16, 1002u16].iter().enumerate() {
            let ts = (base + i as i64 * 20_000) as u64;
            let item = make_rtp_item_with_seq(ts, 0, "127.0.0.1:4000", *seq);
            backend.record(call_id, item).unwrap();
        }
        backend.flush().await.unwrap();

        let stats = backend
            .query_media_stats(
                call_id,
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 500_000),
            )
            .await
            .unwrap();

        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].packet_count, 2);
        assert_eq!(stats[0].lost_packets, 1);
        assert_eq!(stats[0].expected_packets, 3);
    }

    #[tokio::test]
    async fn test_flowdb_media_stream_leg_filter() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
        let call_id = "test-leg-filter";
        let base = chrono::Utc::now().timestamp_micros();

        // Write to leg 0 and leg 1
        for i in 0..3u64 {
            let ts = (base + i as i64 * 20_000) as u64;
            backend
                .record(call_id, make_rtp_item(ts, 0, "127.0.0.1:4000"))
                .unwrap();
            backend
                .record(call_id, make_rtp_item(ts, 1, "127.0.0.1:4002"))
                .unwrap();
        }
        backend.flush().await.unwrap();

        // Query only leg 1
        let wav = backend
            .query_media_stream(
                call_id,
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 200_000),
                Some(1),
            )
            .await
            .unwrap();

        assert!(!wav.is_empty());
        assert_eq!(&wav[..4], b"RIFF");
    }

    #[tokio::test]
    async fn test_flowdb_empty_query() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
        let base = chrono::Utc::now().timestamp_micros();

        let items = backend
            .query_flow(
                "nonexistent",
                local_dt_from_micros(base),
                local_dt_from_micros(base + 100_000),
            )
            .await
            .unwrap();

        assert!(items.is_empty());

        let wav = backend
            .query_media(
                "nonexistent",
                local_dt_from_micros(base),
                local_dt_from_micros(base + 100_000),
            )
            .await
            .unwrap();

        assert!(wav.is_empty());
    }

    #[tokio::test]
    async fn test_flowdb_isolation_between_calls() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
        let base = chrono::Utc::now().timestamp_micros();

        backend
            .record("call-a", make_sip_item(base as u64, "call-a"))
            .unwrap();
        backend
            .record("call-b", make_sip_item(base as u64, "call-b"))
            .unwrap();
        backend.flush().await.unwrap();

        let items_a = backend
            .query_flow(
                "call-a",
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 1),
            )
            .await
            .unwrap();
        let items_b = backend
            .query_flow(
                "call-b",
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 1),
            )
            .await
            .unwrap();

        assert_eq!(items_a.len(), 1);
        assert_eq!(items_b.len(), 1);
        assert!(String::from_utf8_lossy(&items_a[0].payload).contains("call-a"));
        assert!(String::from_utf8_lossy(&items_b[0].payload).contains("call-b"));
    }

    #[tokio::test]
    async fn test_flowdb_flush_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
        backend.flush().await.unwrap();
    }

    #[tokio::test]
    async fn test_flowdb_recovery_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let call_id = "test-recovery";
        let base = chrono::Utc::now().timestamp_micros();
        let path = dir.path().to_path_buf();

        {
            let backend =
                FlowDbBackend::new(&path, SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
            backend
                .record(call_id, make_sip_item(base as u64, call_id))
                .unwrap();
            backend.flush().await.unwrap();
        }

        {
            let backend =
                FlowDbBackend::new(&path, SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();
            let items = backend
                .query_flow(
                    call_id,
                    local_dt_from_micros(base - 1),
                    local_dt_from_micros(base + 1),
                )
                .await
                .unwrap();
            assert_eq!(items.len(), 1);
        }
    }

    #[tokio::test]
    async fn test_flowdb_skip_empty_call_id() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::None, None, 1, 16, 1000, 5).unwrap();

        // Empty call_id should be silently skipped
        backend.record("", make_sip_item(1000, "")).unwrap();
        backend.flush().await.unwrap();
    }

    /// Records routed by `Local::now()` should land in the bucket matching
    /// the current local hour when `subdirs = Hourly`.
    #[tokio::test]
    async fn test_flowdb_subdirs_hourly_writes_to_current_hour_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::Hourly, None, 1, 16, 1000, 5).unwrap();

        let now = Local::now();
        let call_id = "subdirs-hourly";
        let ts = chrono::Utc::now().timestamp_micros() as u64;
        backend.record(call_id, make_sip_item(ts, call_id)).unwrap();
        backend.flush().await.unwrap();

        let expected_subdir = format!(
            "{:04}{:02}{:02}/{:02}",
            now.year(),
            now.month(),
            now.day(),
            now.hour()
        );
        let bucket = dir.path().join(&expected_subdir);
        assert!(
            bucket.exists(),
            "expected hourly bucket {} to exist",
            bucket.display()
        );
        // Engine creates WAL/SST/INDEX subdirs.
        assert!(bucket.join("WAL").exists());
        assert!(bucket.join("SST").exists());
    }

    /// A query spanning multiple hourly buckets should aggregate results from
    /// all of them. We simulate this by writing to "now" and querying a wide
    /// time window that mathematically covers several hourly buckets.
    #[tokio::test]
    async fn test_flowdb_subdirs_hourly_query_aggregates_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::Hourly, None, 1, 16, 1000, 5).unwrap();

        let call_id = "subdirs-hourly-multi";
        let base = chrono::Utc::now().timestamp_micros();
        // Write several SIP messages "now".
        for i in 0..3u64 {
            let ts = (base + i as i64) as u64;
            backend.record(call_id, make_sip_item(ts, call_id)).unwrap();
        }
        backend.flush().await.unwrap();

        // Query a wide range that covers the last 25 hours — even though
        // only the current hour bucket exists, the query must still return
        // the items we wrote.
        let items = backend
            .query_flow(
                call_id,
                local_dt_from_micros(base - 25 * 3_600_000_000),
                local_dt_from_micros(base + 3_600_000_000),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 3, "all three SIP messages must be found");
    }

    /// `subdirs = Daily` should keep buckets separated by day, but a single
    /// day's query still returns everything recorded that day.
    #[tokio::test]
    async fn test_flowdb_subdirs_daily_query_same_day() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::Daily, None, 1, 16, 1000, 5).unwrap();

        let call_id = "subdirs-daily";
        let base = chrono::Utc::now().timestamp_micros();
        backend
            .record(call_id, make_sip_item(base as u64, call_id))
            .unwrap();
        backend.flush().await.unwrap();

        let items = backend
            .query_flow(
                call_id,
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 1),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 1);
        assert!(items[0].payload.starts_with(b"INVITE"));
    }

    /// When `subdirs = Daily`, queries for a different day must not return
    /// data written today (no leakage across buckets).
    #[tokio::test]
    async fn test_flowdb_subdirs_daily_no_leakage_into_other_day_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            FlowDbBackend::new(dir.path(), SipFlowSubdirs::Daily, None, 1, 16, 1000, 5).unwrap();

        let call_id = "subdirs-daily-no-leak";
        let now = Local::now();
        let base = chrono::Utc::now().timestamp_micros();
        backend
            .record(call_id, make_sip_item(base as u64, call_id))
            .unwrap();
        backend.flush().await.unwrap();

        // Query a day far in the past — its bucket does not exist, so we
        // should get nothing back even though the call_id matches.
        let far_past = now
            .checked_sub_signed(chrono::Duration::days(365))
            .unwrap_or(now);
        let far_past_end = far_past
            .checked_add_signed(chrono::Duration::hours(1))
            .unwrap_or(far_past);
        let items = backend
            .query_flow(call_id, far_past, far_past_end)
            .await
            .unwrap();

        assert!(items.is_empty(), "no leakage from today into past bucket");
    }

    /// Reopening the same backend should recover data written previously,
    /// even with `subdirs = Daily`.
    #[tokio::test]
    async fn test_flowdb_subdirs_daily_recovery_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let call_id = "subdirs-daily-recovery";
        let base = chrono::Utc::now().timestamp_micros();

        {
            let backend =
                FlowDbBackend::new(&path, SipFlowSubdirs::Daily, None, 1, 16, 1000, 5).unwrap();
            backend
                .record(call_id, make_sip_item(base as u64, call_id))
                .unwrap();
            backend.flush().await.unwrap();
        }

        {
            let backend =
                FlowDbBackend::new(&path, SipFlowSubdirs::Daily, None, 1, 16, 1000, 5).unwrap();
            let items = backend
                .query_flow(
                    call_id,
                    local_dt_from_micros(base - 1),
                    local_dt_from_micros(base + 1),
                )
                .await
                .unwrap();
            assert_eq!(items.len(), 1);
        }
    }
}
