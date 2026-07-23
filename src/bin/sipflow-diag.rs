use anyhow::Result;
use chrono::Local;
use clap::Parser;
use std::io::Write;
use std::path::Path;

use rustpbx::config::SipFlowSubdirs;
use rustpbx::sipflow::diag::{self, DiagReport};

#[derive(Parser, Debug)]
#[command(
    name = "sipflow-diag",
    about = "SipFlow diagnostic tool — query SIP/RTP data by Call-ID"
)]
struct Args {
    /// Call-ID to search for
    #[arg(short, long)]
    call_id: String,

    /// SipFlow data root directory
    #[arg(short = 'r', long, default_value = "./config/sipflow")]
    dir: String,

    /// Start time: YYYYMMDD, YYYYMMDDHH, YYYYMMDDHH24MISS, or Unix timestamp.
    /// Overrides --date and --window-hours.
    #[arg(long)]
    start: Option<String>,

    /// End time: same formats as --start.
    /// Overrides --date and --window-hours.
    #[arg(long)]
    end: Option<String>,

    /// Limit to a specific date (YYYYMMDD). Ignored when --start/--end are set.
    #[arg(short = 'D', long)]
    date: Option<String>,

    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Show raw SIP message payloads
    #[arg(short, long)]
    verbose: bool,

    /// Time window (hours before/after) when neither --date nor --start/--end
    /// is given
    #[arg(long, default_value = "72")]
    window_hours: i64,

    /// Subdirectory layout: auto, none, daily, hourly
    #[arg(long, default_value = "auto")]
    subdirs: String,

    /// Dump SIP flow as JSONL (default: {callid}.jsonl)
    #[arg(long)]
    dump_jsonl: bool,

    /// JSONL output path (overrides default)
    #[arg(long)]
    jsonl_path: Option<String>,

    /// Dump RTP audio as WAV (default: {callid}.wav)
    #[arg(long)]
    dump_wav: bool,

    /// WAV output path (overrides default)
    #[arg(long)]
    wav_path: Option<String>,
}

const BUCKET_SECS: f64 = 10.0;

fn parse_subdirs(s: &str) -> SipFlowSubdirs {
    match s.to_lowercase().as_str() {
        "none" => SipFlowSubdirs::None,
        "daily" => SipFlowSubdirs::Daily,
        "hourly" => SipFlowSubdirs::Hourly,
        _ => SipFlowSubdirs::Daily,
    }
}

