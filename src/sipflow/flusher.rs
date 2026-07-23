use crate::sipflow::protocol::MsgType;
use anyhow::Result;
use lru::LruCache;
use sqlx::{ConnectOptions, Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

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
    Flush,
    FlushSync { done: oneshot::Sender<()> },
    Rotate { db_path: PathBuf },
    Shutdown,
}

pub(crate) struct SipFlowFlusher {
    sender: mpsc::UnboundedSender<FlushCommand>,
    handle: Option<thread::JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}

impl SipFlowFlusher {
    pub(crate) fn new(
        flush_count: usize,
        flush_interval_secs: u64,
        id_cache_size: usize,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<FlushCommand>();
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_clone = dropped.clone();

        let handle = thread::Builder::new()
            .name("sipflow-flusher".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                    .expect("failed to build flusher tokio runtime");
                rt.block_on(async move {
                    run(rx, flush_count, flush_interval_secs, id_cache_size, dropped_clone).await;
                });
            })
            .expect("failed to spawn sipflow flusher thread");

        Self {
            sender: tx,
            handle: Some(handle),
            dropped,
        }
    }

    pub(crate) fn sender(&self) -> mpsc::UnboundedSender<FlushCommand> {
        self.sender.clone()
    }

    pub(crate) fn dropped_count(&self) -> Arc<AtomicU64> {
        self.dropped.clone()
    }
}

impl Drop for SipFlowFlusher {
    fn drop(&mut self) {
        let _ = self.sender.send(FlushCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

async fn run(
    mut rx: mpsc::UnboundedReceiver<FlushCommand>,
    flush_count: usize,
    flush_interval_secs: u64,
    id_cache_size: usize,
    _dropped: Arc<AtomicU64>,
) {
    let flush_interval = Duration::from_secs(flush_interval_secs);
    let mut db_conn: Option<SqliteConnection> = None;
    let mut batch: Vec<FlushMeta> = Vec::new();
    let mut call_id_cache: LruCache<String, i32> =
        LruCache::new(NonZeroUsize::new(id_cache_size.max(1)).unwrap());
    let mut last_flush = Instant::now();

    loop {
        tokio::select! {
            biased;
            Some(cmd) = rx.recv() => {
                match cmd {
                    FlushCommand::Meta(meta) => {
                        batch.push(meta);
                        let count_trigger = flush_count > 0 && batch.len() >= flush_count;
                        let time_trigger = flush_interval > Duration::ZERO
                            && last_flush.elapsed() >= flush_interval;
                        if count_trigger || time_trigger {
                            flush_to_db(&mut db_conn, &mut batch, &mut call_id_cache).await;
                            last_flush = Instant::now();
                        }
                    }
                    FlushCommand::Flush => {
                        if !batch.is_empty() {
                            flush_to_db(&mut db_conn, &mut batch, &mut call_id_cache).await;
                            last_flush = Instant::now();
                        }
                    }
                    FlushCommand::FlushSync { done } => {
                        if !batch.is_empty() {
                            flush_to_db(&mut db_conn, &mut batch, &mut call_id_cache).await;
                            last_flush = Instant::now();
                        }
                        let _ = done.send(());
                    }
                    FlushCommand::Rotate { db_path } => {
                        if !batch.is_empty() {
                            flush_to_db(&mut db_conn, &mut batch, &mut call_id_cache).await;
                            last_flush = Instant::now();
                        }
                        drop(db_conn.take());
                        db_conn = Some(open_db_with_pragmas(&db_path).await);
                        call_id_cache.clear();
                    }
                    FlushCommand::Shutdown => {
                        if !batch.is_empty() {
                            flush_to_db(&mut db_conn, &mut batch, &mut call_id_cache).await;
                        }
                        break;
                    }
                }
            }
            else => {
                if !batch.is_empty() {
                    flush_to_db(&mut db_conn, &mut batch, &mut call_id_cache).await;
                }
                break;
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

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_media_call ON media_msgs(call_id)")
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
) {
    if batch.is_empty() {
        return;
    }
    let Some(conn) = db_conn.as_mut() else {
        return;
    };
    if let Err(e) = try_flush(conn, batch, call_id_cache).await {
        tracing::warn!("sipflow flusher: flush error: {e:#}");
    }
}

async fn try_flush(
    conn: &mut SqliteConnection,
    batch: &mut Vec<FlushMeta>,
    call_id_cache: &mut LruCache<String, i32>,
) -> Result<()> {
    let mut tx = conn.begin().await?;

    for meta in batch.drain(..) {
        let call_id = if let Some(ref callid) = meta.callid {
            if let Some(&cached_id) = call_id_cache.get(callid) {
                cached_id
            } else {
                let id: i32 = sqlx::query_scalar(
                    "INSERT INTO call_meta (callid) VALUES (?) 
                     ON CONFLICT(callid) DO UPDATE SET callid=callid 
                     RETURNING id",
                )
                .bind(callid)
                .fetch_one(&mut *tx)
                .await?;
                call_id_cache.put(callid.clone(), id);
                id
            }
        } else {
            continue;
        };

        match meta.msg_type {
            MsgType::Sip => {
                sqlx::query(
                    "INSERT INTO sip_msgs (call_id, src, dst, timestamp, offset, size) VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(call_id)
                .bind(meta.src)
                .bind(meta.dst)
                .bind(meta.timestamp as i64)
                .bind(meta.offset as i64)
                .bind(meta.size as i64)
                .execute(&mut *tx)
                .await?;
            }
            MsgType::Rtp => {
                let leg = meta.leg.unwrap_or(0);
                sqlx::query(
                    "INSERT INTO media_msgs (call_id, leg, src, timestamp, offset, size) VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(call_id)
                .bind(leg)
                .bind(meta.src)
                .bind(meta.timestamp as i64)
                .bind(meta.offset as i64)
                .bind(meta.size as i64)
                .execute(&mut *tx)
                .await?;
            }
        }
    }

    tx.commit().await?;
    Ok(())
}
