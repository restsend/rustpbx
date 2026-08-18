//! Local periodic stats log.
//!
//! When `stats_log` is configured, one JSON line is appended to the file every
//! `stats_interval` seconds with system + PBX summary metrics — load, SIP
//! volume / registrations / transaction latency, media loss (rx/tx), DB pool
//! pressure, tokio runtime state, and kernel UDP errors. It is fully
//! Prometheus-independent: it reads `/proc`, the SIP server counters, and the
//! process-wide [`crate::media::telemetry::MediaTelemetry`] aggregate directly.
//!
//! The file is never rotated by this process (handled by logrotate).

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Local;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::app::AppState;
use crate::media::telemetry::{MediaTelemetry, MediaTelemetrySnapshot};
use crate::sip_telemetry::SipTelemetry;

/// Cumulative UDP counters from `/proc/net/snmp`, kept for delta computation.
#[derive(Default, Clone, Copy, serde::Serialize)]
struct UdpCounters {
    in_dgrams: u64,
    in_errors: u64,
    rcvbuf_errors: u64,
    sndbuf_errors: u64,
}

pub struct StatsLogger {
    file: Mutex<std::fs::File>,
    state: AppState,
    cancel: CancellationToken,
    interval: Duration,
    prev_udp: Mutex<UdpCounters>,
    prev_cpu: Mutex<Option<(u64, u64)>>,
    prev_media: Mutex<MediaTelemetrySnapshot>,
    prev_sip_tx_received: Mutex<u64>,
    prev_tx_latency: Mutex<(u64, u64)>,
    prev_endpoint_finished: Mutex<Option<u64>>,
}

