use crate::protocol::MsgType;
use anyhow::Result;
use lru::LruCache;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, QueryBuilder, Row, Sqlite, SqliteConnection, Transaction};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Max rows per multi-row INSERT statement.
///
/// 6 columns × 2000 rows = 12000 bound parameters, safely under SQLite's
/// `SQLITE_LIMIT_VARIABLE_NUMBER` (32766 since 3.32.0; sqlx bundles a recent
/// libsqlite3-sys). Lower for the `call_meta` upsert, which is 1 param/row.
const INSERT_CHUNK_ROWS: usize = 2000;

#[derive(Debug)]
pub(crate) struct FlushMeta {
    pub msg_type: MsgType,
    pub callid: Option<String>,
    pub src: String,
    pub dst: String,
    pub leg: Option<i32>,
    pub timestamp: u64,
    pub offset: u64,
    pub size: usize,
}

pub(crate) enum FlushCommand {
    Meta(FlushMeta),
    Flush {
        enqueued_at: Instant,
    },
    FlushSync {
        done: oneshot::Sender<()>,
        enqueued_at: Instant,
    },
    Rotate {
        db_path: PathBuf,
    },
}

pub(crate) struct SipFlowFlusher {
    sender: Option<mpsc::Sender<FlushCommand>>,
    cancel_token: CancellationToken,
    handle: Option<thread::JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}

/// How often the flusher emits a periodic queue-depth status log line.
const FLUSHER_STATUS_INTERVAL: Duration = Duration::from_secs(5);

/// Capacity for the bounded worker→flusher channel.
///
/// The flusher accumulates up to `flush_count` metas in its in-memory batch
/// before a DB write, so the channel must hold at least that many to keep it
/// busy; `* 2` lets one batch drain while the next accumulates. Bounded so an
/// overloaded flusher cannot grow memory without limit.
pub(crate) fn flusher_capacity(flush_count: usize) -> usize {
    flush_count.max(1000).saturating_mul(2)
}

impl SipFlowFlusher {
    pub(crate) fn new(
        flush_count: usize,
        flush_interval_secs: u64,
        id_cache_size: usize,
        shard: usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<FlushCommand>(flusher_capacity(flush_count));
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_clone = dropped.clone();
        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        let handle = thread::Builder::new()
            .name("sipflow-flusher".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .expect("failed to build flusher tokio runtime");
                rt.block_on(async move {
                    run(
                        rx,
                        flush_count,
                        flush_interval_secs,
                        id_cache_size,
                        dropped_clone,
                        cancel_clone,
                        shard,
                    )
                    .await;
                });
            })
            .expect("failed to spawn sipflow flusher thread");

        Self {
            sender: Some(tx),
            cancel_token,
            handle: Some(handle),
            dropped,
        }
    }

    pub(crate) fn sender(&self) -> mpsc::Sender<FlushCommand> {
        self.sender
            .as_ref()
            .expect("sipflow flusher sender taken")
            .clone()
    }

    pub(crate) fn dropped_count(&self) -> Arc<AtomicU64> {
        self.dropped.clone()
    }
}

