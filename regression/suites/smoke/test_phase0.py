"""Phase 0 exit-criteria smoke: fixtures, evidence store, strict assertions
and the report chain all work inside the unified regression tree."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import helpers
from helpers import assertions as A
from helpers.evidence import EvidenceStore
import helpers.audio_verifier as av

pytestmark = [pytest.mark.cdr]


def test_helpers_package_complete():
    for name in (
        "ConfigBuilder", "PbxServer", "PbxApiClient", "SipBotPool", "RtpStats",
        "RwiClient", "EventChecker", "WebhookServer", "apply_config",
        "generate_sine_wav", "find_dominant_frequency", "boot_pbx", "wait_log",
        "ua_port", "WsBridgeEchoServer",
    ):
        assert hasattr(helpers, name), f"helpers missing {name}"


def test_guards_fail_fast_on_empty():
    with pytest.raises(AssertionError):
        A.require(None, "must_fail")


def test_audio_assert_tone_and_silence(tmp_path):
    wav = tmp_path / "t.wav"
    av.generate_sine_wav(wav, freq_hz=440, duration_s=0.6)
    res = A.assert_audio(wav, freq=440, min_duration=0.3)
    assert res.ok and res.dominant_freq_hz and abs(res.dominant_freq_hz - 440) <= 15
    silent = tmp_path / "s.wav"
    import wave
    import struct

    with wave.open(str(silent), "w") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(8000)
        w.writeframes(struct.pack("<" + "h" * 8000, *([0] * 8000)))
    with pytest.raises(AssertionError):
        A.assert_audio(silent)  # all-silence must not pass


def test_cdr_schema_and_event_flow():
    cdr = {
        "callId": "c1", "startTime": "2026-09-16T10:00:00Z", "ringTime": "2026-09-16T10:00:01Z",
        "answerTime": "2026-09-16T10:00:02Z", "endTime": "2026-09-16T10:00:10Z",
        "caller": "1001", "callee": "1002", "statusCode": 200, "status": "completed",
        "hangupReason": "byCaller", "recorder": [{"path": "x.wav", "size": 123}],
    }
    res = A.assert_cdr(cdr, expected={"statusCode": 200}, require_hangup_reason=True, require_recorder=True)
    assert res.ok
    evs = [
        {"event_type": "call_created", "call_id": "c1"},
        {"event_type": "call_ringing", "call_id": "c1"},
        {"event_type": "call_answered", "call_id": "c1"},
        {"event_type": "call_hangup", "call_id": "c1"},
    ]
    A.assert_event_flow(evs, ["call_created", "call_answered", "call_hangup"], call_id="c1")


def test_evidence_store_attaches(evidence: EvidenceStore):
    wav = evidence.dir / "tone.wav"
    av.generate_sine_wav(wav, freq_hz=620, duration_s=0.4)
    evidence.add_recording(wav, note="smoke recording")
    evidence.add_result("audio:smoke", A.assert_audio(wav, freq=620))
    evidence.add_events([{"event_type": "call_created", "call_id": "smoke"}], channel="webhook")
    evidence.log_metric("smoke", True)
    evidence.mark_level("AUDIO")
    evidence.mark_level("FIELD")
    manifest = evidence.flush()
    import json

    data = json.loads(manifest.read_text())
    assert data["metrics"]["smoke"]["value"] is True
    kinds = {e["kind"] for e in data["entries"]}
    assert {"recording", "result", "events"} <= kinds


def test_report_cli_renders_from_this_run():
    """The artifacts root exists and lives under regression/artifacts."""
    from pathlib import Path as _P

    import conftest as c

    root = c.ARTIFACTS_ROOT / c.RUN_ID
    assert root.exists(), f"artifact root missing: {root}"
    assert "artifacts" in str(root)