impl StatsLogger {
    /// Open the stats file in append mode. Callers log a warning (and skip
    /// starting the task) when this fails — e.g. `/var/log` needs root.
    pub fn try_new(
        path: &str,
        state: AppState,
        cancel: CancellationToken,
        interval: Duration,
    ) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
            state,
            cancel,
            interval,
            prev_udp: Mutex::new(UdpCounters::default()),
            prev_cpu: Mutex::new(None),
            prev_media: Mutex::new(MediaTelemetrySnapshot::default()),
            prev_sip_tx_received: Mutex::new(0),
            prev_tx_latency: Mutex::new((0, 0)),
            prev_endpoint_finished: Mutex::new(None),
        })
    }

    pub fn spawn(self) {
        crate::utils::spawn(self.run());
    }

    async fn run(self) {
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // skip the immediate first tick
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = tick.tick() => {
                    if let Err(e) = self.snapshot_once().await {
                        tracing::warn!(error = %e, "stats_log write failed");
                    }
                }
            }
        }
    }

    async fn snapshot_once(&self) -> std::io::Result<()> {
        let line = self.build_line().await;
        let mut f = self.file.lock().unwrap();
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()?;
        Ok(())
    }

    async fn build_line(&self) -> String {
        let state = &self.state;
        let sip = state.sip_server();
        let tx_stats = sip.inner.endpoint.inner.get_stats();

        // ── SIP / registration / message volume ──
        let locator = sip.inner.locator.online_stats().await.ok();

        let mut sip_tx_received = self.prev_sip_tx_received.lock().unwrap();
        let tx_received_d = SipTelemetry::snapshot()
            .tx_received_total
            .saturating_sub(*sip_tx_received);
        *sip_tx_received += tx_received_d;

        let mut prev_finished = self.prev_endpoint_finished.lock().unwrap();
        let finished_now = tx_stats.finished_transactions as u64;
        let tx_finished_d = prev_finished
            .map(|prev| finished_now.saturating_sub(prev))
            .unwrap_or(0);
        *prev_finished = Some(finished_now);

        let (sum_us, count) = {
            let mut prev = self.prev_tx_latency.lock().unwrap();
            let snap = SipTelemetry::snapshot();
            let d_sum = snap.tx_latency_sum_us.saturating_sub(prev.0);
            let d_count = snap.tx_latency_count.saturating_sub(prev.1);
            *prev = (snap.tx_latency_sum_us, snap.tx_latency_count);
            (d_sum, d_count)
        };

        let sipserver = json!({
            "dialogs": sip.inner.dialog_layer.len(),
            "calls": sip.inner.active_call_registry.count(),
            "running_tx": sip.inner.runnings_tx.load(Ordering::Relaxed),
            "registrations": locator.as_ref().map(|l| l.online_locations).unwrap_or(0),
            "online_users": locator.as_ref().map(|l| l.online_users).unwrap_or(0),
            "webrtc_locations": locator.as_ref().map(|l| l.webrtc_locations).unwrap_or(0),
            "tx_received_d": tx_received_d,
            "tx_finished_d": tx_finished_d,
            "tx_running": tx_stats.running_transactions,
            "tx_waiting_ack": tx_stats.waiting_ack,
            "tx_latency_avg_ms": if count > 0 { (sum_us as f64 / count as f64) / 1000.0 } else { 0.0 },
            "tx_latency_max_ms": SipTelemetry::snapshot().tx_latency_max_us as f64 / 1000.0,
            "tx_latency_count": count,
        });

        // ── Media telemetry (rx/tx buckets, 5s deltas) ──
        let media = {
            let mut prev = self.prev_media.lock().unwrap();
            let cur = MediaTelemetry::snapshot();
            let d = media_delta(&cur, &prev);
            *prev = cur;
            d
        };

        // ── System (/proc) ──
        let sys = json!({
            "cpu_pct": self.read_cpu_pct(),
            "mem": read_meminfo(),
            "load": read_loadavg(),
            "rss_kb": read_self_rss(),
            "udp": self.read_udp_delta(),
        });

        json!({
            "ts": Local::now().to_rfc3339(),
            "uptime": (chrono::Utc::now() - state.uptime).num_seconds().max(0),
            "calls": {
                "total": state.total_calls.load(Ordering::Relaxed),
                "failed": state.total_failed_calls.load(Ordering::Relaxed),
                "active": sip.inner.active_call_registry.count(),
            },
            "sip": sipserver,
            "media": media,
            "db": db_pool_stats(state),
            "tokio": crate::utils::tokio_runtime_metrics(),
            "sys": sys,
        })
        .to_string()
    }

    // ── /proc readers ─────────────────────────────────────────────────────

    /// CPU busy% over the last two ticks (first tick returns null).
    fn read_cpu_pct(&self) -> Option<f64> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let line = stat.lines().next()?;
        let mut parts = line.split_whitespace();
        parts.next()?; // "cpu"
        let mut vals = [0u64; 10];
        for v in vals.iter_mut() {
            *v = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        }
        // busy = total − idle − iowait
        let total: u64 = vals.iter().sum();
        let idle = vals[3] + vals[4];
        let busy = total.saturating_sub(idle);
        let mut prev = self.prev_cpu.lock().unwrap();
        let pct = prev.map(|(pbusy, ptotal)| {
            let d_total = total.saturating_sub(ptotal);
            let d_busy = busy.saturating_sub(pbusy);
            if d_total > 0 {
                d_busy as f64 / d_total as f64 * 100.0
            } else {
                0.0
            }
        });
        *prev = Some((busy, total));
        pct
    }

    /// UDP counters delta vs previous tick (`RcvbufErrors > 0` = kernel buffer
    /// overflow → packets dropped in-kernel).
    fn read_udp_delta(&self) -> UdpCounters {
        let cur = parse_udp_snmp();
        let mut prev = self.prev_udp.lock().unwrap();
        let d = UdpCounters {
            in_dgrams: cur.in_dgrams.saturating_sub(prev.in_dgrams),
            in_errors: cur.in_errors.saturating_sub(prev.in_errors),
            rcvbuf_errors: cur.rcvbuf_errors.saturating_sub(prev.rcvbuf_errors),
            sndbuf_errors: cur.sndbuf_errors.saturating_sub(prev.sndbuf_errors),
        };
        *prev = cur;
        d
    }
}

