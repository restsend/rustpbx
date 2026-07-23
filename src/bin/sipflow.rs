use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use chrono::{Local, TimeZone};
use clap::Parser;
use rustpbx::callrecord::sipflow_upload::{
    SipFlowUploadRequest, SipFlowUploadResponse, build_s3_storage, join_root, upload_media,
    upload_signaling_flow,
};
use rustpbx::callrecord::{
    sipflow_media_key_for, sipflow_signaling_file_name_for, sipflow_signaling_key_for,
};
use rustpbx::config::{SipFlowConfig, SipFlowEngine, SipFlowSubdirs, SipFlowUploadConfig};
use rustpbx::sipflow::{
    SipFlowBackend, SipFlowItem, SipFlowMsgType, create_backend,
    perf::{PerfCounters, PerfDumper},
    protocol::{MsgType, Packet, parse_datagram},
    storage::{extract_callid, maybe_compress_payload},
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use tracing::warn;
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
}

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn SipFlowBackend>,
    root: String,
    subdirs: SipFlowSubdirs,
    client: reqwest::Client,
}

fn convert_packet_to_item(packet: Packet) -> (String, SipFlowItem) {
    let call_id = packet
        .call_id
        .clone()
        .or_else(|| {
            if packet.msg_type == MsgType::Sip {
                extract_callid(&packet.payload)
            } else {
                None
            }
        })
        .unwrap_or_default();

    let src = format!("{}:{}", packet.src.0, packet.src.1);
    let dst = format!("{}:{}", packet.dst.0, packet.dst.1);

    let item = SipFlowItem {
        timestamp: packet.timestamp,
        seq: 0,
        leg: packet.leg,
        msg_type: if packet.msg_type == MsgType::Sip {
            SipFlowMsgType::Sip
        } else {
            SipFlowMsgType::Rtp
        },
        src_addr: src,
        dst_addr: dst,
        payload: packet.payload,
    };

    (call_id, item)
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
        upload: None,
    };

    let backend: Arc<dyn SipFlowBackend> =
        Arc::from(create_backend(&config, CancellationToken::new())?);
    let perf_counters = PerfCounters::new_arc();

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

    let app_state = AppState {
        backend: backend.clone(),
        root: args.root.clone(),
        subdirs,
        client: http_client,
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
                                            // The Call-ID header must be
                                            // extracted before the payload is
                                            // compressed.
                                            if packet.msg_type == MsgType::Sip
                                                && packet.call_id.is_none()
                                            {
                                                packet.call_id = extract_callid(&packet.payload);
                                            }
                                            packet.payload =
                                                maybe_compress_payload(packet.payload, level);
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

    // Storage Worker Task. `recv_many` drains packets in batches so a
    // single wakeup processes up to `RECV_BATCH` packets instead of one.
    // The periodic force-flush honors the CLI `--flush-interval`.
    const RECV_BATCH: usize = 1024;
    let flush_interval_secs = args.flush_interval.max(1);
    let storage_backend = backend.clone();
    let perf_worker = perf_counters.clone();
    rustpbx::utils::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(flush_interval_secs));
        let mut batch = Vec::with_capacity(RECV_BATCH);
        loop {
            tokio::select! {
                n = rx.recv_many(&mut batch, RECV_BATCH) => {
                    if n == 0 {
                        // All senders dropped; nothing more will arrive.
                        break;
                    }
                    for packet in batch.drain(..) {
                        let (call_id, item) = convert_packet_to_item(packet);
                        if !call_id.is_empty() {
                            let _ = storage_backend.record(&call_id, item);
                            perf_worker.items_recorded.fetch_add(1, Ordering::Relaxed);
                        } else {
                            perf_worker.items_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    perf_worker.set_pending(rx.len() as i64);
                }
                _ = interval.tick() => {
                    let flush_start = std::time::Instant::now();
                    let _ = storage_backend.flush().await;
                    let flush_secs = flush_start.elapsed().as_secs_f64();
                    perf_worker.flushes.fetch_add(1, Ordering::Relaxed);
                    metrics::histogram!("sipflow_flush_duration_seconds", "component" => "sipflow")
                        .record(flush_secs);
                }
            }
        }
    });

    // Periodic perf dump (every 10 s, skipped when idle)
    let perf_dump = perf_counters.clone();
    rustpbx::utils::spawn(async move {
        let mut dumper = PerfDumper::new(perf_dump);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            if let Some(msg) = dumper.try_dump() {
                tracing::info!("{msg}");
            }
        }
    });

    // HTTP Server
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/flow", get(flow_handler))
        .route("/media", get(media_handler))
        .route("/diag", get(diag_handler))
        .route("/upload", post(upload_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(app_state);

    let http_addr = SocketAddr::from(([0, 0, 0, 0], args.http_port));
    tracing::info!("HTTP server listening on {}", http_addr);
    let listener = tokio::net::TcpListener::bind(http_addr).await?;
    axum::serve(listener, app).await?;

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

    match state.backend.query_flow(&callid, start_dt, end_dt).await {
        Ok(flow) => axum::Json(serde_json::json!({
            "status": "success",
            "callid": callid,
            "flow": flow
        })),
        Err(e) => axum::Json(serde_json::json!({
            "status": "error",
            "message": e.to_string()
        })),
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

    if stats_only {
        let stats = state
            .backend
            .query_media_stats(&callid, start_dt, end_dt)
            .await
            .unwrap_or_default();

        return axum::Json(serde_json::json!({
            "status": "success",
            "callid": callid,
            "stats": stats
        }))
        .into_response();
    }

    let wav_bytes = state
        .backend
        .query_media(&callid, start_dt, end_dt)
        .await
        .unwrap_or_default();

    if wav_bytes.is_empty() {
        return (axum::http::StatusCode::NOT_FOUND, "No media found").into_response();
    }

    let file_len = wav_bytes.len();
    let body = axum::body::Body::from(wav_bytes);

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

    match rustpbx::sipflow::diag::run_diag(&call_id, &state.root, state.subdirs, start_dt, end_dt)
        .await
    {
        Ok(report) => {
            if report.is_empty() {
                axum::Json(serde_json::json!({
                    "status": "success",
                    "callid": call_id,
                    "found": false,
                    "message": "No data found for this Call-ID"
                }))
                .into_response()
            } else {
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
        Err(e) => axum::Json(serde_json::json!({
            "status": "error",
            "message": e.to_string()
        }))
        .into_response(),
    }
}

async fn upload_handler(
    State(state): State<AppState>,
    Json(req): Json<SipFlowUploadRequest>,
) -> Result<Json<SipFlowUploadResponse>, (StatusCode, String)> {
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

    if let Err(e) = state.backend.flush().await {
        warn!("upload: flush failed: {e}");
    }

    // Compute fallback keys
    let default_media = sipflow_media_key_for(&req.call_id, start.to_utc());
    let default_signaling = sipflow_signaling_key_for(&req.call_id, start.to_utc());
    let default_sig_file = sipflow_signaling_file_name_for(&req.call_id);

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
            &req.call_id,
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
            &req.call_id,
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
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
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

#[cfg(test)]
mod tests {
    use rustpbx::sipflow::wav_utils::generate_wav_from_packets;

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
