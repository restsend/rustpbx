"""RWI media.play / media.stop E2E tests.

Verifies the media.play command lifecycle:
  - media_play_started / media_play_finished events fire correctly
  - loop=True keeps playing until explicit stop (interrupted=True)
  - loop=False finishes naturally (interrupted=False)
  - silence source works
  - audio actually reaches the callee leg (RTP packets increase)

All tests use RWI originate to get a known call_id, then exercise
media.play on the live call. media_proxy="all" is required so the
MediaBridge is active for playback.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.media]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _registered_echo_callee(sipbot_pool, pbx, port, username="1002",
                                  record_file=None):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
        record_file=str(record_file) if record_file else None,
    )
    await h.wait_registered(ua)
    return ua


async def _wait_event_all(rwi, event_type: str, timeout: float = 10.0):
    """wait_for_event that scans all events (handles race where the event
    arrives before the action response is awaited)."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        for ev in rwi.events:
            et = ev.get("event_type") or ev.get("type")
            if et == event_type:
                return ev
        await asyncio.sleep(0.1)
    return None


async def _setup_call(sipbot_pool, pbx, rwi, port, call_prefix, tmp_path=None,
                      record_file=None):
    """Common setup: boot pbx with media_proxy=all, register echo callee, originate."""
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    callee = await _registered_echo_callee(sipbot_pool, pbx, port,
                                           record_file=record_file)
    call_id = _call_id(call_prefix)
    resp = await rwi.originate(call_id, f"sip:1002@{pbx.sip_addr}", "sip:rwi@pbx", "default")
    assert resp.get("status") == "success", resp
    await rwi.wait_for_event("call_answered", timeout=15)
    return callee, call_id


async def _assert_rtp_resumes(ua, label: str):
    """Anti-fake-e2e: after playback ends, relayed media must flow again.

    Event-only assertions pass even when the media route was never restored
    (both sides deaf); a positive RX delta proves the relay is live. The RWI
    app-caller streams only sparse audio, so the threshold is low — but a
    torn route freezes RX at exactly 0 delta.
    """
    before = ua.get_rtp_stats().rx_packets
    await asyncio.sleep(3.0)
    after = ua.get_rtp_stats().rx_packets
    delta = after - before
    assert delta > 10, (
        f"{label}: media did not resume after playback "
        f"(RX delta {delta} packets / 3s): {ua.get_rtp_stats()}"
    )


@pytest.mark.asyncio
async def test_media_play_file_loop_then_stop(pbx, sipbot_pool, rwi, tmp_path):
    """media.play(file, loop=True) -> started -> stop -> finished(interrupted=True).

    3D gate (2026-09 audit — the old version asserted only events + packet
    counts, so playing SILENCE passed):
      content  — the callee's mixdown carries the 440 Hz tone SUSTAINED for
                 the whole play window (loop really loops) and the tone is
                 GONE after stop (stop really stops it);
      format   — anchored MediaBridge implied by media_proxy=all + PCM WAV
                 decode on the recording;
      quantity — the sustained-tone run ≥ the played window length.
    """
    from helpers import (
        generate_sine_wav, read_wav_mono, goertzel_timeline,
        longest_above_run, wait_recording_async, compute_rms_db,
    )

    tone = tmp_path / "tone_440.wav"
    generate_sine_wav(tone, 440.0, 2.0, 8000, 0.5)
    callee_record = tmp_path / "play_callee_rx.wav"

    callee, call_id = await _setup_call(
        sipbot_pool, pbx, rwi, h.ua_port(15083), "media",
        record_file=callee_record)

    rwi.clear_events()
    resp = await rwi.media_play(call_id, "file", str(tone), loop=True)
    assert resp.get("status") == "success", resp
    started = await _wait_event_all(rwi, "media_play_started", timeout=10)
    assert started is not None, "media_play_started not received"
    assert started.get("call_id") == call_id

    await asyncio.sleep(4)  # ≥2 loop iterations of the 2 s file

    rwi.clear_events()
    assert (await rwi.media_stop(call_id)).get("status") == "success"
    finished = await _wait_event_all(rwi, "media_play_finished", timeout=10)
    assert finished is not None, "media_play_finished not received"
    assert finished.get("interrupted") is True, f"expected interrupted=True, got: {finished}"

    await _assert_rtp_resumes(callee, "callee after loop stop")
    await rwi.hangup(call_id)

    # ── CONTENT gate on the callee's mixdown (flushed at call end) ─────
    resolved = await wait_recording_async(callee_record, timeout=15)
    assert resolved is not None, (
        f"callee mixdown recording never flushed: {callee_record}"
    )
    samples, sr = read_wav_mono(resolved)
    assert compute_rms_db(samples) > -45.0, "callee recording silent"
    timeline = goertzel_timeline(samples, sr, 440.0, window_s=0.25, step_s=0.125)
    assert max(timeline, default=0.0) > 0.0, "no 440 Hz energy at all — play never audible"
    thr = 0.25 * max(timeline)
    start, end, dur = longest_above_run(timeline, thr, step_s=0.125)
    assert dur >= 3.5, (
        f"sustained 440Hz run {dur:.2f}s < 3.5s — loop did NOT keep the "
        f"2s file looping (run bounds {start}..{end} windows). "
        "The tone stopped early: loop playback broken."
    )
    tail = timeline[end:]
    assert all(m < thr for m in tail), (
        "440Hz energy present after media_stop — stop did not stop the audio"
    )
    print(f"\n[media-play] loop sustained {dur:.2f}s then stopped cleanly")