impl Drop for SipFlowFlusher {
    fn drop(&mut self) {
        // Explicitly signal the flusher to drain + exit. Unlike relying on
        // channel disconnection, this works even while other sender clones
        // (e.g. the worker's StorageManager) are still alive.
        self.cancel_token.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

async fn run(
    mut rx: mpsc::Receiver<FlushCommand>,
    flush_count: usize,
    flush_interval_secs: u64,
    id_cache_size: usize,
    _dropped: Arc<AtomicU64>,
    cancel: CancellationToken,
    shard: usize,
) {
    let flush_interval = Duration::from_secs(flush_interval_secs);
    let mut db_conn: Option<SqliteConnection> = None;
    let mut db_path: Option<PathBuf> = None;
    let mut batch: Vec<FlushMeta> = Vec::new();
    let mut call_id_cache: LruCache<String, i32> =
        LruCache::new(NonZeroUsize::new(id_cache_size.max(1)).unwrap());
    let mut last_flush = Instant::now();
    let mut last_checkpoint = Instant::now();
    let mut sip_rows_written: u64 = 0;
    let mut media_rows_written: u64 = 0;
    // Periodic water-level status line; consume the first tick so the first
    // log is after one full interval.
    let mut status_interval = tokio::time::interval(FLUSHER_STATUS_INTERVAL);
    status_interval.tick().await;

    loop {
        metrics::gauge!("sipflow_flusher_queue_depth", "component" => "sipflow")
            .set(rx.len() as f64);

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Shutdown: drain everything still queued (e.g. a FlushSync
                // from a concurrently shutting-down worker) so no caller hangs,
                // then flush the final batch and exit.
                while let Ok(cmd) = rx.try_recv() {
                    handle_flush_command(
                        cmd,
                        &mut db_conn,
                        &mut batch,
                        &mut call_id_cache,
                        &mut db_path,
                        &mut sip_rows_written,
                        &mut media_rows_written,
                        flush_count,
                        flush_interval,
                        &mut last_flush,
                        &mut last_checkpoint,
                    )
                    .await;
                }
                if !batch.is_empty() {
                    flush_to_db(
                        &mut db_conn,
                        &mut batch,
                        &mut call_id_cache,
                        &db_path,
                        &mut sip_rows_written,
                        &mut media_rows_written,
                        &mut last_checkpoint,
                    )
                    .await;
                }
                break;
            }
            Some(cmd) = rx.recv() => {
                handle_flush_command(
                    cmd,
                    &mut db_conn,
                    &mut batch,
                    &mut call_id_cache,
                    &mut db_path,
                    &mut sip_rows_written,
                    &mut media_rows_written,
                    flush_count,
                    flush_interval,
                    &mut last_flush,
                    &mut last_checkpoint,
                )
                .await;
            }
            _ = status_interval.tick() => {
                tracing::trace!(
                    shard,
                    queue_depth = rx.len(),
                    batch_len = batch.len(),
                    "sipflow flusher status"
                );
            }
            else => {
                if !batch.is_empty() {
                    flush_to_db(
                        &mut db_conn,
                        &mut batch,
                        &mut call_id_cache,
                        &db_path,
                        &mut sip_rows_written,
                        &mut media_rows_written,
                        &mut last_checkpoint,
                    )
                    .await;
                }
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_flush_command(
    cmd: FlushCommand,
    db_conn: &mut Option<SqliteConnection>,
    batch: &mut Vec<FlushMeta>,
    call_id_cache: &mut LruCache<String, i32>,
    db_path: &mut Option<PathBuf>,
    sip_rows_written: &mut u64,
    media_rows_written: &mut u64,
    flush_count: usize,
    flush_interval: Duration,
    last_flush: &mut Instant,
    last_checkpoint: &mut Instant,
) {
    match cmd {
        FlushCommand::Meta(meta) => {
            batch.push(meta);
            let count_trigger = flush_count > 0 && batch.len() >= flush_count;
            let time_trigger =
                flush_interval > Duration::ZERO && last_flush.elapsed() >= flush_interval;
            if count_trigger || time_trigger {
                flush_to_db(
                    db_conn,
                    batch,
                    call_id_cache,
                    db_path,
                    sip_rows_written,
                    media_rows_written,
                    last_checkpoint,
                )
                .await;
                *last_flush = Instant::now();
            }
        }
        FlushCommand::Flush { enqueued_at } => {
            let dwell = enqueued_at.elapsed().as_secs_f64();
            metrics::histogram!("sipflow_flush_queue_dwell_seconds", "component" => "sipflow")
                .record(dwell);
            if !batch.is_empty() {
                flush_to_db(
                    db_conn,
                    batch,
                    call_id_cache,
                    db_path,
                    sip_rows_written,
                    media_rows_written,
                    last_checkpoint,
                )
                .await;
                *last_flush = Instant::now();
            }
        }
        FlushCommand::FlushSync { done, enqueued_at } => {
            let dwell = enqueued_at.elapsed().as_secs_f64();
            metrics::histogram!("sipflow_flush_queue_dwell_seconds", "component" => "sipflow")
                .record(dwell);
            if !batch.is_empty() {
                flush_to_db(
                    db_conn,
                    batch,
                    call_id_cache,
                    db_path,
                    sip_rows_written,
                    media_rows_written,
                    last_checkpoint,
                )
                .await;
                *last_flush = Instant::now();
            }
            let _ = done.send(());
        }
        FlushCommand::Rotate {
            db_path: new_db_path,
        } => {
            if !batch.is_empty() {
                flush_to_db(
                    db_conn,
                    batch,
                    call_id_cache,
                    db_path,
                    sip_rows_written,
                    media_rows_written,
                    last_checkpoint,
                )
                .await;
                *last_flush = Instant::now();
            }
            // Before switching to the new bucket: checkpoint + truncate the old
            // WAL and shrink its page cache so the previous bucket's WAL
            // doesn't linger on disk and its pages are released from RSS.
            if let Some(conn) = db_conn.as_mut() {
                let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                    .execute(&mut *conn)
                    .await;
                let _ = sqlx::query("PRAGMA shrink_memory")
                    .execute(&mut *conn)
                    .await;
            }
            drop(db_conn.take());
            *db_conn = Some(open_db_with_pragmas(&new_db_path).await);
            *db_path = Some(new_db_path);
            call_id_cache.clear();
            *sip_rows_written = 0;
            *media_rows_written = 0;
            metrics::gauge!("sipflow_sip_rows_written", "component" => "sipflow").set(0.0);
            metrics::gauge!("sipflow_media_rows_written", "component" => "sipflow").set(0.0);
            if let Some(path) = db_path
                && let Ok(md) = std::fs::metadata(path)
            {
                metrics::gauge!("sipflow_db_file_bytes", "component" => "sipflow")
                    .set(md.len() as f64);
            }
        }
    }
}

async fn open_db_with_pragmas(db_path: &PathBuf) -> SqliteConnection {
    let mut conn = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .connect()
        .await
        .expect("failed to open sipflow sqlite db");

    for pragma in [
        "PRAGMA journal_mode=WAL",
        "PRAGMA synchronous=NORMAL",
        "PRAGMA cache_size=-64000",
        "PRAGMA temp_store=MEMORY",
        "PRAGMA busy_timeout=5000",
        "PRAGMA page_size=4096",
    ] {
        if let Err(e) = sqlx::query(pragma).execute(&mut conn).await {
            tracing::warn!("sipflow flusher: PRAGMA failed: {e}");
        }
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS call_meta (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            callid TEXT UNIQUE NOT NULL
        )",
    )
    .execute(&mut conn)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_callid ON call_meta(callid)")
        .execute(&mut conn)
        .await
        .ok();

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sip_msgs (
            id INTEGER PRIMARY KEY,
            call_id INTEGER NOT NULL,
            src TEXT NOT NULL,
            dst TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            offset INTEGER NOT NULL,
            size INTEGER NOT NULL
        )",
    )
    .execute(&mut conn)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sip_call ON sip_msgs(call_id)")
        .execute(&mut conn)
        .await
        .ok();

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS media_msgs (
            id INTEGER PRIMARY KEY,
            call_id INTEGER NOT NULL,
            leg INTEGER NOT NULL,
            src TEXT NOT NULL DEFAULT '',
            timestamp INTEGER NOT NULL,
            offset INTEGER NOT NULL,
            size INTEGER NOT NULL
        )",
    )
    .execute(&mut conn)
    .await
    .ok();

    // `idx_media_call_timestamp` (call_id, timestamp) already covers all
    // call_id-prefix lookups, so the standalone `idx_media_call` index is
    // redundant. Every media row insert updated two B-trees; dropping the
    // redundant one measurably raises sustained write throughput on large
    // DBs. Existing databases are migrated (one-time) by the DROP below.
    sqlx::query("DROP INDEX IF EXISTS idx_media_call")
        .execute(&mut conn)
        .await
        .ok();

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_media_call_timestamp ON media_msgs(call_id, timestamp)",
    )
    .execute(&mut conn)
    .await
    .ok();

    conn
}

async fn flush_to_db(
    db_conn: &mut Option<SqliteConnection>,
    batch: &mut Vec<FlushMeta>,
    call_id_cache: &mut LruCache<String, i32>,
    db_path: &Option<PathBuf>,
    sip_rows_written: &mut u64,
    media_rows_written: &mut u64,
    last_checkpoint: &mut Instant,
) {
    if batch.is_empty() {
        return;
    }
    let Some(conn) = db_conn.as_mut() else {
        return;
    };
    let start = Instant::now();
    let batch_size = batch.len();
    match try_flush(&mut *conn, batch, call_id_cache).await {
        Ok((n_sip, n_rtp, n_new_callids)) => {
            *sip_rows_written += n_sip as u64;
            *media_rows_written += n_rtp as u64;
            let elapsed = start.elapsed();
            metrics::histogram!("sipflow_flush_db_seconds", "component" => "sipflow")
                .record(elapsed.as_secs_f64());
            metrics::histogram!("sipflow_flush_batch_size", "component" => "sipflow")
                .record(batch_size as f64);
            metrics::counter!("sipflow_flush_rows_total", "component" => "sipflow", "type" => "sip")
                .increment(n_sip as u64);
            metrics::counter!("sipflow_flush_rows_total", "component" => "sipflow", "type" => "rtp")
                .increment(n_rtp as u64);
            metrics::gauge!("sipflow_sip_rows_written", "component" => "sipflow")
                .set(*sip_rows_written as f64);
            metrics::gauge!("sipflow_media_rows_written", "component" => "sipflow")
                .set(*media_rows_written as f64);
            if let Some(path) = db_path
                && let Ok(md) = std::fs::metadata(path)
            {
                metrics::gauge!("sipflow_db_file_bytes", "component" => "sipflow")
                    .set(md.len() as f64);
            }
            tracing::trace!(
                batch_size,
                elapsed_ms = elapsed.as_millis() as u64,
                n_sip,
                n_rtp,
                n_new_callids,
                "sipflow flusher: flushed batch"
            );

            // Bound WAL growth and amortise checkpoint cost by throttling it to
            // a coarse cadence instead of after every flush (a per-flush
            // checkpoint measurably dominates flush latency). Also checkpoint
            // early when the WAL file has grown large (a sustained writer can
            // otherwise outpace a time-throttled checkpoint). PASSIVE returns
            // immediately (SQLITE_BUSY) if a reader holds the WAL, so it never
            // blocks; the auto-checkpoint remains as a safety net. Kept outside
            // the flush timing window so `flush_db_seconds` stays comparable.
            let wal_too_big = db_path
                .as_ref()
                .map(|p| {
                    std::fs::metadata(format!("{}-wal", p.display()))
                        .map(|m| m.len() > WAL_MAX_BYTES)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL || wal_too_big {
                let ckpt_start = Instant::now();
                let row = sqlx::query_as::<_, WalCheckpointRow>("PRAGMA wal_checkpoint(PASSIVE)")
                    .fetch_one(&mut *conn)
                    .await;
                if let Ok(row) = row {
                    if row.busy > 0 {
                        metrics::counter!(
                            "sipflow_wal_checkpoint_busy_total",
                            "component" => "sipflow"
                        )
                        .increment(1);
                    }
                }
                *last_checkpoint = Instant::now();
                metrics::histogram!("sipflow_wal_checkpoint_seconds", "component" => "sipflow")
                    .record(ckpt_start.elapsed().as_secs_f64());
            }
        }
        Err(e) => {
            metrics::counter!("sipflow_flush_errors_total", "component" => "sipflow").increment(1);
            tracing::warn!("sipflow flusher: flush error: {e:#}");
        }
    }
}

struct SipRow {
    call_id: i32,
    src: String,
    dst: String,
    timestamp: i64,
    offset: i64,
    size: i64,
}

/// Result row of `PRAGMA wal_checkpoint(PASSIVE)`:
/// `busy` = 1 when a reader held the WAL so the checkpoint was skipped.
#[derive(sqlx::FromRow)]
struct WalCheckpointRow {
    busy: i64,
}

struct RtpRow {
    call_id: i32,
    leg: i32,
    src: String,
    timestamp: i64,
    offset: i64,
    size: i64,
}

fn push_row(sip_rows: &mut Vec<SipRow>, rtp_rows: &mut Vec<RtpRow>, meta: FlushMeta, call_id: i32) {
    match meta.msg_type {
        MsgType::Sip => sip_rows.push(SipRow {
            call_id,
            src: meta.src,
            dst: meta.dst,
            timestamp: meta.timestamp as i64,
            offset: meta.offset as i64,
            size: meta.size as i64,
        }),
        MsgType::Rtp => rtp_rows.push(RtpRow {
            call_id,
            leg: meta.leg.unwrap_or(0),
            src: meta.src,
            timestamp: meta.timestamp as i64,
            offset: meta.offset as i64,
            size: meta.size as i64,
        }),
    }
}

async fn insert_callids(
    tx: &mut Transaction<'_, Sqlite>,
    callids: &[String],
    out: &mut HashMap<String, i32>,
) -> Result<()> {
    for chunk in callids.chunks(INSERT_CHUNK_ROWS) {
        let mut qb = QueryBuilder::<Sqlite>::new("INSERT INTO call_meta (callid) ");
        qb.push_values(chunk, |mut b, c| {
            b.push_bind(c);
        });
        qb.push(" ON CONFLICT(callid) DO UPDATE SET callid=callid RETURNING id");
        let rows = qb.build().fetch_all(&mut **tx).await?;
        for (row, callid) in rows.iter().zip(chunk.iter()) {
            let id: i32 = row.try_get("id")?;
            out.insert(callid.clone(), id);
        }
    }
    Ok(())
}

/// Max total rows committed in a single transaction.
///
/// Bounds the write-lock hold time and WAL growth so one oversized batch
/// (e.g. default `flush_count=0` with a busy 1s tick) can't stall every
/// flush behind it. Each commit is one WAL fsync under `synchronous=NORMAL`,
/// so the chunk is large enough to amortise that cost.
const COMMIT_CHUNK_ROWS: usize = 5000;

/// How often a `wal_checkpoint(PASSIVE)` may run. Checkpointing after every
/// flush measurably dominates flush latency (it walks dirty pages back into
/// the main DB B-trees); throttling it to a coarser cadence lets flush
/// throughput scale while the WAL stays bounded (SQLite's default
/// auto-checkpoint remains as a safety net).
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(10);

/// If the WAL file exceeds this size, checkpoint immediately regardless of
/// `CHECKPOINT_INTERVAL`, so a sustained writer can't outpace the throttle.
const WAL_MAX_BYTES: u64 = 64 * 1024 * 1024;

async fn try_flush(
    conn: &mut SqliteConnection,
    batch: &mut Vec<FlushMeta>,
    call_id_cache: &mut LruCache<String, i32>,
) -> Result<(usize, usize, usize)> {
    let mut total_sip = 0usize;
    let mut total_rtp = 0usize;
    let mut total_new = 0usize;

    while !batch.is_empty() {
        let take = COMMIT_CHUNK_ROWS.min(batch.len());
        let slice: Vec<FlushMeta> = batch.drain(..take).collect();
        let (s, r, n) = flush_slice(conn, slice, call_id_cache).await?;
        total_sip += s;
        total_rtp += r;
        total_new += n;
    }

    Ok((total_sip, total_rtp, total_new))
}

async fn flush_slice(
    conn: &mut SqliteConnection,
    metas: Vec<FlushMeta>,
    call_id_cache: &mut LruCache<String, i32>,
) -> Result<(usize, usize, usize)> {
    let mut tx = conn.begin().await?;

    let mut sip_rows: Vec<SipRow> = Vec::new();
    let mut rtp_rows: Vec<RtpRow> = Vec::new();
    let mut pending: Vec<FlushMeta> = Vec::new();
    let mut new_callids: Vec<String> = Vec::new();

    for meta in metas {
        let Some(callid) = meta.callid.as_deref() else {
            continue;
        };
        if let Some(&cid) = call_id_cache.get(callid) {
            push_row(&mut sip_rows, &mut rtp_rows, meta, cid);
        } else {
            if !new_callids.iter().any(|c| c == callid) {
                new_callids.push(callid.to_string());
            }
            pending.push(meta);
        }
    }

    let mut new_ids: HashMap<String, i32> = HashMap::new();
    if !new_callids.is_empty() {
        insert_callids(&mut tx, &new_callids, &mut new_ids).await?;
        for (k, v) in &new_ids {
            call_id_cache.put(k.clone(), *v);
        }
    }

    for meta in pending {
        let Some(callid) = meta.callid.as_deref() else {
            continue;
        };
        let Some(&cid) = new_ids.get(callid) else {
            continue;
        };
        push_row(&mut sip_rows, &mut rtp_rows, meta, cid);
    }

    let n_sip = sip_rows.len();
    let n_rtp = rtp_rows.len();

    for chunk in sip_rows.chunks(INSERT_CHUNK_ROWS) {
        let mut qb = QueryBuilder::<Sqlite>::new(
            "INSERT INTO sip_msgs (call_id, src, dst, timestamp, offset, size) ",
        );
        qb.push_values(chunk, |mut b, r| {
            b.push_bind(r.call_id)
                .push_bind(&r.src)
                .push_bind(&r.dst)
                .push_bind(r.timestamp)
                .push_bind(r.offset)
                .push_bind(r.size);
        });
        qb.build().execute(&mut *tx).await?;
    }

    for chunk in rtp_rows.chunks(INSERT_CHUNK_ROWS) {
        let mut qb = QueryBuilder::<Sqlite>::new(
            "INSERT INTO media_msgs (call_id, leg, src, timestamp, offset, size) ",
        );
        qb.push_values(chunk, |mut b, r| {
            b.push_bind(r.call_id)
                .push_bind(r.leg)
                .push_bind(&r.src)
                .push_bind(r.timestamp)
                .push_bind(r.offset)
                .push_bind(r.size);
        });
        qb.build().execute(&mut *tx).await?;
    }

    tx.commit().await?;
    Ok((n_sip, n_rtp, new_ids.len()))
}
