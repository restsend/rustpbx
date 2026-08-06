"""Voicemail addon E2E tests.

Covers the full voicemail lifecycle:

* Leave a message (DTMF '#' end + caller-hangup persistence).
* Listen / replay / delete via the `*97` check-voicemail IVR (owner auto-auth).
* Listen / download / delete via the console REST API + web UI form.

Also asserts voicemail recordings are written as **mono** (caller-only) WAVs.
"""

from __future__ import annotations

import asyncio
import struct
from pathlib import Path

import aiohttp
import pytest

import helpers as h

pytestmark = [pytest.mark.voicemail]


async def _await_recording(storage: Path, timeout: float = 25) -> Path | None:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        recordings = list(storage.rglob("*.wav"))
        if recordings and recordings[0].stat().st_size > 44:
            return recordings[0]
        await asyncio.sleep(0.5)
    return None


@pytest.mark.asyncio
async def test_voicemail_records_message(pbx, sipbot_pool, tmp_path):
    """Caller -> route app=voicemail -> greeting + beep -> leave message -> '#' ends.

    The '#' is sent AFTER recording starts (greeting ~5.9s + beep ~0.3s), so it
    actually stops the recorder and triggers persistence.
    """
    storage = tmp_path / "vm_recordings"
    pbx.config_builder.add_voicemail(
        spool_dir=str(tmp_path / "spool"),
        storage_path=str(storage),
        max_duration_secs=60,
    )
    pbx.config_builder.add_route(
        "to-vm",
        match={"to.user": "vm"},
        priority=10,
        action="application",
        app="voicemail",
        app_params={"extension": "1001"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:vm@{pbx.sip_addr}", username="1001", password="123456",
        hangup=20, dtmf_flows="9s:#", audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

    # '#' at 9s stops recording -> persist -> "saved" -> app hangup.
    recording = await _await_recording(storage, timeout=25)
    assert recording, f"no voicemail recording under {storage}"
    assert recording.stat().st_size > 44, "voicemail recording is empty"

    # Caller received the greeting audio (non-silent).
    await h.wait_rtp(caller, "caller", 15)


@pytest.mark.asyncio
async def test_voicemail_hangup_persists_message(pbx, sipbot_pool, tmp_path):
    """Caller leaves a message and hangs up WITHOUT pressing '#'.

    This is the primary voicemail UX. Regression for the bug where caller
    hangup during recording abandoned the message (on_record_complete never
    fired because stop_recording wasn't called on the BYE path).
    """
    storage = tmp_path / "vm_recordings"
    pbx.config_builder.add_voicemail(
        spool_dir=str(tmp_path / "spool"),
        storage_path=str(storage),
        max_duration_secs=60,
    )
    pbx.config_builder.add_route(
        "to-vm",
        match={"to.user": "vm"},
        priority=10,
        action="application",
        app="voicemail",
        app_params={"extension": "1001"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    # No DTMF — caller records for ~6s (after greeting+beep) then hangs up.
    caller = sipbot_pool.caller(
        target=f"sip:vm@{pbx.sip_addr}", username="1001", password="123456",
        hangup=12, audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

    # Caller hangs up at ~12s; the recording must still be persisted.
    recording = await _await_recording(storage, timeout=25)
    assert recording, (
        f"voicemail recording not persisted after caller hangup — check that "
        f"the session finalizes the recording on BYE. output:\n{caller.output[-1500:]}"
    )
    assert recording.stat().st_size > 44, "voicemail recording is empty"


# ── shared helpers ─────────────────────────────────────────────────────────────


def _configure(pbx, tmp_path):
    """Add voicemail storage + a route to the voicemail app for extension 1001."""
    storage = tmp_path / "vm_recordings"
    pbx.config_builder.add_voicemail(
        spool_dir=str(tmp_path / "spool"),
        storage_path=str(storage),
        max_duration_secs=60,
    )
    pbx.config_builder.add_route(
        "to-vm",
        match={"to.user": "vm"},
        priority=10,
        action="application",
        app="voicemail",
        app_params={"extension": "1001"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)
    return storage


async def _http(pbx, method: str, path: str, **kwargs):
    headers = {"Authorization": f"Bearer {pbx.rwi_token}"}
    async with aiohttp.ClientSession() as s:
        async with s.request(
            method, f"{pbx.http_url}{path}", headers=headers,
            timeout=aiohttp.ClientTimeout(total=30), **kwargs,
        ) as resp:
            body = await resp.read()
    return resp.status, body


async def _api_messages(pbx, ext: str) -> list:
    status, body = await _http(pbx, "GET", f"/api/voicemail/{ext}/messages")
    if status != 200:
        return []
    import json
    return json.loads(body).get("messages", [])


async def _await_message_count(pbx, ext: str, count: int, timeout: float = 30) -> list:
    """Poll the voicemail REST API until the mailbox has exactly `count` messages."""
    deadline = asyncio.get_event_loop().time() + timeout
    last: list = []
    while asyncio.get_event_loop().time() < deadline:
        last = await _api_messages(pbx, ext)
        if len(last) == count:
            return last
        await asyncio.sleep(0.5)
    raise AssertionError(
        f"mailbox {ext} message count did not reach {count} within {timeout}s "
        f"(last: {len(last)})"
    )


async def _leave_message(pbx, sipbot_pool, tmp_path, *, end_dtmf="16s:#", hangup=20):
    """Leave one voicemail for 1001 containing a 440 Hz tone."""
    storage = _configure(pbx, tmp_path)
    tone = h.generate_sine_wav(tmp_path / "tone.wav", 440.0, 20.0)
    caller = sipbot_pool.caller(
        target=f"sip:vm@{pbx.sip_addr}", username="1001", password="123456",
        hangup=hangup, dtmf_flows=end_dtmf, play_file=str(tone), audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

    recording = await _await_recording(storage, timeout=30)
    assert recording, f"no voicemail recording persisted. output:\n{caller.output[-1500:]}"
    assert recording.stat().st_size > 44, "voicemail recording is empty"
    await _await_message_count(pbx, "1001", 1, timeout=30)
    return storage


# ── web / REST: listen + delete ────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_voicemail_web_listen_and_delete(pbx, sipbot_pool, tmp_path):
    """Leave a message, then use the console REST API to list + download it and
    the web form to delete it. Also asserts the recording is mono (caller-only).
    """
    storage = await _leave_message(pbx, sipbot_pool, tmp_path)
    msgs = await _api_messages(pbx, "1001")
    assert len(msgs) == 1, f"expected 1 message, got {msgs}"
    m = msgs[0]
    assert m["duration"] > 0, f"message duration should be > 0: {m}"
    assert m["audio_url"], f"message should expose audio_url: {m}"

    # 1) Listen: download the recorded message audio.
    status, body = await _http(pbx, "GET", m["audio_url"])
    assert status == 200, f"audio endpoint returned {status}"
    assert len(body) > 44, "voicemail audio too small"
    # Mono (caller-only) recording — channels field at WAV offset 22.
    channels = struct.unpack("<H", body[22:24])[0]
    assert channels == 1, f"voicemail recording should be mono, got {channels} channels"

    audio_path = tmp_path / "downloaded_msg.wav"
    audio_path.write_bytes(body)
    samples, rate = h.read_wav_mono(audio_path)
    assert len(samples) > 0, "downloaded audio has no samples"
    assert h.has_audio_content(samples, -50), "downloaded audio is silent (expected the tone)"

    # 2) Delete via the console form (Bearer-authenticated, CSRF-exempt).
    status, _ = await _http(
        pbx, "POST", f"/console/voicemail/messages/{m['id']}/delete",
        allow_redirects=False,
    )
    assert status in (302, 303, 307), f"delete returned {status}"

    # 3) Verify: API list empty + audio file removed from storage.
    assert await _await_message_count(pbx, "1001", 0, timeout=20) == []
    leftover = list(storage.rglob("*.wav"))
    assert not leftover, f"audio files should be removed from storage, found {leftover}"


# ── phone (*97) check: listen + replay + delete ────────────────────────────────


@pytest.mark.asyncio
async def test_voicemail_phone_check_listen_replay_delete(pbx, sipbot_pool, tmp_path):
    """Owner calls *97 from their own phone (auto-auth), hears the message,
    replays it (DTMF 1), then deletes it (DTMF 3)."""
    await _leave_message(pbx, sipbot_pool, tmp_path)

    # Extension "1001#" then replay (1) and delete (3). NOTE: sipbot's DTMF
    # delays are *cumulative* (each entry sleeps its delay after the previous),
    # so the last entry lands at 1+1.3+1.6+1.9+2.2+30+35 = 73s — comfortably
    # before hangup. hangup must cover the whole IVR after the delete.
    owner = sipbot_pool.caller(
        target=f"sip:*97@{pbx.sip_addr}", username="1001", password="123456",
        hangup=110,
        dtmf_flows="1s:1,1.3s:0,1.6s:0,1.9s:1,2.2s:#,30s:1,35s:3",
        audio_quality=True,
    )
    assert await owner.wait_output_async(r"200 OK|Call established", timeout=25), owner.output

    # The owner auto-authenticates (no PIN), the recorded message plays back,
    # and DTMF 3 deletes it — this is the end-to-end proof the whole *97 flow
    # (routing → extension → listen → replay → delete) succeeded.
    assert await _await_message_count(pbx, "1001", 0, timeout=110) == []

    # After "no more messages" the call should end naturally.
    await owner.wait_output_async(r"All bots finished|Call ended|has_audio=true", timeout=60)


@pytest.mark.asyncio
async def test_voicemail_custom_delete_key(pbx, sipbot_pool, tmp_path):
    """A custom `[check_voicemail] delete_key` (here "9") replaces the default
    "3" in the *97 flow: pressing 9 deletes the message, pressing 3 does not."""
    storage = tmp_path / "vm_recordings"
    pbx.config_builder.add_voicemail(
        spool_dir=str(tmp_path / "spool"),
        storage_path=str(storage),
        max_duration_secs=60,
        check_voicemail={"delete_key": "9"},
    )
    pbx.config_builder.add_route(
        "to-vm",
        match={"to.user": "vm"},
        priority=10,
        action="application",
        app="voicemail",
        app_params={"extension": "1001"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    # Leave a message (with a tone so the owner hears it).
    tone = h.generate_sine_wav(tmp_path / "tone.wav", 440.0, 20.0)
    caller = sipbot_pool.caller(
        target=f"sip:vm@{pbx.sip_addr}", username="1001", password="123456",
        hangup=20, dtmf_flows="16s:#", play_file=str(tone), audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    recording = await _await_recording(storage, timeout=30)
    assert recording and recording.stat().st_size > 44, "no voicemail recording persisted"
    await _await_message_count(pbx, "1001", 1, timeout=30)

    # Extension "1001#" then the custom delete key "9".
    owner = sipbot_pool.caller(
        target=f"sip:*97@{pbx.sip_addr}", username="1001", password="123456",
        hangup=110,
        dtmf_flows="1s:1,1.3s:0,1.6s:0,1.9s:1,2.2s:#,35s:9",
        audio_quality=True,
    )
    assert await owner.wait_output_async(r"200 OK|Call established", timeout=25), owner.output

    # Custom delete_key "9" removes the message.
    assert await _await_message_count(pbx, "1001", 0, timeout=110) == []
    await owner.wait_output_async(r"All bots finished|Call ended|has_audio=true", timeout=60)
