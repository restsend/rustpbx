"""RWI originate recording E2E tests.

Regression coverage for RWI outbound recording:

1. `call.originate` with a `record` option → recording auto-starts on answer
   (`record_started` event), the WAV file is produced, and the CDR carries the
   recorder entry so the call-record hooks fire `recording_metadata_available`
   and `record_end`.
2. Mid-call `record.start` / `record.stop` on an originated call works the same
   way (the old code rejected this with 'Recording is not enabled for this
   call' because the originate dialplan never set recording.enabled).

`media_proxy="all"` is required so the MediaBridge recorder is active.
The e2e config already enables `[recording]` (Local type), which wires the
RecordingUploadHook that emits the metadata events.
"""

from __future__ import annotations

import asyncio
import uuid
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.outbound]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _registered_echo_callee(sipbot_pool, pbx, port, username="1002"):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await asyncio.sleep(2)
    return ua


async def _wait_event_all(rwi, event_type: str, timeout: float = 10.0):
    """wait_for_event that scans all events (handles the race where the event
    arrives before the command response is awaited)."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        for ev in rwi.events:
            et = ev.get("event_type") or ev.get("type")
            if et == event_type:
                return ev
        await asyncio.sleep(0.1)
    return None


async def _setup(sipbot_pool, pbx, rwi, port):
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    callee = await _registered_echo_callee(sipbot_pool, pbx, port)
    return callee


@pytest.mark.asyncio
async def test_outbound_originate_record_on_answer(pbx, sipbot_pool, rwi, tmp_path):
    """call.originate(record=…) → auto recording → events + file + metadata."""
    callee = await _setup(sipbot_pool, pbx, rwi, 15120)

    rec_path = tmp_path / "ob_record_auto.wav"
    call_id = _call_id("obrec")
    rwi.clear_events()
    resp = await rwi.send_request("call.originate", {
        "call_id": call_id,
        "destination": f"sip:1002@{pbx.sip_addr}",
        "caller_id": "sip:rwi@pbx",
        "context": "default",
        "record": {
            "mode": "mixed",
            "beep": False,
            "storage": {"path": str(rec_path)},
        },
    })
    assert resp.get("status") == "success", resp

    assert await _wait_event_all(rwi, "call_answered", timeout=15) is not None
    assert await _wait_event_all(rwi, "record_started", timeout=10) is not None, (
        "record_started not received — originate record option not applied"
    )

    # Let some audio flow, then hang up.
    await asyncio.sleep(2)
    await rwi.hangup(call_id)

    stopped = await _wait_event_all(rwi, "record_stopped", timeout=15)
    assert stopped is not None, "record_stopped not received after hangup"

    # The recording file must exist and have content.
    assert rec_path.exists(), f"recording file missing: {rec_path}"
    assert rec_path.stat().st_size > 1000, (
        f"recording file too small ({rec_path.stat().st_size} bytes)"
    )

    # The CDR carries the recorder entry → RecordingUploadHook (Local) emits
    # recording_metadata_available + record_end.
    meta = await _wait_event_all(rwi, "recording_metadata_available", timeout=15)
    assert meta is not None, "recording_metadata_available not received (CDR missing recorder entry?)"
    assert meta.get("call_id") == call_id, meta
    record_end = await _wait_event_all(rwi, "record_end", timeout=10)
    assert record_end is not None, "record_end not received"
    assert record_end.get("call_id") == call_id, record_end


@pytest.mark.asyncio
async def test_outbound_midcall_record_start_stop(pbx, sipbot_pool, rwi, tmp_path):
    """Originate without record → mid-call record.start/stop works (old code
    failed with 'Recording is not enabled for this call')."""
    callee = await _setup(sipbot_pool, pbx, rwi, 15121)

    call_id = _call_id("obrec2")
    rwi.clear_events()
    resp = await rwi.originate(call_id, f"sip:1002@{pbx.sip_addr}", "sip:rwi@pbx", "default")
    assert resp.get("status") == "success", resp
    assert await _wait_event_all(rwi, "call_answered", timeout=15) is not None

    rec_path = tmp_path / "ob_record_manual.wav"
    rwi.clear_events()
    start_resp = await rwi.send_request("record.start", {
        "call_id": call_id,
        "mode": "mixed",
        "beep": False,
        "storage": {"path": str(rec_path)},
    })
    assert start_resp.get("status") == "success", (
        f"mid-call record.start failed on originated call: {start_resp}"
    )
    assert await _wait_event_all(rwi, "record_started", timeout=10) is not None

    await asyncio.sleep(2)

    rwi.clear_events()
    stop_resp = await rwi.send_request("record.stop", {"call_id": call_id})
    assert stop_resp.get("status") == "success", stop_resp
    stopped = await _wait_event_all(rwi, "record_stopped", timeout=10)
    assert stopped is not None, "record_stopped not received"
    assert stopped.get("filename") == str(rec_path), stopped

    assert rec_path.exists(), f"recording file missing: {rec_path}"
    assert rec_path.stat().st_size > 1000, (
        f"recording file too small ({rec_path.stat().st_size} bytes)"
    )

    await rwi.hangup(call_id)
    # CDR metadata events (recording already stopped before hangup; the CDR
    # still carries the recorder entry).
    meta = await _wait_event_all(rwi, "recording_metadata_available", timeout=15)
    assert meta is not None, "recording_metadata_available not received"
    assert meta.get("call_id") == call_id, meta
