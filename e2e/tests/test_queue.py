"""Queue E2E tests: queue app + hold music + sequential/parallel routing.

Builds a queue route + queue config pointing at a sipbot callee agent, then
verifies a caller reaching the queue is held on hold-music and connected.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.queue]


@pytest.mark.asyncio
async def test_queue_sequential_agent_answers(pbx, sipbot_pool):
    """Caller -> queue(support, sequential, target sipbot agent) -> agent answers."""
    pbx.config_builder.add_queue(
        "support",
        strategy_mode="sequential",
        targets=[f"sip:1002@127.0.0.1:{h.ua_port(15110)}"],
        accept_immediately=True,
        wait_timeout_secs=15,
    )
    pbx.config_builder.add_route(
        "to-support",
        match={"to.user": "support"},
        priority=10,
        action="queue",
        queue="support",
    )
    h.boot_pbx(pbx)

    agent = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15110), username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(agent)
    caller = sipbot_pool.caller(
        target=f"sip:support@{pbx.sip_addr}", username="1001", password="123456", hangup=8,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    # Assert the *agent* receives RTP from the caller — proves the caller↔agent
    # media bridge is active. Checking only the caller's `has_rx or has_tx`
    # would pass on hold-music RTP alone even if the bridge never activated.
    await h.wait_rtp_rx(agent, "agent", 20)
    await h.wait_rtp(caller, "caller", 10)
    stats = caller.get_rtp_stats()
    assert stats.has_rx or stats.has_tx, f"no RTP: {stats}"


@pytest.mark.asyncio
async def test_queue_hold_music_audio(pbx, sipbot_pool, tmp_path):
    """Queue with hold music: caller receives non-silent audio while waiting."""
    from helpers import generate_sine_wav

    hold = tmp_path / "hold_music.wav"
    generate_sine_wav(hold, 440.0, 2.0, 8000, 0.5)

    pbx.config_builder.add_queue(
        "support",
        strategy_mode="sequential",
        targets=[f"sip:nobody@127.0.0.1:{h.ua_port(15111)}"],  # bogus target -> stays queued
        accept_immediately=True,
        hold_audio=str(hold),
        loop_playback=True,
        wait_timeout_secs=20,
    )
    pbx.config_builder.add_route(
        "to-support",
        match={"to.user": "support"},
        priority=10,
        action="queue",
        queue="support",
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:support@{pbx.sip_addr}", username="1001", password="123456", hangup=8,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 20)


@pytest.mark.asyncio
async def test_queue_transfer_prompt_before_connect_service_after(pbx, sipbot_pool, tmp_path):
    """Caller hears the transfer prompt while the agent rings (pre-connect),
    then the caller-only service prompt after the agent answers."""
    from helpers import (
        generate_sine_wav, read_wav_stereo, find_signal_start,
        extract_audio_region, find_dominant_frequency, has_audio_content,
    )

    transfer = tmp_path / "transfer_440.wav"
    service = tmp_path / "service_700.wav"
    hold = tmp_path / "hold_300.wav"
    generate_sine_wav(transfer, 440.0, 2.0, 8000, 0.5)
    generate_sine_wav(service, 700.0, 1.0, 8000, 0.5)
    generate_sine_wav(hold, 300.0, 2.0, 8000, 0.5)

    pbx.config_builder.add_queue(
        "support",
        strategy_mode="sequential",
        targets=[f"sip:1002@127.0.0.1:{h.ua_port(15113)}"],
        accept_immediately=True,
        hold_audio=str(hold),
        loop_playback=True,
        wait_timeout_secs=20,
        voice_prompts={
            "transfer_prompt": str(transfer),
            "service_prompt": str(service),
        },
    )
    pbx.config_builder.add_route(
        "to-support",
        match={"to.user": "support"},
        priority=10,
        action="queue",
        queue="support",
    )
    h.boot_pbx(pbx)

    agent = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15113), username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(agent)

    rec = tmp_path / "caller_recording.wav"
    caller = sipbot_pool.caller(
        target=f"sip:support@{pbx.sip_addr}", username="1001", password="123456",
        hangup=8, record_file=str(rec),
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 20)
    await asyncio.sleep(5)
    caller.terminate()
    agent.terminate()

    assert rec.exists() and rec.stat().st_size > 44, f"no caller recording: {caller.output[-800:]}"
    rx, _tx, sr = read_wav_stereo(rec)
    start = find_signal_start(rx, 0.01, sr // 50)
    assert start is not None and start >= 0, "recording is silent"

    # While the agent rings: 440 Hz transfer prompt (NOT the 300 Hz hold).
    region_a = extract_audio_region(rx, sr, start, int(sr * 0.4))
    assert has_audio_content(region_a, -40.0), "transfer prompt region is silent"
    freq_a, _ = find_dominant_frequency(region_a, sr, 150, 900, 5)
    assert abs(freq_a - 440.0) < 80.0, f"pre-connect prompt freq {freq_a} != ~440"

    # After the agent answers (~1s ring): 700 Hz service prompt.
    region_b = extract_audio_region(rx, sr, start + int(sr * 1.4), int(sr * 0.5))
    assert has_audio_content(region_b, -40.0), "service prompt region is silent"
    freq_b, _ = find_dominant_frequency(region_b, sr, 150, 900, 5)
    assert abs(freq_b - 700.0) < 80.0, f"post-connect prompt freq {freq_b} != ~700"


@pytest.mark.asyncio
async def test_queue_transfer_prompt_completes_hold_resumes(pbx, sipbot_pool, tmp_path):
    """No agent answers: the transfer prompt plays once, then hold music resumes."""
    from helpers import (
        generate_sine_wav, read_wav_stereo, find_signal_start,
        extract_audio_region, find_dominant_frequency, has_audio_content,
    )

    transfer = tmp_path / "transfer_440.wav"
    hold = tmp_path / "hold_300.wav"
    generate_sine_wav(transfer, 440.0, 1.0, 8000, 0.5)
    generate_sine_wav(hold, 300.0, 2.0, 8000, 0.5)

    pbx.config_builder.add_queue(
        "support",
        strategy_mode="sequential",
        targets=[f"sip:nobody@127.0.0.1:{h.ua_port(15114)}"],  # bogus target -> never answers
        accept_immediately=True,
        hold_audio=str(hold),
        loop_playback=True,
        wait_timeout_secs=30,
        voice_prompts={"transfer_prompt": str(transfer)},
    )
    pbx.config_builder.add_route(
        "to-support",
        match={"to.user": "support"},
        priority=10,
        action="queue",
        queue="support",
    )
    h.boot_pbx(pbx)

    rec = tmp_path / "caller_recording.wav"
    caller = sipbot_pool.caller(
        target=f"sip:support@{pbx.sip_addr}", username="1001", password="123456",
        hangup=8, record_file=str(rec),
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 20)
    await asyncio.sleep(5)
    caller.terminate()

    assert rec.exists() and rec.stat().st_size > 44, f"no caller recording: {caller.output[-800:]}"
    rx, _tx, sr = read_wav_stereo(rec)
    start = find_signal_start(rx, 0.01, sr // 50)
    assert start is not None and start >= 0, "recording is silent"

    # First: 440 Hz transfer prompt while dialing the (unreachable) agent.
    region_a = extract_audio_region(rx, sr, start, int(sr * 0.4))
    assert has_audio_content(region_a, -40.0), "transfer prompt region is silent"
    freq_a, _ = find_dominant_frequency(region_a, sr, 150, 900, 5)
    assert abs(freq_a - 440.0) < 80.0, f"transfer prompt freq {freq_a} != ~440"

    # Prompt finished without an answer -> 300 Hz hold music resumed.
    region_b = extract_audio_region(rx, sr, start + int(sr * 1.8), int(sr * 0.5))
    assert has_audio_content(region_b, -40.0), "resumed hold region is silent"
    freq_b, _ = find_dominant_frequency(region_b, sr, 150, 900, 5)
    assert abs(freq_b - 300.0) < 80.0, f"resumed hold freq {freq_b} != ~300"


@pytest.mark.asyncio
async def test_queue_rwi_enqueue_dequeue(pbx, sipbot_pool, rwi):
    """RWI queue control surface: enqueue + status + dequeue on an active call."""
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    callee = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15112), username="1002", password="123456",
        register=False, ring_secs=1, answer_mode="echo",
    )
    call_id = f"q-{uuid.uuid4().hex[:8]}"
    resp = await rwi.originate(
        call_id, f"sip:1002@127.0.0.1:{h.ua_port(15112)}", "sip:rwi@pbx", "default",
    )
    assert resp.get("status") == "success", resp
    await rwi.wait_for_event("call_answered", timeout=15)

    for cmd, args in [("queue_enqueue", (call_id, "support")), ("queue_status", ("support",))]:
        out = await getattr(rwi, cmd)(*args)
        assert out is not None, f"{cmd} failed: {out}"
    await rwi.hangup(call_id)