@pytest.mark.asyncio
async def test_media_play_natural_finish(pbx, sipbot_pool, rwi, tmp_path):
    """media.play(file, loop=False) with a short file -> finished(interrupted=False).

    3D gate: the 0.3 s 800 Hz beep is AUDIBLE in the callee's mixdown for
    ≈0.3 s (quantity: play window matches the file length) and is GONE
    after EOF (no loop residue).
    """
    from helpers import (
        generate_sine_wav, read_wav_mono, goertzel_timeline,
        longest_above_run, wait_recording_async,
    )

    short = tmp_path / "short_beep.wav"
    generate_sine_wav(short, 800.0, 0.3, 8000, 0.5)
    callee_record = tmp_path / "beep_callee_rx.wav"

    callee, call_id = await _setup_call(
        sipbot_pool, pbx, rwi, h.ua_port(15084), "beep",
        record_file=callee_record)

    rwi.clear_events()
    resp = await rwi.media_play(call_id, "file", str(short), loop=False)
    assert resp.get("status") == "success", resp
    started = await _wait_event_all(rwi, "media_play_started", timeout=10)
    assert started is not None

    finished = await _wait_event_all(rwi, "media_play_finished", timeout=10)
    assert finished is not None, "media_play_finished not received"
    assert finished.get("interrupted") is False, f"expected interrupted=False, got: {finished}"

    await _assert_rtp_resumes(callee, "callee after natural EOF")
    await rwi.hangup(call_id)

    resolved = await wait_recording_async(callee_record, timeout=15)
    assert resolved is not None, (
        f"callee mixdown recording never flushed: {callee_record}"
    )
    samples, sr = read_wav_mono(resolved)
    timeline = goertzel_timeline(samples, sr, 800.0, window_s=0.125, step_s=0.0625)
    assert max(timeline, default=0.0) > 0.0, "800Hz beep never audible"
    thr = 0.25 * max(timeline)
    start, end, dur = longest_above_run(timeline, thr, step_s=0.0625)
    assert 0.05 <= dur <= 0.9, (
        f"beep audible run {dur:.2f}s, want ≈0.3s (the file length) — "
        "playback duration does not match the source (quantity violation)"
    )
    tail = timeline[end:]
    assert all(m < thr for m in tail), (
        "800Hz energy after natural EOF — playback did not stop at EOF"
    )
    print(f"\n[media-play] beep audible {dur:.2f}s (file 0.3s), clean EOF")


@pytest.mark.xfail(reason="silence source_type event delivery not reaching RWI client; file source works")
@pytest.mark.asyncio
async def test_media_play_silence_source(pbx, sipbot_pool, rwi):
    """media.play(silence) -> started event fires -> stop -> finished."""
    callee, call_id = await _setup_call(sipbot_pool, pbx, rwi, 15085, "silence")

    rwi.clear_events()
    resp = await rwi.media_play(call_id, "silence", "", loop=True)
    assert resp.get("status") == "success", resp
    started = await _wait_event_all(rwi, "media_play_started", timeout=10)
    assert started is not None

    await asyncio.sleep(1)
    rwi.clear_events()
    assert (await rwi.media_stop(call_id)).get("status") == "success"
    finished = await _wait_event_all(rwi, "media_play_finished", timeout=10)
    assert finished is not None

    await rwi.hangup(call_id)


@pytest.mark.asyncio
async def test_media_play_loop_persists_until_stop(pbx, sipbot_pool, rwi, tmp_path):
    """loop=True playback must NOT finish naturally within 5s for a 2s file."""
    from helpers import generate_sine_wav

    tone = tmp_path / "tone_loop.wav"
    generate_sine_wav(tone, 440.0, 2.0, 8000, 0.5)

    callee, call_id = await _setup_call(sipbot_pool, pbx, rwi, 15086, "loop")

    rwi.clear_events()
    # leg_id="both": the insert-play path — historically never restored the
    # route after finishing (both sides deaf).
    resp = await rwi.media_play(call_id, "file", str(tone), loop=True, leg_id="both")
    assert resp.get("status") == "success", resp
    assert await _wait_event_all(rwi, "media_play_started", timeout=10) is not None

    premature = await rwi.wait_for_event("media_play_finished", timeout=5)
    assert premature is None, "loop=True playback finished prematurely (loop not working)"

    rwi.clear_events()
    assert (await rwi.media_stop(call_id)).get("status") == "success"
    finished = await _wait_event_all(rwi, "media_play_finished", timeout=10)
    assert finished is not None
    assert finished.get("interrupted") is True

    await _assert_rtp_resumes(callee, "callee after both-leg loop stop")

    await rwi.hangup(call_id)
