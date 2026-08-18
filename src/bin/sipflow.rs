use anyhow::Result;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use chrono::{Local, TimeZone, Utc};
use clap::Parser;
use lru::LruCache;
use rustpbx::callrecord::sipflow_upload::{
    SipFlowUploadRequest, SipFlowUploadResponse, build_s3_storage, join_root, upload_media,
    upload_signaling_flow,
};
use rustpbx::callrecord::{
    sipflow_media_key_for, sipflow_signaling_file_name_for, sipflow_signaling_key_for,
};
use rustpbx::config::{SipFlowConfig, SipFlowEngine, SipFlowSubdirs, SipFlowUploadConfig};
use rustpbx::sipflow::{
    SipFlowBackend, create_backend,
    perf::{PerfCounters, PerfDumper},
    protocol::{MsgType, Packet, parse_datagram},
    storage::{extract_callid, maybe_compress_payload},
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_appender::non_blocking;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(author, version, about = "SipFlow - SIP and RTP flow recording server", long_about = None)]
struct Args {
    /// Bind address for UDP server
    #[arg(short, long, default_value = "0.0.0.0")]
    addr: String,

    /// UDP port for receiving packets
    #[arg(short, long, default_value_t = 3000)]
    port: u16,

    /// HTTP port for query API
    #[arg(long, default_value_t = 3001)]
    http_port: u16,

    /// Data directory for storage
    #[arg(short, long, default_value = "./config/sipflow")]
    root: String,

    /// Storage engine: "flowdb" or "sqlite" (default)
    #[arg(long, default_value = "sqlite")]
    engine: String,

    /// Disable gzip compression of stored payloads (sqlite engine only —
    /// flowdb has built-in compression). Uncompressed and compressed data
    /// are both always readable.
    #[arg(long, default_value_t = false)]
    no_compress: bool,

    /// Gzip compression level 0-9 for stored payloads (sqlite engine)
    #[arg(long, default_value_t = 6)]
    compress_level: u32,

    /// Subdirectory layout for storage: "none", "daily" (YYYYMMDD) or
    /// "hourly" (YYYYMMDD/HH)
    #[arg(long, default_value = "daily")]
    subdirs: String,

    /// Channel buffer size
    #[arg(long, default_value_t = 100000)]
    buffer_size: usize,

    /// UDP receive buffer size in bytes (SO_RCVBUF). Larger buffers absorb
    /// traffic bursts and reduce kernel-side drops under load. The kernel
    /// caps this at net.core.rmem_max; a warning is logged when capped.
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    recv_buffer_size: usize,

    /// Number of parallel UDP receiver tasks. Values > 1 bind extra
    /// SO_REUSEPORT sockets so the kernel load-balances datagrams across
    /// receivers. 0 = number of CPU cores.
    #[arg(long, default_value_t = 0)]
    recv_tasks: usize,

    // ── SQLite options ──
    /// Number of packets to batch before flushing (SQLite)
    #[arg(long, default_value_t = 1000)]
    flush_count: usize,

    /// Flush interval in seconds (SQLite)
    #[arg(long, default_value_t = 5)]
    flush_interval: u64,

    /// Call-ID cache size (SQLite)
    #[arg(long, default_value_t = 8192)]
    id_cache_size: usize,

    // ── Logging options ──
    /// Log file path
    #[arg(long, default_value = "/var/log/sipflow.log")]
    log_file: String,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Interval in seconds for the ingest-side perf/water-level log lines
    /// (UDP pending, recv/drop rates). 0 disables the periodic log.
    #[arg(long, default_value_t = 5)]
    perf_log_interval: u64,

    /// Block (up to 1s) on a full shard worker channel instead of dropping
    /// the record immediately. Keeps the old bounded-backpressure semantics
    /// for callers that prefer stalling over losing records; the non-blocking
    /// default is what keeps the collector from head-of-line blocking.
    #[arg(long, default_value_t = false)]
    blocking_backpressure: bool,

    // ── FlowDB options ──
    /// TTL in seconds for FlowDB records (optional, 0 = no ttl)
    #[arg(long)]
    ttl_secs: Option<u64>,

    /// FlowDB memtable size in MB (default 64)
    #[arg(long, default_value_t = 64)]
    memtable_size_mb: usize,

    /// FlowDB block cache capacity in MB (default 128)
    #[arg(long, default_value_t = 128)]
    block_cache_capacity_mb: usize,

