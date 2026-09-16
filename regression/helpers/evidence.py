"""Per-test evidence store: screenshots, recordings, CDR diffs, event dumps.

Layout (xdist-aware, never pollutes the repo checkout):

    <artifacts>/<run_id>/lane-<lane>/tests/<worker>-<testid>/
        evidence.json     manifest with all metrics/paths
        *.png / *.wav / *.json   captured artifacts

The store is injected via the `evidence` fixture (see regression/conftest.py).
The final report generator (helpers.report_html) consumes the manifests.
"""

from __future__ import annotations

import json
import os
import re
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional


def _sanitize(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9._-]+", "_", name)[-120:] or "test"


@dataclass
class EvidenceStore:
    test_id: str
    worker: str
    lane: str
    run_id: str
    dir: Path
    started_at: float = field(default_factory=time.time)
    entries: list = field(default_factory=list)
    metrics: dict = field(default_factory=dict)
    assertion_levels: set = field(default_factory=set)

    # -- creation -----------------------------------------------------------

    @classmethod
    def create(cls, test_id: str, artifacts_root: Path, *, worker: str = "gw0", lane: str = "lane0", run_id: str = "") -> "EvidenceStore":
        run_id = run_id or os.environ.get("RUSTPBX_RUN_ID", time.strftime("%Y%m%d_%H%M%S"))
        lane = lane[5:] if lane.startswith("lane-") else lane  # avoid lane-lane-x
        d = Path(artifacts_root) / run_id / f"lane-{_sanitize(lane)}" / "tests" / f"{_sanitize(worker)}-{_sanitize(test_id)}"
        d.mkdir(parents=True, exist_ok=True)
        store = cls(test_id=test_id, worker=worker, lane=lane, run_id=run_id, dir=d)
        store.flush()
        return store

    # -- capture API --------------------------------------------------------

    def add_screenshot(self, name: str, png_bytes: bytes, *, note: str = "") -> Optional[Path]:
        """Store a PNG screenshot; returns path (also linked from report)."""
        if not png_bytes:
            self.log_metric("screenshot_empty", name, note="zero-byte screenshot ignored (guard)")
            return None
        p = self.dir / f"{_sanitize(name)}.png"
        p.write_bytes(png_bytes)
        self.entries.append({"kind": "screenshot", "file": p.name, "note": note, "ts": time.time()})
        self.mark_level("AUDIO" if False else "EVIDENCE")
        return p

    def add_screenshot_file(self, path, *, note: str = "") -> Optional[Path]:
        from pathlib import Path as _P

        src = _P(path)
        if not src.exists() or src.stat().st_size == 0:
            return None
        return self.add_screenshot(src.stem, src.read_bytes(), note=note)

    def add_recording(self, path, *, note: str = "", rename: Optional[str] = None) -> Optional[Path]:
        """Copy a WAV/PCMA recording into the evidence dir (survives tmp rotation)."""
        from pathlib import Path as _P

        src = _P(path)
        if not src.exists() or src.stat().st_size == 0:
            self.log_metric("recording_missing", str(src), note=note)
            return None
        dst = self.dir / _sanitize(rename or src.name)
        dst.write_bytes(src.read_bytes())
        self.entries.append({"kind": "recording", "file": dst.name, "src": str(src), "note": note, "ts": time.time()})
        return dst

    def add_file(self, path, *, kind: str = "file", note: str = "") -> Optional[Path]:
        from pathlib import Path as _P

        src = _P(path)
        if not src.exists() or src.stat().st_size == 0:
            return None
        dst = self.dir / _sanitize(src.name)
        dst.write_bytes(src.read_bytes())
        self.entries.append({"kind": kind, "file": dst.name, "note": note, "ts": time.time()})
        return dst

    def add_cdr(self, path_or_doc, *, note: str = "", diff: str = "") -> Optional[Path]:
        """Snapshot a CDR artifact (path or inline dict) + optional field diff."""
        import json

        from pathlib import Path as _P

        if isinstance(path_or_doc, (str, _P)):
            src = _P(path_or_doc)
            if not src.exists() or src.stat().st_size < 2:
                self.log_metric("cdr_missing", str(src), note=note)
                return None
            data = src.read_text(encoding="utf-8")
            name = src.name
        else:
            data = json.dumps(path_or_doc, ensure_ascii=False, indent=2)
            name = f"cdr_{int(time.time())}.json"
        dst = self.dir / _sanitize(name)
        dst.write_text(data, encoding="utf-8")
        if diff:
            (self.dir / (_sanitize(name) + ".diff.txt")).write_text(diff, encoding="utf-8")
        self.entries.append({"kind": "cdr", "file": dst.name, "note": note, "diff": bool(diff), "ts": time.time()})
        return dst

    def add_events(self, events, *, channel: str = "webhook", note: str = "") -> Optional[Path]:
        """Dump captured RWI/webhook event timeline (JSONL) for the report."""
        if not events:
            self.log_metric(f"{channel}_events_empty", 0, note=note or "no events captured")
            return None
        p = self.dir / f"{_sanitize(channel)}_events.jsonl"
        with p.open("w", encoding="utf-8") as f:
            for ev in events:
                try:
                    f.write(json.dumps(ev, ensure_ascii=False, default=str) + "\n")
                except TypeError:
                    f.write(json.dumps({"raw": str(ev)}, ensure_ascii=False) + "\n")
        self.entries.append({"kind": "events", "file": p.name, "channel": channel, "count": len(events), "note": note})
        return p

    def add_result(self, name: str, result: Any, *, note: str = "") -> None:
        """Attach a structured assertion result (AudioCheckResult/CdrCheckResult/...)."""
        payload = result.to_dict() if hasattr(result, "to_dict") else result
        self.entries.append({"kind": "result", "name": name, "data": payload, "note": note})
        self.mark_level("AUDIO" if name.startswith("audio") else "FIELD")

    def add_text(self, name: str, text: str, *, kind: str = "text") -> Path:
        p = self.dir / f"{_sanitize(name)}.txt"
        p.write_text(text or "", encoding="utf-8")
        self.entries.append({"kind": kind, "file": p.name})
        return p

    # -- metrics / levels ---------------------------------------------------

    def log_metric(self, key: str, value: Any, *, note: str = "") -> None:
        self.metrics[key] = {"value": value, "note": note, "ts": time.time()}

    def mark_level(self, level: str) -> None:
        """Record assertion depth: FLOW / FORMAT / FIELD / AUDIO / EVIDENCE."""
        self.assertion_levels.add(level)

    def set_status(self, status: str, error: str = "") -> None:
        self.metrics["_status"] = {"value": status, "error": error[:4000]}

    # -- persistence --------------------------------------------------------

    def flush(self) -> Path:
        (self.dir / "evidence.json").write_text(
            json.dumps(
                {
                    "test_id": self.test_id,
                    "worker": self.worker,
                    "lane": self.lane,
                    "run_id": self.run_id,
                    "started_at": self.started_at,
                    "duration_sec": round(time.time() - self.started_at, 3),
                    "metrics": self.metrics,
                    "assertion_levels": sorted(self.assertion_levels),
                    "entries": self.entries,
                },
                ensure_ascii=False,
                indent=1,
            ),
            encoding="utf-8",
        )
        return self.dir / "evidence.json"
