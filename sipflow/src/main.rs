mod protocol;
mod storage;

use crate::protocol::parse_packet;
use crate::storage::{StorageManager, process_packet};
use audio_codec::{CodecType, create_decoder};
use axum::{
    Router,
    extract::{Query, State},
    response::IntoResponse,
    routing::get,
};
use chrono::{Local, TimeZone};
use clap::Parser;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "0.0.0.0")]
    addr: String,

    #[arg(short, long, default_value_t = 3000)]
    port: u16,

    #[arg(long, default_value_t = 3001)]
    http_port: u16,

    #[arg(short, long, default_value = "storage")]
    data_dir: String,

    #[arg(long, default_value_t = 1000)]
    flush_count: usize,

    #[arg(long, default_value_t = 5)]
    flush_interval: u64,

    #[arg(long, default_value_t = 100000)]
    buffer_size: usize,
}

#[derive(Clone)]
struct AppState {
    storage: Arc<Mutex<StorageManager>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Ensure data directory exists
    std::fs::create_dir_all(&args.data_dir)?;

    let storage = Arc::new(Mutex::new(StorageManager::new(
        std::path::Path::new(&args.data_dir),
        args.flush_count,
        args.flush_interval,
    )));
    let app_state = AppState {
        storage: storage.clone(),
    };

