#!/usr/bin/env python3
"""RustPBX benchmark + regression report generator.

Consumes the artifacts written by `scripts/run_bench_report.sh` and renders a
single markdown report:

  - regression.json   : p2p / wholesale regression summaries
  - results.jsonl     : one line per performance scenario (bench.py output)
  - memleak_*.json    : batched memory-leak analysis (bench.py --memleak)

Usage:
  python tests/bench/report_gen.py <results-dir> [--out report.md]
"""

from __future__ import annotations

import argparse
import glob
import json
import os
from pathlib import Path


def _load_regression(results_dir: Path) -> dict:
    p = results_dir / "regression.json"
    if p.exists():
        return json.loads(p.read_text())
    return {}


def _load_perf(results_dir: Path) -> list[dict]:
    rows = []
    for jf in sorted(glob.glob(str(results_dir / "perf" / "*" / "results.jsonl"))):
        try:
            for line in Path(jf).read_text().splitlines():
                line = line.strip()
                if line:
                    rows.append(json.loads(line))
        except (json.JSONDecodeError, FileNotFoundError):
            pass
    return rows


def _load_memleak(results_dir: Path) -> dict | None:
    files = sorted(glob.glob(str(results_dir / "memleak" / "memleak_*.json")))
    if not files:
        return None
    return json.loads(Path(files[-1]).read_text())


def _verdict_slug(verdict: str) -> str:
    if "NO LEAK" in verdict:
        return "OK"
    if "STABLE" in verdict:
        return "STABLE"
    if "WATCH" in verdict:
        return "WATCH"
    return "LEAK"


def _bool(v) -> str:
    return "✅ PASS" if v else "❌ FAIL"


def _fmt(v, suffix="") -> str:
    if v is None:
        return "-"
    try:
        return f"{v:.2f}{suffix}"
    except (TypeError, ValueError):
        return str(v)


def _regression_block(reg: dict) -> str:
    out = ["## 1. Functional Regression", ""]
    p2p = reg.get("p2p", {})
    ws_e2e = reg.get("wholesale_e2e", {})
    ws_rust = reg.get("wholesale_rust", {})

    out.append("### p2p (unified e2e, marker `p2p`)")
    out.append("")
    if p2p:
        out.append(
            f"| Item | Result |\n"
            f"|------|--------|\n"
            f"| Passed / Total | **{p2p.get('passed', '-')} / {p2p.get('total', '-')}** |\n"
            f"| Failed / Skipped | {p2p.get('failed', '-')} / {p2p.get('skipped', '-')} |\n"
            f"| Duration | {p2p.get('duration_s', '-')} s |\n"
            f"| Verdict | {_verdict_slug(p2p.get('verdict', 'PASS')) if p2p.get('verdict') else _bool(p2p.get('ok', False))} |\n"
        )
    else:
        out.append("_not run / no data_")
    out.append("")

    out.append("### wholesale")
    out.append("")
    out.append("**e2e (marker `wholesale`):**")
    out.append("")
    if ws_e2e:
        out.append(
            f"| Item | Result |\n"
            f"|------|--------|\n"
            f"| Passed / Total | **{ws_e2e.get('passed', '-')} / {ws_e2e.get('total', '-')}** |\n"
            f"| Failed / Skipped | {ws_e2e.get('failed', '-')} / {ws_e2e.get('skipped', '-')} |\n"
            f"| Duration | {ws_e2e.get('duration_s', '-')} s |\n"
            f"| Verdict | {_verdict_slug(ws_e2e.get('verdict', 'PASS')) if ws_e2e.get('verdict') else _bool(ws_e2e.get('ok', False))} |\n"
        )
    else:
        out.append("_not run / no data_")
    out.append("")

    out.append("**Rust integration (`cargo test --test wholesale`):**")
    out.append("")
    if ws_rust:
        out.append(
            f"| Item | Result |\n"
            f"|------|--------|\n"
            f"| Passed / Total | **{ws_rust.get('passed', '-')} / {ws_rust.get('total', '-')}** |\n"
            f"| Failed / Ignored | {ws_rust.get('failed', '-')} / {ws_rust.get('ignored', '-')} |\n"
            f"| Duration | {ws_rust.get('duration_s', '-')} s |\n"
        )
    else:
        out.append("_not run / no data_")
    out.append("")

    return "\n".join(out)