/// Parse the `Udp:` block(s) of `/proc/net/snmp` (IPv4 kernel UDP counters).
/// Unknown/missing files degrade to zeros (never fatal).
fn parse_udp_snmp() -> UdpCounters {
    let mut out = UdpCounters::default();
    let Ok(text) = std::fs::read_to_string("/proc/net/snmp") else {
        return out;
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i + 1 < lines.len() {
        if lines[i].starts_with("Udp:") {
            let headers: Vec<&str> = lines[i].split_whitespace().skip(1).collect();
            let values: Vec<&str> = lines[i + 1].split_whitespace().skip(1).collect();
            for (h, v) in headers.iter().zip(values.iter()) {
                let n = v.parse::<u64>().unwrap_or(0);
                match *h {
                    "InDatagrams" => out.in_dgrams += n,
                    "InErrors" => out.in_errors += n,
                    "RcvbufErrors" => out.rcvbuf_errors += n,
                    "SndbufErrors" => out.sndbuf_errors += n,
                    _ => {}
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

fn read_meminfo() -> serde_json::Value {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return json!(null);
    };
    let mut total = 0u64;
    let mut available = 0u64;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = v
                .trim()
                .split_whitespace()
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            available = v
                .trim()
                .split_whitespace()
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or(0);
        }
    }
    let used_pct = if total > 0 {
        (1.0 - available as f64 / total as f64) * 100.0
    } else {
        0.0
    };
    json!({ "total_kb": total, "available_kb": available, "used_pct": used_pct })
}

fn read_loadavg() -> serde_json::Value {
    let Ok(text) = std::fs::read_to_string("/proc/loadavg") else {
        return json!([null, null, null]);
    };
    let vals: Vec<f64> = text
        .split_whitespace()
        .take(3)
        .filter_map(|p| p.parse().ok())
        .collect();
    json!(vals)
}

fn read_self_rss() -> Option<u64> {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return None;
    };
    text.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().split_whitespace().next())
        .and_then(|p| p.parse().ok())
}

/// Cumulative media deltas over one tick.
fn media_delta(cur: &MediaTelemetrySnapshot, prev: &MediaTelemetrySnapshot) -> serde_json::Value {
    let mk = |c: &crate::media::telemetry::DirectionSnapshot,
              p: &crate::media::telemetry::DirectionSnapshot| {
        let packets_d = c.packets_total.saturating_sub(p.packets_total);
        let lost_d = c.lost_total.saturating_sub(p.lost_total);
        let idrop_d = c
            .internal_drops_total
            .saturating_sub(p.internal_drops_total);
        let loss_pct = if lost_d + packets_d > 0 {
            lost_d as f64 / (lost_d + packets_d) as f64 * 100.0
        } else {
            0.0
        };
        json!({ "packets_d": packets_d, "lost_d": lost_d, "internal_drops_d": idrop_d, "loss_pct": loss_pct })
    };
    json!({
        "active_bridges": cur.active_bridges,
        "rx": mk(&cur.rx, &prev.rx),
        "tx": mk(&cur.tx, &prev.tx),
    })
}

/// SeaORM connection pool pressure. SQLite pools are typically size 1, which is
/// expected; PostgreSQL reflects real pool saturation.
fn db_pool_stats(state: &AppState) -> serde_json::Value {
    let db = state.db();
    let (size, idle) = match db.get_database_backend() {
        sea_orm::DatabaseBackend::Postgres => {
            let p = db.get_postgres_connection_pool();
            (p.size(), p.num_idle() as u32)
        }
        sea_orm::DatabaseBackend::Sqlite => {
            let p = db.get_sqlite_connection_pool();
            (p.size(), p.num_idle() as u32)
        }
        _ => return json!(null),
    };
    json!({
        "max": size,
        "idle": idle,
        "active": size.saturating_sub(idle),
    })
}