    // 1. UDP Server using socket2 for better control
    let udp_addr: SocketAddr = format!("{}:{}", args.addr, args.port).parse()?;
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(udp_addr),
        socket2::Type::DGRAM,
        None,
    )?;
    socket.set_reuse_address(true)?;
    socket.set_recv_buffer_size(10 * 1024 * 1024)?; // 10MB
    socket.bind(&udp_addr.into())?;
    let std_sock: std::net::UdpSocket = socket.into();
    let socket = UdpSocket::from_std(std_sock)?;
    println!("UDP server listening on {}", udp_addr);

    let (tx, mut rx) = tokio::sync::mpsc::channel(args.buffer_size);

    // UDP Receiver Task: Extremely fast, just pushes to channel
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((size, _)) => {
                    if let Ok(packet) = parse_packet(&buf[..size]) {
                        let _ = tx.try_send(packet);
                    }
                }
                Err(e) => {
                    eprintln!("UDP recv error: {}", e);
                }
            }
        }
    });

    // Storage Worker Task: Handles persistence
    let storage_worker = storage.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                Some(packet) = rx.recv() => {
                    // CPU heavy processing: Extract Call-ID, Zstd compress
                    // This happens OUTSIDE the Mutex lock
                    let processed = process_packet(packet);

                    // Persistence: Lock and Write
                    // This is primarily IO bound
                    let mut mg = storage_worker.lock().await;
                    let _ = mg.write_processed(processed).await;
                }
                _ = interval.tick() => {
                    let mut mg = storage_worker.lock().await;
                    let _ = mg.check_flush().await;
                }
            }
        }
    });

    // 2. HTTP Server
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/flow", get(flow_handler))
        .route("/media", get(media_handler))
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], args.http_port));
    println!("HTTP server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
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

    let mut mg = state.storage.lock().await;
    match mg.query_flow(&callid, start_dt, end_dt).await {
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

    let start_dt = Local.timestamp_opt(start_ts_param, 0).unwrap();
    let end_dt = Local.timestamp_opt(end_ts_param, 0).unwrap();

    let mut mg = state.storage.lock().await;

    let flow = match mg.query_flow(&callid, start_dt, end_dt).await {
        Ok(f) => f,
        Err(e) => {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    if flow.is_empty() {
        return (axum::http::StatusCode::NOT_FOUND, "Call-ID not found").into_response();
    }

    // SDP Decoding
    let mut rtp_endpoints = Vec::new();
    let mut min_ts = u64::MAX;
    let mut max_ts = 0;
    let mut pt_map = HashMap::new(); // PT -> CodecType

    for msg in &flow {
        let raw = msg["raw_message"].as_str().unwrap_or_default();
        let ts = msg["timestamp"].as_u64().unwrap_or(0);
        min_ts = min_ts.min(ts);
        max_ts = max_ts.max(ts);

        if raw.contains("m=audio") {
            let mut current_ip = String::new();
            if let Some(c_line) = raw.lines().find(|l| l.starts_with("c=IN IP4")) {
                current_ip = c_line
                    .split_whitespace()
                    .last()
                    .unwrap_or_default()
                    .to_string();
            }

            if let Some(m_line) = raw.lines().find(|l| l.starts_with("m=audio")) {
                let port = m_line.split_whitespace().nth(1).unwrap_or_default();
                if !current_ip.is_empty() && !port.is_empty() {
                    rtp_endpoints.push(format!("{}:{}", current_ip, port));
                }
            }

            // Parse rtpmap
            for line in raw.lines() {
                if line.starts_with("a=rtpmap:") {
                    let parts: Vec<&str> = line[9..].split_whitespace().collect();
                    if parts.len() >= 2 {
                        let pt = parts[0].parse::<u8>().unwrap_or(255);
                        let codec = parts[1].to_uppercase();
                        let codec_type = if codec.contains("G711U") || codec.contains("PCMU") {
                            Some(CodecType::PCMU)
                        } else if codec.contains("G711A") || codec.contains("PCMA") {
                            Some(CodecType::PCMA)
                        } else if codec.contains("G729") {
                            Some(CodecType::G729)
                        } else if codec.contains("OPUS") {
                            Some(CodecType::Opus)
                        } else {
                            None
                        };
                        if let Some(ct) = codec_type {
                            pt_map.insert(pt, ct);
                        }
                    }
                }
            }
        }
    }

    // Default static PTs if not in SDP
    pt_map.entry(0).or_insert(CodecType::PCMU);
    pt_map.entry(8).or_insert(CodecType::PCMA);
    pt_map.entry(18).or_insert(CodecType::G729);

    let packets = if rtp_endpoints.len() >= 2 {
        let parts1: Vec<&str> = rtp_endpoints[0].split(':').collect();
        let parts2: Vec<&str> = rtp_endpoints[1].split(':').collect();
        // Use 5 seconds buffer around SIP message timestamps
        mg.query_media(
            parts1[0],
            parts2[0],
            min_ts.saturating_sub(5000),
            max_ts + 5000,
            start_dt,
            end_dt,
        )
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    if packets.is_empty() {
        return (axum::http::StatusCode::NOT_FOUND, "No RTP packets found").into_response();
    }

    // Transcoding to PCM
    let mut pcm_samples = Vec::new();
    let mut decoders: HashMap<u8, Box<dyn audio_codec::Decoder>> = HashMap::new();
    let mut sample_rate = 8000;

    for p in packets {
        if p.len() < 12 {
            continue;
        }
        let pt = p[1] & 0x7F;
        let payload = &p[12..];

        if let Some(codec_type) = pt_map.get(&pt) {
            let decoder = decoders.entry(pt).or_insert_with(|| {
                // Heuristic: if codec is Opus, assume 48k. Others 8k.
                if *codec_type == CodecType::Opus {
                    sample_rate = 48000;
                }
                create_decoder(*codec_type)
            });

            let samples = decoder.decode(payload);
            pcm_samples.extend_from_slice(&samples);
        }
    }

    if pcm_samples.is_empty() {
        return (
            axum::http::StatusCode::NO_CONTENT,
            "No decodable audio found",
        )
            .into_response();
    }

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: sample_rate as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
        for &sample in &pcm_samples {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    axum::response::Response::builder()
        .header("Content-Type", "audio/wav")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}.wav\"", callid),
        )
        .body(axum::body::Body::from(cursor.into_inner()))
        .unwrap()
        .into_response()
}
