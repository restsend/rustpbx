use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::backend::SipFlowBackend;
use crate::config::SipFlowSubdirs;
use crate::flusher::SipFlowFlusher;
use crate::perf::{PerfCounters, PerfDumper};
use crate::protocol::{MsgType, Packet};
use crate::shard::{MODE_MULTI, RouterState};
use crate::storage::{
    StorageManager, extract_callid, process_packet_with, process_packet_with_addr,
};
use crate::wav_utils::generate_wav_to_writer_with_rate;
use crate::{SipFlowItem, SipFlowMediaStats, SipFlowMsgType};

/// How long `record()` blocks on a full worker channel before dropping the
/// record (bounded backpressure: never grow memory without limit, never drop
/// under brief overload).
const BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(1);

/// Capacity for the bounded record→worker channel per shard.
fn command_capacity(flush_count: usize) -> usize {
    flush_count.max(1024)
}

/// Send a command with bounded backpressure (policy A): block the calling
/// thread with short parks while the worker channel is full, up to `timeout`;
/// if it is still full after that, drop the record and count it rather than
/// grow memory without limit.
fn send_command(tx: &mpsc::Sender<Command>, cmd: Command, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut cmd = cmd;
    loop {
        match tx.try_send(cmd) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(c)) => {
                if Instant::now() >= deadline {
                    metrics::counter!(
                        "sipflow_record_backpressure_dropped_total",
                        "component" => "sipflow"
                    )
                    .increment(1);
                    tracing::warn!(
                        "sipflow record: worker channel full, dropped a record after {timeout:?}"
                    );
                    return Ok(());
                }
                std::thread::park_timeout(Duration::from_micros(100));
                cmd = c;
            }
            Err(TrySendError::Closed(_)) => {
                return Err(anyhow::anyhow!("worker channel closed"));
            }
        }
    }
}

enum Command {
    RecordItem {
        call_id: String,
        item: SipFlowItem,
    },
    RecordPacket {
        packet: Packet,
    },
    Flush {
        done: tokio::sync::oneshot::Sender<()>,
    },
}

/// Local (embedded) backend that runs sipflow storage in background tasks
/// with dedicated OS threads for SQLite writes.
///
/// With `shards > 1` the backend runs `shards` independent worker+flusher
/// pipelines; records are routed to a shard by FNV-1a hash of the call_id.
/// Each bucket's layout is decided by its on-disk state (see `shard::detect_bucket_layout`):
/// new directories and directories with `shard-*` subdirs are written
/// sharded, legacy single-file buckets are written single-threaded until the
/// next bucket rotation.
pub struct LocalBackend {
    senders: Vec<mpsc::Sender<Command>>,
    router: Arc<RouterState>,
    root: String,
    subdirs: SipFlowSubdirs,
    cancel_token: CancellationToken,
    _flushers: Vec<SipFlowFlusher>,
    force_pcm: bool,
    pcm_sample_rate: u32,
}