    /// Number of parallel shard pipelines (1 = legacy single-file layout)
    #[arg(long, default_value_t = 4)]
    shards: usize,
}

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn SipFlowBackend>,
    root: String,
    subdirs: SipFlowSubdirs,
    client: reqwest::Client,
    receiver_counters: Arc<Mutex<LruCache<u32, u64>>>,
    /// Per-sender report tracking: client_id → (last_sent, last_recv), used to
    /// derive per-interval loss on the collector when a report is received.
    /// Bounded (LRU) because `client_id` is random per sender process, so old
    /// entries from restarted senders must not accumulate forever.
    report_tracking: Arc<Mutex<LruCache<u32, (u64, u64)>>>,
}

/// Bind a UDP socket with a custom SO_RCVBUF (and SO_REUSEPORT when
/// several receiver sockets share one address).
///
/// A large kernel receive buffer absorbs traffic bursts while the userspace
/// receiver is busy parsing or momentarily descheduled — with the default
/// ~208 KB buffer a few milliseconds of stall at high packet rates already
/// overflows the buffer and drops packets silently in the kernel.
///
/// The socket is left in blocking mode: receivers run on dedicated OS
/// threads in a tight recv/parse loop, which avoids tokio scheduler and
/// waker overhead on the hot path.
fn bind_udp_socket(
    addr: SocketAddr,
    recv_buffer_size: usize,
    reuse_port: bool,
) -> Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;

    #[cfg(unix)]
    if reuse_port {
        socket.set_reuse_port(true)?;
    }

    if let Err(e) = socket.set_recv_buffer_size(recv_buffer_size) {
        tracing::warn!(
            "failed to set SO_RCVBUF to {} bytes: {}",
            recv_buffer_size,
            e
        );
    }
    socket.bind(&addr.into())?;

    // Linux reports the doubled value; only warn when the kernel actually
    // capped the buffer below what was requested (net.core.rmem_max).
    let effective = socket.recv_buffer_size().unwrap_or(0);
    if effective < recv_buffer_size {
        tracing::warn!(
            "SO_RCVBUF capped at {} bytes (requested {}); raise net.core.rmem_max to allow larger buffers",
            effective,
            recv_buffer_size
        );
    } else {
        tracing::info!("UDP SO_RCVBUF effective size: {} bytes", effective);
    }

    Ok(socket.into())
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let args = Args::parse();

    // Initialize tracing: try log file, fall back to stdout on permission error
    if let Some(parent) = std::path::Path::new(&args.log_file).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let (_guard, writer) = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.log_file)
    {
        Ok(f) => {
            let (w, g) = non_blocking(f);
            (g, w)
        }
        Err(e) => {
            eprintln!(
                "sipflow: cannot open '{}' ({}), falling back to stdout",
                args.log_file, e
            );
            let (w, g) = non_blocking(std::io::stdout());
            (g, w)
        }
    };
    fmt()
        .with_env_filter(EnvFilter::new(&args.log_level))
        .with_writer(writer)
        .with_ansi(false)
        .init();

    // Ensure data directory exists
    std::fs::create_dir_all(&args.root)?;

    let engine = match args.engine.as_str() {
        "flowdb" => SipFlowEngine::FlowDb,
        _ => SipFlowEngine::Sqlite,
    };
    let subdirs = match args.subdirs.as_str() {
        "none" => SipFlowSubdirs::None,
        "hourly" => SipFlowSubdirs::Hourly,
        _ => SipFlowSubdirs::Daily,
    };
    let ttl_secs = args.ttl_secs.filter(|&s| s > 0);

    println!("Sipflow Start at {}", Utc::now());
    println!("{}", rustpbx::version::get_version_info());
    println!("root: {}", args.root);
    println!("subdirs: {:?}", subdirs);
    println!("shards: {}", args.shards);
    let config = SipFlowConfig::Local {
        root: args.root.clone(),
        subdirs: subdirs.clone(),
        flush_count: args.flush_count,
        flush_interval_secs: args.flush_interval,
        id_cache_size: args.id_cache_size,
        engine,
        compress: !args.no_compress,
        compress_level: args.compress_level,
        ttl_secs,
        memtable_size_mb: args.memtable_size_mb,
        block_cache_capacity_mb: args.block_cache_capacity_mb,
        shards: args.shards,
        flowdb_sync_mode: flowdb::SyncMode::IntervalMs(10),
        upload: None,
        blocking_backpressure: args.blocking_backpressure,
    };

    let backend: Arc<dyn SipFlowBackend> =
        Arc::from(create_backend(&config, CancellationToken::new()).await?);
    let perf_counters = PerfCounters::new_arc();

    // Export the ingest-side counters (packets parsed on the receiver threads,
    // UDP-channel drops, items routed to shards, pending backlog) to /metrics.
    // These are the counters that distinguish "collector ingest drops" from
    // "upstream loss" — previously only the backend's own (unused) counters
    // were exported.
    {
        let perf = perf_counters.clone();
        rustpbx::utils::spawn(async move {
            if args.perf_log_interval > 0 {
                let mut dumper = PerfDumper::with_interval(
                    perf,
                    std::time::Duration::from_secs(args.perf_log_interval),
                );
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    if let Some(msg) = dumper.try_dump() {
                        tracing::info!("{msg}");
                    }
                }
            }
        });
    }

    // Install global Prometheus recorder so all metrics::counter!/gauge!/histogram!
    // calls in the process are captured. Idempotent — safe to call even when another
    // crate installs it.
    #[cfg(feature = "addon-observability")]
    if let Err(e) = rustpbx::addons::observability::ObservabilityAddon::install_recorder() {
        tracing::warn!("failed to install Prometheus recorder: {e}");
    }
    metrics::gauge!("sipflow_info", "version" => rustpbx::version::get_short_version()).set(1.0);

    let http_client = rustpbx::http_util::build_keepalive_client(
        Some(std::time::Duration::from_secs(120)),
        Some(std::time::Duration::from_secs(10)),
    )?;

    let receiver_counters: Arc<Mutex<LruCache<u32, u64>>> = Arc::new(Mutex::new(LruCache::new(
        std::num::NonZeroUsize::new(65536).unwrap(),
    )));
    let report_tracking: Arc<Mutex<LruCache<u32, (u64, u64)>>> = Arc::new(Mutex::new(
        LruCache::new(std::num::NonZeroUsize::new(65536).unwrap()),
    ));

    let app_state = AppState {
        backend: backend.clone(),
        root: args.root.clone(),
        subdirs,
        client: http_client,
        receiver_counters: receiver_counters.clone(),
        report_tracking,
    };

    let udp_addr: SocketAddr = format!("{}:{}", args.addr, args.port).parse()?;
    let recv_tasks = if args.recv_tasks == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        args.recv_tasks
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Packet>(args.buffer_size);

    // UDP receiver threads. Receiving and parsing run on dedicated OS
    // threads (blocking sockets — no async scheduler/waker overhead on the
    // hot path); parsed packets are handed to the async storage worker
    // through the channel. With more than one thread, each gets its own
    // SO_REUSEPORT socket so the kernel load-balances datagrams across them.
    // Pre-compress payloads on the receiver threads so gzip work is spread
    // across all receiver cores instead of serializing on the single
    // storage worker. Only the SQLite engine stores gzip-compressed
    // payloads (`maybe_compress_payload` is idempotent, so the storage
    // layer will not re-compress). FlowDB stores raw payloads.
    let compress_early: Option<u32> =
        (engine == SipFlowEngine::Sqlite && !args.no_compress).then_some(args.compress_level);

    for i in 0..recv_tasks {
        let socket = bind_udp_socket(udp_addr, args.recv_buffer_size, recv_tasks > 1)?;
        if i == 0 {
            tracing::info!(
                "UDP server listening on {} ({} receiver thread(s))",
                udp_addr,
                recv_tasks
            );
        }
        let tx = tx.clone();
        let perf_rx = perf_counters.clone();
        std::thread::Builder::new()
            .name(format!("sipflow-recv-{i}"))
            .spawn(move || {
                let mut buf = vec![0u8; 65535];
                loop {
                    match socket.recv_from(&mut buf) {
                        Ok((size, _)) => {
                            // `parse_datagram` handles both legacy single-packet
                            // datagrams and the new batched format transparently.
                            match parse_datagram(&buf[..size]) {
                                Ok(packets) => {
                                    perf_rx
                                        .packets_received
                                        .fetch_add(packets.len() as u64, Ordering::Relaxed);
                                    for mut packet in packets {
                                        if let Some(level) = compress_early {
                                            match packet.msg_type {
                                                MsgType::Sip => {
                                                    if packet.call_id.is_none() {
                                                        packet.call_id =
                                                            extract_callid(&packet.payload);
                                                    }
                                                    packet.payload = maybe_compress_payload(
                                                        packet.payload,
                                                        level,
                                                    );
                                                }
                                                // RTP: small packet, skip to save CPU
                                                MsgType::Rtp => {}
                                            }
                                        }
                                        if tx.try_send(packet).is_err() {
                                            perf_rx.items_dropped.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("malformed datagram dropped: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("UDP recv error: {}", e);
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                }
            })?;
    }

    // Storage ingest task + independent periodic-flush task.
    //
    // The UDP drain runs in its own task and NEVER awaits a storage flush:
    // blocking it on `backend.flush()` (as a single select! did) stalls the
    // entire pipeline for the flush duration every `--flush-interval`, which
    // throttles sustained throughput. `record_packet` is non-blocking
    // (drops-on-full per shard), so a saturated shard only loses its own
    // records while the others keep draining.
    const RECV_BATCH: usize = 1024;
    let flush_interval_secs = args.flush_interval.max(1);
    let storage_backend = backend.clone();
    let perf_worker = perf_counters.clone();
    let recv_counters = receiver_counters.clone();

    // Periodic force-flush in a dedicated task: flush latency must never
    // block the ingest loop.
    {
        let storage_backend = storage_backend.clone();
        let perf_worker = perf_worker.clone();
        rustpbx::utils::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(flush_interval_secs));
            // Consume the immediate first tick so the first real flush is at
            // `flush_interval` seconds, matching prior behavior.
            interval.tick().await;
            loop {
                interval.tick().await;
                let flush_start = std::time::Instant::now();
                let _ = storage_backend.flush().await;
                let flush_secs = flush_start.elapsed().as_secs_f64();
                perf_worker.flushes.fetch_add(1, Ordering::Relaxed);
                metrics::histogram!("sipflow_flush_duration_seconds", "component" => "sipflow")
                    .record(flush_secs);
                if flush_secs > 0.1 {
                    tracing::warn!(
                        elapsed = %format!("{:.2?}", flush_start.elapsed()),
                        "storage flush took > 100ms"
                    );
                }
            }
        });
    }

    rustpbx::utils::spawn(async move {
        let mut batch = Vec::with_capacity(RECV_BATCH);
        loop {
            let n = rx.recv_many(&mut batch, RECV_BATCH).await;
            if n == 0 {
                // All senders dropped; nothing more will arrive.
                break;
            }
            for packet in batch.drain(..) {
                let client_id = packet.client_id;
                let has_call_id = packet.call_id.is_some()
                    || (packet.msg_type == MsgType::Sip && !packet.payload.is_empty());
                if has_call_id {
                    let _ = storage_backend.record_packet(packet);
                    perf_worker.items_recorded.fetch_add(1, Ordering::Relaxed);
                } else {
                    perf_worker.items_dropped.fetch_add(1, Ordering::Relaxed);
                }
                // Track per-client receive count (LRU to bound memory)
                if client_id != 0 {
                    let mut cache = recv_counters.lock().unwrap();
                    let val = cache.get(&client_id).copied().unwrap_or(0) + 1;
                    cache.put(client_id, val);
                }
            }
            perf_worker.set_pending(rx.len() as i64);
        }
    });

    // HTTP Server
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/flow", get(flow_handler))
        .route("/media", get(media_handler))
        .route("/diag", get(diag_handler))
        .route("/debug/flow", get(debug_flow_handler))
        .route("/debug/raw", get(debug_raw_handler))
        .route("/upload", post(upload_handler))
        .route("/report", post(report_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(app_state);

    let http_addr = SocketAddr::from(([0, 0, 0, 0], args.http_port));
    tracing::info!("HTTP server listening on {}", http_addr);
    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

async fn health_handler() -> &'static str {
    "OK"
}

async fn flow_handler(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    let callid = params.get("callid").cloned().unwrap_or_default();
    let start_ts = params
        .get("start")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| Local::now().timestamp() - 3600);
    let end_ts = params
        .get("end")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| Local::now().timestamp() + 3600);

    let start_dt = Local.timestamp_opt(start_ts, 0).unwrap();
    let end_dt = Local.timestamp_opt(end_ts, 0).unwrap();

    let _start = std::time::Instant::now();
    match state.backend.query_flow(&callid, start_dt, end_dt).await {
        Ok(flow) => {
            let elapsed = _start.elapsed();
            info!(
                callid,
                flow_count = flow.len(),
                elapsed = %format!("{:.2?}", elapsed),
                "flow: query success"
            );
            let flow_items: Vec<serde_json::Value> = flow
                .iter()
                .map(|item| {
                    let payload: serde_json::Value = serde_json::Value::String(
                        String::from_utf8_lossy(&item.payload).into_owned(),
                    );
                    serde_json::json!({
                        "timestamp": item.timestamp,
                        "seq": item.seq,
                        "leg": item.leg,
                        "msg_type": item.msg_type,
                        "src_addr": item.src_addr,
                        "dst_addr": item.dst_addr,
                        "payload": payload,
                    })
                })
                .collect();
            axum::Json(serde_json::json!({
                "status": "success",
                "callid": callid,
                "flow": flow_items
            }))
        }
        Err(e) => {
            let elapsed = _start.elapsed();
            warn!(
                callid,
                error = %e,
                elapsed = %format!("{:.2?}", elapsed),
                "flow: query failed"
            );
            axum::Json(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            }))
        }
    }
}

async fn media_handler(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    let callid = params.get("callid").cloned().unwrap_or_default();
    let start_ts_param = params
        .get("start")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| Local::now().timestamp() - 3600);
    let end_ts_param = params
        .get("end")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| Local::now().timestamp() + 3600);

    let stats_only = params
        .get("stats")
        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let start_dt = Local.timestamp_opt(start_ts_param, 0).unwrap();
    let end_dt = Local.timestamp_opt(end_ts_param, 0).unwrap();

    let media_url = format!(
        "/media?callid={}&start={}&end={}",
        callid, start_ts_param, end_ts_param
    );

    if stats_only {
        let _start = std::time::Instant::now();
        let stats = state
            .backend
            .query_media_stats(&callid, start_dt, end_dt)
            .await
            .unwrap_or_default();
        let elapsed = _start.elapsed();

        info!(
            url = %media_url,
            callid,
            stats_count = stats.len(),
            elapsed = %format!("{:.2?}", elapsed),
            "media: stats queried"
        );

        return axum::Json(serde_json::json!({
            "status": "success",
            "callid": callid,
            "stats": stats
        }))
        .into_response();
    }

    let _start = std::time::Instant::now();

    if let Err(e) = state.backend.flush().await {
        warn!("media: flush failed: {e}");
    }

    let wav_bytes = match state.backend.query_media(&callid, start_dt, end_dt).await {
        Ok(b) => b,
        Err(e) => {
            warn!(callid, error = %e, "media: query_media failed");
            Vec::new()
        }
    };

    let elapsed = _start.elapsed();

    if wav_bytes.is_empty() {
        warn!(
            url = %media_url,
            callid,
            elapsed = %format!("{:.2?}", elapsed),
            "media: no media found"
        );
        return (axum::http::StatusCode::NOT_FOUND, "No media found").into_response();
    }

    let file_len = wav_bytes.len();
    let body = axum::body::Body::from(wav_bytes);

    info!(
        url = %media_url,
        callid,
        bytes = file_len,
        elapsed = %format!("{:.2?}", elapsed),
        "media: wav generated successfully"
    );

    axum::response::Response::builder()
        .header("Content-Type", "audio/wav")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}.wav\"", callid),
        )
        .header("Content-Length", file_len)
        .body(body)
        .unwrap()
}

