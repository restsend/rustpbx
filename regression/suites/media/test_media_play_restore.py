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
    file (real ~50 pps audio on the A leg), the callee echoes. The caller
    records its RX audio so tests can assert prompt content, not just packet
    counts. Returns (caller, callee, recording_path)."""
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
    rec = tmp_path / "caller_rx.wav"
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=45, play_file=str(caller_tone), record_file=str(rec),
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, f"call not established: {caller.output[-500:]}"
    await h.wait_rtp_rx(callee, "callee baseline")
    return caller, callee, rec


def _goertzel_timeline(samples, sr, freq, window_s=0.25, step_s=0.25):
    """(t, goertzel magnitude) timeline for one frequency."""
    import numpy as np

    from helpers.audio_verifier import goertzel_magnitude_normalized
    win = int(sr * window_s)
    step = int(sr * step_s)
    out = []
    for start in range(0, max(1, len(samples) - win), step):
        seg = np.asarray(samples[start:start + win], dtype=np.float64)
        out.append((start / sr, goertzel_magnitude_normalized(seg, freq, sr)))
    return out


def _assert_prompt_audible_then_call_restored(
    rec, prompt_hz: float, tone_hz: float = 300.0, prompt_secs: float = PROMPT_SECS
):
    """Audio-content level assertion for the insert-play lifecycle.

    1. PROMPT AUDIBLE: the Goertzel energy at the prompt frequency must rise
       far above its own pre-play baseline — proving media.play actually
       reached a human leg. Packet counts cannot distinguish "prompt heard"
       from "relay kept streaming" (both ~50 pps); even dominant-frequency
       matching fails because the caller's own tone keeps flowing around the
       prompt and the window is a mix.
    2. CALL RESTORED: the tail of the recording (after the prompt ended and
       the route resumed, before hangup) carries the relayed conversation
       tone again — the conversation resumed with real content, not silence
       (the production bug deafened both sides here until hangup).
    """
    import numpy as np

    from helpers.audio_verifier import (
        goertzel_magnitude_normalized,
        read_wav_mono,
    )

    samples, sr = read_wav_mono(rec)
    assert len(samples) > sr * 3, f"recording too short: {len(samples)} samples"

    # 1. PROMPT AUDIBLE — Goertzel energy at the prompt frequency must rise
    #    far above its own pre-play baseline somewhere in the recording.
    #    (Dominant-frequency matching is too strict here: the caller's own
    #    300 Hz source keeps flowing around the prompt, so the prompt window
    #    is a MIX, not a pure tone.)
    tl_prompt = _goertzel_timeline(samples, sr, prompt_hz)
    baseline = sorted(m for t, m in tl_prompt[: min(4, len(tl_prompt))])
    base_med = baseline[len(baseline) // 2] if baseline else 0.0
    peak = max((m for _t, m in tl_prompt), default=0.0)
    peak_t = next((t for t, m in tl_prompt if m == peak), None)
    assert peak >= max(base_med * 10.0, 500.0), (
        f"prompt ({prompt_hz} Hz) never audible on the caller leg — media.play "
        f"did not reach the call (peak Goertzel {peak:.1f} vs baseline "
        f"{base_med:.1f}): {rec}"
    )
    print(
        f"[audio-diag] {rec}: prompt {prompt_hz:.0f}Hz peak={peak:.1f} "
        f"baseline={base_med:.1f} @t={peak_t:.2f}s"
    )

    # 2. CALL RESTORED — the tail of the recording (after EOF + route resume,
    #    before hangup) must carry the relayed conversation tone again, with
    #    the prompt gone: real bidirectional audio, not silence.
    tail_start = max(0, len(samples) - int(sr * 2.0))
    tail = samples[tail_start:]
    m_tail_tone = goertzel_magnitude_normalized(
        np.asarray(tail, dtype=np.float64), tone_hz, sr
    )
    m_tail_prompt = goertzel_magnitude_normalized(
        np.asarray(tail, dtype=np.float64), prompt_hz, sr
    )
    assert m_tail_tone > 500.0 and m_tail_tone > m_tail_prompt, (
        f"post-playback tail does not carry the relayed conversation "
        f"(tone@{tone_hz}Hz={m_tail_tone:.1f}, prompt@{prompt_hz}Hz="
        f"{m_tail_prompt:.1f}) — route not restored with real audio: {rec}"
    )


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


async def _play_both_via_console(api, pbx, prompt, leg_id="both") -> str:
    """Console-API insert-play; returns the session id.

    ``leg_id=None`` mirrors the production insert-play (default target), which
    fans out to the caller leg plus its bridge peer. Waits for the session-
    scoped ``media.play`` INFO logs so the play lifecycle is greppable by call
    id (the bare ``Decoded WAV`` media log carries no session id).
    """
    sid = await _active_session_id(api)
    payload = {"action": "play", "source": {"type": "file", "path": str(prompt)}}
    if leg_id is not None:
        payload["leg_id"] = leg_id
    status, body = await api.raw_request(
        "POST", f"/api/calls/active/{sid}/commands", payload
    )
    assert status == 200, f"console play failed: {status} {body!r}"
    assert await h.wait_log(pbx, r"media\.play command accepted", timeout=10), (
        "PBX never logged 'media.play command accepted'"
    )
    assert await h.wait_log(pbx, r"media\.play started", timeout=10), (
        "PBX never logged 'media.play started'"
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

    caller, callee, rec = await _setup_proxy_call(
        sipbot_pool=sipbot_pool, pbx=pbx, port=h.ua_port(15091), tmp_path=tmp_path
    )

    # Baseline: the echo leg receives the caller's stream through the relay.
    await _assert_media_flowing(callee, "callee baseline")

    sid = await _play_both_via_console(api, pbx, prompt)

    # While the prompt plays, the callee must keep receiving audio (the
    # announcement itself, ~50 pps). A play that silently never reaches the
    # wire (fast-path relay left armed) shows up as a dead RX window here.
    delta_during = await _rtp_rx_delta(callee, window_secs=1.0)
    assert delta_during > 25, (
        f"callee stopped receiving audio during the prompt (RX delta {delta_during}/1s)"
    )

    # Let the prompt finish (EOF + grace tail + margin).
    await asyncio.sleep(PROMPT_SECS + 2.5)

    # The full session-scoped play lifecycle must be greppable by call id —
    # the bare "Decoded WAV" media log carries no session id, which is why the
    # 2026-09-24 production incident could not be traced by grepping.
    assert await h.wait_log(
        pbx, r"media\.play finished; requested media route restore", timeout=10
    ), "PBX never logged the playback-finished ResumeMedia restore"

    # THE anti-fake assertion: relayed media must flow again after EOF.
    # With the bug, RX froze here and both sides stayed deaf until hangup.
    await _assert_media_flowing(callee, "callee after both-leg prompt")

    stats = callee.get_rtp_stats()
    assert stats.is_bidirectional, f"call must stay bidirectional: {stats}"

    # Audio-content level: the 440 Hz prompt was actually heard, and the tail
    # of the call carries the relayed 300 Hz conversation again.
    _assert_prompt_audible_then_call_restored(rec, prompt_hz=440.0)

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

    caller, callee, rec = await _setup_proxy_call(
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
    assert await h.wait_log(pbx, r"media\.play started", timeout=10)
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

    # Audio-content level: looping prompt was heard, tail carries the
    # relayed conversation again after the stop.
    _assert_prompt_audible_then_call_restored(rec, prompt_hz=440.0, prompt_secs=1.0)

    await api.raw_request(
        "POST", f"/api/calls/active/{sid}/commands", {"action": "hangup"}
    )
    caller.terminate()
    callee.terminate()


@pytest.mark.asyncio
async def test_console_play_default_leg_restores_route(
    pbx, sipbot_pool, api, tmp_path
):
    """Regression (2026-09-24 production incident, session h3cehgd8sme1ucv0t8m1).

    A console play WITHOUT ``leg_id`` (defaults to the caller leg plus its
    bridge peer) must detach the fast-path relay so the prompt is actually
    heard, and restore the relayed route once the prompt ends. On the deployed
    build the EOF resume silently no-op'd (the logical bridge pair was never
    populated on the direct-dial path), the relay was torn at EOF with nothing
    re-armed (rx_idrop == all, tx == 0) and both sides stayed deaf until the
    callee hung up 12s later.
    """
    prompt = tmp_path / "prompt_default.wav"
    generate_sine_wav(prompt, 600.0, PROMPT_SECS, 8000, 0.5)

    caller, callee, rec = await _setup_proxy_call(
        sipbot_pool=sipbot_pool, pbx=pbx, port=h.ua_port(15093), tmp_path=tmp_path
    )
    await _assert_media_flowing(callee, "callee baseline")

    sid = await _play_both_via_console(api, pbx, prompt, leg_id=None)

    # The prompt must actually reach the callee (announcement audible).
    delta_during = await _rtp_rx_delta(callee, window_secs=1.0)
    assert delta_during > 25, (
        f"prompt never reached the callee during playback (RX delta {delta_during}/1s)"
    )

    await asyncio.sleep(PROMPT_SECS + 2.5)
    assert await h.wait_log(
        pbx, r"media\.play finished; requested media route restore", timeout=10
    )

    # THE anti-fake assertion: relayed media must flow again after EOF.
    await _assert_media_flowing(callee, "callee after default-leg prompt")

    stats = callee.get_rtp_stats()
    assert stats.is_bidirectional, f"call must stay bidirectional: {stats}"

    # Audio-content level: the 600 Hz prompt was actually heard by the caller
    # leg, and the tail of the call carries the relayed 300 Hz conversation.
    _assert_prompt_audible_then_call_restored(rec, prompt_hz=600.0)

    await api.raw_request(
        "POST", f"/api/calls/active/{sid}/commands", {"action": "hangup"}
    )
    caller.terminate()
    callee.terminate()