fn print_report(report: &DiagReport, verbose: bool) {
    let line = "\u{2550}".repeat(78);
    let dash = "\u{2500}".repeat(78);

    println!("{line}");
    println!(" SipFlow Diagnostic Report");
    println!("{line}");
    println!();
    println!(" Call-ID:     {}", report.call_id);
    println!(" Directory:   {}", report.root_dir);
    println!(" Buckets:     {} directories scanned", report.bucket_count);
    println!(
        " Engine(s):   {}",
        report
            .buckets
            .iter()
            .map(|b| b.engine.as_str())
            .collect::<std::collections::BTreeSet<&str>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(" + ")
    );
    if !report.start_time.is_empty() && !report.end_time.is_empty() {
        println!(
            " Range:       {}  →  {}",
            report.start_time, report.end_time
        );
        println!(
            " Duration:    {:.1}s ({})",
            report.duration_secs,
            format_duration(report.duration_secs)
        );
    }
    println!();

    // ── SIP Flow ────────────────────────────────────────────
    println!("{dash}");
    println!(" SIP Signaling  ({} messages)", report.sip_count);
    println!("{dash}");

    if report.sip_messages.is_empty() {
        println!(" ─ No SIP messages found for this Call-ID.");
    } else {
        println!(
            " {:<26} {:<19} {:<19} {}",
            "Timestamp", "Source", "Destination", "Message"
        );
        println!(" {dash}");

        for msg in &report.sip_messages {
            let ts = diag::dt_from_micros(msg.timestamp as i64);
            let first_line = diag::sip_message_status(&msg.payload);
            println!(
                " {}  {:<19} {:<19} {}",
                ts, msg.src_addr, msg.dst_addr, first_line
            );

            if verbose {
                if let Some(text) = msg.message_text() {
                    for line in text.lines() {
                        if line.trim().is_empty() {
                            break;
                        }
                        println!("   {}", line.trim());
                    }
                }
            }
        }
    }
    println!();

    // ── RTP Media ───────────────────────────────────────────
    println!("{dash}");
    println!(" RTP MEDIA");
    println!("{dash}");
    println!("  Total packets : {}", report.rtp_detail.total_packets);
    println!(
        "  Duration      : {:.1}s ({})",
        report.duration_secs,
        format_duration(report.duration_secs)
    );
    if report.rtp_detail.max_jitter_ms.is_some() || report.rtp_detail.loss_percent > 0.0 {
        println!(
            "  Loss          : {} pkts ({:.2}%)",
            report.rtp_detail.total_lost, report.rtp_detail.loss_percent
        );
        if let Some(j) = report.rtp_detail.max_jitter_ms {
            println!("  Max jitter    : {:.2}ms", j);
        }
    }

    // Payload map
    if !report.rtp_detail.payload_map.is_empty() {
        let pm: Vec<String> = report
            .rtp_detail
            .payload_map
            .iter()
            .map(|(pt, (name, clock))| format!("{}: ('{}', {})", pt, name, clock))
            .collect();
        println!("  Payload map   : {{{}}}", pm.join(", "));
    }

    if report.rtp_detail.legs.is_empty() {
        println!();
        println!(" ─ No RTP media data found for this Call-ID.");
    } else {
        // Leg sources
        let leg_srcs: Vec<String> = report
            .rtp_detail
            .legs
            .iter()
            .map(|leg| {
                format!(
                    "{}: [{}]",
                    leg.leg,
                    leg.ssrcs
                        .iter()
                        .map(|s| format!("{}:{:.0}", s.ssrc, s.packet_count))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect();
        println!("  Leg sources   : {{{}}}", leg_srcs.join(", "));
        println!();

        for leg in &report.rtp_detail.legs {
            let short_line = "\u{2500}".repeat(78);
            println!("{}", short_line);
            println!(
                "  Leg {} ({}{}): {} packets",
                leg.leg,
                if leg.leg == 0 { "caller" } else { "callee" },
                if leg.leg == 0 { "/agent" } else { "/customer" },
                leg.total_packets
            );
            println!("{}", short_line);
            println!("  Time range    : {:.1}s", leg.duration_secs);

            // SSRCs
            if !leg.ssrcs.is_empty() {
                println!("  SSRCs         : {}", leg.ssrcs.len());
                for ssrc in &leg.ssrcs {
                    println!(
                        "    0x{:08X}:  {} pkts, t={:8.1}s..{:8.1}s ({:.1}s)",
                        ssrc.ssrc,
                        ssrc.packet_count,
                        ssrc.time_start_secs,
                        ssrc.time_end_secs,
                        ssrc.time_end_secs - ssrc.time_start_secs
                    );
                }
            }

            // Payload types
            if !leg.payload_type_counts.is_empty() {
                let pt_str: Vec<String> = leg
                    .payload_type_counts
                    .iter()
                    .map(|(pt, count)| {
                        format!(
                            "PT={}({}):{}",
                            pt,
                            diag::rtp_codec_name(*pt, &report.rtp_detail.payload_map),
                            count
                        )
                    })
                    .collect();
                println!("  Payload types : {}", pt_str.join(", "));
            }

            // Time distribution
            if !leg.time_buckets.is_empty() {
                let expected = (BUCKET_SECS * 50.0) as usize; // ~50pps default
                println!();
                println!(
                    "  Time distribution ({}s buckets, expected ~{} pkts/bucket):",
                    BUCKET_SECS, expected
                );
                println!("      {:>8} {:>8} {}", "Bucket", "Time", "Pkts  Bar");
                for bucket in &leg.time_buckets {
                    let t_start = bucket.bucket_start as usize;
                    let t_end = t_start + BUCKET_SECS as usize;
                    let low_flag = if bucket.packet_count < expected / 2 {
                        " ** LOW **"
                    } else {
                        ""
                    };
                    println!(
                        "  [{:>3}] {:>4}-{:<4} s {:>6}  {} {}",
                        t_start / (BUCKET_SECS as usize),
                        t_start,
                        t_end,
                        bucket.packet_count,
                        bucket.bar,
                        low_flag,
                    );
                }
            }

            // Gaps
            if leg.gaps_over_1s.is_empty() {
                println!();
                println!("  No inter-packet gaps > 1s");
            } else {
                println!();
                println!("  Inter-packet gaps > 1s: {}", leg.gaps_over_1s.len());
                for gap in leg.gaps_over_1s.iter().take(5) {
                    println!("    t={:.1}s gap={:.0}ms", gap.time_secs, gap.gap_ms);
                }
                if leg.gaps_over_1s.len() > 5 {
                    println!("    ... and {} more", leg.gaps_over_1s.len() - 5);
                }
            }

            // TS jumps
            if leg.ts_jumps.is_empty() {
                println!("  No RTP timestamp discontinuities detected");
            } else {
                println!("  RTP ts jumps: {}", leg.ts_jumps.len());
                for jump in leg.ts_jumps.iter().take(5) {
                    println!(
                        "    t={:.1}s ssrc=0x{:08X} delta={} (capture_delta={}us)",
                        jump.time_secs, jump.ssrc, jump.delta, jump.capture_delta_us
                    );
                }
                if leg.ts_jumps.len() > 5 {
                    println!("    ... and {} more", leg.ts_jumps.len() - 5);
                }
            }
            println!();
        }

        // Cross-leg summary
        if let Some(ref cross) = report.rtp_detail.cross_leg {
            let short_line = "\u{2500}".repeat(78);
            println!("{}", short_line);
            println!("  CROSS-LEG SUMMARY");
            println!("{}", short_line);

            for leg in &report.rtp_detail.legs {
                let t_start_s = leg.time_start_secs as usize;
                let t_end_s = leg.time_end_secs as usize;
                println!(
                    "  Leg {} ({}):  {} pkts, t={}s..t={}s, duration={:.1}s",
                    leg.leg,
                    if leg.leg == 0 { "caller" } else { "callee" },
                    leg.total_packets,
                    t_start_s,
                    t_end_s,
                    leg.duration_secs
                );
            }
            println!();

            let leg0 = report.rtp_detail.legs.first();
            let leg1 = report.rtp_detail.legs.get(1);
            println!(
                "      {:>8} {:>8} {:>8} {:>8}",
                "Time",
                if leg0.is_some() { "Leg A" } else { "Leg0" },
                if leg1.is_some() { "Leg B" } else { "Leg1" },
                "Diff"
            );
            for bucket in &cross.buckets {
                let diff = (bucket.leg0_packets as i64 - bucket.leg1_packets as i64).unsigned_abs();
                println!(
                    "  {:>4}-{:<4} s {:>8} {:>8} {:>8}",
                    bucket.time_start as usize,
                    bucket.time_end as usize,
                    bucket.leg0_packets,
                    bucket.leg1_packets,
                    diff,
                );
            }
        }
    }
    println!();
    println!("{line}");
    println!();
}

fn format_duration(secs: f64) -> String {
    let total_secs = secs as u64;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{}h {}m {}s", h, m, s)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let subdirs = parse_subdirs(&args.subdirs);

    // Determine time range
    let (start, end) = if let (Some(s), Some(e)) = (args.start.as_ref(), args.end.as_ref()) {
        let start = diag::parse_datetime(s)
            .ok_or_else(|| anyhow::anyhow!("Invalid --start format: {}", s))?;
        let end = diag::parse_datetime(e)
            .ok_or_else(|| anyhow::anyhow!("Invalid --end format: {}", e))?;
        (start, end)
    } else if let Some(ref date_str) = args.date {
        let start = diag::parse_datetime(date_str)
            .ok_or_else(|| anyhow::anyhow!("Invalid --date format: {}", date_str))?;
        let end = start + chrono::Duration::days(1);
        (start, end)
    } else {
        let now = Local::now();
        let window = chrono::Duration::hours(args.window_hours);
        (now - window, now + window)
    };

    if !Path::new(&args.dir).exists() {
        anyhow::bail!("Directory not found: {}", args.dir);
    }

    let report = diag::run_diag(&args.call_id, &args.dir, subdirs, start, end).await?;

    if report.is_empty() {
        eprintln!("No data found for Call-ID: {}", args.call_id);
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        std::process::exit(1);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report, args.verbose);
    }

    // ── Dump JSONL ────────────────────────────────────────────
    if args.dump_jsonl || args.jsonl_path.is_some() {
        let path = args
            .jsonl_path
            .unwrap_or_else(|| format!("{}.jsonl", &args.call_id));
        let file = std::fs::File::create(&path)
            .map_err(|e| anyhow::anyhow!("Cannot create {}: {}", path, e))?;
        let mut writer = std::io::BufWriter::new(file);
        for item in &report.sip_messages {
            let payload_str = String::from_utf8_lossy(&item.payload);
            let line = serde_json::json!({
                "timestamp": item.timestamp,
                "time": diag::dt_from_micros(item.timestamp as i64),
                "src_addr": item.src_addr,
                "dst_addr": item.dst_addr,
                "msg_type": item.msg_type,
                "message": payload_str,
            });
            writeln!(writer, "{}", serde_json::to_string(&line)?)?;
        }
        writer.flush()?;
        eprintln!("Wrote {} SIP messages to {}", report.sip_count, path);
    }

    // ── Dump WAV ──────────────────────────────────────────────
    if args.dump_wav || args.wav_path.is_some() {
        if report.rtp_raw_packets.is_empty() {
            eprintln!("No RTP packets to export as WAV");
        } else {
            let path = args
                .wav_path
                .unwrap_or_else(|| format!("{}.wav", &args.call_id));
            let wav_bytes =
                rustpbx::sipflow::wav_utils::generate_wav_from_packets(&report.rtp_raw_packets)?;
            std::fs::write(&path, &wav_bytes)
                .map_err(|e| anyhow::anyhow!("Cannot write {}: {}", path, e))?;
            eprintln!(
                "Wrote {} RTP packets to {} ({} bytes WAV)",
                report.rtp_raw_packets.len(),
                path,
                wav_bytes.len()
            );
        }
    }

    Ok(())
}