async fn diag_handler(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    let call_id = params.get("callid").cloned().unwrap_or_default();
    if call_id.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "status": "error",
                "message": "Missing 'callid' query parameter"
            })),
        )
            .into_response();
    }

    let start_dt = params
        .get("start")
        .and_then(|s| rustpbx::sipflow::diag::parse_datetime(s))
        .unwrap_or_else(|| Local::now() - chrono::Duration::hours(1));

    let end_dt = params
        .get("end")
        .and_then(|s| rustpbx::sipflow::diag::parse_datetime(s))
        .unwrap_or_else(|| Local::now() + chrono::Duration::hours(1));

    let _start = std::time::Instant::now();
    match rustpbx::sipflow::diag::run_diag(&call_id, &state.root, state.subdirs, start_dt, end_dt)
        .await
    {
        Ok(report) => {
            let elapsed = _start.elapsed();
            if report.is_empty() {
                info!(
                    call_id,
                    elapsed = %format!("{:.2?}", elapsed),
                    "diag: no data found"
                );
                axum::Json(serde_json::json!({
                    "status": "success",
                    "callid": call_id,
                    "found": false,
                    "message": "No data found for this Call-ID"
                }))
                .into_response()
            } else {
                info!(
                    call_id,
                    sip_count = report.sip_count,
                    rtp_streams = report.rtp_stats.len(),
                    buckets_scanned = report.bucket_count,
                    elapsed = %format!("{:.2?}", elapsed),
                    "diag: success"
                );

                // Convert report to a clean JSON response
                let sip_flow: Vec<serde_json::Value> = report
                    .sip_messages
                    .iter()
                    .map(|item| {
                        serde_json::json!({
                            "timestamp": item.timestamp,
                            "time": rustpbx::sipflow::diag::dt_from_micros(item.timestamp as i64),
                            "src_addr": item.src_addr,
                            "dst_addr": item.dst_addr,
                            "msg_type": item.msg_type,
                            "message": rustpbx::sipflow::diag::sip_message_status(&item.payload),
                        })
                    })
                    .collect();

                let rtp_streams: Vec<serde_json::Value> = report
                    .rtp_stats
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "leg": s.leg,
                            "src": s.src,
                            "codec": s.payload_type,
                            "clock_rate": s.clock_rate,
                            "packet_count": s.packet_count,
                            "lost_packets": s.lost_packets,
                            "loss_percent": s.loss_percent,
                            "jitter_ms": s.jitter_ms,
                            "ssrc": s.ssrc,
                        })
                    })
                    .collect();

                let rtp_detail =
                    serde_json::to_value(&report.rtp_detail).unwrap_or(serde_json::Value::Null);

                axum::Json(serde_json::json!({
                    "status": "success",
                    "callid": call_id,
                    "found": true,
                    "sip_count": report.sip_count,
                    "rtp_streams_count": report.rtp_stats.len(),
                    "duration_secs": report.duration_secs,
                    "buckets_scanned": report.bucket_count,
                    "sip_flow": sip_flow,
                    "rtp_streams": rtp_streams,
                    "rtp_detail": rtp_detail,
                }))
                .into_response()
            }
        }
        Err(e) => {
            let elapsed = _start.elapsed();
            warn!(
                call_id,
                error = %e,
                elapsed = %format!("{:.2?}", elapsed),
                "diag: failed"
            );
            axum::Json(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            }))
            .into_response()
        }
    }
}

