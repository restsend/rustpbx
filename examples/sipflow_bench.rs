//! SipFlow backend performance benchmark: FlowDB vs SQLite.
//!
//! Measures write throughput, disk-space usage, and query latency
//! for both storage engines under a realistic mixed SIP+RTP workload.
//!
//! ```sh
//! cargo run --release --example sipflow_bench -- [options]
//! Options:
//!   --calls N        Number of simulated calls (default 50)
//!   --rtp-per-call N RTP packets per call (default 1000)
//!   --sip-per-call N SIP messages per call (default 20)
//! ```

use bytes::Bytes;
use chrono::{DateTime, Local, TimeZone};
use clap::Parser;
use rustpbx::config::{SipFlowConfig, SipFlowEngine, SipFlowSubdirs};
use rustpbx::sipflow::{SipFlowItem, SipFlowMsgType};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 50)]
    calls: usize,
    #[arg(long, default_value_t = 1000)]
    rtp_per_call: usize,
    #[arg(long, default_value_t = 20)]
    sip_per_call: usize,
}

fn make_sip_item(ts_micros: u64, call_id: &str) -> SipFlowItem {
    let payload = format!(
        "INVITE sip:{call_id}@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776asdhds\r\n\
         From: <sip:alice@example.com>;tag=1928301774\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.1:5060>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: 142\r\n\r\n\
         v=0\r\no=alice 123456 654321 IN IP4 10.0.0.1\r\n\
         s=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
         m=audio 5004 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n"
    );
    SipFlowItem {
        timestamp: ts_micros,
        seq: 0,
        leg: None,
        msg_type: SipFlowMsgType::Sip,
        src_addr: "10.0.0.1:5060".to_string(),
        dst_addr: "10.0.0.2:5060".to_string(),
        payload: Bytes::from(payload),
    }
}

fn make_rtp_item(ts_micros: u64, leg: i32, seq: u16) -> SipFlowItem {
    let mut payload = vec![0x80u8, 0x08]; // V=2, PT=8 (PCMA)
    payload.extend_from_slice(&seq.to_be_bytes());
    payload.extend_from_slice(&(160u32 * seq as u32).to_be_bytes());
    payload.extend_from_slice(&0x12345678u32.to_be_bytes());
    payload.extend_from_slice(&vec![0x55u8; 160]);
    SipFlowItem {
        timestamp: ts_micros,
        seq: 0,
        leg: Some(leg),
        msg_type: SipFlowMsgType::Rtp,
        src_addr: format!("10.0.0.{}:5004", leg + 1),
        dst_addr: String::new(),
        payload: Bytes::from(payload),
    }
}

fn dir_size(path: &PathBuf) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            } else if ft.is_dir() {
                total += dir_size(&entry.path());
            }
        }
    }
    total
}

fn local_dt_from_micros(ts: i64) -> DateTime<Local> {
    Local.timestamp_micros(ts).single().expect("valid ts")
}

struct BenchResult {
    engine: &'static str,
    total_records: usize,
    write_secs: f64,
    write_throughput: f64,
    disk_bytes: u64,
    query_flow_ms: f64,
    query_media_ms: f64,
    flow_count: usize,
    stats_count: usize,
    stats_packets: usize,
    isolation_ok: bool,
}

impl BenchResult {
    fn print_table(results: &[Self]) {
        println!();
        println!(
            "┌──────────┬───────────┬────────────┬───────────────┬───────────┬────────────────┬──────────────────┐"
        );
        println!(
            "│ Engine   │ Records   │ Write (s)  │ Write rec/s   │ Disk (KB) │ Flow Query(ms) │ Media Query(ms)  │"
        );
        println!(
            "├──────────┼───────────┼────────────┼───────────────┼───────────┼────────────────┼──────────────────┤"
        );
        for r in results {
            println!(
                "│ {:<8} │ {:>9} │ {:>10.2} │ {:>13.0} │ {:>9.1} │ {:>14.1} │ {:>16.1} │",
                r.engine,
                r.total_records,
                r.write_secs,
                r.write_throughput,
                r.disk_bytes as f64 / 1024.0,
                r.query_flow_ms,
                r.query_media_ms,
            );
        }
        println!(
            "└──────────┴───────────┴────────────┴───────────────┴───────────┴────────────────┴──────────────────┘"
        );
    }
}

