"""Strict CDR schema + semantic validation (field map from src/callrecord/mod.rs).

CallRecord is serde camelCase. On-disk CDR JSON may be wrapped in a top-level
"record" key — unwrap transparently. Every check is evidence-grade: failures
carry expected/got, and :class:`CdrCheckResult` serializes into evidence.json.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Optional

from .guards import require, require_keys

# src/callrecord/mod.rs — CallRecordHangupReason (serde camelCase / variant names)
HANGUP_REASONS = {
    "byCaller", "byCallee", "byRefer", "bySystem", "autohangup", "noAnswer",
    "noBalance", "answerMachine", "serverUnavailable", "canceled", "rejected",
    "failed", "rtpTimeout", "abandoned",
}

CORE_KEYS = ("callId", "startTime", "endTime", "caller", "callee", "statusCode")
# CallDetails (flattened) — present when the CC/CDR detail writer ran; checked
# only via `expect` or when require_details=True.
DETAIL_KEYS = ("direction", "status", "fromNumber", "toNumber")

_ISO_TS = re.compile(r"^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}")


@dataclass
class CdrCheckResult:
    file: str
    call_id: Optional[str] = None
    status: Optional[str] = None
    hangup_reason: Optional[str] = None
    checks: list = field(default_factory=list)
    diff: str = ""
    ok: bool = False

    def to_dict(self) -> dict:
        return asdict(self)


def unwrap_cdr(doc: dict) -> dict:
    """Support {'callId':...} and {'record': {'callId':...}} envelopes."""
    if isinstance(doc, dict) and isinstance(doc.get("record"), dict):
        return doc["record"]
    if isinstance(doc, dict) and isinstance(doc.get("rec"), dict):
        return doc["rec"]
    return doc


def load_cdr(path) -> tuple[dict, dict]:
    """Read CDR JSON from *path*; returns (unwrapped, raw_doc)."""
    import json

    p = Path(path)
    if not p.exists():
        raise AssertionError(f"[cdr] file missing: {p}")
    if p.stat().st_size < 2:
        raise AssertionError(f"[cdr] file empty: {p}")
    try:
        doc = json.loads(p.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise AssertionError(f"[cdr] invalid JSON in {p}: {exc}") from exc
    return unwrap_cdr(doc), doc


def _check(res: CdrCheckResult, name: str, expected, got, ok: bool):
    res.checks.append({"name": name, "expected": expected, "got": got, "pass": bool(ok)})
    return ok


def _ts(value) -> Optional[float]:
    """Parse CDR timestamp: ISO-8601 or epoch (s/ms)."""
    if value is None:
        return None
    if isinstance(value, (int, float)):
        v = float(value)
        return v / 1000.0 if v > 1e11 else v
    s = str(value)
    if _ISO_TS.match(s):
        from datetime import datetime, timezone

        s2 = s.replace("Z", "+00:00")
        dt = datetime.fromisoformat(s2)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return dt.timestamp()
    try:
        v = float(s)
        return v / 1000.0 if v > 1e11 else v
    except ValueError:
        return None


def assert_cdr(
    cdr,
    expected: Optional[dict] = None,
    *,
    require_hangup_reason: bool = False,
    require_time_chain: bool = True,
    require_recorder: bool = False,
    expect_recorder_count: Optional[int] = None,
    label: str = "",
) -> CdrCheckResult:
    """Validate a CDR (dict, or path to JSON file) against schema + expectations.

    expected: subset of CDR fields that must match loosely (substring for
    strings, numeric equality otherwise), e.g. {"status": "completed",
    "statusCode": 200, "caller": "1001"}.
    """
    prefix = f"[cdr]{(' ' + label) if label else ''}"
    if isinstance(cdr, (str, Path)):
        rec, raw = load_cdr(cdr)
        src = str(cdr)
    else:
        rec, raw = unwrap_cdr(cdr), cdr
        src = "<inline>"
    res = CdrCheckResult(file=src)
    require(rec, "cdr record")
    if not isinstance(rec, dict):
        raise AssertionError(f"{prefix}: CDR is not a dict — {rec!r:.200}")

    require_keys(rec, CORE_KEYS, f"{prefix} CDR core fields")
    res.call_id = str(rec.get("callId") or "") or None
    res.status = rec.get("status") or (rec.get("details", {}) or {}).get("status")
    res.hangup_reason = rec.get("hangupReason")

    if require_hangup_reason:
        hr = res.hangup_reason
        ok = isinstance(hr, str) and (hr in HANGUP_REASONS or hr.startswith("other("))
        _check(res, "hangup_reason_enum", f"one of {len(HANGUP_REASONS)}", hr, ok)
        if not ok:
            raise AssertionError(f"{prefix}: hangupReason {hr!r} outside enum {sorted(HANGUP_REASONS)}")

    if require_time_chain:
        t0, t1, t2, t3 = (_ts(rec.get(k)) for k in ("startTime", "ringTime", "answerTime", "endTime"))
        ok = t0 is not None and t3 is not None and t3 >= t0
        _check(res, "time_chain_start_end", "endTime >= startTime", {"start": rec.get("startTime"), "end": rec.get("endTime")}, ok)
        if not ok:
            raise AssertionError(f"{prefix}: invalid time chain start={rec.get('startTime')} end={rec.get('endTime')}")
        pairs = [("ringTime", t0, t1), ("answerTime", t1 if t1 else t0, t2)]
        for name, lo, hi in pairs:
            if hi is not None:
                ok = hi >= lo
                _check(res, f"time_chain_{name}", f">= prior", {"lo": lo, "hi": hi}, ok)
                if not ok:
                    raise AssertionError(f"{prefix}: {name} out of order ({hi} < {lo})")
        if t2 is not None and rec.get("durationSecs") is None:
            # duration consistency when answerTime/endTime both ISO
            dur = t3 - t2
            ok = dur >= 0
            _check(res, "duration_nonneg", ">= 0", round(dur, 3), ok)

    if require_recorder:
        recorder = rec.get("recorder") or []
        ok = isinstance(recorder, list) and len(recorder) > 0
        _check(res, "recorder_present", ">=1 entries", len(recorder) if isinstance(recorder, list) else type(recorder).__name__, ok)
        if not ok:
            raise AssertionError(f"{prefix}: no recorder[] entries but recording expected")
        for i, r in enumerate(recorder):
            require_keys(r, ("path", "size"), f"{prefix} recorder[{i}]")
            if int(r.get("size") or 0) <= 0:
                raise AssertionError(f"{prefix}: recorder[{i}] zero-size recording (invalid data): {r!r:.200}")

    if expect_recorder_count is not None:
        n = len(rec.get("recorder") or [])
        ok = n == expect_recorder_count
        _check(res, "recorder_count", expect_recorder_count, n, ok)
        if not ok:
            raise AssertionError(f"{prefix}: recorder entries {n} != expected {expect_recorder_count}")

    if expected:
        from .guards import diff_map

        mismatches = []
        for k, want in expected.items():
            have = rec.get(k, "<MISSING>")
            match = _loose_eq(want, have)
            if not match:
                mismatches.append(f"  {k}: expected={want!r} got={have!r}")
            _check(res, f"field:{k}", want, have if have != "<MISSING>" else None, match)
        res.diff = "\n".join([f"{prefix} field diff:"] + mismatches) if mismatches else ""
        if mismatches:
            raise AssertionError(f"{prefix} CDR field mismatch:\n" + "\n".join(mismatches))

    res.ok = True
    return res


def _loose_eq(want, have) -> bool:
    if have == want:
        return True
    if isinstance(want, str) and isinstance(have, str):
        return want in have or have in want
    try:
        return float(want) == float(have)
    except (TypeError, ValueError):
        return False