async fn debug_flow_handler(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    let callid = params.get("callid").cloned().unwrap_or_default();
    let start_ts = params
        .get("start")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| Local::now().timestamp() - 3600);
    let end_ts = params
        .get("end")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| Local::now().timestamp() + 3600);

    let start_dt = Local.timestamp_opt(start_ts, 0).unwrap();
    let end_dt = Local.timestamp_opt(end_ts, 0).unwrap();

    match state.backend.query_flow(&callid, start_dt, end_dt).await {
        Ok(flow) => {
            let items: Vec<serde_json::Value> = flow
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "timestamp": item.timestamp,
                        "seq": item.seq,
                        "leg": item.leg,
                        "msg_type": item.msg_type,
                        "src_addr": item.src_addr,
                        "dst_addr": item.dst_addr,
                        "payload_debug": rustpbx::sipflow::diag::payload_analysis(&item.payload),
                    })
                })
                .collect();

            axum::Json(serde_json::json!({
                "status": "success",
                "callid": callid,
                "count": items.len(),
                "flow": items,
            }))
        }
        Err(e) => axum::Json(serde_json::json!({
            "status": "error",
            "message": e.to_string(),
        })),
    }
}

async fn debug_raw_handler(
    Query(params): Query<HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    let path = match params.get("path") {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "Missing 'path' parameter".to_string(),
            )
                .into_response();
        }
    };
    let offset = match params.get("offset").and_then(|s| s.parse::<u64>().ok()) {
        Some(o) => o,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "Missing or invalid 'offset' parameter".to_string(),
            )
                .into_response();
        }
    };
    let size = match params.get("size").and_then(|s| s.parse::<usize>().ok()) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "Missing or invalid 'size' parameter".to_string(),
            )
                .into_response();
        }
    };

    let data = match rustpbx::sipflow::diag::raw_read_range(path, offset, size).await {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Read error: {e}"),
            )
                .into_response();
        }
    };

    let analysis = rustpbx::sipflow::diag::payload_analysis(&data);
    let hex_dump = rustpbx::sipflow::diag::hex_dump(&data, 16);

    (
        StatusCode::OK,
        format!(
            "path: {}\noffset: {}\nsize: {}\n\nanalysis: {}\n\nhex dump:\n{}",
            path,
            offset,
            data.len(),
            serde_json::to_string_pretty(&analysis).unwrap_or_default(),
            hex_dump,
        ),
    )
        .into_response()
}

