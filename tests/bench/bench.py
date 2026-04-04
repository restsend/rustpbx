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
DEFAULT_RUSTPBX_CONFIG = "config.toml.dev"
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
    ):
        self.process_name = process_name
        self.interval = interval
        self.health_url = health_url
        self.samples: list[dict[str, float]] = []
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def _run(self) -> None:
        while not self._stop.is_set():
            sample = self._sample()
            if sample is not None:
                self.samples.append(sample)
            self._stop.wait(self.interval)

    def _sample(self) -> dict[str, float] | None:
        try:
            result = subprocess.run(
                ["ps", "-C", self.process_name, "-o", "pid,pcpu,rss", "--no-headers"],
                capture_output=True,
                text=True,
                timeout=5,
            )
            if result.returncode != 0 or not result.stdout.strip():
                return None
            total_cpu = 0.0
            total_mem_kb = 0.0
            for line in result.stdout.strip().split("\n"):
                parts = line.split()
                if len(parts) >= 3:
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
            except Exception:
                pass

        return sample

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=5)

    def summary(self) -> dict[str, Any]:
        if not self.samples:
            return {
                "cpu_avg": 0.0, "cpu_peak": 0.0,
                "mem_avg_mb": 0.0, "mem_peak_mb": 0.0,
                "samples": 0, "calls_peak": 0, "calls_avg": 0.0,
            }
        cpus = [s["cpu_pct"] for s in self.samples]
        mems = [s["mem_mb"] for s in self.samples]
        result: dict[str, Any] = {
            "cpu_avg": sum(cpus) / len(cpus),
            "cpu_peak": max(cpus),
            "mem_avg_mb": sum(mems) / len(mems),
            "mem_peak_mb": max(mems),
            "samples": len(self.samples),
        }
        calls_list = [s["calls"] for s in self.samples if "calls" in s]
        if calls_list:
            result["calls_peak"] = int(max(calls_list))
            result["calls_avg"] = sum(calls_list) / len(calls_list)
        else:
            result["calls_peak"] = 0
            result["calls_avg"] = 0.0
        return result


# ---------------------------------------------------------------------------
# SipProcess — manages a single sipbot process
# ---------------------------------------------------------------------------

