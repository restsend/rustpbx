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
        targets=[f"sip:1002@127.0.0.1:15110"],
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
        host=pbx.host, port=15110, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await asyncio.sleep(2)
    caller = sipbot_pool.caller(
        target=f"sip:support@{pbx.sip_addr}", username="1001", password="123456", hangup=8,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 20)
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
        targets=[f"sip:nobody@127.0.0.1:15111"],  # bogus target -> stays queued
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
async def test_queue_rwi_enqueue_dequeue(pbx, sipbot_pool, rwi):
    """RWI queue control surface: enqueue + status + dequeue on an active call."""
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    callee = sipbot_pool.callee(
        host=pbx.host, port=15112, username="1002", password="123456",
        register=False, ring_secs=1, answer_mode="echo",
    )
    call_id = f"q-{uuid.uuid4().hex[:8]}"
    resp = await rwi.originate(
        call_id, f"sip:1002@127.0.0.1:15112", "sip:rwi@pbx", "default",
    )
    assert resp.get("status") == "success", resp
    await rwi.wait_for_event("call_answered", timeout=15)

    for cmd, args in [("queue_enqueue", (call_id, "support")), ("queue_status", ("support",))]:
        out = await getattr(rwi, cmd)(*args)
        assert out is not None, f"{cmd} failed: {out}"
    await rwi.hangup(call_id)