async fn upload_handler(
    State(state): State<AppState>,
    Json(req): Json<SipFlowUploadRequest>,
) -> Result<Json<SipFlowUploadResponse>, (StatusCode, String)> {
    let call_id = &req.call_id;
    let _start = std::time::Instant::now();

    let s3_storage = match build_s3_storage(&req.upload) {
        Ok(s) => s,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Storage init failed: {e}"),
            ));
        }
    };

    let start = Local.timestamp_opt(req.start, 0).single().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "Invalid start timestamp".to_string(),
        )
    })?;
    let end = Local
        .timestamp_opt(req.end, 0)
        .single()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Invalid end timestamp".to_string()))?;

    info!(
        call_id,
        upload_type = ?req.upload,
        "upload: start"
    );

    if let Err(e) = state.backend.flush().await {
        warn!("upload: flush failed: {e}");
    }

    // Compute fallback keys
    let default_media = sipflow_media_key_for(call_id, start.to_utc());
    let default_signaling = sipflow_signaling_key_for(call_id, start.to_utc());
    let default_sig_file = sipflow_signaling_file_name_for(call_id);

    let root = match &req.upload {
        SipFlowUploadConfig::S3 { root, .. } => root.as_str(),
        SipFlowUploadConfig::Http { .. } => "",
    };

    // Client-specified key → verbatim (final path); absent → join_root(root, default)
    let full_media_key = match &req.media_key {
        Some(k) if !k.is_empty() => k.clone(),
        _ => join_root(root, &default_media),
    };
    let full_signaling_key = match &req.signaling_key {
        Some(k) if !k.is_empty() => k.clone(),
        _ => join_root(root, &default_signaling),
    };
    let sig_file_name = req
        .signaling_file_name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or(default_sig_file);

    // Media upload
    let media_enabled = match &req.upload {
        SipFlowUploadConfig::S3 { media, .. } => media.unwrap_or(true),
        SipFlowUploadConfig::Http { media, .. } => media.unwrap_or(true),
    };

    let (media_url, media_size) = if media_enabled {
        match upload_media(
            state.backend.as_ref(),
            &req.upload,
            call_id,
            start,
            end,
            &full_media_key,
            None,
            0,
            &state.client,
            s3_storage.as_ref(),
        )
        .await
        {
            Some((url, size)) => (Some(url), size),
            None => (None, 0),
        }
    } else {
        (None, 0)
    };

    // Signaling upload
    let signaling_enabled = match &req.upload {
        SipFlowUploadConfig::S3 { signaling, .. } => signaling.unwrap_or(false),
        SipFlowUploadConfig::Http { signaling, .. } => signaling.unwrap_or(false),
    };

    let signaling_uploaded = if signaling_enabled {
        upload_signaling_flow(
            &req.upload,
            state.backend.as_ref(),
            call_id,
            start,
            end,
            &full_signaling_key,
            &sig_file_name,
            &state.client,
            s3_storage.as_ref(),
        )
        .await
    } else {
        false
    };

    let elapsed = _start.elapsed();
    info!(
        call_id,
        media_url = media_url.as_deref().unwrap_or("(none)"),
        media_size,
        signaling_uploaded,
        elapsed = %format!("{:.2?}", elapsed),
        "upload: complete"
    );

    Ok(Json(SipFlowUploadResponse {
        media_url,
        media_size,
        signaling_uploaded,
    }))
}

