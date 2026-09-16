"""CLI: aggregate a run directory into the final self-contained HTML report.

Usage: python3 -m helpers.report_cli --run-dir <artifacts/<run_id>>

Scans <run_dir>/lane-*.junit.xml, attaches evidence manifests, renders
<run_dir>/report/index.html. Works with zero lanes (fresh run) as well.
"""

from __future__ import annotations

import argparse
from pathlib import Path

from .report_html import ReportBuilder

KNOWN_ISSUES = [
    "call_forward when_busy / no_answer modes not implemented in SipSession (strict-xfail pinned)",
    "trunk sequential failover semantics: dest list is load-balanced, health auto-failover not consumed by routing (strict-xfail)",
    "MCU dial-in leg UA->mixer reverse path silent (strict-xfail, product bug recorded)",
    "RWI record.start/stop e2e xfail (media layer WIP under WebRTC+sipflow coexistence)",
    "SDES-SRTP sipbot 0.2.56 self-interop broken (xfail)",
]


def main() -> int:
    parser = argparse.ArgumentParser(description="Build the unified regression HTML report")
    parser.add_argument("--run-dir", required=True, type=Path)
    parser.add_argument("--title", default="RustPBX Unified Regression Report")
    args = parser.parse_args()

    run_dir: Path = args.run_dir
    rb = ReportBuilder(run_dir, title=args.title)
    lanes = sorted(run_dir.glob("lane-*.junit.xml"))
    for junit in lanes:
        lane = junit.name[len("lane-"):-len(".junit.xml")]
        rb.add_junit(junit, lane)
    if not lanes:
        rb.lane_meta["(no lanes executed)"] = {"status": "empty run", "total": 0, "failed": 0, "duration": 0}
    attached = rb.load_evidence()
    rb.set_known_issues(KNOWN_ISSUES)
    mapping_path = Path(__file__).resolve().parent.parent / "reports" / "acceptance_map.json"
    if mapping_path.exists():
        import json

        try:
            rb.set_acceptance_map(json.loads(mapping_path.read_text(encoding="utf-8")))
        except json.JSONDecodeError:
            pass
    out = rb.render()
    print(f"[report] lanes={len(lanes)} evidence_attached={attached} -> {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
