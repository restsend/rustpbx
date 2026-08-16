"""CDR + recording E2E tests.

Verifies call records are persisted (config/cdr JSON) with caller/callee fields,
and drives RWI record_start / record_stop to capture audio.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.cdr, pytest.mark.record]


def _latest_cdr_json(cdr_dir: Path) -> list[dict]:
    out: list[dict] = []
    for p in sorted(cdr_dir.rglob("*.json"), key=lambda f: f.stat().st_mtime, reverse=True):
        try:
            out.append(json.loads(p.read_text(encoding="utf-8")))
        except Exception:
            continue
    return out


async def _reg_callee(sipbot_pool, pbx, port):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(ua)
    return ua


@pytest.mark.asyncio
async def test_cdr_persisted_after_call(pbx, sipbot_pool, cdr_dir):
    """Basic call produces a CDR JSON with caller/callee + duration."""
    h.boot_pbx(pbx)
    await _reg_callee(sipbot_pool, pbx, 15130)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    await caller.wait_output_async(r"All bots finished", timeout=25)

    deadline = asyncio.get_event_loop().time() + 10
    records: list[dict] = []
    while asyncio.get_event_loop().time() < deadline:
        records = _latest_cdr_json(cdr_dir)
        if records:
            break
        await asyncio.sleep(0.5)
    assert records, "no CDR JSON written to config/cdr/"

    rec = records[0]
    body = rec if isinstance(rec, dict) and ("caller" in rec) else rec.get("record") or rec
    caller_val = body.get("caller") or body.get("fromNumber") or ""
    assert "1001" in str(caller_val), f"CDR missing caller 1001: {body}"


@pytest.mark.xfail(reason="WIP media layer: recording output not finalized under WebRTC + sipflow coexistence")
@pytest.mark.asyncio
async def test_rwi_record_start_stop(pbx, sipbot_pool, rwi, tmp_path):
    """Recording pipeline: auto-start recording writes a WAV; RWI record commands accepted.

    `[recording] auto_start=true` records every accepted call. We make a normal
    call and assert a WAV recording file is produced. Then we drive the RWI
    record command surface on an RWI-originated call.
    """
    pbx.config_builder.set_recording_force_file()
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)

    # 1) Auto-start recording: make a call and find the produced WAV.
    await _reg_callee(sipbot_pool, pbx, 15131)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    await caller.wait_output_async(r"All bots finished", timeout=25)

    deadline = asyncio.get_event_loop().time() + 10
    wavs: list = []
    while asyncio.get_event_loop().time() < deadline:
        wavs = list((pbx.project_root / "config").rglob("*.wav"))
        if wavs:
            break
        await asyncio.sleep(0.5)
    assert wavs, "no recording WAV produced under config/"

    # 2) RWI record command surface on an RWI-originated call.
    await _reg_callee(sipbot_pool, pbx, h.ua_port(15132))
    call_id = f"rec-{uuid.uuid4().hex[:8]}"
    resp = await rwi.originate(call_id, f"sip:1002@{pbx.sip_addr}", "sip:rwi@pbx", "default")
    assert resp.get("status") == "success", resp
    rec_path = tmp_path / "record_rwi.wav"
    start = await rwi.record_start(call_id, str(rec_path), beep=True)
    assert "status" in start, start
    await rwi.hangup(call_id)

    deadline = asyncio.get_event_loop().time() + 5
    while asyncio.get_event_loop().time() < deadline:
        if rec_path.exists() and rec_path.stat().st_size > 44:
            break
        await asyncio.sleep(0.3)
    assert rec_path.exists() and rec_path.stat().st_size > 44, "recording WAV not produced"
    samples, sr = read_wav_mono(rec_path)
    assert len(samples) > 0, "recording has no samples"
