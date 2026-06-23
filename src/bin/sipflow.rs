use anyhow::Result;
use axum::{
    Router,
    extract::{Query, State},
    response::IntoResponse,
    routing::get,
};
use chrono::{Local, TimeZone};
use clap::Parser;
use rustpbx::config::{SipFlowConfig, SipFlowEngine, SipFlowSubdirs};
use rustpbx::sipflow::{
    SipFlowBackend, SipFlowItem, SipFlowMsgType, create_backend,
    perf::{PerfCounters, PerfDumper},
    protocol::{MsgType, Packet, parse_packet},
    storage::extract_callid,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::net::UdpSocket;
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

    /// Channel buffer size
    #[arg(long, default_value_t = 100000)]
    buffer_size: usize,

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
        "sqlite" | _ => SipFlowEngine::Sqlite,
    };
    let ttl_secs = args.ttl_secs.filter(|&s| s > 0);

    let config = SipFlowConfig::Local {
        root: args.root.clone(),
        subdirs: SipFlowSubdirs::None,
        flush_count: args.flush_count,
        flush_interval_secs: args.flush_interval,
        id_cache_size: args.id_cache_size,
        engine,
        ttl_secs,
        memtable_size_mb: args.memtable_size_mb,
        block_cache_capacity_mb: args.block_cache_capacity_mb,
        upload: None,
    };

    let backend: Arc<dyn SipFlowBackend> = Arc::from(create_backend(&config)?);
    let perf_counters = PerfCounters::new_arc();

    let app_state = AppState {
        backend: backend.clone(),
    };

    let udp_addr: SocketAddr = format!("{}:{}", args.addr, args.port).parse()?;
    let socket = UdpSocket::bind(udp_addr).await?;
    tracing::info!("UDP server listening on {}", udp_addr);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Packet>(args.buffer_size);

    // UDP Receiver Task
    let perf_rx = perf_counters.clone();
    rustpbx::utils::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((size, _)) => {
                    if let Ok(packet) = parse_packet(&buf[..size]) {
                        perf_rx.packets_received.fetch_add(1, Ordering::Relaxed);
                        let _ = tx.try_send(packet);
                    }
                }
                Err(e) => {
                    tracing::error!("UDP recv error: {}", e);
                }
            }
        }
    });

    // Storage Worker Task
    let storage_backend = backend.clone();
    let perf_worker = perf_counters.clone();
    rustpbx::utils::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                Some(packet) = rx.recv() => {
                    let (call_id, item) = convert_packet_to_item(packet);
                    if !call_id.is_empty() {
                        let _ = storage_backend.record(&call_id, item);
                        perf_worker.items_recorded.fetch_add(1, Ordering::Relaxed);
                    } else {
                        perf_worker.items_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    perf_worker.set_pending(rx.len() as i64);
                }
                _ = interval.tick() => {
                    let _ = storage_backend.flush().await;
                    perf_worker.flushes.fetch_add(1, Ordering::Relaxed);
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
        let stats_json: Vec<_> = stats
            .into_iter()
            .map(|stat| {
                serde_json::json!({
                    "leg": stat.leg,
                    "src": stat.src,
                    "count": stat.packet_count,
                    "packet_count": stat.packet_count,
                    "lost_packets": stat.lost_packets,
                    "expected_packets": stat.expected_packets,
                    "loss_percent": stat.loss_percent,
                    "jitter_ms": stat.jitter_ms,
                    "ssrc": stat.ssrc,
                    "payload_type": stat.payload_type,
                    "clock_rate": stat.clock_rate,
                })
            })
            .collect();

        return axum::Json(serde_json::json!({
            "status": "success",
            "callid": callid,
            "stats": stats_json
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