impl LocalBackend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: String,
        subdirs: SipFlowSubdirs,
        flush_count: usize,
        flush_interval_secs: u64,
        id_cache_size: usize,
        compress: Option<u32>,
        shards: usize,
        force_pcm: bool,
        pcm_sample_rate: u32,
    ) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        let shards = shards.max(1);

        // Initial routing mode mirrors the active bucket's on-disk layout so a
        // mid-day upgrade keeps writing today's legacy bucket single-threaded
        // until the next rotation.
        let router = Arc::new(RouterState::new(MODE_MULTI, shards));
        {
            let now = Local::now();
            let subdir = crate::shard::bucket_subdir(&subdirs, now);
            let active = PathBuf::from(&root).join(&subdir);
            let _ = router.current_layout(&subdir, &active);
        }

        let cancel_token = CancellationToken::new();
        let mut senders = Vec::with_capacity(shards);
        let mut flushers = Vec::with_capacity(shards);

        for shard in 0..shards {
            // Spawn the dedicated SQLite flush thread for this shard.
            let flusher = SipFlowFlusher::new(flush_count, flush_interval_secs, id_cache_size);
            let flusher_tx = flusher.sender();
            let dropped = flusher.dropped_count();

            let (tx, mut rx) = mpsc::channel::<Command>(command_capacity(flush_count));
            let cancel_token_clone = cancel_token.clone();
            let root_clone = root.clone();
            let subdirs_clone = subdirs.clone();
            let router_clone = router.clone();

            // Spawn background worker task (handles packet processing + raw file write)
            let perf = PerfCounters::new_arc();
            let perf_dumper = perf.clone();
            tokio::spawn(async move {
                let mut storage = StorageManager::new(
                    &PathBuf::from(&root_clone),
                    subdirs_clone,
                    Some(flusher_tx),
                    Some(dropped),
                    shard,
                    shards,
                    Some(router_clone),
                );

                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                let mut dumper = PerfDumper::new(perf_dumper);

                loop {
                    tokio::select! {
                        _ = cancel_token_clone.cancelled() => {
                            let _ = storage.force_flush().await;
                            break;
                        }
                        Some(cmd) = rx.recv() => {
                            match cmd {
                                Command::RecordItem { call_id, item } => {
                                    perf.items_recorded.fetch_add(1, Ordering::Relaxed);
                                    let default_port = if matches!(&item.msg_type, SipFlowMsgType::Sip)
                                    {
                                        5060
                                    } else {
                                        0
                                    };
                                    // item already carries "ip:port" strings — reuse them
                                    // directly instead of parsing to (IpAddr, u16) and
                                    // reformatting (String → IpAddr → String round-trip).
                                    let src_addr = if !item.src_addr.is_empty() && item.src_addr != "synth" {
                                        item.src_addr
                                    } else {
                                        format!("0.0.0.0:{default_port}")
                                    };
                                    let dst_addr = if !item.dst_addr.is_empty() && item.dst_addr != "synth" {
                                        item.dst_addr
                                    } else {
                                        format!("0.0.0.0:{default_port}")
                                    };
                                    let msg_type = match item.msg_type {
                                        SipFlowMsgType::Sip => MsgType::Sip,
                                        SipFlowMsgType::Rtp => MsgType::Rtp,
                                    };
                                    let packet_call_id = if call_id.is_empty() {
                                        None
                                    } else {
                                        Some(call_id)
                                    };
                                    let packet_leg = if msg_type == MsgType::Rtp {
                                        item.leg
                                    } else {
                                        None
                                    };

                                    let processed = process_packet_with_addr(
                                        msg_type,
                                        src_addr,
                                        dst_addr,
                                        item.timestamp,
                                        packet_call_id,
                                        packet_leg,
                                        item.payload,
                                        compress,
                                    );
                                    let _ = storage.write_processed(processed).await;
                                }
                                Command::RecordPacket { packet } => {
                                    perf.items_recorded.fetch_add(1, Ordering::Relaxed);
                                    let processed = process_packet_with(packet, compress);
                                    let _ = storage.write_processed(processed).await;
                                }
                                Command::Flush { done } => {
                                    let wait_start = Instant::now();
                                    let _ = storage.force_flush().await;
                                    metrics::histogram!(
                                        "sipflow_force_flush_wait_seconds",
                                        "component" => "sipflow"
                                    )
                                    .record(wait_start.elapsed().as_secs_f64());
                                    perf.flushes.fetch_add(1, Ordering::Relaxed);
                                    perf.set_pending(storage.dropped() as i64);
                                    let _ = done.send(());
                                }
                            }
                        }
                        _ = interval.tick() => {
                            let _ = storage.check_flush().await;
                            metrics::gauge!("sipflow_worker_queue_depth", "component" => "sipflow")
                                .set(rx.len() as f64);
                            perf.set_pending(storage.dropped() as i64);
                            if let Some(msg) = dumper.try_dump() {
                                tracing::info!("{msg}");
                            }
                        }
                    }
                }
            });

            senders.push(tx);
            flushers.push(flusher);
        }

        Ok(Self {
            senders,
            router,
            root,
            subdirs,
            cancel_token,
            _flushers: flushers,
            force_pcm,
            pcm_sample_rate,
        })
    }

    /// Pick the shard pipeline for a call via the shared router: worker 0 while
    /// the active bucket is a legacy single-file bucket, otherwise hash-based.
    fn route(&self, call_id: &str) -> usize {
        self.router.route_index(call_id)
    }
}