async fn run_bench(engine: SipFlowEngine, args: &Args) -> BenchResult {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let engine_name = if engine == SipFlowEngine::FlowDb {
        "FlowDB"
    } else {
        "SQLite"
    };

    let config = SipFlowConfig::Local {
        root: root.to_string_lossy().to_string(),
        subdirs: SipFlowSubdirs::Daily,
        flush_count: 1000,
        flush_interval_secs: 5,
        id_cache_size: 8192,
        engine,
        ttl_secs: None,
        memtable_size_mb: 64,
        block_cache_capacity_mb: 128,
        upload: None,
    };

    let backend = rustpbx::sipflow::create_backend(&config).unwrap();

    let base_ts = chrono::Utc::now().timestamp_micros() as u64;
    let total_records = args.calls * (args.sip_per_call + args.rtp_per_call);

    println!(
        "\n[{engine_name}] Writing {total_records} records ({calls} calls × {sip} SIP + {rtp} RTP)…",
        calls = args.calls,
        sip = args.sip_per_call,
        rtp = args.rtp_per_call
    );

    let write_start = Instant::now();

    for call_idx in 0..args.calls {
        let call_id = format!("bench-call-{call_idx:06}");
        for sip_idx in 0..args.sip_per_call {
            let ts = base_ts + (call_idx as u64 * 1_000_000) + (sip_idx as u64 * 100_000);
            backend
                .record(&call_id, make_sip_item(ts, &call_id))
                .unwrap();
        }
        for rtp_idx in 0..args.rtp_per_call {
            let leg = (rtp_idx % 2) as i32;
            let ts = base_ts
                + (call_idx as u64 * 1_000_000)
                + (args.sip_per_call as u64 * 100_000)
                + (rtp_idx as u64 * 20_000);
            backend
                .record(&call_id, make_rtp_item(ts, leg, rtp_idx as u16))
                .unwrap();
        }
    }

    backend.flush().await.unwrap();

    let write_secs = write_start.elapsed().as_secs_f64();
    let write_throughput = total_records as f64 / write_secs;

    let disk_bytes = dir_size(&root);

    // ── Correctness verification (full time range) ───────────────────────
    // Query the first call with a wide time range to verify all records are returned.
    let probe_call = format!("bench-call-{:06}", 0);
    let call_start = base_ts as i64;
    let call_end = (base_ts
        + (args.sip_per_call as u64 * 100_000)
        + (args.rtp_per_call as u64 * 20_000)
        + 1_000_000) as i64;

    let flow_items = backend
        .query_flow(
            &probe_call,
            local_dt_from_micros(call_start - 1),
            local_dt_from_micros(call_end),
        )
        .await
        .unwrap();
    let flow_count = flow_items.len();

    let media_stats = backend
        .query_media_stats(
            &probe_call,
            local_dt_from_micros(call_start - 1),
            local_dt_from_micros(call_end),
        )
        .await
        .unwrap();
    let stats_count = media_stats.len();
    let stats_packets: usize = media_stats.iter().map(|s| s.packet_count).sum();

    // Cross-talk isolation: querying a non-existent call should return 0 flow items.
    let ghost = backend
        .query_flow(
            "does-not-exist-999",
            local_dt_from_micros(call_start - 1),
            local_dt_from_micros(call_end),
        )
        .await
        .unwrap();
    let isolation_ok = ghost.is_empty();

    // ── Latency probe (narrow range — first second of a middle call) ─────
    let probe_call2 = format!("bench-call-{:06}", args.calls / 2);
    let probe_start_ts = base_ts + ((args.calls / 2) as u64 * 1_000_000);

    let q_start = Instant::now();
    let _flow = backend
        .query_flow(
            &probe_call2,
            local_dt_from_micros(probe_start_ts as i64 - 1),
            local_dt_from_micros(probe_start_ts as i64 + 1_000_000),
        )
        .await
        .unwrap();
    let query_flow_ms = q_start.elapsed().as_secs_f64() * 1000.0;

    let q_start2 = Instant::now();
    let _wav = backend
        .query_media(
            &probe_call2,
            local_dt_from_micros(probe_start_ts as i64 - 1),
            local_dt_from_micros(probe_start_ts as i64 + 1_000_000),
        )
        .await
        .unwrap();
    let query_media_ms = q_start2.elapsed().as_secs_f64() * 1000.0;

    let expected_sip = args.sip_per_call;
    let expected_rtp = args.rtp_per_call;
    let flow_ok = if flow_count == expected_sip {
        "OK"
    } else {
        "MISMATCH"
    };
    let rtp_ok = if stats_packets == expected_rtp {
        "OK"
    } else {
        "MISMATCH"
    };
    let iso_ok = if isolation_ok { "OK" } else { "FAIL" };

    println!(
        "  Write: {write_secs:.2}s ({write_throughput:.0} rec/s)  Disk: {:.1} KB  Flow: {query_flow_ms:.1} ms  Media: {query_media_ms:.1} ms",
        disk_bytes as f64 / 1024.0
    );
    println!(
        "  Verify: SIP flow={flow_count}/{expected_sip} [{flow_ok}]  RTP stats={stats_packets}/{expected_rtp} [{rtp_ok}]  legs={stats_count}  isolation=[{iso_ok}]"
    );

    drop(dir);

    BenchResult {
        engine: engine_name,
        total_records,
        write_secs,
        write_throughput,
        disk_bytes,
        query_flow_ms,
        query_media_ms,
        flow_count,
        stats_count,
        stats_packets,
        isolation_ok,
    }
}

