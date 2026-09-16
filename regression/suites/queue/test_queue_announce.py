"""Queue comfort-prompt E2E — interval-based reassurance while waiting.

The existing queue tests cover hold music, the transfer prompt and the
service prompt at the frequency level. This file adds the missing piece:
``voice_prompts.comfort_prompts`` — a distinct tone played at an interval to
a WAITING caller, interleaved with the hold music.

Frequencies are chosen to be separable in the 200–900 Hz analysis band:
  hold music = 300 Hz (looped)   comfort prompt = 440 Hz (every 3s)

The caller's recording during the wait must contain mostly-300 Hz windows
AND at least one ~440 Hz window — proving the comfort prompt actually played.
(Position announcement is runtime-gated (QueueConfig::announce_position) and
cannot be enabled via TOML — see docs/e2e_coverage_checklist.md 5.7.)
"""

from __future__ import annotations

import asyncio

import pytest

import helpers as h
from helpers import (
    extract_audio_region,
    find_dominant_frequency,
    generate_sine_wav,
    has_audio_content,
    read_wav_mono,
)

pytestmark = [pytest.mark.queue]

# Comfort prompts play CONCURRENTLY with the restarted hold-music loop (see
# queue.rs on_audio_complete: maybe_play_comfort_or_ewt then start_hold_music),
# so the two tones must be separable even when mixed: distant frequencies and
# a louder comfort tone make the comfort prompt dominate during overlap.
HOLD_HZ = 300.0
COMFORT_HZ = 800.0


@pytest.mark.asyncio
@pytest.mark.xfail(
    reason="comfort prompts are scheduled but never audible: queue.rs "
           "on_audio_complete plays the comfort prompt and immediately "
           "restarts the hold loop concurrently; the media layer delivers "
           "neither — Goertzel analysis of the caller recording shows only "
           "hold-music 300 Hz for ~4.5s and then DIGITAL SILENCE (the hold "
           "loop dies too), with zero 800 Hz comfort energy. Logs claim "
           "'playing comfort prompt', so the bug is below the app layer. "
           "Turns XPASS when fixed.",
    strict=True,
)
async def test_queue_comfort_prompt_plays_while_waiting(
    pbx, pbx_config, sipbot_pool, tmp_path
):
    agent_port = h.ua_port(15518)
    hold = tmp_path / "hold_300.wav"
    comfort = tmp_path / "comfort_800.wav"
    generate_sine_wav(hold, HOLD_HZ, 2.0, 8000, 0.3)
    generate_sine_wav(comfort, COMFORT_HZ, 1.5, 8000, 0.9)

    pbx_config.add_queue(
        "comfort",
        strategy_mode="sequential",
        targets=[f"sip:1002@{pbx.sip_addr}"],
        accept_immediately=True,
        hold_audio=str(hold),
        loop_playback=True,
        voice_prompts={
            "comfort_prompts": [
                {"audio_file": str(comfort), "interval_secs": 3},
            ],
        },
    )
    pbx_config.add_route(
        "to-comfort", match={"to.user": "comfort"}, priority=10,
        action="queue", queue="comfort",
    )
    h.boot_pbx(pbx)

    # The agent rings 8s before answering → a real waiting phase.
    agent = sipbot_pool.callee(
        host=pbx.host, port=agent_port, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=8, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(agent)

    rec = tmp_path / "caller.wav"
    caller = sipbot_pool.caller(
        target=f"sip:comfort@{pbx.sip_addr}", username="1001", password="123456",
        hangup=20, record_file=str(rec),
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), (
        caller.output
    )
    await h.wait_rtp(caller, "queued caller", 20)
    # Let the wait + connect + a few comfort intervals elapse.
    deadline = asyncio.get_event_loop().time() + 25
    while asyncio.get_event_loop().time() < deadline:
        if not caller.is_alive:
            break
        await asyncio.sleep(0.5)

    assert rec.exists() and rec.stat().st_size > 44, "no caller recording"
    samples, sr = read_wav_mono(rec)
    assert samples.size > sr * 4, f"recording too short: {samples.size / sr:.1f}s"

    # Scan the waiting phase in 0.3s windows (0.15s hop): classify each
    # window by its dominant frequency.
    win = int(sr * 0.3)
    hop = win // 2
    hold_windows = comfort_windows = total = 0
    end = min(int(sr * 8.0), samples.size - win)  # agent answers ~8s
    for off in range(int(sr * 0.5), end, hop):
        region = samples[off : off + win]
        if not has_audio_content(region, -42.0):
            continue
        total += 1
        freq, _ = find_dominant_frequency(region, sr, 200, 900, 5)
        if abs(freq - HOLD_HZ) < 45:
            hold_windows += 1
        elif abs(freq - COMFORT_HZ) < 45:
            comfort_windows += 1

    assert total >= 8, (
        f"too few audible windows in the waiting phase (total={total})"
    )
    assert hold_windows >= total // 3, (
        f"hold music ({HOLD_HZ:.0f}Hz) not dominant while waiting: "
        f"hold={hold_windows} comfort={comfort_windows} total={total}"
    )
    assert comfort_windows >= 1, (
        f"no comfort-prompt ({COMFORT_HZ:.0f}Hz) window detected while waiting: "
        f"hold={hold_windows} comfort={comfort_windows} total={total}"
    )