class SipProcess:
    """Manages a sipbot process (UAS or UAC)."""

    def __init__(self, name: str, log_file: str | None = None):
        self.name = name
        self.process: subprocess.Popen[str] | None = None
        self.lines: list[str] = []
        self._lock = threading.Lock()
        self._reader: threading.Thread | None = None
        self._log_file = log_file

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
                if self._log_file:
                    with open(self._log_file, "a") as f:
                        f.write(line + "\n")

    def output(self) -> str:
        with self._lock:
            return "\n".join(self.lines)

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

        os.makedirs(log_dir, exist_ok=True)

    # -----------------------------------------------------------------------
    # Server management
    # -----------------------------------------------------------------------

    def start_rustpbx(self, mediaproxy: str = "all", sipflow: bool = False) -> bool:
        """Start rustpbx with specified configuration."""
        print(f"\n{'='*60}")
        print(f"Starting rustpbx (mediaproxy={mediaproxy}, sipflow={sipflow})")
        print(f"{'='*60}")

        self._kill_rustpbx()

        config_path = self._create_config(mediaproxy, sipflow)
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

    def _create_config(self, mediaproxy: str, sipflow: bool) -> str | None:
        """Create a temporary config file with specified settings."""
        try:
            with open(self.rustpbx_config, "r") as f:
                config_content = f.read()

            # Modify mediaproxy
            config_content = re.sub(
                r'media_proxy\s*=\s*"[^"]*"',
                f'media_proxy = "{mediaproxy}"',
                config_content,
            )

            # Disable recording — recording.enabled=true forces media proxy on
            # regardless of media_proxy setting, which would invalidate the
            # mediaproxy=none scenario.
            config_content = re.sub(
                r'(\[recording\][^\[]*enabled\s*=\s*)true',
                lambda m: m.group(1) + "false",
                config_content,
                flags=re.DOTALL,
            )

            # Modify sipflow
            if sipflow:
                if "[sipflow]" not in config_content:
                    config_content += '\n[sipflow]\ntype = "local"\nroot = "./config/sipflow"\n'
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
                self.log_dir, f"config_{mediaproxy}_{int(sipflow)}.toml"
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
        try:
            subprocess.run(["pkill", "-TERM", "-f", "rustpbx"], capture_output=True)
            time.sleep(1)
            subprocess.run(["pkill", "-KILL", "-f", "rustpbx"], capture_output=True)
            time.sleep(0.5)
        except Exception:
            pass

    # -----------------------------------------------------------------------
    # UAS management (sipbot wait with registration)
    # -----------------------------------------------------------------------

    def start_uas_instances(
        self, count: int, base_port: int = DEFAULT_UAS_BASE_PORT, hangup: int = 120
    ) -> bool:
        """Start UAS instances registered as extension users.

        Each UAS registers as bob/alice (cycling through users).
        sipbot handles multiple concurrent calls per instance.
        """
        print(f"\n{'='*60}")
        print(f"Starting {count} UAS instances (sipbot wait + register)")
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
                "--codecs", "pcmu",
                "--hangup", str(hangup),
                "--echo",  # echo mode for realistic bidirectional RTP
                "-v",
            ]

            uas = SipProcess(f"uas-{i+1}", log_file=log_file)
            uas.start(cmd)
            self.uas_list.append(uas)
            print(f"[UAS] #{i+1} started: user={username}, port={port}, log={log_file}")

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
    ) -> tuple[str, float]:
        """Run batch UAC calls via sipbot call --total --cps.

        Calls are placed to extension users (bob/alice) through the PBX.
        Returns (output_text, wall_time_seconds).
        """
        print(f"\n{'='*60}")
        print(f"Starting UAC batch: {total} calls @ {cps} CPS, duration={duration}s")
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
            "--codecs", "pcmu",
            "--hangup", str(duration),
            "--total", str(total),
            "--cps", str(cps),
            "-v",
        ]

        self.uac_process = SipProcess("uac-batch", log_file=log_file)
        self.uac_process.start(cmd)
        print(f"[UAC] Batch started (log: {log_file})")

        # Wait for completion with generous timeout
        timeout = max(120, total // max(cps, 1) + duration + 60)
        t_start = time.time()
        self.uac_process.wait(timeout=timeout)
        wall_time = time.time() - t_start

        output = self.uac_process.output()
        return output, wall_time

    # -----------------------------------------------------------------------
    # Monitoring
    # -----------------------------------------------------------------------

    def start_monitoring(self, interval: float = 1.0) -> None:
        health_url = f"{self.http_base}/ami/v1/health"
        self.monitor = ResourceMonitor(
            process_name="rustpbx",
            interval=interval,
            health_url=health_url,
        )
        self.monitor.start()
        print(f"[monitor] Started (interval={interval}s)")

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
    ) -> BenchmarkResult:
        """Run a single benchmark scenario."""
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

        print(f"\n{'='*70}")
        print(f"BENCHMARK: {scenario_name}")
        print(f"{'='*70}")
        print(f"Configuration:")
        print(f"  Total Calls     : {total}")
        print(f"  CPS             : {cps}")
        print(f"  Call Duration   : {duration}s")
        print(f"  UAS Count       : {uas_count}")
        print(f"  Media Proxy     : {mediaproxy}")
        print(f"  SIP Flow        : {sipflow}")
        print(f"  Est. Concurrent : {min(cps * duration, total)}")
        print(f"{'='*70}\n")

        try:
            # 1. Start rustpbx
            if not self.start_rustpbx(mediaproxy=mediaproxy, sipflow=sipflow):
                result.errors.append("Failed to start rustpbx")
                return result

            time.sleep(2)

            # 2. Start UAS instances (hangup > call_duration so UAS doesn't hang up early)
            if not self.start_uas_instances(uas_count, base_port=uas_base_port, hangup=duration + 30):
                result.errors.append("Failed to start UAS instances")
                return result

            # 3. Start monitoring
            self.start_monitoring(interval=1.0)

            # 4. Run UAC batch
            uac_output, wall_time = self.run_uac_batch(total, cps, duration)
            result.test_duration_s = wall_time

            # 5. Allow stats to settle
            time.sleep(2)

            # 6. Stop monitoring
            resource_summary = self.stop_monitoring()

            # 7. Collect results
            self._collect_results(result, uac_output, resource_summary)

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
  # Run 500-concurrent benchmark (all scenarios)
  python bench.py --scenario all

  # Single scenario with defaults (500 concurrent)
  python bench.py --scenario mediaproxy_all

  # Custom load profile
  python bench.py --scenario all --total 800 --cps 200 --duration 60 --uas-count 4

  # Quick test with low load
  python bench.py --scenario mediaproxy_none --total 50 --cps 10 --duration 10
        """,
    )

    parser.add_argument(
        "--scenario",
        choices=["mediaproxy_none", "mediaproxy_all", "sipflow", "all"],
        default="all",
        help="Benchmark scenario (default: all)",
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

    args = parser.parse_args()

    # Check sipbot
    try:
        r = subprocess.run(["sipbot", "--version"], capture_output=True, text=True, timeout=5)
        print(f"✓ sipbot available: {r.stdout.strip()}")
    except FileNotFoundError:
        print("❌ Error: sipbot not found. Install with: cargo install sipbot")
        return 1

    # Check rustpbx binary exists
    if not os.path.exists(args.rustpbx_bin):
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

    # Define scenarios
    scenarios = []
    if args.scenario == "all":
        scenarios = [
            ("mediaproxy_none", "none", False),
            ("mediaproxy_all", "all", False),
            ("mediaproxy_all_sipflow", "all", True),
        ]
    elif args.scenario == "mediaproxy_none":
        scenarios = [("mediaproxy_none", "none", False)]
    elif args.scenario == "mediaproxy_all":
        scenarios = [("mediaproxy_all", "all", False)]
    elif args.scenario == "sipflow":
        scenarios = [("mediaproxy_all_sipflow", "all", True)]

    # Run scenarios
    all_results: list[BenchmarkResult] = []
    try:
        for idx, (name, mediaproxy, sipflow) in enumerate(scenarios):
            print(f"\n{'#'*70}")
            print(f"# SCENARIO {idx + 1}/{len(scenarios)}: {name}")
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
