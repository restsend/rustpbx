#!/usr/bin/env python3
"""
RustPBX P2P Benchmark Test using sipbot

Features:
1. Extension-to-extension (P2P) call benchmark
2. UAS with SIP registration via sipbot (multiple instances for high concurrency)
3. UAC via sipbot batch mode (--total / --cps)
4. Monitor PBX CPU, memory, concurrent calls via /ami/v1/health
5. Parse sipbot Progress output for setup latency, RTT, packet loss, TX/RX
6. Test 3 scenarios: mediaproxy=none, mediaproxy=all, sipflow enabled/disabled

Requirements:
    - sipbot 0.2.28+ (with audio loop fix, batch mode)
    - rustpbx compiled (target/release/rustpbx or target/debug/rustpbx)
    - Python 3.8+

Usage:
    # Run 500-concurrent benchmark (all scenarios)
    python bench.py --scenario all

    # Single scenario
    python bench.py --scenario mediaproxy_all

    # Custom concurrency
    python bench.py --scenario all --total 500 --cps 100 --duration 60

    # 800 concurrent
    python bench.py --scenario all --total 800 --cps 200 --uas-count 4
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

# ---------------------------------------------------------------------------
# Default configuration
# ---------------------------------------------------------------------------

DEFAULT_PROXY_HOST = "127.0.0.1"
DEFAULT_PROXY_PORT = 15061
DEFAULT_HTTP_BASE = "http://127.0.0.1:8083"
DEFAULT_RUSTPBX_BIN = "target/release/rustpbx"
DEFAULT_RUSTPBX_CONFIG = "tests/bench/config_bench.toml"
DEFAULT_RUSTPBX_CWD = "."

DEFAULT_UAS_BASE_PORT = 5090
DEFAULT_CALL_DURATION = 60  # seconds
DEFAULT_TOTAL = 500
DEFAULT_CPS = 100  # fast ramp for true concurrency
DEFAULT_UAS_COUNT = 5

# Pre-configured extension users from config.toml.dev
EXTENSION_USERS = [
    ("bob", "123456"),
    ("alice", "123456"),
]

# Regex patterns for sipbot output
PROGRESS_PAT = re.compile(
    r"Progress:\s*(\d+)/(\d+).*"
    r"Avg Setup Latency:\s*([\d.]+)ms.*"
    r"Avg RTCP RTT:\s*([\d.]+)ms.*"
    r"Avg Loss:\s*([\d.]+)%"
)
PROGRESS_COUNTS_PAT = re.compile(r"Progress:\s*(\d+)/(\d+)")
SETUP_LATENCY_PAT = re.compile(r"Avg Setup Latency:\s*([\d.]+)ms")
RTT_PAT = re.compile(r"Avg RTCP RTT:\s*([\d.]+)ms")
AVG_LOSS_PAT = re.compile(r"Avg Loss:\s*([\d.]+)%")
STATUS_COUNTS_PAT = re.compile(r"Status:\s*\[([^\]]+)\]")
TX_PAT = re.compile(r"TX:\s*(\d+)p/(\d+)b", re.IGNORECASE)
RX_PAT = re.compile(r"RX:\s*(\d+)p/(\d+)b", re.IGNORECASE)


# ---------------------------------------------------------------------------
# Data structures
# ---------------------------------------------------------------------------

@dataclass
class BenchmarkResult:
    """Results from a single benchmark run."""
    scenario: str
    total_calls: int
    duration: int
    mediaproxy: str
    sipflow_enabled: bool
    uas_count: int
    cps: int

    # Call statistics
    calls_completed: int = 0
    calls_failed: int = 0
    success_rate: float = 0.0
    status_counts: dict[str, int] = field(default_factory=dict)

    # Media quality
    avg_setup_latency_ms: float = 0.0
    avg_rtt_ms: float = 0.0
    avg_loss_pct: float = 0.0
    max_loss_pct: float = 0.0
    tx_packets: int = 0
    rx_packets: int = 0

    # Resource usage
    cpu_avg: float = 0.0
    cpu_peak: float = 0.0
    mem_avg_mb: float = 0.0
    mem_peak_mb: float = 0.0
    calls_peak: int = 0
    calls_avg: float = 0.0

    # Metadata
    test_duration_s: float = 0.0
    start_time: str = ""
    end_time: str = ""
    errors: list[str] = field(default_factory=list)

    # Memory-leak analysis (soak mode)
    leak_final_assessment: str | None = None
    leak_final_slope_mb_per_min: float = 0.0
    leak_base_delta_mb: float = 0.0

    # Media/session task-count drift after drain
    task_drift: dict[str, Any] = field(default_factory=dict)
    drain_passed: bool = True
    drain_details: dict[str, Any] = field(default_factory=dict)
    audio_format: dict[str, Any] = field(default_factory=dict)
    media_continuity: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "scenario": self.scenario,
            "total_calls": self.total_calls,
            "duration": self.duration,
            "mediaproxy": self.mediaproxy,
            "sipflow_enabled": self.sipflow_enabled,
            "uas_count": self.uas_count,
            "cps": self.cps,
            "calls_completed": self.calls_completed,
            "calls_failed": self.calls_failed,
            "success_rate": self.success_rate,
            "status_counts": self.status_counts,
            "avg_setup_latency_ms": self.avg_setup_latency_ms,
            "avg_rtt_ms": self.avg_rtt_ms,
            "avg_loss_pct": self.avg_loss_pct,
            "max_loss_pct": self.max_loss_pct,
            "tx_packets": self.tx_packets,
            "rx_packets": self.rx_packets,
            "cpu_avg": self.cpu_avg,
            "cpu_peak": self.cpu_peak,
            "mem_avg_mb": self.mem_avg_mb,
            "mem_peak_mb": self.mem_peak_mb,
            "calls_peak": self.calls_peak,
            "calls_avg": self.calls_avg,
            "test_duration_s": round(self.test_duration_s, 1),
            "start_time": self.start_time,
            "end_time": self.end_time,
            "errors": self.errors,
            "leak_final_assessment": self.leak_final_assessment,
            "leak_final_slope_mb_per_min": self.leak_final_slope_mb_per_min,
            "leak_base_delta_mb": self.leak_base_delta_mb,
            "task_drift": self.task_drift,
            "drain_passed": self.drain_passed,
            "drain_details": self.drain_details,
            "audio_format": self.audio_format,
            "media_continuity": self.media_continuity,
        }


# ---------------------------------------------------------------------------
# Resource Monitor
# ---------------------------------------------------------------------------

class ResourceMonitor:
    """Monitor rustpbx CPU/Memory/ConcurrentCalls via ps + /ami/v1/health."""

    def __init__(
        self,
        process_name: str = "rustpbx",
        interval: float = 1.0,
        health_url: str | None = None,
        leak_check_interval: float = 0.0,
        target_concurrency: int = 0,
        leak_csv: str | None = None,
        leak_slope_warn_mb_per_min: float = 0.5,
        leak_slope_watch_mb_per_min: float = 0.1,
    ):
        self.process_name = process_name
        self.interval = interval
        self.health_url = health_url
        self.samples: list[dict[str, float]] = []
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._lock = threading.Lock()
        # Memory-leak detection
        self.leak_check_interval = leak_check_interval
        self.target_concurrency = target_concurrency
        self.leak_csv = leak_csv
        self.leak_slope_warn = leak_slope_warn_mb_per_min
        self.leak_slope_watch = leak_slope_watch_mb_per_min
        self._leak_thread: threading.Thread | None = None
        self._baseline_mem: float | None = None
        self._last_check_mem: float | None = None
        self.leak_reports: list[dict[str, Any]] = []

    def start(self) -> None:
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        if self.leak_check_interval > 0:
            self._leak_thread = threading.Thread(target=self._leak_run, daemon=True)
            self._leak_thread.start()

    def _run(self) -> None:
        while not self._stop.is_set():
            sample = self._sample()
            if sample is not None:
                with self._lock:
                    self.samples.append(sample)
            self._stop.wait(self.interval)

    def _sample(self) -> dict[str, float] | None:
        try:
            import platform
            if platform.system() == "Darwin":
                # macOS: pgrep to find PIDs, then ps to get stats
                pgrep = subprocess.run(
                    ["pgrep", "-f", self.process_name],
                    capture_output=True, text=True, timeout=5,
                )
                pids = [p.strip() for p in pgrep.stdout.strip().split("\n") if p.strip()]
                if not pids:
                    return None
                pid_arg = ",".join(pids)
                result = subprocess.run(
                    ["ps", "-o", "pid,%cpu,rss", "-p", pid_arg],
                    capture_output=True, text=True, timeout=5,
                )
            else:
                result = subprocess.run(
                    ["ps", "-C", self.process_name, "-o", "pid,pcpu,rss", "--no-headers"],
                    capture_output=True, text=True, timeout=5,
                )
            if result.returncode != 0 or not result.stdout.strip():
                return None
            total_cpu = 0.0
            total_mem_kb = 0.0
            for line in result.stdout.strip().split("\n"):
                parts = line.split()
                # Skip header lines (PID, %CPU, RSS)
                if len(parts) >= 3 and parts[0].isdigit():
                    total_cpu += float(parts[1])
                    total_mem_kb += float(parts[2])
            if total_cpu == 0.0 and total_mem_kb == 0.0:
                return None
            sample: dict[str, float] = {
                "timestamp": time.time(),
                "cpu_pct": total_cpu,
                "mem_mb": total_mem_kb / 1024.0,
            }
        except Exception:
            return None

        if self.health_url:
            try:
                req = urllib.request.Request(self.health_url)
                with urllib.request.urlopen(req, timeout=3) as resp:
                    data = json.loads(resp.read())
                    calls = data.get("sipserver", {}).get("calls", 0)
                    sample["calls"] = float(calls)
                    # Media/session task-count leak signals.
                    tokio = data.get("tokio", {})
                    sample["tasks_total"] = float(data.get("tasks", {}).get("total", 0) or 0)
                    sample["media_alive_tasks"] = float(
                        (tokio.get("media") or {}).get("num_alive_tasks", 0) or 0
                    )
                    sample["sip_alive_tasks"] = float(
                        (tokio.get("sip") or {}).get("num_alive_tasks", 0) or 0
                    )
                    leak = data.get("sipserver", {}).get("leak", {}) or {}
                    sample["leak_handles"] = float(leak.get("handles_by_dialog", 0) or 0)
                    sample["leak_dialogs"] = float(leak.get("dialogs_by_session", 0) or 0)
            except Exception:
                pass

        return sample

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=5)
        # Final leak check so the last interval is captured
        if self._leak_thread is not None:
            self._leak_thread.join(timeout=5)
            self._do_leak_check()

    # ------------------------------------------------------------------
    # Memory-leak detection
    # ------------------------------------------------------------------

    def _leak_run(self) -> None:
        """Periodically analyse the memory trend and report leaks."""
        while not self._stop.is_set():
            if self._stop.wait(self.leak_check_interval):
                break
            self._do_leak_check()

    @staticmethod
    def _regress(win: list[dict[str, float]]) -> tuple[float, float]:
        """Least-squares regression of mem(t) over the window.

        Returns (slope_mb_per_min, r_squared)."""
        n = len(win)
        t0 = win[0]["timestamp"]
        xs = [s["timestamp"] - t0 for s in win]
        ys = [s["mem_mb"] for s in win]
        sx, sy = sum(xs), sum(ys)
        sxx = sum(x * x for x in xs)
        sxy = sum(x * y for x, y in zip(xs, ys))
        denom = n * sxx - sx * sx
        if denom == 0:
            return 0.0, 0.0
        slope_per_s = (n * sxy - sx * sy) / denom
        intercept = (sy - slope_per_s * sx) / n
        slope_per_min = slope_per_s * 60.0
        mean_y = sy / n
        ss_tot = sum((y - mean_y) ** 2 for y in ys)
        ss_res = sum((y - (slope_per_s * x + intercept)) ** 2 for x, y in zip(xs, ys))
        r2 = 1.0 - (ss_res / ss_tot) if ss_tot > 0 else 0.0
        return slope_per_min, r2

    def _do_leak_check(self) -> None:
        with self._lock:
            samples = list(self.samples)
        if len(samples) < 10:
            return

        now_sample = samples[-1]
        cur_mem = now_sample["mem_mb"]
        cur_calls = now_sample.get("calls")
        elapsed = int(now_sample["timestamp"] - samples[0]["timestamp"])

        cutoff = now_sample["timestamp"] - self.leak_check_interval
        win = [s for s in samples if s["timestamp"] >= cutoff]
        if len(win) < 5:
            win = samples  # fall back to everything if window too small

        slope_mb_min, r2 = self._regress(win)

        if self._baseline_mem is None:
            self._baseline_mem = win[0]["mem_mb"]
        if self._last_check_mem is None:
            self._last_check_mem = self._baseline_mem

        win_delta = cur_mem - self._last_check_mem
        base_delta = cur_mem - self._baseline_mem
        self._last_check_mem = cur_mem

        # Concurrency stability (only meaningful once ramp-up is done)
        calls_list = [s["calls"] for s in win if "calls" in s]
        concurrency_stable = True
        calls_avg_str = "n/a"
        if calls_list and self.target_concurrency > 0 and elapsed > self.leak_check_interval:
            calls_avg = sum(calls_list) / len(calls_list)
            calls_avg_str = f"{calls_avg:.0f}"
            concurrency_stable = abs(calls_avg - self.target_concurrency) / self.target_concurrency < 0.25

        if slope_mb_min > self.leak_slope_warn and r2 > 0.5 and concurrency_stable:
            assessment = "LEAK SUSPECTED"
        elif slope_mb_min > self.leak_slope_watch:
            assessment = "WATCH"
        else:
            assessment = "STABLE"

        report = {
            "elapsed_s": elapsed,
            "mem_mb": round(cur_mem, 2),
            "calls": cur_calls,
            "window_delta_mb": round(win_delta, 2),
            "base_delta_mb": round(base_delta, 2),
            "slope_mb_per_min": round(slope_mb_min, 3),
            "r2": round(r2, 3),
            "assessment": assessment,
            "ts": datetime.now(timezone.utc).isoformat(),
        }
        self.leak_reports.append(report)

        # Health counters validation
        health_ok, health_issues = self._check_health()
        if not health_ok:
            report["health_issues"] = health_issues
            print(f"  ⚠ health: {', '.join(health_issues)}", flush=True)

        print(
            f"[LEAK-CHECK {elapsed//60:02d}m{elapsed%60:02d}s] "
            f"mem={cur_mem:.1f}MB (win {win_delta:+.1f}MB, base {base_delta:+.1f}MB) | "
            f"slope={slope_mb_min:.3f} MB/min R²={r2:.2f} | "
            f"calls={calls_avg_str}/{self.target_concurrency or '-'} | "
            f"→ {assessment}",
            flush=True,
        )

        if self.leak_csv:
            write_header = not os.path.exists(self.leak_csv)
            try:
                with open(self.leak_csv, "a", newline="") as f:
                    writer = csv.DictWriter(f, fieldnames=list(report.keys()))
                    if write_header:
                        writer.writeheader()
                    writer.writerow(report)
            except Exception:
                pass

    def _check_health(self) -> tuple[bool, list[str]]:
        """Fetch /ami/v1/health and validate all numeric counters are sensible.

        Returns (ok, [issues]).
        """
        if not self.health_url:
            return True, []
        try:
            req = urllib.request.Request(self.health_url)
            with urllib.request.urlopen(req, timeout=3) as resp:
                data = json.loads(resp.read())
        except Exception as e:
            return False, [f"health fetch failed: {e}"]

        issues: list[str] = []

        def check(name: str, val: int, minv: int = 0, maxv: int | None = None) -> None:
            if not isinstance(val, int) or val < minv:
                issues.append(f"{name}={val!r} (expected int ≥{minv})")
                return
            if maxv is not None and val > maxv:
                issues.append(f"{name}={val} > {maxv}")

        # Top-level counters
        total = data.get("total")
        failed = data.get("failed")
        if total is not None:
            check("total", total)
        if failed is not None:
            check("failed", failed)
        if isinstance(total, int) and isinstance(failed, int) and failed > total:
            issues.append(f"failed={failed} > total={total}")

        # sipserver counters
        ss = data.get("sipserver", {})
        if ss:
            calls = ss.get("calls")
            if calls is not None:
                check("sipserver.calls", calls)
            dialogs = ss.get("dialogs")
            if dialogs is not None:
                check("sipserver.dialogs", dialogs)
            running_tx = ss.get("running_tx")
            if running_tx is not None:
                check("sipserver.running_tx", running_tx)

            tx = ss.get("transactions", {})
            if tx:
                for k in ("running", "finished", "waiting_ack"):
                    v = tx.get(k)
                    if v is not None:
                        check(f"transactions.{k}", v)

            # DashMap sizes
            dm = ss.get("dashmaps", {})
            if dm:
                for k in ("trunks", "queues", "debug_routes",
                          "presence_states", "presence_subscribers", "mwi_subscribers"):
                    v = dm.get(k)
                    if v is not None:
                        check(f"dashmaps.{k}", v)

        # tasks.total
        tasks = data.get("tasks", {})
        if tasks:
            t = tasks.get("total")
            if t is not None:
                check("tasks.total", t)

        return len(issues) == 0, issues

    def summary(self) -> dict[str, Any]:
        with self._lock:
            snap = list(self.samples)
        if not snap:
            return {
                "cpu_avg": 0.0, "cpu_peak": 0.0,
                "mem_avg_mb": 0.0, "mem_peak_mb": 0.0,
                "samples": 0, "calls_peak": 0, "calls_avg": 0.0,
            }
        cpus = [s["cpu_pct"] for s in snap]
        mems = [s["mem_mb"] for s in snap]
        result: dict[str, Any] = {
            "cpu_avg": sum(cpus) / len(cpus),
            "cpu_peak": max(cpus),
            "mem_avg_mb": sum(mems) / len(mems),
            "mem_peak_mb": max(mems),
            "samples": len(snap),
        }
        calls_list = [s["calls"] for s in snap if "calls" in s]
        if calls_list:
            result["calls_peak"] = int(max(calls_list))
            result["calls_avg"] = sum(calls_list) / len(calls_list)
            result["calls_end"] = int(calls_list[-1])
        else:
            result["calls_peak"] = 0
            result["calls_avg"] = 0.0
            result["calls_end"] = 0

        # Media/session task-count leak signals: after calls drain, live tasks
        # should return to the idle baseline. Report min/end/drift.
        for key in ("tasks_total", "media_alive_tasks", "sip_alive_tasks",
                    "leak_handles", "leak_dialogs"):
            vals = [s[key] for s in snap if key in s]
            if vals:
                result[f"{key}_min"] = int(min(vals))
                result[f"{key}_end"] = int(vals[-1])
                result[f"{key}_drift"] = int(vals[-1] - min(vals))

        if self.leak_reports:
            final = self.leak_reports[-1]
            result["leak_final_assessment"] = final["assessment"]
            result["leak_final_slope_mb_per_min"] = final["slope_mb_per_min"]
            result["leak_base_delta_mb"] = final["base_delta_mb"]
            result["leak_reports"] = self.leak_reports
        return result


# ---------------------------------------------------------------------------
# SipProcess — manages a single sipbot process
# ---------------------------------------------------------------------------

class SipProcess:
    """Manages a sipbot process (UAS or UAC)."""

    def __init__(self, name: str, log_file: str | None = None, retain_lines: int = 0):
        self.name = name
        self.process: subprocess.Popen[str] | None = None
        self.lines: list[str] = []
        self._lock = threading.Lock()
        self._reader: threading.Thread | None = None
        self._log_file = log_file
        self._retain_lines = retain_lines  # 0 = keep all

    def start(self, cmd: list[str]) -> None:
        self.process = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
        self._reader = threading.Thread(target=self._read, daemon=True)
        self._reader.start()

    def _read(self) -> None:
        if not self.process or not self.process.stdout:
            return
        for line in self.process.stdout:
            line = line.rstrip("\n")
            if line:
                with self._lock:
                    self.lines.append(line)
                    if self._retain_lines > 0 and len(self.lines) > self._retain_lines:
                        # drop oldest 20% to amortise the trim cost
                        del self.lines[: int(self._retain_lines * 0.2)]
                if self._log_file:
                    with open(self._log_file, "a") as f:
                        f.write(line + "\n")

    def output(self) -> str:
        with self._lock:
            return "\n".join(self.lines)

    def log_tail(self, n_bytes: int = 2_000_000) -> str:
        """Return the last ~n_bytes of the log file (for parsing final metrics
        on very long runs where in-memory retention is capped)."""
        if not self._log_file or not os.path.exists(self._log_file):
            return self.output()
        try:
            size = os.path.getsize(self._log_file)
            with open(self._log_file, "rb") as f:
                if size > n_bytes:
                    f.seek(-n_bytes, os.SEEK_END)
                    f.readline()  # skip partial line
                return f.read().decode("utf-8", errors="replace")
        except Exception:
            return self.output()

    def terminate(self) -> None:
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()

    def wait(self, timeout: int = 300) -> int:
        if self.process:
            try:
                return self.process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.terminate()
                return -1
        return 0


# ---------------------------------------------------------------------------
# Metric parsing
# ---------------------------------------------------------------------------

def parse_stress_metrics(output: str) -> dict[str, Any]:
    """Parse sipbot batch-mode output for stress test metrics."""
    result: dict[str, Any] = {
        "completed": 0,
        "total": 0,
        "avg_setup_latency_ms": 0.0,
        "avg_rtt_ms": 0.0,
        "avg_loss_pct": 0.0,
        "max_loss_pct": 0.0,
        "tx_packets": 0,
        "rx_packets": 0,
        "status_counts": {},
    }

    progress_lines = [l for l in output.split("\n") if "Progress:" in l]
    if not progress_lines:
        return result
    final = progress_lines[-1]

    m = PROGRESS_COUNTS_PAT.search(final)
    if m:
        result["completed"] = int(m.group(1))
        result["total"] = int(m.group(2))

    m = SETUP_LATENCY_PAT.search(final)
    if m:
        result["avg_setup_latency_ms"] = float(m.group(1))

    m = RTT_PAT.search(final)
    if m:
        result["avg_rtt_ms"] = float(m.group(1))

    m = AVG_LOSS_PAT.search(final)
    if m:
        result["avg_loss_pct"] = float(m.group(1))

    # TX/RX from final progress line
    tx_matches = TX_PAT.findall(final)
    if tx_matches:
        result["tx_packets"] = sum(int(x[0]) for x in tx_matches)

    rx_matches = RX_PAT.findall(final)
    if rx_matches:
        result["rx_packets"] = sum(int(x[0]) for x in rx_matches)

    m = STATUS_COUNTS_PAT.search(final)
    if m:
        for part in m.group(1).split(","):
            if ":" in part:
                code, count = part.split(":", 1)
                result["status_counts"][code.strip()] = int(count.strip())

    # Per-call Loss lines for max_loss
    losses = [float(x.group(1)) for x in re.finditer(r"Loss:\s*([\d.]+)%", output)]
    result["max_loss_pct"] = max(losses) if losses else result["avg_loss_pct"]

    return result


# ---------------------------------------------------------------------------
# P2PBenchmark orchestrator
# ---------------------------------------------------------------------------

class P2PBenchmark:
    """P2P Benchmark test orchestrator using sipbot."""

    def __init__(
        self,
        proxy_host: str = DEFAULT_PROXY_HOST,
        proxy_port: int = DEFAULT_PROXY_PORT,
        http_base: str = DEFAULT_HTTP_BASE,
        rustpbx_bin: str = DEFAULT_RUSTPBX_BIN,
        rustpbx_config: str = DEFAULT_RUSTPBX_CONFIG,
        rustpbx_cwd: str = DEFAULT_RUSTPBX_CWD,
        log_dir: str = "tests/bench/results",
    ):
        self.proxy_host = proxy_host
        self.proxy_port = proxy_port
        self.http_base = http_base
        self.rustpbx_bin = rustpbx_bin
        self.rustpbx_config = rustpbx_config
        self.rustpbx_cwd = rustpbx_cwd
        self.log_dir = log_dir
        self.rustpbx_process: subprocess.Popen[str] | None = None
        self.uas_list: list[SipProcess] = []
        self.uac_process: SipProcess | None = None
        self.monitor: ResourceMonitor | None = None
        self.results: list[BenchmarkResult] = []
        self.cancel_prob = 0  # set from CLI; used by run_uac_batch
        # SipFlow remote server management
        self.sipflow_process: subprocess.Popen[str] | None = None
        self.sipflow_udp_port = 3000
        self.sipflow_http_port = 3001

        os.makedirs(log_dir, exist_ok=True)

    # -----------------------------------------------------------------------
    # Server management
    # -----------------------------------------------------------------------

    def start_sipflow_server(self) -> bool:
        """Start the sipflow standalone server (flowdb engine) as a separate process."""
        import portpicker
        self.sipflow_udp_port = portpicker.pick_unused_port() or 3000
        self.sipflow_http_port = portpicker.pick_unused_port() or 3001

        sipflow_bin = os.path.join(os.path.dirname(self.rustpbx_bin), "sipflow")
        if not os.path.exists(sipflow_bin):
            sipflow_bin = "sipflow"  # fallback to PATH

        log_file = os.path.join(self.log_dir, f"sipflow_server_{int(time.time())}.log")
        sipflow_data = os.path.join(self.log_dir, "sipflow_data")

        cmd = [
            sipflow_bin,
            "-a", "127.0.0.1",
            "-p", str(self.sipflow_udp_port),
            "--http-port", str(self.sipflow_http_port),
            "-r", sipflow_data,
            "--engine", "flowdb",
            "--log-level", "info",
            "--log-file", log_file,
        ]
        try:
            with open(log_file, "w") as lf:
                self.sipflow_process = subprocess.Popen(
                    cmd, stdout=lf, stderr=subprocess.STDOUT,
                )
            time.sleep(2)
            if self.sipflow_process.poll() is not None:
                print(f"[sipflow] Server failed to start — check {log_file}")
                self.sipflow_process = None
                return False
            print(f"[sipflow] Server started (PID: {self.sipflow_process.pid}, "
                  f"UDP:{self.sipflow_udp_port}, HTTP:{self.sipflow_http_port})")
            print(f"[sipflow] Log: {log_file}")
            return True
        except Exception as e:
            print(f"[sipflow] Failed to start server: {e}")
            self.sipflow_process = None
            return False

    def stop_sipflow_server(self) -> None:
        if self.sipflow_process:
            self.sipflow_process.terminate()
            try:
                self.sipflow_process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.sipflow_process.kill()
                self.sipflow_process.wait()
            self.sipflow_process = None

    def start_rustpbx(self, mediaproxy: str = "all", sipflow: bool = False, wholesale: bool = False,
                      recording: bool = False, webrtc: bool = False) -> bool:
        """Start rustpbx with specified configuration."""
        print(f"\n{'='*60}")
        print(f"Starting rustpbx (mediaproxy={mediaproxy}, sipflow={sipflow}, wholesale={wholesale}, "
              f"recording={recording}, webrtc={webrtc})")
        print(f"{'='*60}")

        self._kill_rustpbx()

        self._ensure_mysql_proxy()

        # Start standalone sipflow server for remote mode
        if sipflow:
            self.start_sipflow_server()

        db_suffix = self._create_database()
        config_path = self._create_config(mediaproxy, sipflow, db_suffix, wholesale=wholesale,
                                          recording=recording, webrtc=webrtc)
        if not config_path:
            return False

        try:
            log_file = os.path.join(self.log_dir, f"rustpbx_{int(time.time())}.log")
            with open(log_file, "w") as log_f:
                self.rustpbx_process = subprocess.Popen(
                    [self.rustpbx_bin, "--conf", config_path],
                    cwd=self.rustpbx_cwd,
                    stdout=log_f,
                    stderr=subprocess.STDOUT,
                )

            print(f"[rustpbx] Started (PID: {self.rustpbx_process.pid})")
            print(f"[rustpbx] Log: {log_file}")

            if not self._wait_for_rustpbx():
                print("[rustpbx] Failed to start")
                return False

            print("[rustpbx] Ready")
            return True

        except Exception as e:
            print(f"[rustpbx] Failed to start: {e}")
            return False

    # -----------------------------------------------------------------------
    # Database management
    # -----------------------------------------------------------------------

    _mysql_proxy_process: subprocess.Popen[str] | None = None

    def _ensure_mysql_proxy(self) -> None:
        """Start mysql_proxy.py if the config database_url points to 127.0.0.1:13307.

        macOS Tahoe blocks locally-compiled binaries (Rust/C) from accessing
        LAN hosts.  Python framework binaries are exempt, so we run a small
        TCP forwarder in Python to bridge Rustpbx → MySQL.
        """
        import platform
        if platform.system() != "Darwin":
            return

        # Check if the config uses the proxy port
        try:
            with open(self.rustpbx_config, "r") as f:
                content = f.read()
            if ":13307" not in content:
                return  # Not using the proxy, skip
        except Exception:
            return

        # Check if proxy is already running
        try:
            import socket as _sock
            s = _sock.socket(_sock.AF_INET, _sock.SOCK_STREAM)
            s.settimeout(1)
            if s.connect_ex(("127.0.0.1", 13307)) == 0:
                s.close()
                print("[mysql-proxy] Already running on 127.0.0.1:13307")
                return
            s.close()
        except Exception:
            pass

        # Start the proxy
        proxy_script = os.path.join(os.path.dirname(__file__), "mysql_proxy.py")
        if not os.path.exists(proxy_script):
            print(f"[mysql-proxy] Script not found: {proxy_script}")
            return

        log_file = os.path.join(self.log_dir, f"mysql_proxy_{int(time.time())}.log")
        with open(log_file, "w") as log_f:
            self._mysql_proxy_process = subprocess.Popen(
                ["python3", proxy_script],
                stdout=log_f,
                stderr=subprocess.STDOUT,
            )
        time.sleep(1)
        if self._mysql_proxy_process.poll() is None:
            print(f"[mysql-proxy] Started (PID: {self._mysql_proxy_process.pid}, log: {log_file})")
        else:
            print(f"[mysql-proxy] Failed to start — check {log_file}")
            self._mysql_proxy_process = None

    def _create_database(self) -> str:
        """Create a fresh MySQL database for this scenario run.

        Returns a suffix string (e.g. '_s1234567890') appended to the base
        database name in the config.  If the config uses SQLite or the MySQL
        connection fails, returns an empty string (reuse existing DB).
        """
        try:
            import pymysql
        except ImportError:
            # pymysql not installed — skip DB creation, use whatever's in config
            return ""

        # Parse the database_url from the base config to get MySQL credentials
        try:
            with open(self.rustpbx_config, "r") as f:
                for line in f:
                    if line.strip().startswith("database_url"):
                        url = line.split("=", 1)[1].strip().strip('"')
                        break
                else:
                    return ""
        except Exception:
            return ""

        # mysql://user:pass@host:port/dbname
        m = re.match(r"mysql://([^:]+):([^@]+)@([^:]+):(\d+)/(.+)", url)
        if not m:
            return ""

        user, password, host, port, base_db = m.groups()

        # If using the local proxy (127.0.0.1:13307), connect to the real
        # MySQL host directly (Python is not affected by macOS Local Network
        # Privacy, but Rust is — so the proxy is only for the Rust binary).
        if host == "127.0.0.1" and port == "13307":
            host = "192.168.3.152"
            port = "13306"

        suffix = f"_s{int(time.time())}"
        new_db = f"{base_db}{suffix}"

        try:
            conn = pymysql.connect(
                host=host, port=int(port), user=user, password=password,
                autocommit=True,
            )
            with conn.cursor() as cur:
                cur.execute(f"CREATE DATABASE IF NOT EXISTS `{new_db}` "
                            f"CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci")
            conn.close()
            print(f"[mysql] Created database: {new_db}")
            return suffix
        except Exception as e:
            print(f"[mysql] Failed to create database ({e}), using base config")
            return ""

    def _create_config(self, mediaproxy: str, sipflow: bool, db_suffix: str = "",
                       wholesale: bool = False, recording: bool = False,
                       webrtc: bool = False) -> str | None:
        """Create a temporary config file with specified settings."""
        try:
            with open(self.rustpbx_config, "r") as f:
                config_content = f.read()

            # Inject fresh database name if a suffix was provided
            if db_suffix:
                config_content = re.sub(
                    r'(database_url\s*=\s*"mysql://)([^"]+)(/)([^"]+)(")',
                    lambda m: f'{m.group(1)}{m.group(2)}{m.group(3)}{m.group(4)}{db_suffix}{m.group(5)}',
                    config_content,
                )

            # Modify mediaproxy
            config_content = re.sub(
                r'media_proxy\s*=\s*"[^"]*"',
                f'media_proxy = "{mediaproxy}"',
                config_content,
            )

            # Recording: by default disable (recording.enabled=true forces media
            # proxy on regardless of media_proxy, invalidating mediaproxy=none).
            # When `recording` is requested, keep it enabled; if sipflow is also
            # on, set force_file=true so both coexist.
            if not recording:
                config_content = re.sub(
                    r'(\[recording\][^\[]*enabled\s*=\s*)true',
                    lambda m: m.group(1) + "false",
                    config_content,
                    flags=re.DOTALL,
                )
            elif sipflow:
                if not re.search(r'(?m)\[recording\].*?force_file\s*=',
                                 config_content, flags=re.DOTALL):
                    config_content = re.sub(
                        r'(?m)(\[recording\])(\s*\n[^[]*?auto_start\s*=\s*true)',
                        lambda m: m.group(1) + m.group(2) + "\nforce_file = true",
                        config_content,
                        flags=re.DOTALL,
                    )
                    if "force_file" not in config_content:
                        config_content = re.sub(
                            r'(?m)^\[recording\]\n',
                            "[recording]\nforce_file = true\n",
                            config_content,
                        )

            # WebRTC: mark the memory users WebRTC-capable (DTLS-SRTP).
            if webrtc:
                config_content = re.sub(
                    r'(?m)^(username\s*=\s*"(?:bob|alice)")[^\S\n]*\n',
                    lambda m: m.group(1) + "\nis_support_webrtc = true\n",
                    config_content,
                )

            # Ensure an RWI token exists (needed for conference scenarios).
            if not re.search(r'(?m)^\[\[rwi\.tokens\]\]', config_content):
                config_content += (
                    "\n[[rwi.tokens]]\n"
                    'token = "bench-rwi-token"\n'
                    'scopes = ["call", "session", "media", "record", "conference", "queue"]\n'
                )
            else:
                config_content = re.sub(
                    r'(?m)(\[\[rwi\.tokens\]\]\s*\n\s*token\s*=\s*)"[^"]*"',
                    lambda m: m.group(1) + '"bench-rwi-token"',
                    config_content,
                )

            # Enable wholesale addon under [proxy] (addons is a ProxyConfig field).
            # wholesale is a commercial addon; initialize() runs migrations +
            # background tasks and per-call hooks regardless of license (license
            # only gates the admin UI), so it loads cleanly for soak testing.
            if wholesale:
                if re.search(r'(?m)^\s*addons\s*=', config_content):
                    # Replace existing top-level/[proxy] addons line
                    def _ensure_wholesale(m: re.Match) -> str:
                        line = m.group(0)
                        # parse current list
                        cur = m.group(1)
                        items = [a.strip().strip('"').strip("'") for a in cur.split(",") if a.strip().strip('"').strip("'")]
                        if "wholesale" not in items:
                            items.append("wholesale")
                        rendered = ", ".join(f'"{a}"' for a in items)
                        return f'addons = [{rendered}]'
                    config_content = re.sub(
                        r'(?m)^\s*addons\s*=\s*\[([^\]]*)\]',
                        _ensure_wholesale,
                        config_content,
                    )
                else:
                    # Insert right after the [proxy] header
                    config_content = re.sub(
                        r'(\[proxy\]\s*\n)',
                        r'\1addons = ["wholesale"]\n',
                        config_content,
                        count=1,
                    )

            # Modify sipflow — use remote mode (separate sipflow process with flowdb)
            if sipflow:
                # Remote mode: sipflow server runs as a separate process.
                # rustpbx sends SIP messages + RTP via UDP, offloading all
                # serialization/storage I/O from the main process.
                sipflow_block = (
                    '[sipflow]\n'
                    'type = "remote"\n'
                    f'udp_addr = "127.0.0.1:{self.sipflow_udp_port}"\n'
                    f'http_addr = "http://127.0.0.1:{self.sipflow_http_port}"\n'
                    'timeout_secs = 10\n'
                )
                if "[sipflow]" in config_content:
                    config_content = re.sub(
                        r'\[sipflow\][\s\S]*?(?=\n\[|\Z)',
                        sipflow_block.rstrip("\n"),
                        config_content,
                    )
                else:
                    config_content += "\n" + sipflow_block
            else:
                config_content = re.sub(
                    r'(\[sipflow\][^\[]*)',
                    lambda m: '\n'.join(
                        '# ' + line if line.strip() and not line.startswith('#') else line
                        for line in m.group(1).split('\n')
                    ),
                    config_content,
                )

            temp_config = os.path.join(
                self.log_dir, f"config_{mediaproxy}_{int(sipflow)}_ws{int(wholesale)}.toml"
            )
            with open(temp_config, "w") as f:
                f.write(config_content)
            return temp_config

        except Exception as e:
            print(f"[config] Failed to create config: {e}")
            return None

    def _wait_for_rustpbx(self, timeout: int = 30) -> bool:
        start_time = time.time()
        while time.time() - start_time < timeout:
            try:
                if self.rustpbx_process and self.rustpbx_process.poll() is not None:
                    return False
                req = urllib.request.Request(f"{self.http_base}/ami/v1/health")
                with urllib.request.urlopen(req, timeout=2) as resp:
                    if resp.status == 200:
                        return True
            except Exception:
                pass
            time.sleep(1)
        return False

    def _kill_rustpbx(self) -> None:
        # Kill only the rustpbx process(es) we spawned — never blanket-match
        # "rustpbx" in a cmdline, which would also hit unrelated processes such
        # as `tmux new -t rustpbx`.
        target = self.rustpbx_process.pid if self.rustpbx_process else None
        try:
            if target:
                subprocess.run(["kill", "-TERM", str(target)], capture_output=True)
                time.sleep(1)
                subprocess.run(["kill", "-KILL", str(target)], capture_output=True)
                time.sleep(0.5)
            else:
                pat = "rustpbx --conf"
                subprocess.run(["pkill", "-TERM", "-f", pat], capture_output=True)
                time.sleep(1)
                subprocess.run(["pkill", "-KILL", "-f", pat], capture_output=True)
                time.sleep(0.5)
        except Exception:
            pass

    # -----------------------------------------------------------------------
    # UAS management (sipbot wait with registration)
    # -----------------------------------------------------------------------

    def start_uas_instances(
        self,
        count: int,
        base_port: int = DEFAULT_UAS_BASE_PORT,
        hangup: int = 120,
        verbose: bool = True,
        ring_duration: float = 0.0,
        codecs: str = "pcmu",
        webrtc: bool = False,
        audio_quality: bool = False,
    ) -> bool:
        """Start UAS instances registered as extension users.

        Each UAS registers as bob/alice (cycling through users).
        sipbot handles multiple concurrent calls per instance.

        Pass verbose=False for long soak runs: sipbot's -v logs every RTP
        packet, which produces GB-sized logs in minutes and starves the
        load generator (observed 2.6 GB / 10 min from a single UAS).

        When ``ring_duration`` > 0 the UAS rings (180) for that many seconds
        before answering — this keeps calls in the early/ringing phase so a
        UAC CANCEL lands before 200 OK (reliably producing 487).
        """
        ring_info = f", ring={ring_duration}s" if ring_duration else ""
        quiet_info = " [quiet]" if not verbose else ""
        print(f"\n{'='*60}")
        print(f"Starting {count} UAS instances (sipbot wait + register"
              + ring_info + quiet_info + ")")
        print(f"{'='*60}")

        self.uas_list = []

        for i in range(count):
            username, password = EXTENSION_USERS[i % len(EXTENSION_USERS)]
            port = base_port + i

            # Kill any existing sipbot on this port
            subprocess.run(
                ["pkill", "-9", "-f", f"sipbot.*127.0.0.1:{port}"],
                capture_output=True,
            )

            log_file = os.path.join(self.log_dir, f"sipbot_uas_{i+1:03d}_{int(time.time())}.log")

            cmd = [
                "sipbot", "wait",
                "--username", username,
                "--password", password,
                "--register", f"{self.proxy_host}:{self.proxy_port}",
                "-a", f"127.0.0.1:{port}",
                "--codecs", codecs,
                "--hangup", str(hangup),
            ]
            if ring_duration and ring_duration > 0:
                cmd += ["--ring-duration", str(int(round(ring_duration)))]
            cmd += [
                "--echo",  # echo mode for realistic bidirectional RTP
            ]
            if webrtc:
                cmd.append("--webrtc")
            if audio_quality:
                cmd.append("--audio-quality")
            if verbose:
                cmd.append("-v")

            uas = SipProcess(f"uas-{i+1}", log_file=log_file)
            uas.start(cmd)
            self.uas_list.append(uas)
            print(f"[UAS] #{i+1} started: user={username}, port={port}, "
                  f"ring={ring_duration}s, log={log_file}")

        # Wait for registrations to complete
        time.sleep(3)
        print(f"[UAS] All {count} instances registered")
        return True

    # -----------------------------------------------------------------------
    # UAC management (sipbot call batch mode)
    # -----------------------------------------------------------------------

    def run_uac_batch(
        self,
        total: int,
        cps: int,
        duration: int,
        soak: bool = False,
        wall_time: int = 0,
        batch_window: int = 120,
        cancel_prob: int = 0,
        codecs: str = "pcmu",
        webrtc: bool = False,
        audio_quality: bool = False,
    ) -> tuple[str, float]:
        """Run batch UAC calls via sipbot call --total --cps.

        Calls are placed to extension users (bob/alice) through the PBX.
        When ``cancel_prob`` > 0, sipbot will CANCEL that percentage of calls
        right after INVITE (before answer) — exercises the CANCEL cleanup path.
        Returns (output_text, wall_time_seconds).

        When soak=True (long-duration run), sipbot is invoked in a loop of
        smaller batches (each batch_window seconds). This is required because
        sipbot's batch mode accumulates per-call state ("Active" counter) and
        dies after ~100k calls in a single invocation, so a single --total of
        cps*wall_time (e.g. 360000) is unsustainable. The resource monitor
        (leak checker) runs across all batches since it is started/stopped by
        run_benchmark around this call.
        """
        if soak and wall_time > 0:
            return self._run_uac_soak(cps, duration, wall_time, batch_window, codecs=codecs, webrtc=webrtc)
        return self._run_uac_single(total, cps, duration, soak=False, codecs=codecs, cancel_prob=cancel_prob, webrtc=webrtc, audio_quality=audio_quality)

    def _run_uac_single(
        self, total: int, cps: int, duration: int, soak: bool,
        codecs: str = "pcmu", cancel_prob: int = 0, webrtc: bool = False,
        audio_quality: bool = False,
    ) -> tuple[str, float]:
        """Run one sipbot call batch."""
        print(f"\n{'='*60}")
        print(f"Starting UAC batch: {total} calls @ {cps} CPS, duration={duration}s"
              f", codecs={codecs}, webrtc={webrtc}"
              f"{' [SOAK batch]' if soak else ''}")
        print(f"{'='*60}")

        # Target: call bob through the PBX (PBX routes to registered bob UAS)
        target = f"sip:bob@{self.proxy_host}:{self.proxy_port}"

        # UAC registers as alice so it's a proper P2P call
        username, password = EXTENSION_USERS[1]  # alice

        log_file = os.path.join(self.log_dir, f"uac_batch_{int(time.time())}.log")

        cmd = [
            "sipbot", "call",
            "-t", target,
            "--username", username,
            "--password", password,
            "--register", f"{self.proxy_host}:{self.proxy_port}",
            "--codecs", codecs,
            "--hangup", str(duration),
            "--total", str(total),
            "--cps", str(cps),
        ]
        if cancel_prob > 0:
            cmd += ["--cancel-prob", str(cancel_prob)]
        if webrtc:
            cmd.append("--webrtc")
        if audio_quality:
            cmd.append("--audio-quality")
        if not soak:
            cmd.append("-v")  # -v logs every RTP packet — unusable over long runs

        # In soak mode, cap retained lines (full output still goes to log file)
        self.uac_process = SipProcess(
            "uac-batch", log_file=log_file,
            retain_lines=8000 if soak else 0,
        )
        self.uac_process.start(cmd)
        print(f"[UAC] Batch started (log: {log_file})")

        # Wait for completion with generous timeout
        timeout = max(120, total // max(cps, 1) + duration + 60)
        t_start = time.time()
        self.uac_process.wait(timeout=timeout)
        wall_time = time.time() - t_start

        if soak:
            output = self.uac_process.log_tail(n_bytes=2_000_000)
        else:
            output = self.uac_process.output()
        return output, wall_time

    def _run_uac_soak(
        self, cps: int, duration: int, wall_time: int, batch_window: int,
        codecs: str = "pcmu", webrtc: bool = False, audio_quality: bool = False,
    ) -> tuple[str, float]:
        """Sustain load for wall_time by looping sipbot batches.

        Each batch places cps*batch_window calls. Sequential batches keep
        concurrency approximately stable (brief ~1-2s dip between batches as
        calls churn at duration-second hangup)."""
        import math
        batches = max(1, math.ceil(wall_time / batch_window))
        batch_total = cps * batch_window
        print(f"\n{'='*60}")
        print(f"[SOAK] loop batching: {batches} batches × {batch_total} calls"
              f" (≈{batch_window}s each) ≈ {batches * batch_total} calls over ~{wall_time}s")
        print(f"{'='*60}")

        agg_completed = 0
        agg_total = 0
        agg_status: dict[str, int] = {}
        t0 = time.time()

        try:
            for i in range(batches):
                output, _bw = self._run_uac_single(batch_total, cps, duration, soak=True, codecs=codecs, webrtc=webrtc, audio_quality=audio_quality)
                completed, ctotal, status = self._parse_batch_progress(output)
                agg_completed += completed
                agg_total += ctotal
                for k, v in status.items():
                    agg_status[k] = agg_status.get(k, 0) + v
                elapsed = time.time() - t0
                status_str = ", ".join(f"{k}:{v}" for k, v in sorted(agg_status.items())) or "-"
                print(f"[SOAK] batch {i+1}/{batches} done: +{completed} completed "
                      f"(cum {agg_completed}/{agg_total}), elapsed {elapsed:.0f}s "
                      f"[{status_str}]", flush=True)
        except KeyboardInterrupt:
            print("\n[SOAK] interrupted — stopping batch loop")

        wall = time.time() - t0
        # Synthesize a final Progress line with cumulative counts for the
        # result collector (parse_stress_metrics reads the last Progress line).
        status_str = ", ".join(f"{k}:{v}" for k, v in sorted(agg_status.items())) or "-"
        synthetic = (
            f"Progress: {agg_completed}/{agg_total}, Active: 0, {status_str}\n"
        )
        return synthetic, wall

    @staticmethod
    def _parse_batch_progress(output: str) -> tuple[int, int, dict[str, int]]:
        """Parse the LAST 'Progress: a/b, ... 200: n, 4xx: n' line from sipbot.

        Returns (completed, total, status_counts)."""
        completed = 0
        total = 0
        status: dict[str, int] = {}
        prog_lines = [l for l in output.split("\n") if l.startswith("Progress:")]
        if not prog_lines:
            return completed, total, status
        line = prog_lines[-1]
        m = re.search(r"Progress:\s*(\d+)\s*/\s*(\d+)", line)
        if m:
            completed = int(m.group(1))
            total = int(m.group(2))
        # status tokens like "200: 123", "4xx: 5", "3xx: 0"
        for tok in re.findall(r"(\d+[xsx]{0,2}):\s*(\d+)", line):
            key, val = tok[0], int(tok[1])
            if key in ("200", "180", "183") or key.endswith("xx"):
                status[key] = status.get(key, 0) + val
        return completed, total, status

    # -----------------------------------------------------------------------
    # Monitoring
    # -----------------------------------------------------------------------

    def start_monitoring(
        self,
        interval: float = 1.0,
        leak_check_interval: float = 0.0,
        target_concurrency: int = 0,
        leak_csv: str | None = None,
    ) -> None:
        health_url = f"{self.http_base}/ami/v1/health"
        self.monitor = ResourceMonitor(
            process_name="rustpbx",
            interval=interval,
            health_url=health_url,
            leak_check_interval=leak_check_interval,
            target_concurrency=target_concurrency,
            leak_csv=leak_csv,
        )
        self.monitor.start()
        extra = f", leak-check every {leak_check_interval:.0f}s (target≈{target_concurrency or 'auto'} calls)" if leak_check_interval else ""
        print(f"[monitor] Started (interval={interval}s{extra})")

    def stop_monitoring(self) -> dict[str, Any]:
        if self.monitor:
            self.monitor.stop()
            return self.monitor.summary()
        return {}

    # -----------------------------------------------------------------------
    # Main benchmark runner
    # -----------------------------------------------------------------------

    def run_benchmark(
        self,
        scenario_name: str,
        total: int,
        cps: int,
        duration: int,
        mediaproxy: str,
        sipflow: bool,
        uas_count: int,
        uas_base_port: int = DEFAULT_UAS_BASE_PORT,
        wholesale: bool = False,
        wall_time: int = 0,
        leak_check_interval: int = 0,
        uas_codecs: str = "pcmu",
        uac_codecs: str = "pcmu",
        recording: bool = False,
        webrtc: bool = False,
        audio_quality: bool = False,
    ) -> BenchmarkResult:
        """Run a single benchmark scenario."""
        # In wall-time (soak) mode, derive total so the batch sustains for the
        # full duration: total = cps * wall_time. Concurrency ≈ cps * duration.
        soak = wall_time > 0
        if soak:
            total = cps * wall_time

        result = BenchmarkResult(
            scenario=scenario_name,
            total_calls=total,
            duration=duration,
            mediaproxy=mediaproxy,
            sipflow_enabled=sipflow,
            uas_count=uas_count,
            cps=cps,
            start_time=datetime.now(timezone.utc).isoformat(),
        )

        target_concurrency = min(cps * duration, total)

        print(f"\n{'='*70}")
        print(f"BENCHMARK: {scenario_name}")
        print(f"{'='*70}")
        print(f"Configuration:")
        print(f"  Total Calls     : {total}")
        print(f"  CPS             : {cps}")
        print(f"  Call Duration   : {duration}s")
        print(f"  UAS Count       : {uas_count}")
        print(f"  Media Proxy     : {mediaproxy}")
        print(f"  SIP Flow        : {sipflow} ({'remote' if sipflow else 'off'})")
        print(f"  Wholesale       : {wholesale}")
        if soak:
            print(f"  Wall-Time       : {wall_time}s ({wall_time//3600}h{(wall_time%3600)//60}m)")
            print(f"  Leak Check      : every {leak_check_interval}s")
        print(f"  Est. Concurrent : {target_concurrency}")
        print(f"{'='*70}\n")

        try:
            # 1. Start rustpbx
            if not self.start_rustpbx(mediaproxy=mediaproxy, sipflow=sipflow, wholesale=wholesale,
                                      recording=recording, webrtc=webrtc):
                result.errors.append("Failed to start rustpbx")
                return result

            time.sleep(2)

            # 2. Start UAS instances (hangup > call_duration so UAS doesn't hang up early)
            if not self.start_uas_instances(uas_count, base_port=uas_base_port, hangup=duration + 30,
                                            verbose=not soak, codecs=uas_codecs, webrtc=webrtc,
                                            audio_quality=audio_quality):
                result.errors.append("Failed to start UAS instances")
                return result

            # 3. Start monitoring (with periodic leak analysis in soak mode)
            leak_csv = os.path.join(self.log_dir, "leak_check.csv") if leak_check_interval else None
            self.start_monitoring(
                interval=1.0,
                leak_check_interval=float(leak_check_interval),
                target_concurrency=target_concurrency,
                leak_csv=leak_csv,
            )

            # 4. Run UAC batch (loops sipbot batches in soak mode)
            uac_output, wall_time = self.run_uac_batch(
                total, cps, duration, soak=soak, wall_time=wall_time,
                cancel_prob=self.cancel_prob, codecs=uac_codecs, webrtc=webrtc,
                audio_quality=audio_quality,
            )
            result.test_duration_s = wall_time

            # 5. Allow stats to settle
            time.sleep(2)

            # 6. Stop monitoring (triggers final leak check)
            resource_summary = self.stop_monitoring()

            # 7. Drain + assert: calls/tasks/sessions must return to baseline.
            drain_ok = self.drain_and_assert(result)

            # 8. Collect results
            self._collect_results(result, uac_output, resource_summary)
            result.drain_passed = drain_ok

            # 9. Verify audio format (PT + samplerate) on each leg.
            self.verify_audio_format(result, uac_codecs, uas_codecs)

            # 10. Verify media continuity (no seq/ts jumps → no audio glitches).
            self.verify_media_continuity(result)

            result.end_time = datetime.now(timezone.utc).isoformat()

        except Exception as e:
            result.errors.append(f"Exception: {e}")
            import traceback
            traceback.print_exc()

        finally:
            self.cleanup()

        return result

    def _collect_results(
        self,
        result: BenchmarkResult,
        uac_output: str,
        resource_summary: dict[str, Any],
    ) -> None:
        """Collect results from UAC output and resource monitor."""
        # Resource usage
        result.cpu_avg = resource_summary.get("cpu_avg", 0.0)
        result.cpu_peak = resource_summary.get("cpu_peak", 0.0)
        result.mem_avg_mb = resource_summary.get("mem_avg_mb", 0.0)
        result.mem_peak_mb = resource_summary.get("mem_peak_mb", 0.0)
        result.calls_peak = resource_summary.get("calls_peak", 0)
        result.calls_avg = resource_summary.get("calls_avg", 0.0)
        # Leak analysis (soak mode)
        result.leak_final_assessment = resource_summary.get("leak_final_assessment")
        result.leak_final_slope_mb_per_min = resource_summary.get("leak_final_slope_mb_per_min", 0.0)
        result.leak_base_delta_mb = resource_summary.get("leak_base_delta_mb", 0.0)

        # Media/session task-count leak signals (drift after drain)
        result.task_drift = {
            "tasks_total": resource_summary.get("tasks_total_drift"),
            "media_alive_tasks": resource_summary.get("media_alive_tasks_drift"),
            "sip_alive_tasks": resource_summary.get("sip_alive_tasks_drift"),
            "leak_handles": resource_summary.get("leak_handles_drift"),
            "leak_dialogs": resource_summary.get("leak_dialogs_drift"),
            "calls_end": resource_summary.get("calls_end"),
        }

        # Parse UAC metrics
        metrics = parse_stress_metrics(uac_output)

        result.calls_completed = metrics["completed"]
        result.calls_failed = result.total_calls - metrics["completed"]
        if result.total_calls > 0:
            result.success_rate = (metrics["completed"] / result.total_calls) * 100

        result.avg_setup_latency_ms = metrics["avg_setup_latency_ms"]
        result.avg_rtt_ms = metrics["avg_rtt_ms"]
        result.avg_loss_pct = metrics["avg_loss_pct"]
        result.max_loss_pct = metrics["max_loss_pct"]
        result.tx_packets = metrics["tx_packets"]
        result.rx_packets = metrics["rx_packets"]
        result.status_counts = metrics["status_counts"]

    # -----------------------------------------------------------------------
    # Drain + leak assertion (the core media leak check)
    # -----------------------------------------------------------------------

    def drain_and_assert(self, result: BenchmarkResult, drain_timeout: int = 45,
                         task_drift_max: int = 20, handles_max: int = 5) -> bool:
        """After the load wave, poll /ami/v1/health until active calls drain to 0,
        then assert sessions/tasks return to baseline.

        Fails (returns False) when calls stay stuck, leaked handles/dialogs
        remain, or the tracked task count drifts beyond `task_drift_max`.
        """
        health_url = f"{self.http_base}/ami/v1/health"
        details: dict[str, Any] = {}

        def fetch() -> dict:
            try:
                req = urllib.request.Request(health_url)
                with urllib.request.urlopen(req, timeout=3) as resp:
                    return json.loads(resp.read())
            except Exception:
                return {}

        # 1. Wait for active calls to drain. A successful fetch must be observed;
        #    if the health endpoint is unreachable the whole time, calls stays -1
        #    and we FAIL (cannot confirm drain).
        deadline = time.time() + drain_timeout
        calls = -1
        saw_health = False
        while time.time() < deadline:
            data = fetch()
            if "sipserver" in data:
                saw_health = True
                calls = data.get("sipserver", {}).get("calls", calls)
                if calls in (0, None):
                    break
            time.sleep(1)
        details["calls_at_end"] = calls
        details["health_observed"] = saw_health

        # 2. Final leak gauges + task counts (best-effort).
        data = fetch()
        leak = data.get("sipserver", {}).get("leak", {}) or {}
        tokio = data.get("tokio", {})
        details["leak_handles"] = leak.get("handles_by_dialog", 0)
        details["leak_dialogs"] = leak.get("dialogs_by_session", 0)
        details["tasks_total"] = data.get("tasks", {}).get("total", 0)
        details["media_alive_tasks"] = (tokio.get("media") or {}).get("num_alive_tasks", 0)

        result.drain_details = details

        problems: list[str] = []
        if not saw_health:
            problems.append("health endpoint unreachable — cannot confirm drain")
        if calls not in (0, None):
            problems.append(f"calls stuck at {calls}")
        if details.get("leak_handles", 0) > handles_max:
            problems.append(f"leaked handles_by_dialog={details['leak_handles']}")
        if details.get("leak_dialogs", 0) > handles_max:
            problems.append(f"leaked dialogs_by_session={details['leak_dialogs']}")
        if details.get("tasks_total", 0) > task_drift_max:
            problems.append(f"tasks.total={details['tasks_total']} > {task_drift_max}")
        if details.get("media_alive_tasks", 0) > task_drift_max:
            problems.append(f"media alive tasks={details['media_alive_tasks']} > {task_drift_max}")

        ok = not problems
        if ok:
            print(f"[drain] PASS — calls={calls}, handles={details.get('leak_handles')}, "
                  f"dialogs={details.get('leak_dialogs')}, tasks={details.get('tasks_total')}")
        else:
            msg = "; ".join(problems)
            result.errors.append(f"drain/leak failed: {msg}")
            print(f"  ⚠ [drain] FAIL — {msg}", flush=True)
        return ok

    # -----------------------------------------------------------------------
    # Conference (MCU mixer) benchmark
    # -----------------------------------------------------------------------

    # Negotiated codec → (payload_type, sample_rate). Used to assert each leg
    # of every scenario negotiated the expected audio format end-to-end.
    AUDIO_FORMAT: dict[str, tuple[int, int]] = {
        "pcmu": (0, 8000),
        "pcma": (8, 8000),
        "g722": (9, 8000),
        "g729": (18, 8000),
        "opus": (111, 48000),
    }

    @staticmethod
    def _codec_normalize(name: str) -> str:
        return name.strip().lower()

    def verify_audio_format(self, result: BenchmarkResult,
                            uac_codecs: str, uas_codecs: str) -> bool:
        """Assert the UAC and UAS legs negotiated the expected codec (hence the
        expected PT + sample rate) across the whole run.

        Parses sipbot logs: UAC `codec: X` / `Preferred Codec: X`; UAS
        `Negotiated Codec: X` / `Preferred Codec: X`. The dominant negotiated
        codec on each leg must equal the scenario's codec.
        """
        def dominant(paths: list[str], patterns: list[str]) -> str | None:
            counts: dict[str, int] = {}
            import glob as _glob
            for p in paths:
                for f in _glob.glob(p):
                    try:
                        text = open(f, errors="replace").read()
                    except Exception:
                        continue
                    for pat in patterns:
                        for m in re.finditer(pat, text):
                            c = self._codec_normalize(m.group(1))
                            counts[c] = counts.get(c, 0) + 1
            if not counts:
                return None
            return max(counts, key=counts.get)

        uac_codes = ["codec: ([A-Za-z0-9]+)", "Preferred Codec: ([A-Za-z0-9]+)"]
        uas_codes = ["Negotiated Codec: ([A-Za-z0-9]+)", "Preferred Codec: ([A-Za-z0-9]+)"]

        uac_actual = dominant(
            [getattr(self, "uac_process", None) and getattr(self.uac_process, "_log_file", "") or ""],
            uac_codes,
        )
        uas_paths = [getattr(u, "_log_file", "") for u in getattr(self, "uas_list", [])]
        uas_actual = dominant(uas_paths, uas_codes)

        # Actual-audio samplerate check: sipbot's --audio-quality analyzer reports
        # `mismatch=N` when the decoded frame sample count does not match the codec's
        # declared sample rate (i.e. the actual audio rate is wrong). N>0 = FAIL.
        def max_mismatch(paths: list[str]) -> int:
            worst = 0
            import glob as _glob
            for p in paths:
                for f in _glob.glob(p):
                    try:
                        text = open(f, errors="replace").read()
                    except Exception:
                        continue
                    for m in re.finditer(r"mismatch=(\d+)", text):
                        worst = max(worst, int(m.group(1)))
            return worst

        uac_mm = max_mismatch(
            [getattr(self, "uac_process", None) and getattr(self.uac_process, "_log_file", "") or ""])
        uas_mm = max_mismatch(uas_paths)

        exp_uac = self._codec_normalize(uac_codecs)
        exp_uas = self._codec_normalize(uas_codecs)
        uac_pt, uac_sr = self.AUDIO_FORMAT.get(exp_uac, (None, None))
        uas_pt, uas_sr = self.AUDIO_FORMAT.get(exp_uas, (None, None))

        def fmt(codec: str | None, pt, sr) -> dict:
            if codec is None:
                return {"codec": None, "pt": None, "samplerate": None, "observed": False}
            return {"codec": codec, "pt": pt, "samplerate": sr, "observed": True}

        ok_uac = uac_actual is not None and uac_actual == exp_uac
        ok_uas = uas_actual is not None and uas_actual == exp_uas
        ok_rate = (uac_mm == 0) and (uas_mm == 0)

        result.audio_format = {
            "uac": {"expected": exp_uac, "actual": uac_actual,
                    "pt": uac_pt, "samplerate": uac_sr,
                    "rate_mismatch": uac_mm, "ok": ok_uac},
            "uas": {"expected": exp_uas, "actual": uas_actual,
                    "pt": uas_pt, "samplerate": uas_sr,
                    "rate_mismatch": uas_mm, "ok": ok_uas},
            "pass": ok_uac and ok_uas and ok_rate,
        }
        if not ok_uac:
            result.errors.append(
                f"audio-format UAC: expected {exp_uac} (PT {uac_pt}, {uac_sr}Hz), "
                f"negotiated {uac_actual}")
        if not ok_uas:
            result.errors.append(
                f"audio-format UAS: expected {exp_uas} (PT {uas_pt}, {uas_sr}Hz), "
                f"negotiated {uas_actual}")
        if uac_mm > 0:
            result.errors.append(f"actual-audio UAC: {uac_mm} samplerate mismatches")
        if uas_mm > 0:
            result.errors.append(f"actual-audio UAS: {uas_mm} samplerate mismatches")
        return result.audio_format["pass"]

    @staticmethod
    def _parse_jump_max(paths: list[str], key: str) -> int:
        """Worst (max) value of one seq/ts jump stat grepped from sipbot logs
        (`jumps=[seq_gap_events=.., ts_jump_events=.., ...]`)."""
        import glob as _glob

        worst = 0
        for p in paths:
            if not p:
                continue
            for f in _glob.glob(p):
                try:
                    text = open(f, errors="replace").read()
                except Exception:
                    continue
                for m in re.finditer(r"jumps=\[(.*?)\]", text):
                    for kv in m.group(1).split(","):
                        k, v = kv.strip().split("=", 1)
                        if k.strip() == key:
                            try:
                                worst = max(worst, int(v))
                            except ValueError:
                                pass
        return worst

    def verify_media_continuity(self, result: BenchmarkResult) -> bool:
        """Assert the media stream stayed continuous (no seq/ts jumps).

        The UAC is authoritative: each caller call has its own track, so its
        `seq_gap_events`/`ts_jump_events` directly reflect stream glitches.
        The UAS recorder path collapses many concurrent calls into one seq
        tracker, so it can false-positive `gap=1` under concurrency — reported
        but not a hard failure.
        """
        uac_paths = [
            getattr(self, "uac_process", None) and getattr(self.uac_process, "_log_file", "") or ""
        ]
        uas_paths = [getattr(u, "_log_file", "") for u in getattr(self, "uas_list", [])]

        def grab(paths, key):
            w = 0
            for p in paths:
                w = max(w, self._parse_jump_max([p], key))
            return w

        uac_gap = grab(uac_paths, "seq_gap_events")
        uac_ts = grab(uac_paths, "ts_jump_events")
        uas_gap = grab(uas_paths, "seq_gap_events")
        uas_ts = grab(uas_paths, "ts_jump_events")

        ok_uac = uac_gap == 0 and uac_ts == 0
        ok_uas = uas_gap == 0 and uas_ts == 0

        result.media_continuity = {
            "pass": ok_uac,
            "uac": {"seq_gap_events": uac_gap, "ts_jump_events": uac_ts, "ok": ok_uac},
            "uas": {"seq_gap_events": uas_gap, "ts_jump_events": uas_ts, "ok": ok_uas},
        }
        if not ok_uac:
            result.errors.append(
                f"media-continuity UAC: seq_gap_events={uac_gap} ts_jump_events={uac_ts} "
                f"(audio glitch detected)")
        if not ok_uas:
            result.errors.append(
                f"media-continuity UAS: seq_gap_events={uas_gap} ts_jump_events={uas_ts} "
                f"(may be multi-call tracker artifact if UAC clean)")
        return ok_uac

    def run_audio_verify(self, uac_codecs: str = "pcmu", uas_codecs: str = "opus",
                         webrtc: bool = False, hold: int = 6) -> dict:
        """Definitive actual-audio samplerate check via a tone.

        Injects a known 440 Hz tone (8 kHz WAV) from the UAC; the UAS echoes and
        the UAC records. If the media/samplerate path is correct, the recorded
        echo's dominant frequency is ~440 Hz; if a codec/resampler applies the
        wrong sample rate, the frequency shifts proportionally (e.g. 440 Hz at
        8 kHz played as 48 kHz would appear near 2640 Hz).
        """
        try:
            sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "e2e"))
            from helpers.audio_verifier import (generate_sine_wav, read_wav_stereo,
                                                find_signal_start, extract_audio_region,
                                                find_dominant_frequency)
        except Exception as e:
            return {"pass": False, "error": f"audio_verifier import failed: {e}"}

        # Start PBX + one echo UAS with the target callee codec.
        if not self.start_rustpbx(mediaproxy="all", sipflow=False):
            return {"pass": False, "error": "start rustpbx failed"}
        time.sleep(2)
        if not self.start_uas_instances(1, base_port=DEFAULT_UAS_BASE_PORT, hangup=hold + 10,
                                        verbose=True, codecs=uas_codecs, webrtc=webrtc):
            return {"pass": False, "error": "start UAS failed"}

        # Generate a 440 Hz tone at 8 kHz.
        tone = os.path.join(self.log_dir, f"tone_440_{int(time.time())}.wav")
        generate_sine_wav(tone, 440.0, 1.0, 8000, 0.5)

        # UAC: call bob, play the tone, record RX (the echo).
        rec = os.path.join(self.log_dir, f"caller_rec_{int(time.time())}.wav")
        target = f"sip:bob@{self.proxy_host}:{self.proxy_port}"
        username, password = EXTENSION_USERS[1]  # alice
        cmd = ["sipbot", "call", "-t", target, "--username", username, "--password", password,
               "--register", f"{self.proxy_host}:{self.proxy_port}",
               "--codecs", uac_codecs, "--hangup", str(hold),
               "--play", tone, "--record", rec, "-v"]
        if webrtc:
            cmd.append("--webrtc")
        uac = SipProcess("audio-verify-uac", log_file=rec + ".log")
        uac.start(cmd)
        uac.wait(timeout=hold + 30)
        time.sleep(2)

        # Analyze the caller's recorded RX for the dominant frequency.
        try:
            rx, _tx, sr = read_wav_stereo(rec)
            start = find_signal_start(rx, 0.01, sr // 50)
            region = extract_audio_region(rx, sr, start, 4000)
            freq, mag = find_dominant_frequency(region, sr, 100, 1000, 2.0)
            ok = abs(freq - 440.0) < 40.0
        except Exception as e:
            return {"pass": False, "error": f"analyze failed: {e}", "note": "no valid recorded audio"}

        result = {
            "pass": ok,
            "injected_hz": 440.0,
            "detected_hz": round(freq, 1),
            "wav_samplerate": sr,
            "uac_codec": uac_codecs,
            "uas_codec": uas_codecs,
            "webrtc": webrtc,
        }
        print(f"[audio-verify] tone 440Hz -> detected {freq:.1f}Hz (wav sr={sr}) "
              f"[{'PASS' if ok else 'FAIL'}]")
        return result

    def run_idle_gap(
        self,
        uac_codecs: str = "pcmu",
        uas_codecs: str = "pcmu",
        webrtc: bool = False,
        gap: int = 2,
        hold: int = 6,
    ) -> dict:
        """Verify the "no source / on-hold" window keeps the stream continuous.

        The caller (UAC) sends a re-INVITE hold for `gap` seconds mid-call —
        during that window the PBX egress for the caller has no media source and
        must emit continuous comfort-noise/silence frames (fixed seq/ts cadence),
        not a gap. On resume the audio must come back cleanly.

        Assertions:
          - UAC rx: seq_gap_events=0 and ts_jump_events=0 across the whole call
            (proves the hold window + resume transition did not glitch).
          - The recorded RX has signal after resume (no dead tail / corruption).
        """
        try:
            sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "e2e"))
            from helpers.audio_verifier import (generate_sine_wav, read_wav_stereo)
        except Exception as e:
            return {"pass": False, "error": f"audio_verifier import failed: {e}"}

        # Delays are cumulative: hold at `gap`s, resume at 2*`gap`s. Keep resume
        # well before hangup (`hold`s) so there is clean audio after resume.
        flows = f"{gap}s:hold,{gap}s:resume"
        if not self.start_rustpbx(mediaproxy="all", sipflow=False, webrtc=webrtc):
            return {"pass": False, "error": "start rustpbx failed"}
        time.sleep(2)
        if not self.start_uas_instances(1, base_port=DEFAULT_UAS_BASE_PORT,
                                        hangup=hold + 10, verbose=True,
                                        codecs=uas_codecs, webrtc=webrtc,
                                        audio_quality=True):
            return {"pass": False, "error": "start UAS failed"}

        tone = os.path.join(self.log_dir, f"tone_440_{int(time.time())}.wav")
        generate_sine_wav(tone, 440.0, float(hold), 8000, 0.5)
        rec = os.path.join(self.log_dir, f"idle_gap_rec_{int(time.time())}.wav")
        target = f"sip:bob@{self.proxy_host}:{self.proxy_port}"
        username, password = EXTENSION_USERS[1]  # alice
        cmd = ["sipbot", "call", "-t", target, "--username", username, "--password", password,
               "--register", f"{self.proxy_host}:{self.proxy_port}",
               "--codecs", uac_codecs, "--hangup", str(hold),
               "--play", tone, "--record", rec, "--audio-quality", "-v",
               "--reinvite-flows", flows]
        if webrtc:
            cmd.append("--webrtc")
        uac = SipProcess("idle-gap-uac", log_file=rec + ".log")
        uac.start(cmd)
        uac.wait(timeout=hold + 30)
        time.sleep(2)

        # 1. Jump stats on the caller (authoritative, single call).
        uac_gap = self._parse_jump_max([rec + ".log"], "seq_gap_events")
        uac_ts = self._parse_jump_max([rec + ".log"], "ts_jump_events")
        jumps_ok = uac_gap == 0 and uac_ts == 0

        # 2. Recorded RX must contain signal after the hold/resume (tail).
        tail_ok = False
        sr = 0
        tail_rms = 0.0
        try:
            rx, _tx, sr = read_wav_stereo(rec)
            if len(rx) > sr:  # at least ~1s of audio
                tail = rx[-int(sr * 1.5):]
                tail_rms = float((tail.astype("float64") ** 2).mean() ** 0.5)
                tail_ok = tail_rms > 200.0
        except Exception as e:
            return {"pass": False, "error": f"analyze failed: {e}", "note": "no valid recorded audio"}

        ok = jumps_ok and tail_ok
        result = {
            "pass": ok,
            "flows": flows,
            "uac_seq_gap_events": uac_gap,
            "uac_ts_jump_events": uac_ts,
            "tail_rms": round(tail_rms, 1),
            "wav_samplerate": sr,
            "uac_codec": uac_codecs,
            "uas_codec": uas_codecs,
            "webrtc": webrtc,
        }
        print(f"[idle-gap] flows={flows} seq_gap={uac_gap} ts_jump={uac_ts} "
              f"tail_rms={tail_rms:.1f} [{'PASS' if ok else 'FAIL'}]")
        return result

    @staticmethod
    def _rwi_call(ws, action: str, params: dict, aid: str) -> dict | None:
        """Send one RWI request and wait for the matching response."""
        import websockets  # local import: conference mode only

        req = {"rwi": "1.0", "action_id": aid, "action": action, "params": params}
        ws.send(json.dumps(req))
        deadline = time.time() + 15
        while time.time() < deadline:
            raw = ws.recv(timeout=15)
            try:
                msg = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if msg.get("action_id") == aid:
                return msg
        return None

    def run_conference_benchmark(
        self,
        members: int,
        rounds: int,
        hold_secs: int,
        wall_time: int = 0,
        leak_check_interval: int = 0,
        uas_base_port: int = DEFAULT_UAS_BASE_PORT,
        mediaproxy: str = "all",
    ) -> BenchmarkResult:
        """Sustain an N-way conference (MCU mixer path) via RWI + sipbot echo UAs.

        Each round: originate calls to `members` UAs, conference.create + add all,
        hold `hold_secs` (mixer runs), destroy + hangup. RSS + task counts are
        sampled throughout; a final task-drift check flags leaked media tasks.
        """
        import websockets  # local import: conference mode only

        scenario = f"conference_{members}way"
        result = BenchmarkResult(
            scenario=scenario,
            total_calls=rounds * members,
            duration=hold_secs,
            mediaproxy=mediaproxy,
            sipflow_enabled=False,
            uas_count=members,
            cps=0,
            start_time=datetime.now(timezone.utc).isoformat(),
        )
        print(f"\n{'='*70}\nCONFERENCE BENCHMARK: {scenario} "
              f"({rounds} rounds × {members} members, hold {hold_secs}s)\n{'='*70}")

        try:
            if not self.start_rustpbx(mediaproxy=mediaproxy, sipflow=False):
                result.errors.append("Failed to start rustpbx")
                return result
            time.sleep(2)
            if not self.start_uas_instances(members, base_port=uas_base_port,
                                            hangup=hold_secs + 60, verbose=True):
                result.errors.append("Failed to start UAS instances")
                return result

            leak_csv = os.path.join(self.log_dir, "leak_check.csv") if leak_check_interval else None
            self.start_monitoring(
                interval=1.0,
                leak_check_interval=float(leak_check_interval),
                target_concurrency=members,
                leak_csv=leak_csv,
            )

            token = "bench-rwi-token"
            ws_url = f"ws://127.0.0.1:{self.http_base.rsplit(':', 1)[-1]}/rwi/v1?token={token}"
            t0 = time.time()
            total_rounds = rounds if wall_time <= 0 else max(1, int(wall_time / (hold_secs + 3)))
            ok_rounds = 0
            try:
                with websockets.connect(ws_url) as ws:
                    for r in range(total_rounds):
                        conf_id = f"bench-conf-{r}"
                        call_ids: list[str] = []
                        for m in range(members):
                            user, pw = EXTENSION_USERS[m % len(EXTENSION_USERS)]
                            cid = f"bench-{r}-{m}"
                            aid = f"o{r}-{m}"
                            resp = self._rwi_call(ws, "call.originate", {
                                "call_id": cid,
                                "destination": f"sip:{user}@{self.proxy_host}:{self.proxy_port}",
                                "caller_id": f"sip:bench@{self.proxy_host}",
                                "context": "default",
                                "timeout_secs": 20,
                            }, aid)
                            if not resp or resp.get("type") != "command_completed":
                                continue
                            call_ids.append(cid)
                            # Give the call a moment to answer before joining.
                            time.sleep(1.0)
                        if len(call_ids) < 2:
                            print(f"[conf] round {r}: only {len(call_ids)} answered, skipping")
                            continue
                        self._rwi_call(ws, "conference.create",
                                       {"conference_id": conf_id, "max_members": members}, f"c{r}")
                        for m, cid in enumerate(call_ids):
                            self._rwi_call(ws, "conference.add",
                                           {"conference_id": conf_id, "call_id": cid}, f"a{r}-{m}")
                        time.sleep(hold_secs)  # mixer running
                        self._rwi_call(ws, "conference.destroy", {"conference_id": conf_id}, f"d{r}")
                        for cid in call_ids:
                            self._rwi_call(ws, "call.hangup", {"call_id": cid}, f"h{r}-{cid}")
                        ok_rounds += 1
                        print(f"[conf] round {r+1}/{total_rounds} ok "
                              f"({len(call_ids)} members), elapsed {time.time()-t0:.0f}s", flush=True)
            except KeyboardInterrupt:
                print("[conf] interrupted")
            except Exception as e:
                print(f"[conf] error: {e}")
                result.errors.append(str(e))

            result.test_duration_s = time.time() - t0
            time.sleep(3)  # drain
            summary = self.stop_monitoring()

            result.calls_completed = ok_rounds * members
            result.calls_failed = result.total_calls - result.calls_completed
            if result.total_calls > 0:
                result.success_rate = (result.calls_completed / result.total_calls) * 100
            result.cpu_avg = summary.get("cpu_avg", 0.0)
            result.cpu_peak = summary.get("cpu_peak", 0.0)
            result.mem_avg_mb = summary.get("mem_avg_mb", 0.0)
            result.mem_peak_mb = summary.get("mem_peak_mb", 0.0)
            result.calls_peak = summary.get("calls_peak", 0)
            result.calls_avg = summary.get("calls_avg", 0.0)
            result.leak_final_assessment = summary.get("leak_final_assessment")
            result.leak_final_slope_mb_per_min = summary.get("leak_final_slope_mb_per_min", 0.0)
            result.leak_base_delta_mb = summary.get("leak_base_delta_mb", 0.0)
            result.task_drift = {
                "tasks_total": summary.get("tasks_total_drift"),
                "media_alive_tasks": summary.get("media_alive_tasks_drift"),
                "sip_alive_tasks": summary.get("sip_alive_tasks_drift"),
                "leak_handles": summary.get("leak_handles_drift"),
                "leak_dialogs": summary.get("leak_dialogs_drift"),
                "calls_end": summary.get("calls_end"),
            }
            result.end_time = datetime.now(timezone.utc).isoformat()
        finally:
            pass
        return result

    # -----------------------------------------------------------------------
    # Cleanup
    # -----------------------------------------------------------------------

    def cleanup(self) -> None:
        print("[cleanup] Stopping all processes...")
        if self.uac_process:
            self.uac_process.terminate()
            self.uac_process = None
        for uas in self.uas_list:
            uas.terminate()
        self.uas_list = []
        if self.monitor:
            self.monitor.stop()
            self.monitor = None
        if self.rustpbx_process:
            self.rustpbx_process.terminate()
            try:
                self.rustpbx_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.rustpbx_process.kill()
                self.rustpbx_process.wait()
            self.rustpbx_process = None
        self.stop_sipflow_server()

    def kill_mysql_proxy(self) -> None:
        if self._mysql_proxy_process:
            self._mysql_proxy_process.terminate()
            try:
                self._mysql_proxy_process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self._mysql_proxy_process.kill()
            self._mysql_proxy_process = None

    # -----------------------------------------------------------------------
    # Memory-leak batch test
    # -----------------------------------------------------------------------

    def _rustpbx_rss_mb(self) -> float:
        """Return total RSS (MB) of all running rustpbx processes (excludes self)."""
        import platform
        try:
            my_pid = os.getpid()
            if platform.system() == "Darwin":
                pgrep = subprocess.run(
                    ["pgrep", "-f", "rustpbx"],
                    capture_output=True, text=True, timeout=5,
                )
                pids = [
                    p.strip() for p in pgrep.stdout.strip().split("\n")
                    if p.strip() and p.strip().isdigit() and int(p.strip()) != my_pid
                ]
                if not pids:
                    return 0.0
                result = subprocess.run(
                    ["ps", "-o", "pid,rss", "-p", ",".join(pids)],
                    capture_output=True, text=True, timeout=5,
                )
            else:
                result = subprocess.run(
                    ["ps", "-C", "rustpbx", "-o", "pid,rss", "--no-headers"],
                    capture_output=True, text=True, timeout=5,
                )
            total_kb = 0.0
            for line in result.stdout.strip().split("\n"):
                parts = line.split()
                if len(parts) >= 2 and parts[0].isdigit() and int(parts[0]) != my_pid:
                    total_kb += float(parts[-1])
                elif len(parts) == 1:
                    try:
                        total_kb += float(parts[0])
                    except ValueError:
                        pass
            return total_kb / 1024.0
        except Exception:
            return 0.0

    def _memleak_snapshot(self) -> dict[str, Any]:
        """One-shot snapshot of rustpbx RSS + /ami/v1/health sipserver + task stats.

        Task reclamation (``tasks.total`` returning to baseline after calls
        drain) is the reliable leak signal; RSS is unreliable under jemalloc.
        """
        health_url = f"{self.http_base}/ami/v1/health"
        snap: dict[str, Any] = {
            "rss_mb": self._rustpbx_rss_mb(),
            "calls": 0,
            "dialogs": 0,
            "running_tx": 0,
            "tx_finished": 0,
            "tasks_total": 0,
            "tasks_by_location": {},
        }
        try:
            req = urllib.request.Request(health_url)
            with urllib.request.urlopen(req, timeout=3) as resp:
                data = json.loads(resp.read())
            ss = data.get("sipserver", {}) if isinstance(data, dict) else {}
            snap["calls"] = ss.get("calls", 0)
            snap["dialogs"] = ss.get("dialogs", 0)
            snap["running_tx"] = ss.get("running_tx", 0)
            snap["tx_finished"] = ss.get("transactions", {}).get("finished", 0)
            tasks = data.get("tasks", {}) if isinstance(data, dict) else {}
            snap["tasks_total"] = int(tasks.get("total", 0) or 0)
            locs = tasks.get("by_location", []) or []
            snap["tasks_by_location"] = {
                (e.get("loc", "?")): int(e.get("count", 0) or 0) for e in locs
            }
        except Exception:
            pass
        return snap

    def _wait_drain(self, timeout: float = 30.0) -> bool:
        """Wait until sipserver.calls==0 and running_tx==0 (all calls released)."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            s = self._memleak_snapshot()
            if s["calls"] == 0 and s["running_tx"] == 0:
                return True
            time.sleep(0.5)
        return False

    def run_memleak(
        self,
        total: int,
        batch_size: int,
        cps: int,
        duration: int,
        cancel_prob: int,
        uas_count: int,
        uas_base_port: int,
        external_server: bool,
        uas_ring_duration: float = 0.0,
    ) -> int:
        """Batched memory-leak test.

        Runs ``total`` calls in batches of ``batch_size`` with ``--cancel-prob``,
        sampling RSS + /ami/v1/health after each batch to detect leaked memory.
        Memory that is reclaimed after calls drain indicates no leak; a
        monotonically rising baseline signals a leak.

        ``uas_ring_duration`` > 0 makes UAS ring before answering, so CANCEL
        lands during early media (reliably 487) instead of racing a fast answer.
        """
        print(f"\n{'='*70}")
        print(f"MEMORY-LEAK TEST  (external_server={external_server})")
        print(f"{'='*70}")
        print(f"  Total Calls     : {total}")
        print(f"  Batch Size      : {batch_size} calls (sample /health after each)")
        print(f"  CPS             : {cps}")
        print(f"  Call Duration   : {duration}s (non-cancelled legs)")
        print(f"  Cancel Prob     : {cancel_prob}%  (INVITE then CANCEL)")
        print(f"  UAS Ring Dur    : {uas_ring_duration}s before answer"
              + ("  (CANCEL lands in ringing -> 487)" if uas_ring_duration else ""))
        print(f"  UAS Count       : {uas_count}")
        print(f"  Proxy           : {self.proxy_host}:{self.proxy_port}")
        print(f"  HTTP            : {self.http_base}")
        print(f"{'='*70}\n")

        # 1. Server readiness
        if external_server:
            print("[memleak] Verifying external rustpbx health...")
            if not self._wait_for_rustpbx(timeout=10):
                print("[memleak] External server not reachable — abort.")
                return 1
            print("[memleak] External server healthy")
        else:
            if not self.start_rustpbx(mediaproxy="all", sipflow=False):
                print("[memleak] Failed to start rustpbx")
                return 1
            time.sleep(2)

        # 2. UAS registration (bob/alice)
        if not self.start_uas_instances(
            uas_count, base_port=uas_base_port, hangup=duration + 30,
            ring_duration=uas_ring_duration,
        ):
            print("[memleak] Failed to start UAS instances")
            return 1

        # 3. Baseline (after UAS registered, fully idle)
        self._wait_drain(timeout=30)
        time.sleep(1)
        baseline = self._memleak_snapshot()
        base_rss = baseline["rss_mb"]
        base_tasks = baseline["tasks_total"]
        base_task_loc = dict(baseline["tasks_by_location"])
        if base_rss <= 0:
            print("[memleak] Could not measure rustpbx RSS (is rustpbx running?).")
            return 1
        print(f"[memleak] Baseline RSS = {base_rss:.1f} MB, tasks.total = {base_tasks} "
              f"(calls={baseline['calls']}, dialogs={baseline['dialogs']}, "
              f"running_tx={baseline['running_tx']}, tx_finished={baseline['tx_finished']})\n")

        n_batches = (total + batch_size - 1) // batch_size
        rows: list[dict[str, Any]] = []
        rss_series: list[float] = [base_rss]
        tasks_series: list[int] = [base_tasks]
        comp_total = 0
        canc_total = 0

        print(f"{'batch':>5} {'calls':>5} {'comp':>5} {'cancel':>7} "
              f"{'RSS_MB':>8} {'dVsBase':>9} {'tasks':>6} {'dTsk':>5} "
              f"{'calls':>5} {'runtx':>5} {'drain':>7}")
        print("-" * 86)

        for i in range(1, n_batches + 1):
            this_batch = min(batch_size, total - (i - 1) * batch_size)
            out, _ = self.run_uac_batch(
                this_batch, cps, duration, cancel_prob=cancel_prob
            )
            m = parse_stress_metrics(out)
            comp = m["completed"]
            canc = m["status_counts"].get("487", 0)
            comp_total += comp
            canc_total += canc

            drained = self._wait_drain(timeout=max(30.0, float(duration) * 3))
            time.sleep(0.5)
            s = self._memleak_snapshot()
            delta = s["rss_mb"] - base_rss
            dtasks = s["tasks_total"] - base_tasks
            rss_series.append(s["rss_mb"])
            tasks_series.append(s["tasks_total"])
            rows.append({
                "batch": i,
                "calls": this_batch,
                "completed": comp,
                "cancelled_487": canc,
                "rss_mb": round(s["rss_mb"], 2),
                "delta_mb": round(delta, 2),
                "tasks_total": s["tasks_total"],
                "tasks_delta": dtasks,
                "calls_active": s["calls"],
                "dialogs": s["dialogs"],
                "running_tx": s["running_tx"],
                "tx_finished": s["tx_finished"],
                "drained_ok": bool(drained),
            })
            print(f"{i:>5} {this_batch:>5} {comp:>5} {canc:>7} "
                  f"{s['rss_mb']:>8.1f} {delta:>+9.1f} {s['tasks_total']:>6} "
                  f"{dtasks:>+5} {s['calls']:>5} {s['running_tx']:>5} "
                  f"{'ok' if drained else 'TIMEOUT':>7}")

        # 4. Final drain + tail sample (let caches/GC settle)
        self._wait_drain(timeout=30)
        time.sleep(3)
        tail = self._memleak_snapshot()
        rss_series.append(tail["rss_mb"])
        tasks_series.append(tail["tasks_total"])

        # 5. Analysis + save
        self._memleak_analyze(
            rss_series, base_rss, comp_total, canc_total, rows, cancel_prob, tail,
            tasks_series=tasks_series,
            base_tasks=base_tasks,
            base_task_loc=base_task_loc,
        )
        self._save_memleak(
            rows, base_rss, rss_series, cancel_prob, cps, duration, batch_size, total,
            tasks_series=tasks_series,
            base_tasks=base_tasks,
        )
        return 0

    def _memleak_analyze(
        self,
        rss_series: list[float],
        base_rss: float,
        comp_total: int,
        canc_total: int,
        rows: list[dict[str, Any]],
        cancel_prob: int,
        tail: dict[str, Any],
        tasks_series: list[int] | None = None,
        base_tasks: int = 0,
        base_task_loc: dict[str, int] | None = None,
    ) -> None:
        tasks_series = tasks_series or [base_tasks]
        base_task_loc = base_task_loc or {}

        n = len(rss_series)
        xs = list(range(n))
        mean_x = sum(xs) / n
        mean_y = sum(rss_series) / n
        num = sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, rss_series))
        den = sum((x - mean_x) ** 2 for x in xs) or 1.0
        slope = num / den  # MB per batch
        growth = rss_series[-1] - base_rss
        peak = max(rss_series)
        peak_delta = peak - base_rss

        # ---- Task reclamation analysis (the reliable leak signal) ----
        final_tasks = int(tail.get("tasks_total", tasks_series[-1] if tasks_series else 0))
        task_growth = final_tasks - base_tasks
        # max tasks observed at a drained sample (skip the very first = baseline)
        drained_tasks = tasks_series[1:] if len(tasks_series) > 1 else [base_tasks]
        max_tasks_drained = max(drained_tasks)
        min_tasks_drained = min(drained_tasks)
        # slope of tasks (tasks/batch) — positive monotonic slope = leak
        tn = len(tasks_series)
        txs = list(range(tn))
        tmx = sum(txs) / tn
        tmy = sum(tasks_series) / tn
        tnum = sum((x - tmx) * (y - tmy) for x, y in zip(txs, tasks_series))
        tden = sum((x - tmx) ** 2 for x in txs) or 1.0
        task_slope = tnum / tden

        # Locations whose live task count grew above baseline at the tail
        tail_loc = dict(tail.get("tasks_by_location", {}) or {})
        loc_growth: list[tuple[str, int, int]] = []
        for loc, cnt in tail_loc.items():
            diff = cnt - base_task_loc.get(loc, 0)
            if diff > 0:
                loc_growth.append((loc, cnt, diff))
        # also locations present in baseline but missing at tail are fine
        loc_growth.sort(key=lambda t: -t[2])

        # ---- Verdict: task reclamation is authoritative; RSS is advisory ----
        if task_growth <= 1 and abs(task_slope) < 0.5:
            verdict = "[OK] NO LEAK — tasks fully reclaimed after each batch"
        elif task_growth <= 1:
            verdict = "[OK] NO LEAK — tasks return to baseline (transient growth during calls)"
        elif task_growth < 5 and abs(task_slope) < 1.0:
            verdict = "[STABLE] small residual task growth — likely fine, monitor"
        elif task_growth < 10:
            verdict = "[WATCH] POSSIBLE TASK LEAK — tasks not fully reclaimed"
        else:
            verdict = "[LEAK] TASK LEAK — tasks accumulate monotonically"

        print(f"\n{'='*74}")
        print("MEMORY-LEAK ANALYSIS")
        print(f"{'='*74}")
        print(f"  Baseline tasks.total: {base_tasks}")
        print(f"  Final tasks.total   : {final_tasks}  (growth {task_growth:+d})")
        print(f"  Drained task range  : {min_tasks_drained} .. {max_tasks_drained} "
              f"(slope {task_slope:+.2f} tasks/batch)")
        print(f"  Baseline RSS        : {base_rss:.1f} MB")
        print(f"  Final RSS           : {rss_series[-1]:.1f} MB")
        print(f"  Peak RSS            : {peak:.1f} MB  (d {peak_delta:+.1f} MB)")
        print(f"  RSS slope           : {slope:+.2f} MB/batch  (advisory under jemalloc)")
        print(f"  Calls completed     : {comp_total}")
        print(f"  Calls cancelled(487): {canc_total}  (cancel_prob={cancel_prob}%)")
        print(f"  Final active calls  : {tail.get('calls', 0)}")
        print(f"  Final dialogs       : {tail.get('dialogs', 0)}")
        print(f"  Final running_tx    : {tail.get('running_tx', 0)}")
        print(f"  VERDICT             : {verdict}")
        if loc_growth:
            print(f"  Task locations still above baseline (possible leak sites):")
            for loc, cnt, diff in loc_growth[:12]:
                print(f"      +{diff:>3}  (now {cnt:>3})  {loc}")
        else:
            print(f"  No task location above baseline — all per-call tasks reclaimed.")
        print(f"{'='*74}")

        # Mini trend bars
        print("\n  tasks.total trend:")
        tlo = min(tasks_series)
        thi = max(tasks_series)
        tspan = max(thi - tlo, 1)
        for idx, v in enumerate(tasks_series):
            label = "base" if idx == 0 else ("tail" if idx == tn - 1 else f"b{idx}")
            bar_len = int(round((v - tlo) / tspan * 30))
            print(f"    {label:>4} {v:>5} {'#' * bar_len}")

        print("\n  RSS trend (advisory under jemalloc):")
        lo = min(rss_series)
        hi = max(rss_series)
        span = max(hi - lo, 1.0)
        for idx, v in enumerate(rss_series):
            if idx == 0:
                label = "base"
            elif idx == len(rss_series) - 1:
                label = "tail"
            else:
                label = f"b{idx}"
            bar_len = int(round((v - lo) / span * 30))
            print(f"    {label:>4} {v:>7.1f} MB {'#' * bar_len}")

    def _save_memleak(
        self,
        rows: list[dict[str, Any]],
        base_rss: float,
        rss_series: list[float],
        cancel_prob: int,
        cps: int,
        duration: int,
        batch_size: int,
        total: int,
        tasks_series: list[int] | None = None,
        base_tasks: int = 0,
    ) -> None:
        ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
        detail = {
            "type": "memleak",
            "timestamp": ts,
            "total": total,
            "batch_size": batch_size,
            "cps": cps,
            "duration": duration,
            "cancel_prob": cancel_prob,
            "baseline_rss_mb": round(base_rss, 2),
            "final_rss_mb": round(rss_series[-1], 2),
            "growth_mb": round(rss_series[-1] - base_rss, 2),
            "rss_series": [round(v, 2) for v in rss_series],
            "baseline_tasks": base_tasks,
            "tasks_series": list(tasks_series or [base_tasks]),
            "batches": rows,
        }
        jf = os.path.join(self.log_dir, f"memleak_{ts}.json")
        with open(jf, "w") as f:
            json.dump(detail, f, indent=2)
        cf = os.path.join(self.log_dir, f"memleak_{ts}.csv")
        with open(cf, "w", newline="") as f:
            if rows:
                w = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
                w.writeheader()
                w.writerows(rows)
        print(f"\n[memleak] Saved: {jf}")
        print(f"[memleak] Saved: {cf}")

    # -----------------------------------------------------------------------
    # Result output
    # -----------------------------------------------------------------------

    def save_results(self, result: BenchmarkResult) -> None:
        self.results.append(result)

        json_file = os.path.join(self.log_dir, "results.jsonl")
        with open(json_file, "a") as f:
            f.write(json.dumps(result.to_dict(), default=str) + "\n")

        csv_file = os.path.join(self.log_dir, "results.csv")
        file_exists = os.path.exists(csv_file)
        with open(csv_file, "a", newline="") as f:
            writer = csv.DictWriter(f, fieldnames=result.to_dict().keys())
            if not file_exists:
                writer.writeheader()
            writer.writerow(result.to_dict())

    def print_summary(self, result: BenchmarkResult) -> None:
        print(f"\n{'='*70}")
        print(f"BENCHMARK RESULTS: {result.scenario}")
        print(f"{'='*70}")

        print(f"\n--- Configuration ---")
        print(f"Total Calls       : {result.total_calls}")
        print(f"CPS               : {result.cps}")
        print(f"Call Duration     : {result.duration}s")
        print(f"UAS Count         : {result.uas_count}")
        print(f"Media Proxy       : {result.mediaproxy}")
        print(f"SIP Flow          : {result.sipflow_enabled}")

        print(f"\n--- Call Statistics ---")
        print(f"Calls Completed   : {result.calls_completed}")
        print(f"Calls Failed      : {result.calls_failed}")
        print(f"Success Rate      : {result.success_rate:.2f}%")
        if result.status_counts:
            codes = ", ".join(f"{k}:{v}" for k, v in sorted(result.status_counts.items()))
            print(f"Status Codes      : {codes}")

        print(f"\n--- Media Quality ---")
        print(f"Avg Setup Latency : {result.avg_setup_latency_ms:.2f} ms")
        print(f"Avg RTT           : {result.avg_rtt_ms:.2f} ms")
        print(f"Avg Packet Loss   : {result.avg_loss_pct:.2f}%")
        print(f"Max Packet Loss   : {result.max_loss_pct:.2f}%")
        print(f"TX Packets        : {result.tx_packets}")
        print(f"RX Packets        : {result.rx_packets}")

        print(f"\n--- Resource Usage ---")
        print(f"CPU Average       : {result.cpu_avg:.1f}%")
        print(f"CPU Peak          : {result.cpu_peak:.1f}%")
        print(f"Memory Average    : {result.mem_avg_mb:.1f} MB")
        print(f"Memory Peak       : {result.mem_peak_mb:.1f} MB")
        print(f"Peak Concurrent   : {result.calls_peak}")
        print(f"Avg Concurrent    : {result.calls_avg:.1f}")
        print(f"Test Duration     : {result.test_duration_s:.1f}s")

        if result.leak_final_assessment:
            print(f"\n--- Memory Leak Analysis ---")
            print(f"Final Assessment  : {result.leak_final_assessment}")
            print(f"Final Slope       : {result.leak_final_slope_mb_per_min:.3f} MB/min")
            print(f"Total Growth      : {result.leak_base_delta_mb:+.1f} MB (vs first window)")

        if result.task_drift:
            td = result.task_drift
            print(f"\n--- Media Task Drift (after drain) ---")
            print(f"tasks.total drift     : {td.get('tasks_total')}")
            print(f"media alive tasks     : {td.get('media_alive_tasks')}")
            print(f"sip alive tasks       : {td.get('sip_alive_tasks')}")
            print(f"leak handles          : {td.get('leak_handles')}")
            print(f"leak dialogs          : {td.get('leak_dialogs')}")
            print(f"active calls at end   : {td.get('calls_end')}")

        if result.drain_details:
            dd = result.drain_details
            status = "PASS" if result.drain_passed else "FAIL"
            print(f"\n--- Drain / Leak Assertion ---")
            print(f"Result               : {status}")
            print(f"active calls at end  : {dd.get('calls_at_end')}")
            print(f"leak handles         : {dd.get('leak_handles')}")
            print(f"leak dialogs         : {dd.get('leak_dialogs')}")
            print(f"tasks.total          : {dd.get('tasks_total')}")
            print(f"media alive tasks    : {dd.get('media_alive_tasks')}")

        if result.audio_format:
            af = result.audio_format
            uac = af.get("uac", {}); uas = af.get("uas", {})
            print(f"\n--- Audio Format (PT + samplerate) ---")
            print(f"Result               : {'PASS' if af.get('pass') else 'FAIL'}")
            print(f"UAC leg              : expected={uac.get('expected')} "
                  f"negotiated={uac.get('actual')} PT={uac.get('pt')} sr={uac.get('samplerate')} "
                  f"rate_mismatch={uac.get('rate_mismatch')}")
            print(f"UAS leg              : expected={uas.get('expected')} "
                  f"negotiated={uas.get('actual')} PT={uas.get('pt')} sr={uas.get('samplerate')} "
                  f"rate_mismatch={uas.get('rate_mismatch')}")

        if result.media_continuity:
            mc = result.media_continuity
            uacj = mc.get("uac", {}); uasj = mc.get("uas", {})
            print(f"\n--- Media Continuity (seq/ts jumps = audio-glitch warnings) ---")
            print(f"Result               : {'PASS' if mc.get('pass') else 'FAIL'}")
            print(f"UAC leg (authorit.)  : seq_gap_events={uacj.get('seq_gap_events')} "
                  f"ts_jump_events={uacj.get('ts_jump_events')} {'OK' if uacj.get('ok') else 'GLITCH'}")
            print(f"UAS leg (info)       : seq_gap_events={uasj.get('seq_gap_events')} "
                  f"ts_jump_events={uasj.get('ts_jump_events')} {'OK' if uasj.get('ok') else 'artifact?'}")

        if result.errors:
            print(f"\n--- Errors ---")
            for error in result.errors:
                print(f"  - {error}")

        print(f"{'='*70}\n")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(
        description="RustPBX P2P Benchmark using sipbot",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Run all 4 scenarios (100 concurrent quick test)
  python bench.py --scenario all --total 100 --cps 20 --duration 15

  # Media bypass only (RTP direct, PBX signaling only)
  python bench.py --scenario bypass --total 500 --cps 100

  # Media forward + sipflow with flowdb
  python bench.py --scenario forward_sipflow --total 500 --cps 100

  # 5000 concurrent (final goal)
  python bench.py --scenario all --total 5000 --cps 200 --duration 120 --uas-count 10

  # 1-hour soak test: cps=100, ~1000 concurrent, leak check every 5 min,
  # sipflow remote + wholesale addon enabled
  python bench.py --scenario forward_sipflow --cps 100 --duration 10 \
      --wall-time 3600 --leak-check-interval 300 --wholesale --uas-count 5
        """,
    )

    parser.add_argument(
        "--scenario",
        choices=["bypass", "forward", "bypass_sipflow", "forward_sipflow",
                 "transcode", "transcode_g729", "transcode_opus",
                 "transcode_opus_rev", "transcode_sipflow", "matrix", "all",
                 "rtp_fastpath", "rtp_fastpath_rec", "rtp_fastpath_sipflow",
                 "rtp_fastpath_rec_sipflow", "rtp_transcode", "rtp_transcode_rec",
                 "rtp_transcode_sipflow", "rtp_transcode_rec_sipflow",
                 "webrtc_fastpath", "webrtc_fastpath_rec", "webrtc_fastpath_sipflow",
                 "webrtc_fastpath_rec_sipflow", "webrtc_transcode",
                 "webrtc_transcode_rec", "webrtc_transcode_sipflow",
                 "webrtc_transcode_rec_sipflow"],
        default="all",
        help="Benchmark scenario (default: all). "
             "matrix = full 16-combo media matrix (RTP|WebRTC × fastpath|transcode "
             "× recording × sipflow), each at --total/--cps/--duration. "
             "bypass=media_proxy:none, forward=media_proxy:all, "
             "transcode=UAC pcmu -> UAS pcma (PCMU↔PCMA), "
             "transcode_g729=UAC pcmu -> UAS g729, "
             "transcode_opus=UAC pcmu -> UAS opus (pcmu→opus), "
             "transcode_opus_rev=UAC opus -> UAS pcmu (opus→pcmu), "
             "transcode_sipflow=transcode + sipflow",
    )
    parser.add_argument(
        "--recording",
        action="store_true",
        help="Enable [recording] (WAV output) for the scenario. With sipflow on, "
             "force_file=true is set so both recording and sipflow coexist.",
    )
    parser.add_argument(
        "--webrtc",
        action="store_true",
        help="Use WebRTC (DTLS-SRTP) media: sipbot UAC/UAS get --webrtc and the "
             "config users are marked is_support_webrtc=true.",
    )
    parser.add_argument(
        "--audio-quality",
        action="store_true",
        help="Enable sipbot --audio-quality on UAC/UAS so the actual decoded "
             "audio sample-rate is verified (mismatch count must be 0).",
    )
    parser.add_argument(
        "--audio-verify",
        nargs="?", const="opus", default=None,
        metavar="CODEC",
        help="Definitive actual-audio samplerate check: play a 440 Hz tone from "
             "the UAC, record the echo, and assert the detected frequency is "
             "~440 Hz (a wrong sample-rate path shifts it). Optional CODEC = the "
             "callee codec (default opus). Runs standalone.",
    )
    parser.add_argument(
        "--idle-gap",
        nargs="?", const=2, default=0, type=int,
        metavar="SECONDS",
        help="Standalone continuity check: UAC calls the echo UAS which holds for "
             "SECONDS (re-INVITE hold) mid-call, then resumes. Asserts the caller's "
             "rx had seq_gap=0 and ts_jump=0 across the hold/silence window and the "
             "recorded audio has signal after resume (proves the PBX egress keeps "
             "the stream continuous when it has no media source). Default 2s.",
    )
    parser.add_argument(
        "--uas-codecs",
        default=None,
        help="UAS (callee) codec for --idle-gap (e.g. pcmu, opus).",
    )
    parser.add_argument(
        "--uac-codecs",
        default=None,
        help="UAC (caller) codec for --idle-gap (e.g. pcmu, opus).",
    )
    parser.add_argument(
        "--total",
        type=int,
        default=DEFAULT_TOTAL,
        help=f"Total number of calls (default: {DEFAULT_TOTAL})",
    )
    parser.add_argument(
        "--cps",
        type=int,
        default=DEFAULT_CPS,
        help=f"Calls per second (default: {DEFAULT_CPS})",
    )
    parser.add_argument(
        "--duration",
        type=int,
        default=DEFAULT_CALL_DURATION,
        help=f"Call duration in seconds (default: {DEFAULT_CALL_DURATION})",
    )
    parser.add_argument(
        "--uas-count",
        type=int,
        default=DEFAULT_UAS_COUNT,
        help=f"Number of UAS instances (default: {DEFAULT_UAS_COUNT})",
    )
    parser.add_argument(
        "--uas-base-port",
        type=int,
        default=DEFAULT_UAS_BASE_PORT,
        help=f"Base port for UAS instances (default: {DEFAULT_UAS_BASE_PORT})",
    )
    parser.add_argument(
        "--proxy-host",
        default=DEFAULT_PROXY_HOST,
        help=f"SIP proxy host (default: {DEFAULT_PROXY_HOST})",
    )
    parser.add_argument(
        "--proxy-port",
        type=int,
        default=DEFAULT_PROXY_PORT,
        help=f"SIP proxy port (default: {DEFAULT_PROXY_PORT})",
    )
    parser.add_argument(
        "--http-base",
        default=DEFAULT_HTTP_BASE,
        help=f"HTTP base URL (default: {DEFAULT_HTTP_BASE})",
    )
    parser.add_argument(
        "--log-dir",
        default="tests/bench/results",
        help="Directory for logs and results (default: tests/bench/results)",
    )
    parser.add_argument(
        "--rustpbx-bin",
        default=DEFAULT_RUSTPBX_BIN,
        help=f"Path to rustpbx binary (default: {DEFAULT_RUSTPBX_BIN})",
    )
    parser.add_argument(
        "--rustpbx-config",
        default=DEFAULT_RUSTPBX_CONFIG,
        help=f"Path to rustpbx config (default: {DEFAULT_RUSTPBX_CONFIG})",
    )
    parser.add_argument(
        "--cooldown",
        type=int,
        default=10,
        help="Cooldown between scenarios in seconds (default: 10)",
    )
    parser.add_argument(
        "--wholesale",
        action="store_true",
        help="Enable the wholesale addon (injects addons=[\"wholesale\"] under [proxy]). "
             "Requires rustpbx built with the 'wholesale' feature.",
    )
    parser.add_argument(
        "--wall-time",
        type=int,
        default=0,
        help="Total sustained load duration in seconds (soak mode). When set, "
             "--total is ignored and recomputed as cps * wall_time so load is "
             "sustained for the full duration. e.g. --cps 100 --wall-time 3600 "
             "= 1 hour. In soak mode only one scenario runs.",
    )
    parser.add_argument(
        "--leak-check-interval",
        type=int,
        default=300,
        help="Memory-leak analysis interval in seconds (default: 300 = 5 min). "
             "Active when --wall-time > 0. Reports slope (MB/min), R², and an "
             "assessment (STABLE/WATCH/LEAK SUSPECTED) and appends to leak_check.csv.",
    )
    parser.add_argument(
        "--cancel-prob",
        type=int,
        default=0,
        help="sipbot --cancel-prob (0-99): probability of INVITE then CANCEL "
             "(default 0). Applied in both batch and --memleak modes.",
    )
    parser.add_argument(
        "--memleak",
        action="store_true",
        help="Batched memory-leak test: run --total calls in batches of "
             "--batch-size, sampling /health + RSS after each batch to detect "
             "leaked memory. Use with --external-server to target a running PBX.",
    )
    parser.add_argument(
        "--external-server",
        action="store_true",
        help="Use an already-running rustpbx (skip starting/killing our own). "
             "Set --proxy-host/--proxy-port/--http-base to match it.",
    )
    parser.add_argument(
        "--batch-size",
        type=int,
        default=10,
        help="Calls per batch in --memleak mode (default 10).",
    )
    parser.add_argument(
        "--uas-ring-duration",
        type=float,
        default=0.0,
        help="UAS ring duration (seconds) before answering. >0 keeps calls in "
             "ringing so CANCEL lands before 200 (reliably 487). Default 0.",
    )
    parser.add_argument(
        "--conference-members",
        type=int,
        default=0,
        help="Conference benchmark mode: N-way MCU conference via RWI + sipbot "
             "echo UAs (exercises the mixer). When >0, runs the conference "
             "benchmark instead of the UAC scenario loop.",
    )
    parser.add_argument(
        "--conference-rounds",
        type=int,
        default=20,
        help="Number of conference rounds in conference mode (default 20).",
    )


    args = parser.parse_args()

    # Check sipbot
    try:
        r = subprocess.run(["sipbot", "--version"], capture_output=True, text=True, timeout=5)
        print(f"✓ sipbot available: {r.stdout.strip()}")
    except FileNotFoundError:
        print("❌ Error: sipbot not found. Install with: cargo install sipbot")
        return 1

    # Check rustpbx binary exists (not needed when targeting an external server)
    need_binary = not (args.memleak and args.external_server)
    if need_binary and not os.path.exists(args.rustpbx_bin):
        print(f"❌ Error: {args.rustpbx_bin} not found. Build with: cargo build --release")
        return 1

    benchmark = P2PBenchmark(
        proxy_host=args.proxy_host,
        proxy_port=args.proxy_port,
        http_base=args.http_base,
        rustpbx_bin=args.rustpbx_bin,
        rustpbx_config=args.rustpbx_config,
        log_dir=args.log_dir,
    )
    benchmark.cancel_prob = args.cancel_prob

    # ------------------------------------------------------------------
    # Memory-leak mode: batched calls with per-batch /health sampling.
    # Bypasses the normal scenario loop.
    # ------------------------------------------------------------------
    if args.memleak:
        try:
            return benchmark.run_memleak(
                total=args.total,
                batch_size=args.batch_size,
                cps=args.cps,
                duration=args.duration,
                cancel_prob=args.cancel_prob,
                uas_count=args.uas_count,
                uas_base_port=args.uas_base_port,
                external_server=args.external_server,
                uas_ring_duration=args.uas_ring_duration,
            )
        except KeyboardInterrupt:
            print("\n\n⚠ Memory-leak test interrupted by user")
            return 130
        finally:
            benchmark.cleanup()

    # ------------------------------------------------------------------
    # Conference (MCU mixer) benchmark mode.
    # ------------------------------------------------------------------
    if args.conference_members > 0:
        try:
            result = benchmark.run_conference_benchmark(
                members=args.conference_members,
                rounds=args.conference_rounds,
                hold_secs=args.duration,
                wall_time=args.wall_time,
                leak_check_interval=args.leak_check_interval,
                uas_base_port=args.uas_base_port,
            )
            benchmark.print_summary(result)
            benchmark.save_results(result)
        except KeyboardInterrupt:
            print("\n\n⚠ Conference benchmark interrupted by user")
            return 130
        except Exception as e:
            print(f"\n\n❌ Unexpected error: {e}")
            import traceback
            traceback.print_exc()
            return 1
        finally:
            benchmark.cleanup()
        return 0

    # ------------------------------------------------------------------
    # Actual-audio samplerate verification (440 Hz tone echo check).
    # ------------------------------------------------------------------
    if args.audio_verify:
        try:
            result = benchmark.run_audio_verify(
                uac_codecs="pcmu", uas_codecs=args.audio_verify,
                webrtc=args.webrtc,
            )
            ok = result.get("pass")
            print(f"\n{'='*70}")
            print(f"AUDIO VERIFY: {'PASS' if ok else 'FAIL'}")
            for k, v in result.items():
                print(f"  {k:<18}: {v}")
            print(f"{'='*70}")
            return 0 if ok else 1
        except KeyboardInterrupt:
            return 130
        finally:
            benchmark.cleanup()

    if args.idle_gap:
        try:
            result = benchmark.run_idle_gap(
                uac_codecs=args.uac_codecs or "pcmu",
                uas_codecs=args.uas_codecs or "pcmu",
                webrtc=args.webrtc,
                gap=args.idle_gap,
            )
            ok = result.get("pass")
            print(f"\n{'='*70}")
            print(f"IDLE-GAP (hold/silence window continuity): {'PASS' if ok else 'FAIL'}")
            for k, v in result.items():
                print(f"  {k:<22}: {v}")
            print(f"{'='*70}")
            return 0 if ok else 1
        except KeyboardInterrupt:
            return 130
        finally:
            benchmark.cleanup()

    # Define scenarios: (name, mediaproxy, sipflow, uas_codecs, uac_codecs, recording, webrtc)
    MATRIX_COMBOS = [
        ("rtp_fastpath",              "all", False, "pcmu", "pcmu", False, False),
        ("rtp_fastpath_rec",          "all", False, "pcmu", "pcmu", True,  False),
        ("rtp_fastpath_sipflow",      "all", True,  "pcmu", "pcmu", False, False),
        ("rtp_fastpath_rec_sipflow",  "all", True,  "pcmu", "pcmu", True,  False),
        ("rtp_transcode",             "all", False, "opus", "pcmu", False, False),
        ("rtp_transcode_rec",         "all", False, "opus", "pcmu", True,  False),
        ("rtp_transcode_sipflow",     "all", True,  "opus", "pcmu", False, False),
        ("rtp_transcode_rec_sipflow", "all", True,  "opus", "pcmu", True,  False),
        ("webrtc_fastpath",           "all", False, "pcmu", "pcmu", False, True),
        ("webrtc_fastpath_rec",       "all", False, "pcmu", "pcmu", True,  True),
        ("webrtc_fastpath_sipflow",   "all", True,  "pcmu", "pcmu", False, True),
        ("webrtc_fastpath_rec_sipflow", "all", True, "pcmu", "pcmu", True, True),
        ("webrtc_transcode",          "all", False, "opus", "pcmu", False, True),
        ("webrtc_transcode_rec",      "all", False, "opus", "pcmu", True,  True),
        ("webrtc_transcode_sipflow",  "all", True,  "opus", "pcmu", False, True),
        ("webrtc_transcode_rec_sipflow", "all", True, "opus", "pcmu", True, True),
    ]
    _combo_map = {c[0]: c for c in MATRIX_COMBOS}

    scenarios = []
    if args.scenario == "all":
        scenarios = [
            ("bypass",            "none", False, "pcmu", "pcmu", False, False),
            ("forward",           "all",  False, "pcmu", "pcmu", False, False),
            ("bypass_sipflow",    "none", True,  "pcmu", "pcmu", False, False),
            ("forward_sipflow",   "all",  True,  "pcmu", "pcmu", False, False),
        ]
    elif args.scenario == "bypass":
        scenarios = [("bypass", "none", False, "pcmu", "pcmu", False, False)]
    elif args.scenario == "forward":
        scenarios = [("forward", "all", False, "pcmu", "pcmu", False, False)]
    elif args.scenario == "bypass_sipflow":
        scenarios = [("bypass_sipflow", "none", True, "pcmu", "pcmu", False, False)]
    elif args.scenario == "forward_sipflow":
        scenarios = [("forward_sipflow", "all", True, "pcmu", "pcmu", False, False)]
    elif args.scenario == "transcode":
        scenarios = [("transcode", "all", False, "pcma", "pcmu", False, False)]
    elif args.scenario == "transcode_g729":
        scenarios = [("transcode_g729", "all", False, "g729", "pcmu", False, False)]
    elif args.scenario == "transcode_opus":
        scenarios = [("transcode_opus", "all", False, "opus", "pcmu", False, False)]
    elif args.scenario == "transcode_opus_rev":
        scenarios = [("transcode_opus_rev", "all", False, "pcmu", "opus", False, False)]
    elif args.scenario == "transcode_sipflow":
        scenarios = [("transcode_sipflow", "all", True, "pcma", "pcmu", False, False)]
    elif args.scenario == "matrix":
        # Full media performance/leak matrix: 16 combos.
        # RTP|WebRTC × fastpath(pcmu→pcmu)|transcode(pcmu→opus) × recording × sipflow.
        scenarios = list(MATRIX_COMBOS)
    elif args.scenario in _combo_map:
        scenarios = [_combo_map[args.scenario]]

    # In soak (wall-time) mode, running all 4 scenarios back-to-back would take
    # 4× the wall time. Force a single scenario, defaulting to forward_sipflow
    # (media proxy all + sipflow remote) which exercises the most code paths and
    # is the typical choice for leak detection.
    if args.wall_time > 0 and args.scenario == "all":
        print(f"[soak] --wall-time={args.wall_time}s set with --scenario all; "
              f"narrowing to forward_sipflow only (media_proxy=all + sipflow remote)")
        scenarios = [("forward_sipflow", "all", True, "pcmu", "pcmu", False, False)]

    leak_interval = args.leak_check_interval if args.wall_time > 0 else 0

    # Run scenarios
    all_results: list[BenchmarkResult] = []
    try:
        for idx, (name, mediaproxy, sipflow, uas_codecs, uac_codecs, recording, webrtc) in enumerate(scenarios):
            print(f"\n{'#'*70}")
            print(f"# SCENARIO {idx + 1}/{len(scenarios)}: {name}")
            print(f"# codecs: UAS={uas_codecs} UAC={uac_codecs} "
                  f"recording={recording} webrtc={webrtc} sipflow={sipflow}")
            print(f"{'#'*70}")

            result = benchmark.run_benchmark(
                scenario_name=name,
                total=args.total,
                cps=args.cps,
                duration=args.duration,
                mediaproxy=mediaproxy,
                sipflow=sipflow,
                uas_count=args.uas_count,
                uas_base_port=args.uas_base_port,
                wholesale=args.wholesale,
                wall_time=args.wall_time,
                leak_check_interval=leak_interval,
                uas_codecs=uas_codecs,
                uac_codecs=uac_codecs,
                recording=recording,
                webrtc=webrtc,
                audio_quality=args.audio_quality,
            )

            benchmark.print_summary(result)
            benchmark.save_results(result)
            all_results.append(result)

            if idx < len(scenarios) - 1:
                print(f"\n[cooldown] Waiting {args.cooldown}s before next scenario...")
                time.sleep(args.cooldown)

    except KeyboardInterrupt:
        print("\n\n⚠ Benchmark interrupted by user")
        return 130
    except Exception as e:
        print(f"\n\n❌ Unexpected error: {e}")
        import traceback
        traceback.print_exc()
        return 1
    finally:
        benchmark.cleanup()
        benchmark.kill_mysql_proxy()

    # Print comparison table
    if len(all_results) > 1:
        print(f"\n{'='*80}")
        print("SCENARIO COMPARISON")
        print(f"{'='*80}")
        print(
            f"{'Scenario':<25} {'Success%':>9} {'ConcPeak':>9} "
            f"{'Setup ms':>9} {'RTT ms':>8} {'Loss%':>7} "
            f"{'CPU Peak':>9} {'Mem Peak':>9} {'TX Pkts':>9}"
        )
        print("-" * 80)
        for r in all_results:
            print(
                f"{r.scenario:<25} "
                f"{r.success_rate:>8.1f}% "
                f"{r.calls_peak:>9} "
                f"{r.avg_setup_latency_ms:>8.2f} "
                f"{r.avg_rtt_ms:>7.2f} "
                f"{r.avg_loss_pct:>6.2f}% "
                f"{r.cpu_peak:>8.1f}% "
                f"{r.mem_peak_mb:>8.1f}M "
                f"{r.tx_packets:>9}"
            )
        print(f"{'='*80}\n")

    # Per-channel overhead
    for r in all_results:
        if r.calls_peak > 0:
            cpu_per_ch = r.cpu_peak / r.calls_peak
            mem_per_ch = r.mem_peak_mb / r.calls_peak
            print(f"[{r.scenario}] Per-channel: CPU={cpu_per_ch:.3f}%, Mem={mem_per_ch:.3f} MB")

    # Final summary
    passed = sum(1 for r in all_results if not r.errors)
    print(f"\n{'='*70}")
    print(f"FINAL SUMMARY: {passed}/{len(all_results)} scenarios passed")
    print(f"{'='*70}")
    print(f"Results saved to: {args.log_dir}")
    print(f"  - JSON: {os.path.join(args.log_dir, 'results.jsonl')}")
    print(f"  - CSV:  {os.path.join(args.log_dir, 'results.csv')}")
    print(f"{'='*70}\n")

    return 0 if passed == len(all_results) else 1


if __name__ == "__main__":
    sys.exit(main())