#[async_trait]
impl SipFlowBackend for LocalBackend {
    fn kind(&self) -> &'static str {
        "local"
    }

    async fn flush(&self) -> Result<()> {
        let mut pending = Vec::new();
        for (i, tx) in self.senders.iter().enumerate() {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            if tx.send(Command::Flush { done: done_tx }).await.is_err() {
                warn!("SipFlowBackend flush: worker {i} channel closed, skipping");
                continue;
            }
            pending.push((i, done_rx));
        }
        if pending.is_empty() {
            return Ok(());
        }

        let total = Instant::now();
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let mut timed_out = false;
        for (i, rx) in pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                timed_out = true;
                break;
            }
            match tokio::time::timeout(remaining, rx).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    warn!("SipFlowBackend flush: worker {i} oneshot cancelled");
                }
                Err(_) => {
                    timed_out = true;
                    break;
                }
            }
        }
        metrics::histogram!("sipflow_flush_total_seconds", "component" => "sipflow")
            .record(total.elapsed().as_secs_f64());
        if timed_out {
            metrics::counter!("sipflow_flush_timeout_total", "component" => "sipflow").increment(1);
            tracing::error!("SipFlowBackend flush: timed out after 30s");
        }
        Ok(())
    }

    fn record(&self, call_id: Cow<'_, str>, item: SipFlowItem) -> Result<()> {
        let idx = self.route(&call_id);
        send_command(
            &self.senders[idx],
            Command::RecordItem {
                call_id: call_id.into_owned(),
                item,
            },
            BACKPRESSURE_TIMEOUT,
        )
    }

    fn record_packet(&self, packet: Packet) -> Result<()> {
        let call_id = packet.call_id.clone().or_else(|| {
            if packet.msg_type == MsgType::Sip {
                extract_callid(&packet.payload)
            } else {
                None
            }
        });
        let idx = match call_id.as_deref() {
            Some(cid) => self.route(cid),
            None => 0,
        };
        send_command(
            &self.senders[idx],
            Command::RecordPacket { packet },
            BACKPRESSURE_TIMEOUT,
        )
    }

    async fn query_flow(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>> {
        let call_id = call_id.to_string();
        let root = self.root.clone();
        let subdirs = self.subdirs.clone();

        let mut items = tokio::task::spawn(async move {
            let mut storage =
                StorageManager::new(&PathBuf::from(&root), subdirs, None, None, 0, 1, None);
            storage.query_flow(&call_id, start_time, end_time).await
        })
        .await??;

        items.sort_by_key(|i| i.timestamp);

        Ok(items)
    }

    async fn query_media_stats(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowMediaStats>> {
        let call_id = call_id.to_string();
        let root = self.root.clone();
        let subdirs = self.subdirs.clone();

        let stats = tokio::task::spawn(async move {
            let mut storage =
                StorageManager::new(&PathBuf::from(&root), subdirs, None, None, 0, 1, None);
            storage
                .query_media_stats(&call_id, start_time, end_time)
                .await
        })
        .await??;

        Ok(stats)
    }

    async fn query_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>> {
        let call_id = call_id.to_string();
        let root = self.root.clone();
        let subdirs = self.subdirs.clone();
        let force_pcm = self.force_pcm;
        let pcm_sample_rate = self.pcm_sample_rate;

        let result = tokio::task::spawn(async move {
            let mut storage =
                StorageManager::new(&PathBuf::from(&root), subdirs, None, None, 0, 1, None);
            let packets = storage.query_media(&call_id, start_time, end_time).await?;
            if packets.is_empty() {
                return Ok(Vec::<u8>::new());
            }
            let (payload_map, leg_payload_map) =
                build_payload_maps(&mut storage, &call_id, start_time, end_time).await;
            // CPU-bound WAV encoding must run on the blocking pool, never a
            // tokio worker, or per-call exports would stall the write path.
            tokio::task::spawn_blocking(move || {
                let mut cursor = std::io::Cursor::new(Vec::new());
                generate_wav_to_writer_with_rate(
                    &call_id,
                    &packets,
                    &payload_map,
                    &leg_payload_map,
                    force_pcm,
                    pcm_sample_rate,
                    false,
                    &mut cursor,
                )?;
                Ok::<Vec<u8>, anyhow::Error>(cursor.into_inner())
            })
            .await?
        })
        .await??;

        Ok(result)
    }

    async fn query_media_stream(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<Vec<u8>> {
        let call_id = call_id.to_string();
        let root = self.root.clone();
        let subdirs = self.subdirs.clone();
        let force_pcm = self.force_pcm;
        let pcm_sample_rate = self.pcm_sample_rate;

        let result = tokio::task::spawn(async move {
            let mut storage =
                StorageManager::new(&PathBuf::from(&root), subdirs, None, None, 0, 1, None);
            let mut packets = storage.query_media(&call_id, start_time, end_time).await?;
            if let Some(leg) = stream_leg {
                packets.retain(|(packet_leg, _, _)| *packet_leg == leg);
            }
            if packets.is_empty() {
                return Ok::<Vec<u8>, anyhow::Error>(Vec::new());
            }
            let (payload_map, leg_payload_map) = build_payload_maps_filtered(
                &mut storage,
                &call_id,
                start_time,
                end_time,
                stream_leg,
            )
            .await;
            tokio::task::spawn_blocking(move || {
                let mut cursor = std::io::Cursor::new(Vec::new());
                generate_wav_to_writer_with_rate(
                    &call_id,
                    &packets,
                    &payload_map,
                    &leg_payload_map,
                    force_pcm,
                    pcm_sample_rate,
                    false,
                    &mut cursor,
                )?;
                Ok::<Vec<u8>, anyhow::Error>(cursor.into_inner())
            })
            .await?
        })
        .await??;

        Ok(result)
    }

    async fn generate_wav_file(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<tempfile::NamedTempFile> {
        let call_id = call_id.to_string();
        let root = self.root.clone();
        let subdirs = self.subdirs.clone();
        let force_pcm = self.force_pcm;
        let pcm_sample_rate = self.pcm_sample_rate;

        let file = tokio::task::spawn(async move {
            let mut storage =
                StorageManager::new(&PathBuf::from(&root), subdirs, None, None, 0, 1, None);
            let mut packets = storage.query_media(&call_id, start_time, end_time).await?;
            if let Some(leg) = stream_leg {
                packets.retain(|(packet_leg, _, _)| *packet_leg == leg);
            }
            if packets.is_empty() {
                return Ok::<Option<tempfile::NamedTempFile>, anyhow::Error>(None);
            }
            let (payload_map, leg_payload_map) = build_payload_maps_filtered(
                &mut storage,
                &call_id,
                start_time,
                end_time,
                stream_leg,
            )
            .await;

            let file = tokio::task::spawn_blocking(move || {
                let mut file = tempfile::NamedTempFile::new()?;
                generate_wav_to_writer_with_rate(
                    &call_id,
                    &packets,
                    &payload_map,
                    &leg_payload_map,
                    force_pcm,
                    pcm_sample_rate,
                    false,
                    &mut file,
                )?;
                std::io::Write::flush(&mut file)?;
                Ok::<tempfile::NamedTempFile, anyhow::Error>(file)
            })
            .await??;
            Ok::<Option<tempfile::NamedTempFile>, anyhow::Error>(Some(file))
        })
        .await??
        .ok_or_else(|| anyhow::anyhow!("No media packets found"))?;

        Ok(file)
    }
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

async fn build_payload_maps(
    storage: &mut StorageManager,
    call_id: &str,
    start_time: DateTime<Local>,
    end_time: DateTime<Local>,
) -> (
    crate::wav_utils::PayloadTypeMap,
    crate::wav_utils::LegPayloadTypeMap,
) {
    use crate::wav_utils::{build_payload_type_map, build_payload_type_map_by_leg};
    let media_sources = storage
        .query_media_sources(call_id, start_time, end_time)
        .await
        .unwrap_or_default();
    let mut leg_sources = std::collections::HashMap::<i32, Vec<String>>::new();
    for source in media_sources {
        leg_sources.entry(source.leg).or_default().push(source.src);
    }
    let flow = storage
        .query_flow(call_id, start_time, end_time)
        .await
        .unwrap_or_default();
    let payload_map = build_payload_type_map(&flow);
    let leg_payload_map = build_payload_type_map_by_leg(&flow, &leg_sources);
    (payload_map, leg_payload_map)
}

type LegPayloadTypeMap = crate::wav_utils::LegPayloadTypeMap;

async fn build_payload_maps_filtered(
    storage: &mut StorageManager,
    call_id: &str,
    start_time: DateTime<Local>,
    end_time: DateTime<Local>,
    stream_leg: Option<i32>,
) -> (crate::wav_utils::PayloadTypeMap, LegPayloadTypeMap) {
    use crate::wav_utils::{build_payload_type_map, build_payload_type_map_by_leg};
    let media_sources = storage
        .query_media_sources(call_id, start_time, end_time)
        .await
        .unwrap_or_default();
    let mut leg_sources = std::collections::HashMap::<i32, Vec<String>>::new();
    for source in media_sources {
        if stream_leg.is_none_or(|selected| selected == source.leg) {
            leg_sources.entry(source.leg).or_default().push(source.src);
        }
    }
    let flow = storage
        .query_flow(call_id, start_time, end_time)
        .await
        .unwrap_or_default();
    let payload_map = build_payload_type_map(&flow);
    let leg_payload_map = build_payload_type_map_by_leg(&flow, &leg_sources);
    (payload_map, leg_payload_map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::MODE_SINGLE;
    use crate::storage::DEFAULT_COMPRESS_LEVEL;
    use std::net::IpAddr;

    fn local_dt_from_micros(ts_micros: i64) -> DateTime<Local> {
        chrono::TimeZone::timestamp_micros(&Local, ts_micros)
            .single()
            .expect("valid local datetime")
    }

    fn make_sip_item(ts_micros: u64, call_id: &str) -> SipFlowItem {
        let payload = format!("INVITE sip:test@example.com SIP/2.0\r\nCall-ID: {call_id}\r\n");
        SipFlowItem {
            timestamp: ts_micros,
            seq: 0,
            leg: None,
            msg_type: SipFlowMsgType::Sip,
            src_addr: "127.0.0.1:5060".into(),
            dst_addr: "127.0.0.2:5060".into(),
            payload: bytes::Bytes::from(payload),
        }
    }

    fn make_rtp_item(ts_micros: u64, leg: i32, seed: u8) -> SipFlowItem {
        let mut payload = vec![0x80u8, 0x08];
        payload.extend_from_slice(&seed.to_be_bytes());
        payload.extend_from_slice(&[0u8; 4]);
        payload.extend_from_slice(&[0u8; 4]);
        payload.extend_from_slice(&vec![seed; 160]);
        SipFlowItem {
            timestamp: ts_micros,
            seq: 0,
            leg: Some(leg),
            msg_type: SipFlowMsgType::Rtp,
            src_addr: "127.0.0.1:5004".into(),
            dst_addr: String::new(),
            payload: bytes::Bytes::from(payload),
        }
    }

    fn new_backend(root: &str, subdirs: SipFlowSubdirs, shards: usize) -> LocalBackend {
        LocalBackend::new(
            root.to_string(),
            subdirs,
            1000,
            3600,
            128,
            Some(DEFAULT_COMPRESS_LEVEL),
            shards,
            false,
            16000,
        )
        .unwrap()
    }

    /// Sharded backend: records must be routed across `shard-*` dirs and
    /// round-trip through query_flow / query_media_stats.
    #[tokio::test]
    async fn test_sharded_backend_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let backend = new_backend(dir.path().to_str().unwrap(), SipFlowSubdirs::None, 4);
        let base = chrono::Utc::now().timestamp_micros() as u64;

        for i in 0..40usize {
            let call_id = format!("sharded-call-{i:04}");
            for s in 0..2u64 {
                backend
                    .record(
                        Cow::Borrowed(&call_id),
                        make_sip_item(base + s * 1000, &call_id),
                    )
                    .unwrap();
            }
            for r in 0..50u64 {
                backend
                    .record(
                        Cow::Borrowed(&call_id),
                        make_rtp_item(base + 100_000 + r * 1000, (r % 2) as i32, r as u8),
                    )
                    .unwrap();
            }
        }
        backend.flush().await.unwrap();

        let shard_dirs: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("shard-"))
            .collect();
        assert_eq!(shard_dirs.len(), 4, "all 4 shard dirs must exist");

        let start = local_dt_from_micros(base as i64 - 1);
        let end = local_dt_from_micros(base as i64 + 1_000_000);
        for i in (0..40).step_by(7) {
            let call_id = format!("sharded-call-{i:04}");
            let flow = backend.query_flow(&call_id, start, end).await.unwrap();
            assert_eq!(flow.len(), 2, "call {call_id} SIP must round-trip");
            let stats = backend
                .query_media_stats(&call_id, start, end)
                .await
                .unwrap();
            let total: usize = stats.iter().map(|s| s.packet_count).sum();
            assert_eq!(total, 50, "call {call_id} RTP must round-trip");
        }
    }

    /// Mid-day upgrade: the active bucket already has legacy single-file data,
    /// so the new sharded backend must keep writing single-threaded (worker 0,
    /// bucket root) until the next rotation — no shard dirs appear.
    #[tokio::test]
    async fn test_upgrade_keeps_legacy_bucket_single() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        // Old-version bucket: legacy files for today's bucket.
        let today = crate::shard::active_bucket_dir(
            PathBuf::from(&root).as_path(),
            &SipFlowSubdirs::Daily,
            Local::now(),
        );
        std::fs::create_dir_all(&today).unwrap();
        std::fs::write(today.join("sipflow.db"), b"").unwrap();

        let backend = new_backend(&root, SipFlowSubdirs::Daily, 4);
        assert_eq!(
            backend.router.mode(),
            MODE_SINGLE,
            "active legacy bucket must start in single mode"
        );

        let call_id = "legacy-upgrade-call";
        let base = chrono::Utc::now().timestamp_micros() as u64;
        for r in 0..20u64 {
            backend
                .record(
                    Cow::Borrowed(&call_id),
                    make_rtp_item(base + r * 1000, (r % 2) as i32, r as u8),
                )
                .unwrap();
        }
        backend.flush().await.unwrap();

        let has_shards = std::fs::read_dir(&today)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("shard-"));
        assert!(
            !has_shards,
            "legacy bucket must not be sharded during upgrade"
        );
        assert!(today.join("data.raw").exists(), "legacy raw file written");

        let stats = backend
            .query_media_stats(
                call_id,
                local_dt_from_micros(base as i64 - 1),
                local_dt_from_micros(base as i64 + 1_000_000),
            )
            .await
            .unwrap();
        let total: usize = stats.iter().map(|s| s.packet_count).sum();
        assert_eq!(total, 20, "single-threaded legacy write must round-trip");
    }

    fn dummy_packet() -> Packet {
        use bytes::Bytes;
        Packet {
            msg_type: MsgType::Rtp,
            src: (IpAddr::from([127, 0, 0, 1]), 5000),
            dst: (IpAddr::from([127, 0, 0, 2]), 5002),
            timestamp: 0,
            call_id: Some("backpressure-test".to_string()),
            leg: Some(0),
            payload: Bytes::from(vec![0u8; 10]),
            client_id: 0,
        }
    }

    /// Bounded backpressure (policy A): when the worker channel is full and the
    /// timeout has elapsed, `send_command` must drop + return Ok instead of
    /// blocking forever or growing memory.
    #[test]
    fn test_send_command_drops_when_full() {
        let (tx, _rx) = mpsc::channel::<Command>(2);
        for _ in 0..2 {
            tx.try_send(Command::RecordPacket {
                packet: dummy_packet(),
            })
            .unwrap();
        }
        // A zero timeout means "never wait": the full channel must drop the
        // record and return Ok (not block).
        let result = send_command(
            &tx,
            Command::RecordPacket {
                packet: dummy_packet(),
            },
            Duration::ZERO,
        );
        assert!(
            result.is_ok(),
            "full channel with zero timeout must drop, not block"
        );
    }

    /// Backpressure must not lose records when the receiver drains promptly.
    #[test]
    fn test_send_command_succeeds_when_drained() {
        let (tx, mut rx) = mpsc::channel::<Command>(1);
        let result = send_command(
            &tx,
            Command::RecordPacket {
                packet: dummy_packet(),
            },
            Duration::from_millis(100),
        );
        assert!(result.is_ok());
        assert!(matches!(rx.try_recv(), Ok(Command::RecordPacket { .. })));
    }
}