fn main() {
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let total_records = args.calls * (args.sip_per_call + args.rtp_per_call);

        println!("╔═══════════════════════════════════════════════════════════════════════╗");
        println!("║           SipFlow Backend Benchmark: FlowDB vs SQLite                  ║");
        println!("╚═══════════════════════════════════════════════════════════════════════╝");
        println!();
        println!("Configuration:");
        println!("  Calls:         {}", args.calls);
        println!("  SIP/call:      {}", args.sip_per_call);
        println!("  RTP/call:      {}", args.rtp_per_call);
        println!("  Total records: {}", total_records);

        let mut results = Vec::new();

        results.push(run_bench(SipFlowEngine::Sqlite, &args).await);
        results.push(run_bench(SipFlowEngine::FlowDb, &args).await);

        BenchResult::print_table(&results);

        let sqlite = &results[0];
        let flowdb = &results[1];

        // Correctness cross-check
        println!();
        println!("Correctness:");
        let flow_match = sqlite.flow_count == flowdb.flow_count;
        let rtp_match = sqlite.stats_packets == flowdb.stats_packets;
        println!(
            "  SIP flow count:  SQLite={} FlowDB={}  [{}]",
            sqlite.flow_count,
            flowdb.flow_count,
            if flow_match { "MATCH" } else { "DIFF" }
        );
        println!(
            "  RTP packet sum:  SQLite={} FlowDB={}  [{}]",
            sqlite.stats_packets,
            flowdb.stats_packets,
            if rtp_match { "MATCH" } else { "DIFF" }
        );
        println!(
            "  Isolation:       SQLite={} FlowDB={}",
            sqlite.isolation_ok, flowdb.isolation_ok
        );

        println!();
        println!("Summary:");
        let speedup = flowdb.write_throughput / sqlite.write_throughput;
        println!(
            "  Write throughput: FlowDB {:.0} rec/s vs SQLite {:.0} rec/s ({:.2}x {})",
            flowdb.write_throughput,
            sqlite.write_throughput,
            speedup,
            if speedup >= 1.0 { "faster" } else { "slower" }
        );
        let space_ratio = flowdb.disk_bytes as f64 / sqlite.disk_bytes.max(1) as f64;
        println!(
            "  Disk space:       FlowDB {:.1} KB vs SQLite {:.1} KB ({:.2}x {})",
            flowdb.disk_bytes as f64 / 1024.0,
            sqlite.disk_bytes as f64 / 1024.0,
            if space_ratio >= 1.0 {
                space_ratio
            } else {
                1.0 / space_ratio
            },
            if space_ratio < 1.0 {
                "smaller"
            } else {
                "larger"
            }
        );
        let query_ratio = sqlite.query_flow_ms / flowdb.query_flow_ms.max(0.001);
        println!(
            "  Flow query:       FlowDB {:.1} ms vs SQLite {:.1} ms ({:.2}x {})",
            flowdb.query_flow_ms,
            sqlite.query_flow_ms,
            query_ratio,
            if query_ratio >= 1.0 {
                "faster"
            } else {
                "slower"
            }
        );
    });
}
