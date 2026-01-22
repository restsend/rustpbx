use anyhow::Result;
use axum::{
    Router,
    extract::{Query, State},
    response::IntoResponse,
    routing::get,
};
use chrono::{Local, TimeZone};
use clap::Parser;
use rustpbx::sipflow::{
    protocol::{Packet, parse_packet},
    storage::{StorageManager, process_packet},
};
use rustpbx::{config::SipFlowSubdirs, sipflow::wav_utils::generate_wav_from_packets};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

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

    /// Number of packets to batch before flushing
    #[arg(long, default_value_t = 1000)]
    flush_count: usize,

    /// Flush interval in seconds
    #[arg(long, default_value_t = 5)]
    flush_interval: u64,

    /// Channel buffer size
    #[arg(long, default_value_t = 100000)]
    buffer_size: usize,
}

#[derive(Clone)]
struct AppState {
    storage: Arc<Mutex<StorageManager>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Ensure data directory exists
    std::fs::create_dir_all(&args.root)?;

    let storage = Arc::new(Mutex::new(StorageManager::new(
        std::path::Path::new(&args.root),
        args.flush_count,
        args.flush_interval,
        1024,
        SipFlowSubdirs::None,
    )));
    let app_state = AppState {
        storage: storage.clone(),
    };

    let udp_addr: SocketAddr = format!("{}:{}", args.addr, args.port).parse()?;
    let socket = UdpSocket::bind(udp_addr).await?;
    tracing::info!("UDP server listening on {}", udp_addr);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Packet>(args.buffer_size);

    // UDP Receiver Task
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
                    tracing::error!("UDP recv error: {}", e);
                }
            }
        }
    });

    // Storage Worker Task
    let storage_worker = storage.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                Some(packet) = rx.recv() => {
                    let processed = process_packet(packet);
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

    let packets = {
        let mut mg = state.storage.lock().await;
        mg.query_media(&callid, start_dt, end_dt)
            .await
            .unwrap_or_default()
    };

    match generate_wav_from_packets(&packets) {
        Ok(wav_data) => axum::response::Response::builder()
            .header("Content-Type", "audio/wav")
            .header(
                "Content-Disposition",
                format!("attachment; filename=\"{}.wav\"", callid),
            )
            .body(axum::body::Body::from(wav_data))
            .unwrap()
            .into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_wav_pcmu_no_transcode() {
        // Setup: Two legs, PCMU packets
        // Leg 0: A
        // Leg 1: B
        // Packet: (leg, timestamp, data) - Timestamp is u64

        let mut packets = Vec::new(); // should use Vec<(i32, u64, Vec<u8>)>
        let payload = vec![0x7F; 160]; // Silence

        // 12 bytes dummy header
        let mut header = vec![0u8; 12];
        header[1] = 0; // PCMU

        let mut p1 = header.clone();
        p1.extend_from_slice(&payload);
        packets.push((0, 1000u64, p1));

        let mut p2 = header.clone();
        p2.extend_from_slice(&payload);
        packets.push((1, 1000u64, p2));

        // Next 20ms
        let mut p3 = header.clone();
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
