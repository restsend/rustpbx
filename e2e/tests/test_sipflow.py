"""SipFlow E2E tests.

Verifies that calls are captured by the sipflow pipeline into the local
sipflow root — both SIP signalling AND RTP media (regression: media capture
was dropped after the media-layer rewrite when `force_file` is not set).
"""

from __future__ import annotations

import asyncio
import json
import shutil
import sqlite3
import tempfile
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.sipflow]


def _count_media_msgs(db_path: Path) -> int:
    try:
        con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=2)
        try:
            return int(con.execute("SELECT COUNT(*) FROM media_msgs").fetchone()[0])
        except sqlite3.Error:
            return 0
        finally:
            con.close()
    except (sqlite3.Error, OSError):
        return 0


async def _wait_media_count(sipflow_dir: Path, timeout: float = 15) -> int:
    deadline = asyncio.get_event_loop().time() + timeout
    total = 0
    while asyncio.get_event_loop().time() < deadline:
        total = sum(
            _count_media_msgs(p)
            for p in list(sipflow_dir.rglob("**/sipflow.db"))
        )
        if total > 0:
            break
        await asyncio.sleep(0.5)
    return total


def _latest_cdr_call_id(cdr_dir: Path) -> str:
    for p in sorted(cdr_dir.rglob("*.json"), key=lambda f: f.stat().st_mtime, reverse=True):
        try:
            data = json.loads(p.read_text(encoding="utf-8"))
        except Exception:
            continue
        body = data if isinstance(data, dict) else {}
        cid = body.get("callId") or body.get("call_id")
        if cid:
            return cid
    raise AssertionError(f"no CDR with callId under {cdr_dir}")


@pytest.mark.asyncio
async def test_sipflow_media_export(pbx, sipbot_pool, sipflow_dir, cdr_dir, tmp_path):
    """After a call, RTP media is captured by sipflow and exports to a WAV.

    Without force_file, an enabled sipflow backend must store RTP packets (the
    media-layer regression that produced empty recordings). We verify both that
    media packets land in the sqlite store and that the console API serves a
    playable WAV with audio content.
    """
    pbx.config_builder.set_sipflow(engine="sqlite", root=str(sipflow_dir))
    h.boot_pbx(pbx)
    for stale in list(sipflow_dir.rglob("*")):
        if stale.is_dir():
            shutil.rmtree(stale, ignore_errors=True)
        else:
            stale.unlink(missing_ok=True)
    sine_path = tmp_path / "tone_440.wav"
    h.generate_sine_wav(sine_path, 440.0, 5.0, sample_rate=8000, amplitude=0.5)
    callee = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15140), username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(callee)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=6, audio_quality=True, play_file=str(sine_path),
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    await caller.wait_output_async(r"All bots finished", timeout=25)

    # 1) RTP media packets must be stored (not just SIP signalling).
    media_count = await _wait_media_count(sipflow_dir)
    assert media_count > 0, f"no media packets captured under {sipflow_dir}"

    # 2) The console API must export a playable WAV for the call.
    import aiohttp

    call_id = _latest_cdr_call_id(cdr_dir)
    url = f"{pbx.http_url}/api/sipflow/media/{call_id}"
    headers = {"Authorization": f"Bearer {pbx.rwi_token}"}
    async with aiohttp.ClientSession() as session:
        async with session.get(url, headers=headers, timeout=aiohttp.ClientTimeout(total=30)) as resp:
            assert resp.status == 200, f"sipflow media API returned {resp.status}"
            wav_bytes = await resp.read()
    assert len(wav_bytes) > 44, "exported WAV too small"

    with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
        tmp.write(wav_bytes)
        wav_path = Path(tmp.name)
    try:
        samples, rate = h.read_wav_mono(wav_path)
        assert len(samples) > 0, "exported WAV has no samples"
        assert h.has_audio_content(samples, -50), "exported WAV contains no audio"
        freq, _ = h.find_dominant_frequency(samples, rate, low=300, high=600)
        assert 380 <= freq <= 500, f"expected ~440Hz tone in recording, got {freq:.0f}Hz"
    finally:
        wav_path.unlink(missing_ok=True)
