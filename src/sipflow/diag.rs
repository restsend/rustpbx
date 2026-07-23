use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::SipFlowSubdirs;
use crate::sipflow::flowdb_backend::FlowDbBackend;
use crate::sipflow::rtp_stats::parse_rtp_stats_header;
use crate::sipflow::sdp_utils::extract_sdp;
use crate::sipflow::storage::{StorageManager, discover_data_dirs};
use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMediaStats, SipFlowMsgType};

// ── SDP / Payload Map ─────────────────────────────────────

pub fn parse_payload_map(sip_messages: &[SipFlowItem]) -> HashMap<u8, (String, u32)> {
    let mut map: HashMap<u8, (String, u32)> = HashMap::new();
    for item in sip_messages {
        if item.msg_type != SipFlowMsgType::Sip {
            continue;
        }
        let text = String::from_utf8_lossy(&item.payload);
        if let Some(sdp) = extract_sdp(&text) {
            for line in sdp.lines() {
                if let Some(rest) = line.trim().strip_prefix("a=rtpmap:") {
                    let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                    if parts.len() == 2 {
                        if let Ok(pt) = parts[0].parse::<u8>() {
                            let codec_parts: Vec<&str> = parts[1].split('/').collect();
                            if let Some(name) = codec_parts.first() {
                                let clock = codec_parts
                                    .get(1)
                                    .and_then(|s| s.parse::<u32>().ok())
                                    .unwrap_or(8000);
                                map.entry(pt).or_insert_with(|| (name.to_string(), clock));
                            }
                        }
                    }
                }
            }
        }
    }
    map
}

pub fn rtp_codec_name(pt: u8, payload_map: &HashMap<u8, (String, u32)>) -> String {
    if let Some((name, _)) = payload_map.get(&pt) {
        return name.clone();
    }
    match pt {
        0 => "PCMU",
        3 => "GSM",
        4 => "G723",
        8 => "PCMA",
        9 => "G722",
        13 => "CN",
        18 => "G729",
        63 => "RED",
        96..=127 => "Dynamic",
        _ => "Unknown",
    }
    .to_string()
}