#[cfg(feature = "addon-observability")]
async fn metrics_handler() -> impl axum::response::IntoResponse {
    use axum::http::{StatusCode, header};
    use rustpbx::addons::observability::ObservabilityAddon;
    match ObservabilityAddon::render_prometheus() {
        Some(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Prometheus recorder not initialised",
        )
            .into_response(),
    }
}

#[cfg(not(feature = "addon-observability"))]
async fn metrics_handler() -> impl axum::response::IntoResponse {
    (
        axum::http::StatusCode::NOT_FOUND,
        "Prometheus support not enabled (build with --features addon-observability)",
    )
        .into_response()
}

async fn report_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl axum::response::IntoResponse {
    let client_id = body["client_id"].as_u64().unwrap_or(0) as u32;
    let sent_count = body["sent_count"].as_u64().unwrap_or(0);
    let packets_received = state
        .receiver_counters
        .lock()
        .unwrap()
        .get(&client_id)
        .copied()
        .unwrap_or(0);

    // Derive a per-interval loss rate from the sender's cumulative sent count
    // vs. what this collector actually received, and log it per peer IP.
    let mut tracking = state.report_tracking.lock().unwrap();
    let (mut last_sent, mut last_recv) = tracking.get(&client_id).copied().unwrap_or((0, 0));
    // Sender restarted (counter reset): reset the baseline so the first report
    // after a restart reports the fresh interval rather than negative loss.
    if sent_count < last_sent {
        last_sent = 0;
        last_recv = 0;
    }
    let sent_delta = sent_count.saturating_sub(last_sent);
    let recv_delta = packets_received.saturating_sub(last_recv);
    let loss = sent_delta.saturating_sub(recv_delta);
    let loss_rate = if sent_delta > 0 {
        loss as f64 / sent_delta as f64
    } else {
        0.0
    };
    tracking.put(client_id, (sent_count, packets_received));
    drop(tracking);

    tracing::info!(
        peer_ip = %peer.ip(),
        client_id,
        sent = sent_delta,
        recv = recv_delta,
        loss = loss,
        loss_rate = loss_rate,
        "sipflow collector report"
    );

    axum::Json(serde_json::json!({
        "status": "success",
        "client_id": client_id,
        "packets_received": packets_received,
    }))
}

