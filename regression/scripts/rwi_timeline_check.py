#!/usr/bin/env python3
"""Offline RWI event-timeline checker for rustpbx production/troubleshooting logs.

Parses `RWI webhook delivered ... body={...}` lines out of one or more rustpbx
log files, rebuilds each call's event timeline, and runs the SAME contract the
live regression suites assert (regression/helpers/assertions/event_schema.py):
per-event attribution, cross-event ordering, uniqueness, and stability.

Built after the recurring "call_ringing without agent_id" incidents: point it
at any log drop and get a verdict in seconds instead of hand-diffing JSON.

Usage:
    python3 regression/scripts/rwi_timeline_check.py 20260929_155908_42.log
    python3 regression/scripts/rwi_timeline_check.py *.log --call-id 32e80795
    python3 regression/scripts/rwi_timeline_check.py *.log --json

Exit code: 0 when no contract violations, 1 otherwise (CI-friendly).
"""

from __future__ import annotations

import argparse
import glob
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from helpers.assertions.event_schema import check_call_timeline  # noqa: E402

# rustpbx log line: "<ts>  INFO rustpbx::rwi::webhook: <loc>: RWI webhook
# delivered url=... event_type="call_ringing" call_id="..." status_code=200
# latency_ms=7 body={...}"
LINE_RE = re.compile(
    r"^(?P<file_ts>\S+)\s+\S+\s+rustpbx::rwi::webhook:.*?"
    r"RWI webhook delivered\s+url=(?P<url>\S+)\s+"
    r'event_type="(?P<event_type>[^"]+)"\s+'
    r'call_id="(?P<call_id>[^"]+)".*?'
    r"body=(?P<body>\{.*)$"
)

# Also accept raw JSONL captures (one envelope per line) for replay files.
def parse_log_file(path: Path) -> list[dict]:
    events: list[dict] = []
    for lineno, raw in enumerate(path.read_text(errors="replace").splitlines(), 1):
        if "RWI webhook delivered" in raw:
            m = LINE_RE.match(raw)
            if not m:
                continue
            try:
                body = json.loads(m.group("body"))
            except json.JSONDecodeError:
                continue
        elif raw.lstrip().startswith("{") and '"event_type"' in raw:
            # JSONL capture line.
            try:
                body = json.loads(raw)
            except json.JSONDecodeError:
                continue
            lineno = lineno
        else:
            continue
        payload = body.get("event") if isinstance(body.get("event"), dict) else {}
        events.append(
            {
                "file": path.name,
                "line": lineno,
                "log_ts": m.group("file_ts") if "RWI webhook delivered" in raw else "",
                "timestamp": body.get("timestamp", ""),
                "event_type": body.get("event_type") or payload.get("event_type", ""),
                "call_id": body.get("call_id") or payload.get("call_id", ""),
                "payload": payload,
            }
        )
    return events


def short(payload: dict, keys: tuple[str, ...]) -> str:
    parts = []
    for k in keys:
        v = payload.get(k)
        if v is not None:
            shown = str(v)
            if len(shown) > 36:
                shown = shown[:33] + "..."
            parts.append(f"{k}={shown}")
    return " ".join(parts) if parts else "-"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("logs", nargs="+", help="rustpbx log file(s) / JSONL captures (globs ok)")
    ap.add_argument("--call-id", help="only report this call_id")
    ap.add_argument("--agent-id", help="expected agent attribution (default: inferred)")
    ap.add_argument("--json", action="store_true", help="machine-readable findings")
    args = ap.parse_args()

    files: list[Path] = []
    for pattern in args.logs:
        matched = [Path(p) for p in glob.glob(pattern)]
        files.extend(matched or [Path(pattern)])
    files = [f for f in files if f.is_file()]
    if not files:
        print("no input files matched", file=sys.stderr)
        return 2

    events: list[dict] = []
    for f in sorted(files):
        events.extend(parse_log_file(f))
    if args.call_id:
        events = [e for e in events if args.call_id in (e["call_id"] or "")]

    by_call: dict[str, list[dict]] = {}
    for e in events:
        by_call.setdefault(e["call_id"] or "<no-call-id>", []).append(e)

    total_violations = 0
    report_calls = []
    for call_id, evs in by_call.items():
        violations, warnings = check_call_timeline(
            evs, call_id=call_id, agent_id=args.agent_id
        )
        total_violations += len(violations)
        report_calls.append(
            {
                "call_id": call_id,
                "events": len(evs),
                "violations": violations,
                "warnings": warnings,
                "timeline": [
                    {
                        "ts": (e["timestamp"] or e["log_ts"])[11:26],
                        "type": e["event_type"],
                        "fields": short(
                            e["payload"],
                            ("leg_id", "agent_id", "agent_name", "queue_id"),
                        ),
                    }
                    for e in evs
                ],
            }
        )

    if args.json:
        print(
            json.dumps(
                {"calls": report_calls, "total_violations": total_violations},
                indent=2,
                ensure_ascii=False,
            )
        )
    else:
        for rc in report_calls:
            print(f"══ call {rc['call_id']}  ({rc['events']} events) " + "═" * 30)
            for t in rc["timeline"]:
                print(f"  {t['ts']}  {t['type']:32s} {t['fields']}")
            for v in rc["violations"]:
                print(f"  ✗ VIOLATION: {v}")
            for w in rc["warnings"]:
                print(f"  ⚠ warning:   {w}")
            if not rc["violations"] and not rc["warnings"]:
                print("  ✓ contract clean")
            print()
        verdict = (
            f"✗ {total_violations} contract violation(s) across {len(report_calls)} call(s)"
            if total_violations
            else f"✓ all {len(report_calls)} call(s) clean"
        )
        print(verdict)

    return 1 if total_violations else 0


if __name__ == "__main__":
    sys.exit(main())