def _perf_block(rows: list[dict]) -> str:
    out = ["## 2. Performance", ""]
    if not rows:
        out.append("_not run / no data_")
        return "\n".join(out)

    out.append("| Scenario | Calls | Success | PeakConc | SetupLat | RTT | Loss | TX Pkts | CPU avg/peak | Mem avg/peak(MB) | MediaCont |")
    out.append("|----------|------:|--------:|---------:|---------:|----:|-----:|--------:|-------------:|-----------------:|:---------:|")
    for r in rows:
        out.append(
            f"| {r.get('scenario', '-')} | {r.get('calls_completed', '-')}/{r.get('total_calls', '-')} "
            f"| {_fmt(r.get('success_rate'))}% "
            f"| {r.get('calls_peak', '-')} "
            f"| {_fmt(r.get('avg_setup_latency_ms'))} ms "
            f"| {_fmt(r.get('avg_rtt_ms'))} ms "
            f"| {_fmt(r.get('avg_loss_pct'))}% "
            f"| {r.get('tx_packets', '-')} "
            f"| {_fmt(r.get('cpu_avg'))}/{_fmt(r.get('cpu_peak'))} "
            f"| {_fmt(r.get('mem_avg_mb'))}/{_fmt(r.get('mem_peak_mb'))} "
            f"| {_bool(r.get('media_continuity', {}).get('pass')) if isinstance(r.get('media_continuity'), dict) else '-'} |"
        )
    out.append("")
    return "\n".join(out)


def _memleak_block(m: dict | None) -> str:
    out = ["## 3. Memory Leak", ""]
    if not m:
        out.append("_not run / no data_")
        return "\n".join(out)

    a = m.get("analysis", {})
    verdict = a.get("verdict", "N/A")
    rows = m.get("batches", [])
    drained_ok = all(r.get("drained_ok") for r in rows) if rows else True

    out.append(
        f"| Item | Value |\n"
        f"|------|-------|\n"
        f"| Calls / batch | {m.get('total', '-')} / batch={m.get('batch_size', '-')} |\n"
        f"| Baseline RSS | {_fmt(m.get('baseline_rss_mb'))} MB |\n"
        f"| Final RSS | {_fmt(m.get('final_rss_mb'))} MB (growth {_fmt(m.get('growth_mb'))} MB) |\n"
        f"| Peak RSS | {_fmt(a.get('peak_rss_mb'))} MB (Δ {_fmt(a.get('peak_delta_mb'))} MB) |\n"
        f"| RSS slope | {_fmt(a.get('rss_slope_mb_per_batch'))} MB/batch |\n"
        f"| tasks baseline → final | {a.get('baseline_tasks', '-')} → {a.get('final_tasks', '-')} (Δ {a.get('task_growth', '-')}) |\n"
        f"| tasks slope | {_fmt(a.get('task_slope'))} tasks/batch |\n"
        f"| Completed / 487 | {a.get('calls_completed', '-')} / {a.get('calls_cancelled_487', '-')} |\n"
        f"| Per-batch drain | {'all reclaimed' if drained_ok else 'timeouts present'} |\n"
        f"| **Verdict** | **{verdict}** |\n"
    )
    out.append("")

    sites = a.get("leak_sites", [])
    if sites:
        out.append("**Task locations still above baseline (possible leak sites):**\n")
        out.append("| Location | Now | Diff |\n|----------|----:|-----:|")
        for s in sites:
            out.append(f"| {s.get('location', '-')} | {s.get('now', '-')} | +{s.get('diff', 0)} |")
        out.append("")
    return "\n".join(out)


def _env_block() -> str:
    import datetime

    return (
        "## 0. Environment\n"
        "\n"
        f"| Item | Value |\n"
        f"|------|-------|\n"
        f"| Generated at | {datetime.datetime.now().strftime('%Y-%m-%d %H:%M:%S')} |\n"
        f"| Git commit | {os.environ.get('BENCH_GIT_COMMIT', '-')} |\n"
        f"| Branch | {os.environ.get('BENCH_GIT_BRANCH', '-')} |\n"
        f"| Host | {os.environ.get('BENCH_HOST', os.uname().nodename)} |\n"
        f"| CPU | {os.environ.get('BENCH_CPU', os.cpu_count() or '-')} cores |\n"
        f"| sipbot | {os.environ.get('BENCH_SIPBOT', '-')} |\n"
        f"| Binary | {os.environ.get('BENCH_RUSTPBX_BIN', '-')} |\n"
        "\n"
    )


def generate(results_dir: Path) -> str:
    reg = _load_regression(results_dir)
    perf = _load_perf(results_dir)
    memleak = _load_memleak(results_dir)

    sections = [
        "# RustPBX Test Report (Benchmark & Regression)",
        "",
        _env_block(),
        _regression_block(reg),
        _perf_block(perf),
        _memleak_block(memleak),
        "---",
        f"_Auto-generated by `tests/bench/report_gen.py` · data dir: `{results_dir}`_",
        "",
    ]
    return "\n".join(sections)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("results_dir", type=Path, help="results directory from run_bench_report.sh")
    ap.add_argument("--out", default="report.md", help="output markdown file")
    args = ap.parse_args()

    report = generate(args.results_dir)
    Path(args.out).write_text(report)
    print(report)
    print(f"\n[report] written to {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
