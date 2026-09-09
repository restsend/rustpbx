"""Post-playback media-restore E2E tests (anti-"fake e2e" suite).

Historically the playback e2e tests asserted only RWI events
(``media_play_started`` / ``media_play_finished``) and never verified that
media actually flows again AFTER a prompt ends. That gap let the
"play leg=both never restores the media route" bug (both sides deaf after an
insert-play) ship undetected: the events fired fine while the relay stayed
torn down.

These tests use TWO sipbot legs through the proxy (caller streams a real
tone file at ~50 pps, callee echoes) so RX deltas are a strong bridge-active
signal, and drive playback through the console API — the exact endpoint the
production insert-play uses, which had zero e2e coverage:

  - baseline bidirectional media before the prompt
  - POST /api/calls/active/{id}/commands {action:"play", leg_id:"both"}
  - after the prompt finishes, the callee's RX packet delta must RESUME —
    a torn route freezes RX at the moment of EOF (rx_idrop == all)

media_proxy="all" is required so the MediaBridge is active.
"""

from __future__ import annotations

import asyncio

import pytest

import helpers as h
from helpers import generate_sine_wav

pytestmark = [pytest.mark.media]

PROMPT_SECS = 1.5


async def _setup_proxy_call(pbx, sipbot_pool, port, tmp_path):
    """Two sipbot legs through the proxy: the caller streams a 300 Hz tone
    file (real ~50 pps audio on the A leg), the callee echoes. Returns
    (caller, callee)."""
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)

    callee = sipbot_pool.callee(
        host=pbx.host, port=port, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(callee)

    caller_tone = tmp_path / "caller_tone.wav"
    generate_sine_wav(caller_tone, 300.0, 30.0, 8000, 0.5)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=45, play_file=str(caller_tone),
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, f"call not established: {caller.output[-500:]}"
    await h.wait_rtp_rx(callee, "callee baseline")
    return caller, callee


async def _active_session_id(api) -> str:
    status, body = await api.raw_request("GET", "/api/calls/active")
    assert status == 200, f"list active calls failed: {status} {body!r}"
    entries = body.get("data") if isinstance(body, dict) else body
    assert entries, f"no active calls: {body!r}"
    sid = entries[0].get("meta", {}).get("session_id")
    assert sid, f"no session_id in active call entry: {entries[0]!r}"
    return sid


async def _rtp_rx_delta(ua, window_secs: float = 3.0) -> int:
    """RX packet delta over a time window — the bridge-active signal.

    During a torn route the peer keeps sending (TX keeps rising on our side)
    but nothing is relayed back, so RX freezes. A healthy relay keeps RX
    growing at the packet cadence (~50 pps).
    """
    before = ua.get_rtp_stats().rx_packets
    await asyncio.sleep(window_secs)
    return ua.get_rtp_stats().rx_packets - before


async def _assert_media_flowing(ua, label: str):
    delta = await _rtp_rx_delta(ua, window_secs=3.0)
    assert delta > 50, (
        f"{label}: media is not flowing "
        f"(RX delta {delta} packets / 3s — route torn?): {ua.get_rtp_stats()}"
    )


async def _play_both_via_console(api, pbx, prompt) -> str:
    """Console-API insert-play to both legs; returns the session id."""
    sid = await _active_session_id(api)
    status, body = await api.raw_request(
        "POST",
        f"/api/calls/active/{sid}/commands",
        {
            "action": "play",
            "source": {"type": "file", "path": str(prompt)},
            "leg_id": "both",
        },
    )
    assert status == 200, f"console play(both) failed: {status} {body!r}"
    assert await h.wait_log(pbx, r"Playback started \(both\)", timeout=10), (
        "PBX never logged 'Playback started (both)'"
    )
    return sid


@pytest.mark.asyncio
async def test_console_play_both_restores_bidirectional_media(
    pbx, sipbot_pool, api, tmp_path
):
    """Regression (insert-play bug): console play(leg_id="both") must restore
    the relay route once the prompt ends — historically the "both" path never
    sent ResumeMedia and BOTH sides went deaf after the prompt finished."""
    prompt = tmp_path / "prompt_both.wav"
    generate_sine_wav(prompt, 440.0, PROMPT_SECS, 8000, 0.5)

    caller, callee = await _setup_proxy_call(
        sipbot_pool=sipbot_pool, pbx=pbx, port=h.ua_port(15091), tmp_path=tmp_path
    )

    # Baseline: the echo leg receives the caller's stream through the relay.
    await _assert_media_flowing(callee, "callee baseline")

    sid = await _play_both_via_console(api, pbx, prompt)

    # Let the prompt finish (EOF + grace tail + margin).
    await asyncio.sleep(PROMPT_SECS + 2.5)

    # THE anti-fake assertion: relayed media must flow again after EOF.
    # With the bug, RX froze here and both sides stayed deaf until hangup.
    await _assert_media_flowing(callee, "callee after both-leg prompt")

    stats = callee.get_rtp_stats()
    assert stats.is_bidirectional, f"call must stay bidirectional: {stats}"

    await api.raw_request(
        "POST", f"/api/calls/active/{sid}/commands", {"action": "hangup"}
    )
    caller.terminate()
    callee.terminate()


@pytest.mark.asyncio
async def test_console_play_stop_restores_media_after_loop(
    pbx, sipbot_pool, api, tmp_path
):
    """A looping prompt stopped via the console API must leave a working
    relay — a stopped loop that never restores the route deafens both sides."""
    prompt = tmp_path / "prompt_loop.wav"
    generate_sine_wav(prompt, 440.0, 1.0, 8000, 0.5)

    caller, callee = await _setup_proxy_call(
        sipbot_pool=sipbot_pool, pbx=pbx, port=h.ua_port(15092), tmp_path=tmp_path
    )
    await _assert_media_flowing(callee, "callee baseline")

    sid = await _active_session_id(api)
    status, body = await api.raw_request(
        "POST",
        f"/api/calls/active/{sid}/commands",
        {
            "action": "play",
            "source": {"type": "file", "path": str(prompt)},
            "leg_id": "both",
            "loop_playback": True,
        },
    )
    assert status == 200, f"console play(loop) failed: {status} {body!r}"
    assert await h.wait_log(pbx, r"Playback started \(both\)", timeout=10)
    await asyncio.sleep(2)

    status, body = await api.raw_request(
        "POST",
        f"/api/calls/active/{sid}/commands",
        {"action": "stop_playback", "leg_id": "both"},
    )
    assert status == 200, f"console stop_playback failed: {status} {body!r}"

    await _assert_media_flowing(callee, "callee after loop stop")
    stats = callee.get_rtp_stats()
    assert stats.is_bidirectional, f"call must stay bidirectional: {stats}"

    await api.raw_request(
        "POST", f"/api/calls/active/{sid}/commands", {"action": "hangup"}
    )
    caller.terminate()
    callee.terminate()