#[cfg(test)]
mod tests {
    use rustpbx_sipflow::wav_utils::generate_wav_from_packets;

    #[test]
    fn test_generate_wav_pcmu_no_transcode() {
        // Setup: Two legs, PCMU packets
        // Leg 0: A
        // Leg 1: B
        // Packet: (leg, timestamp, data) - Timestamp is u64

        let mut packets = Vec::new(); // should use Vec<(i32, u64, Vec<u8>)>
        let payload = vec![0x7F; 160]; // Silence

        // 12 bytes RTP header
        let mut header = vec![0u8; 12];
        header[0] = 0x80; // RTP v2
        header[1] = 0; // PCMU

        let mut p1 = header.clone();
        p1[4..8].copy_from_slice(&1000u32.to_be_bytes());
        p1.extend_from_slice(&payload);
        packets.push((0, 1000u64, p1));

        let mut p2 = header.clone();
        p2[4..8].copy_from_slice(&1000u32.to_be_bytes());
        p2.extend_from_slice(&payload);
        packets.push((1, 1000u64, p2));

        // Next 20ms
        let mut p3 = header.clone();
        p3[4..8].copy_from_slice(&1160u32.to_be_bytes());
        p3.extend_from_slice(&payload);
        packets.push((0, 1160u64, p3));

        let result = generate_wav_from_packets(&packets);
        assert!(result.is_ok());
        let wav_bytes = result.unwrap();

        // Check RIFF
        assert_eq!(&wav_bytes[0..4], b"RIFF");
        // Check format tag
        let fmt_tag = u16::from_le_bytes([wav_bytes[20], wav_bytes[21]]);
        assert_eq!(fmt_tag, 7); // PCMU
    }