// ── RTP Packet Raw Analysis ───────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct RtpSsrcReport {
    pub ssrc: u32,
    pub packet_count: usize,
    pub time_start_secs: f64,
    pub time_end_secs: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimeBucket {
    pub bucket_start: f64,
    pub packet_count: usize,
    pub bar: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GapReport {
    pub time_secs: f64,
    pub gap_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsJumpReport {
    pub time_secs: f64,
    pub ssrc: u32,
    pub delta: i64,
    pub capture_delta_us: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrossLegBucket {
    pub time_start: f64,
    pub time_end: f64,
    pub leg0_packets: usize,
    pub leg1_packets: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrossLegReport {
    pub bucket_interval_secs: f64,
    pub buckets: Vec<CrossLegBucket>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RtpLegReport {
    pub leg: i32,
    pub label: String,
    pub src: String,
    pub total_packets: usize,
    pub time_start_secs: f64,
    pub time_end_secs: f64,
    pub duration_secs: f64,
    pub ssrcs: Vec<RtpSsrcReport>,
    pub payload_type_counts: BTreeMap<u8, usize>,
    pub time_buckets: Vec<TimeBucket>,
    pub gaps_over_1s: Vec<GapReport>,
    pub ts_jumps: Vec<TsJumpReport>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DetailedRtpReport {
    pub total_packets: usize,
    pub total_lost: u64,
    pub loss_percent: f64,
    pub legs: Vec<RtpLegReport>,
    pub cross_leg: Option<CrossLegReport>,
    pub payload_map: HashMap<u8, (String, u32)>,
    pub leg_pt_map: HashMap<i32, BTreeMap<u8, (String, u32)>>,
    pub max_jitter_ms: Option<f64>,
}

// ── Enhanced DiagReport ───────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct BucketInfo {
    pub path: String,
    pub engine: String,
    pub sip_msgs: usize,
    pub rtp_packets: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagReport {
    pub call_id: String,
    pub root_dir: String,
    pub buckets: Vec<BucketInfo>,
    pub bucket_count: usize,
    pub sip_messages: Vec<SipFlowItem>,
    pub sip_count: usize,
    pub rtp_stats: Vec<SipFlowMediaStats>,
    pub duration_secs: f64,
    pub start_time: String,
    pub end_time: String,
    pub rtp_detail: DetailedRtpReport,
    #[serde(skip)]
    pub rtp_raw_packets: Vec<(i32, u64, Vec<u8>)>,
}

impl DiagReport {
    pub fn is_empty(&self) -> bool {
        self.sip_messages.is_empty() && self.rtp_stats.is_empty()
    }
}

// ── SIP Status Extraction ─────────────────────────────────

pub fn sip_message_first_line(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    text.lines()
        .next()
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

pub fn sip_message_status(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("INVITE ")
            || trimmed.starts_with("ACK ")
            || trimmed.starts_with("BYE ")
            || trimmed.starts_with("CANCEL")
            || trimmed.starts_with("REGISTER")
            || trimmed.starts_with("OPTIONS")
            || trimmed.starts_with("INFO ")
            || trimmed.starts_with("UPDATE")
            || trimmed.starts_with("PRACK")
            || trimmed.starts_with("REFER")
            || trimmed.starts_with("NOTIFY")
            || trimmed.starts_with("SUBSCRIBE")
            || trimmed.starts_with("MESSAGE")
            || trimmed.starts_with("PUBLISH")
        {
            let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
            if parts.len() >= 2 {
                return format!("{} {}", parts[0], parts[1]);
            }
            return trimmed.to_string();
        }

        if let Some(rest) = trimmed.strip_prefix("SIP/2.0 ") {
            if rest.len() >= 3 {
                let code_str = &rest[..3];
                if code_str.chars().all(|c| c.is_ascii_digit()) {
                    let phrase = rest[3..].trim();
                    if !phrase.is_empty() {
                        return format!("{} {}", code_str, phrase);
                    }
                    return code_str.to_string();
                }
            }
        }
    }
    text.lines()
        .next()
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

// ── RTP Packet Analysis ───────────────────────────────────

const BUCKET_SECS: f64 = 10.0;
const BAR_WIDTH: usize = 40;
const GAP_THRESHOLD_MS: f64 = 1000.0;
const TS_JUMP_THRESHOLD_TICKS: i64 = 8000; // ~1s of PCMU ticks

fn compute_expected_pps(payload_map: &HashMap<u8, (String, u32)>, pt: u8) -> f64 {
    let clock = payload_map.get(&pt).map(|(_, c)| *c).unwrap_or(8000);
    let ptime_secs = 0.020; // default 20ms
    (clock as f64 * ptime_secs).round().max(1.0)
}

fn leg_label(leg: i32) -> String {
    match leg {
        0 => "Caller (Leg A)".into(),
        1 => "Callee (Leg B)".into(),
        _ => format!("Leg {}", leg),
    }
}

/// Analyze raw RTP packets collected from storage and return detailed per-leg report.
pub fn analyze_raw_rtp_packets(
    packets: Vec<(i32, u64, Vec<u8>)>,
    sip_messages: &[SipFlowItem],
    rtp_stats: &[SipFlowMediaStats],
) -> DetailedRtpReport {
    let payload_map = parse_payload_map(sip_messages);

    // Build leg PT map from SDP
    let mut leg_pt_map: HashMap<i32, BTreeMap<u8, (String, u32)>> = HashMap::new();
    for item in sip_messages {
        if item.msg_type != SipFlowMsgType::Sip {
            continue;
        }
        let text = String::from_utf8_lossy(&item.payload);
        if let Some(sdp) = extract_sdp(&text) {
            // Determine leg from addr: if src_addr contains pbx addr it's leg1
            for line in sdp.lines() {
                if let Some(rest) = line.trim().strip_prefix("a=rtpmap:") {
                    let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                    if parts.len() == 2 {
                        if let Ok(pt) = parts[0].parse::<u8>() {
                            let cp: Vec<&str> = parts[1].split('/').collect();
                            let name = cp.first().unwrap_or(&"?").to_string();
                            let clock = cp
                                .get(1)
                                .and_then(|s| s.parse::<u32>().ok())
                                .unwrap_or(8000);
                            // add to all legs - we can't reliably map SDP to leg
                            for leg in 0..=1 {
                                leg_pt_map
                                    .entry(leg)
                                    .or_default()
                                    .entry(pt)
                                    .or_insert((name.clone(), clock));
                            }
                        }
                    }
                }
            }
        }
    }

    // Aggregate stats
    let total_lost: u64 = rtp_stats.iter().map(|s| s.lost_packets).sum();
    let total_expected: u64 = rtp_stats.iter().map(|s| s.expected_packets).sum();
    let loss_percent = if total_expected > 0 {
        total_lost as f64 / total_expected as f64 * 100.0
    } else {
        0.0
    };
    let max_jitter = rtp_stats
        .iter()
        .filter_map(|s| s.jitter_ms)
        .fold(f64::NEG_INFINITY, f64::max);
    let max_jitter_ms = if max_jitter.is_finite() {
        Some(max_jitter)
    } else {
        None
    };

    if packets.is_empty() {
        return DetailedRtpReport {
            total_packets: 0,
            total_lost,
            loss_percent,
            payload_map,
            leg_pt_map,
            max_jitter_ms,
            ..Default::default()
        };
    }

    // Group by leg
    let mut leg_packets: BTreeMap<i32, Vec<(u64, Vec<u8>)>> = BTreeMap::new();
    for (leg, ts, payload) in packets {
        leg_packets.entry(leg).or_default().push((ts, payload));
    }

    let ref_start = leg_packets
        .values()
        .flat_map(|v| v.first())
        .map(|(ts, _)| *ts)
        .min()
        .unwrap_or(0) as f64;

    let mut legs = Vec::new();

    for (&leg, pkts) in &leg_packets {
        let first_ts = pkts.first().map(|(t, _)| *t).unwrap_or(0) as f64;
        let last_ts = pkts.last().map(|(t, _)| *t).unwrap_or(0) as f64;

        let time_start = (first_ts - ref_start) / 1_000_000.0;
        let time_end = (last_ts - ref_start) / 1_000_000.0;
        let duration = if time_end > time_start {
            time_end - time_start
        } else {
            0.0
        };

        // SSRC tracking
        let mut ssrcs: HashMap<u32, RtpSsrcReport> = HashMap::new();
        let mut pt_counts: BTreeMap<u8, usize> = BTreeMap::new();
        let mut gaps: Vec<GapReport> = Vec::new();
        let mut jumps: Vec<TsJumpReport> = Vec::new();
        let num_buckets = (duration / BUCKET_SECS).ceil() as usize + 1;
        let mut bucket_counts = vec![0usize; num_buckets.max(1)];

        let mut prev_ts: Option<u64> = None;
        let mut prev_ssrc: Option<u32> = None;
        let mut prev_rtp_ts: Option<u32> = None;

        for (capture_ts, payload) in pkts {
            let rel = (*capture_ts as f64 - ref_start) / 1_000_000.0;
            let bi = (rel / BUCKET_SECS) as usize;
            if bi < bucket_counts.len() {
                bucket_counts[bi] += 1;
            }

            if let Some(header) = parse_rtp_stats_header(payload) {
                // SSRC
                let e = ssrcs.entry(header.ssrc).or_insert_with(|| RtpSsrcReport {
                    ssrc: header.ssrc,
                    packet_count: 0,
                    time_start_secs: rel,
                    time_end_secs: rel,
                });
                e.packet_count += 1;
                if rel < e.time_start_secs {
                    e.time_start_secs = rel;
                }
                if rel > e.time_end_secs {
                    e.time_end_secs = rel;
                }

                // PT count
                *pt_counts.entry(header.payload_type).or_insert(0) += 1;

                // Gap detection
                if let Some(pt) = prev_ts {
                    let gap_us = *capture_ts - pt;
                    if gap_us > (GAP_THRESHOLD_MS * 1000.0) as u64 {
                        gaps.push(GapReport {
                            time_secs: rel,
                            gap_ms: gap_us as f64 / 1000.0,
                        });
                    }
                }
                prev_ts = Some(*capture_ts);

                // TS jump detection
                if let Some(prev_ssrc_val) = prev_ssrc {
                    if prev_ssrc_val == header.ssrc {
                        if let Some(prev_ts_val) = prev_rtp_ts {
                            let delta =
                                (header.rtp_timestamp as i64).wrapping_sub(prev_ts_val as i64);
                            let expected = (capture_ts.wrapping_sub(prev_ts.unwrap_or(*capture_ts)) as f64 / 1_000_000.0
                                * compute_expected_pps(&payload_map, header.payload_type)
                                * 160.0) // approximate PCMU samples per tick
                                as i64;

                            if delta.abs() >= TS_JUMP_THRESHOLD_TICKS
                                && (delta - expected).abs() > TS_JUMP_THRESHOLD_TICKS
                            {
                                jumps.push(TsJumpReport {
                                    time_secs: rel,
                                    ssrc: header.ssrc,
                                    delta,
                                    capture_delta_us: (capture_ts
                                        .wrapping_sub(prev_ts.unwrap_or(*capture_ts)))
                                        as i64,
                                });
                            }
                        }
                    }
                }
                prev_ssrc = Some(header.ssrc);
                prev_rtp_ts = Some(header.rtp_timestamp);
            }
        }

        // Bucket bar charts
        let max_count = bucket_counts.iter().max().copied().unwrap_or(1).max(1);
        let time_buckets: Vec<TimeBucket> = bucket_counts
            .into_iter()
            .enumerate()
            .map(|(i, count)| {
                let bar_len = (count as f64 / max_count as f64 * BAR_WIDTH as f64).round() as usize;
                let bar = "#".repeat(bar_len.min(BAR_WIDTH)).to_string();
                TimeBucket {
                    bucket_start: i as f64 * BUCKET_SECS,
                    packet_count: count,
                    bar,
                }
            })
            .collect();

        legs.push(RtpLegReport {
            leg,
            label: leg_label(leg),
            src: leg_packets
                .get(&leg)
                .and_then(|p| p.first())
                .map(|(_, p)| {
                    parse_rtp_stats_header(p)
                        .map(|h| format!("0x{:08X}", h.ssrc))
                        .unwrap_or_default()
                })
                .unwrap_or_default(),
            total_packets: pkts.len(),
            time_start_secs: time_start,
            time_end_secs: time_end,
            duration_secs: duration,
            ssrcs: ssrcs.into_values().collect(),
            payload_type_counts: pt_counts,
            time_buckets,
            gaps_over_1s: gaps,
            ts_jumps: jumps,
        });
    }

    // Cross-leg comparison
    let cross_leg = if legs.len() >= 2 {
        let max_buckets = legs.iter().map(|l| l.time_buckets.len()).max().unwrap_or(0);
        let buckets: Vec<CrossLegBucket> = (0..max_buckets)
            .map(|i| {
                let b0 = legs[0]
                    .time_buckets
                    .get(i)
                    .map(|b| b.packet_count)
                    .unwrap_or(0);
                let b1 = legs[1]
                    .time_buckets
                    .get(i)
                    .map(|b| b.packet_count)
                    .unwrap_or(0);
                CrossLegBucket {
                    time_start: i as f64 * BUCKET_SECS,
                    time_end: (i + 1) as f64 * BUCKET_SECS,
                    leg0_packets: b0,
                    leg1_packets: b1,
                }
            })
            .collect();
        Some(CrossLegReport {
            bucket_interval_secs: BUCKET_SECS,
            buckets,
        })
    } else {
        None
    };

    let total_packets: usize = legs.iter().map(|l| l.total_packets).sum();

    DetailedRtpReport {
        total_packets,
        total_lost,
        loss_percent,
        legs,
        cross_leg,
        payload_map,
        leg_pt_map,
        max_jitter_ms,
    }
}

// ── Main Diagnostic Query ─────────────────────────────────

pub async fn run_diag(
    call_id: &str,
    root_dir: &str,
    subdirs: SipFlowSubdirs,
    start: DateTime<Local>,
    end: DateTime<Local>,
) -> Result<DiagReport> {
    let base = PathBuf::from(root_dir);
    if !base.exists() {
        anyhow::bail!("Directory does not exist: {}", root_dir);
    }

    let buckets = discover_data_dirs(&base, &subdirs, start, end);

    let mut bucket_infos = Vec::new();

    let mut all_sip: Vec<SipFlowItem> = Vec::new();
    let mut all_rtp_stats: Vec<SipFlowMediaStats> = Vec::new();
    let mut all_rtp_packets: Vec<(i32, u64, Vec<u8>)> = Vec::new();

    // SQLite pass
    {
        let mut sqlite = StorageManager::new(&base, subdirs.clone(), None, None);

        if let Ok(items) = sqlite.query_flow(call_id, start, end).await {
            let n = items.len();
            all_sip.extend(items);
            bucket_infos.push(BucketInfo {
                path: root_dir.to_string(),
                engine: "sqlite".into(),
                sip_msgs: n,
                rtp_packets: 0,
            });
        } else {
            bucket_infos.push(BucketInfo {
                path: root_dir.to_string(),
                engine: "sqlite".into(),
                sip_msgs: 0,
                rtp_packets: 0,
            });
        }

        if let Ok(stats) = sqlite.query_media_stats(call_id, start, end).await {
            let n: usize = stats.iter().map(|s| s.packet_count).sum();
            all_rtp_stats.extend(stats);
            if let Some(b) = bucket_infos.last_mut() {
                b.rtp_packets += n;
            }
        }

        if let Ok(pkts) = sqlite.query_media_packets(call_id, start, end).await {
            for p in pkts {
                all_rtp_packets.push((p.leg, p.timestamp, p.payload));
            }
        }
    }

    // FlowDB pass
    if let Ok(flowdb) = FlowDbBackend::new(&base, subdirs, None, 64, 128, 1000, 3600) {
        let flowdb = Arc::new(flowdb);

        if let Ok(items) = flowdb.query_flow(call_id, start, end).await {
            let n = items.len();
            all_sip.extend(items);
            if n > 0 {
                bucket_infos.push(BucketInfo {
                    path: root_dir.to_string(),
                    engine: "flowdb".into(),
                    sip_msgs: n,
                    rtp_packets: 0,
                });
            }
        }

        if let Ok(stats) = flowdb.query_media_stats(call_id, start, end).await {
            let n: usize = stats.iter().map(|s| s.packet_count).sum();
            all_rtp_stats.extend(stats);
            if n > 0 {
                if let Some(b) = bucket_infos.iter_mut().find(|b| b.engine == "flowdb") {
                    b.rtp_packets += n;
                }
            }
        }

        if let Ok(pkts) = flowdb.scan_rtp_packets(
            call_id,
            start.timestamp_micros(),
            end.timestamp_micros(),
            None,
        ) {
            all_rtp_packets.extend(pkts);
        }
    }

    // Sort & dedup SIP
    all_sip.sort_by_key(|i| i.timestamp);
    all_sip.dedup_by(|a, b| {
        a.timestamp == b.timestamp
            && a.src_addr == b.src_addr
            && a.dst_addr == b.dst_addr
            && a.payload.len() == b.payload.len()
            && (a.payload.is_empty()
                || b.payload.is_empty()
                || a.payload[..a.payload.len().min(50)] == b.payload[..b.payload.len().min(50)])
    });

    // Sort RTP by timestamp for analysis
    all_rtp_packets.sort_by_key(|(_, ts, _)| *ts);

    let sip_count = all_sip.len();
    let ts_min = all_sip.first().map(|i| i.timestamp).unwrap_or(0);
    let ts_max = all_sip.last().map(|i| i.timestamp).unwrap_or(0);

    let duration_secs = if ts_max > ts_min {
        (ts_max - ts_min) as f64 / 1_000_000.0
    } else {
        0.0
    };

    let start_time =
        if let Some(dt) = chrono::TimeZone::timestamp_micros(&Local, ts_min as i64).single() {
            dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
        } else {
            String::new()
        };

    let end_time =
        if let Some(dt) = chrono::TimeZone::timestamp_micros(&Local, ts_max as i64).single() {
            dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
        } else {
            String::new()
        };

    let rtp_raw = all_rtp_packets.clone();
    let rtp_detail = analyze_raw_rtp_packets(all_rtp_packets, &all_sip, &all_rtp_stats);

    Ok(DiagReport {
        call_id: call_id.to_string(),
        root_dir: root_dir.to_string(),
        bucket_count: buckets.len(),
        buckets: bucket_infos,
        sip_messages: all_sip,
        sip_count,
        rtp_stats: all_rtp_stats,
        rtp_raw_packets: rtp_raw,
        duration_secs,
        start_time,
        end_time,
        rtp_detail,
    })
}

// ── Date/Time helpers ─────────────────────────────────────

pub fn dt_from_micros(ts: i64) -> String {
    chrono::TimeZone::timestamp_micros(&Local, ts)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// Parse a datetime string into `DateTime<Local>`. Supported formats:
///
/// - `YYYYMMDD`          (e.g. 20260721)             → midnight of that day
/// - `YYYYMMDDHH`        (e.g. 2026072114)           → start of that hour
/// - `YYYYMMDDHH24MISS`  (e.g. 20260721143000)       → exact second
/// - `<unix-seconds>`    (e.g. 1721500000)           → UTC timestamp
/// - `<unix-micros>`     (e.g. 1721500000000000)     → UTC timestamp (µs)
/// - `@<unix-seconds>`   (e.g. @1721500000)          → explicit timestamp
/// - `@<unix-micros>`    (e.g. @1721500000000000)    → explicit timestamp (µs)
pub fn parse_datetime(input: &str) -> Option<DateTime<Local>> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    if let Some(num) = input.strip_prefix('@') {
        return parse_timestamp(num);
    }

    if let Ok(n) = input.parse::<u64>() {
        return match input.len() {
            8 => {
                let y = (n / 10000) as i32;
                let m = ((n / 100) % 100) as u32;
                let d = (n % 100) as u32;
                NaiveDate::from_ymd_opt(y, m, d).and_then(|date| {
                    Local
                        .from_local_datetime(&NaiveDateTime::new(date, chrono::NaiveTime::MIN))
                        .single()
                })
            }
            10 => {
                let y = (n / 1000000) as i32;
                let mo = ((n / 10000) % 100) as u32;
                let d = ((n / 100) % 100) as u32;
                let h = (n % 100) as u32;
                if (1970..=2100).contains(&y)
                    && (1..=12).contains(&mo)
                    && (1..=31).contains(&d)
                    && h < 24
                {
                    NaiveDate::from_ymd_opt(y, mo, d).and_then(|date| {
                        Local
                            .from_local_datetime(&NaiveDateTime::new(
                                date,
                                chrono::NaiveTime::from_hms_opt(h, 0, 0).unwrap(),
                            ))
                            .single()
                    })
                } else {
                    parse_timestamp(input)
                }
            }
            14 => {
                let y = (n / 10000000000) as i32;
                let mo = ((n / 100000000) % 100) as u32;
                let d = ((n / 1000000) % 100) as u32;
                let h = ((n / 10000) % 100) as u32;
                let mi = ((n / 100) % 100) as u32;
                let s = (n % 100) as u32;
                NaiveDate::from_ymd_opt(y, mo, d).and_then(|date| {
                    chrono::NaiveTime::from_hms_opt(h, mi, s).and_then(|time| {
                        Local
                            .from_local_datetime(&NaiveDateTime::new(date, time))
                            .single()
                    })
                })
            }
            _ => parse_timestamp(input),
        };
    }

    None
}

fn parse_timestamp(num_str: &str) -> Option<DateTime<Local>> {
    let n: i64 = num_str.parse().ok()?;
    match num_str.len() {
        l if l >= 16 => chrono::TimeZone::timestamp_micros(&Local, n).single(),
        _ => Local.timestamp_opt(n, 0).single(),
    }
}