    #[test]
    fn test_generate_wav_mixed_transcode() {
        // Leg 0: PCMU
        // Leg 1: G722 (PT 9)
        // Target should be L16 (PCM 16k -> Format Tag 1)

        let mut packets = Vec::new();

        // Leg 0 PCMU (8000Hz)
        let mut header_pcmu = vec![0u8; 12];
        header_pcmu[0] = 0x80; // RTP v2
        header_pcmu[1] = 0; // PT 0 = PCMU
        let payload_pcmu = vec![0x7F; 160];
        let mut p1 = header_pcmu.clone();
        p1.extend_from_slice(&payload_pcmu);
        packets.push((0, 1000u64, p1));

        // Leg 1 G722 (16000Hz)
        let mut header_g722 = vec![0u8; 12];
        header_g722[0] = 0x80; // RTP v2
        header_g722[1] = 9; // PT 9 = G722
        let payload_g722 = vec![0u8; 160];
        let mut p2 = header_g722.clone();
        p2.extend_from_slice(&payload_g722);
        packets.push((1, 1000u64, p2));

        let result = generate_wav_from_packets(&packets);
        assert!(result.is_ok());
        let wav_bytes = result.unwrap();

        let fmt_tag = u16::from_le_bytes([wav_bytes[20], wav_bytes[21]]);
        assert_eq!(fmt_tag, 1); // PCM

        let rate = u32::from_le_bytes([wav_bytes[24], wav_bytes[25], wav_bytes[26], wav_bytes[27]]);
        assert_eq!(rate, 16000);
    }
}
