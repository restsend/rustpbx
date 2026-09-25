"""Tier 2 — Advanced queue tests.

Verifies parallel ringing, hold music, queue fallback, CSAT config,
and queue event prompts.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h
from helpers import (
    compute_rms_db,
    find_dominant_frequency,
    find_signal_start,
    has_audio_content,
    read_wav_mono,
)

pytestmark = [pytest.mark.tier2, pytest.mark.queue]


async def _bind_call_id(event_checker, destination: str, mark: int, timeout: float = 10.0):
    """Bind the server-assigned call_id of a call to `destination` from the
    webhook call_created events that arrive after `mark`."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        for ev in event_checker.webhook.all_events()[mark:]:
            if ev.event_type != "call_created":
                continue
            payload = ev.payload if isinstance(ev.payload, dict) else {}
            if payload.get("callee") == destination and ev.call_id:
                return ev.call_id
        await asyncio.sleep(0.2)
    return None


@pytest.mark.asyncio
async def test_queue_parallel_ringing(pbx, sipbot_pool, event_checker):
    """Queue parallel — two registered agents; a call to the first is
    answered and produces a bound call_answered webhook."""
    sipbot_pool.terminate_user("1001")
    sipbot_pool.terminate_user("1002")
    agent1 = sipbot_pool.callee(
        host=pbx.host,
        port=15170,
        username="1001",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    agent2 = sipbot_pool.callee(
        host=pbx.host,
        port=15171,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    destination = f"sip:1001@{pbx.sip_addr}"
    mark = len(event_checker.webhook.all_events())
    caller = sipbot_pool.caller(
        target=destination,
        username="1001",
        password="123456",
        hangup=5,
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, f"Call not answered. Output:\n{caller.output[-500:]}"

    call_id = await _bind_call_id(event_checker, destination, mark)
    assert call_id, "no matching call_created for the answered call"
    await event_checker.expect_webhook_event(
        "call_answered", call_id=call_id, timeout=15)


@pytest.mark.asyncio
async def test_queue_hold_music(pbx, sipbot_pool, event_checker, tmp_path):
    """Queue hold — the caller is answered and audio actually flows while
    the agent leg is on the call.

    Content gate: the caller plays a 620 Hz tone and records its mixdown;
    after the call the recording must be dominated by that tone (the
    agent's echo proves the round trip caller→agent→caller actually
    carried audio — `is_bidirectional` alone cannot: it counts packets,
    and CNG/noise also counts).
    """
    from helpers import (
        compute_rms_db,
        find_dominant_frequency,
        find_signal_start,
        has_audio_content,
        read_wav_mono,
    )

    tone = tmp_path / "qhold_tone620.wav"
    record = tmp_path / "qhold_caller_rx.wav"
    h.generate_sine_wav(tone, 620.0, 12.0, 8000, 0.4)

    sipbot_pool.terminate_user("1001")
    agent = sipbot_pool.callee(
        host=pbx.host,
        port=15172,
        username="1001",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1001@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=5,
        play_file=str(tone),
        record_file=str(record),
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, f"Call not answered. Output:\n{caller.output[-500:]}"

    # Let the call run to the bot's own hangup so the final RTP counters are
    # in the status line before we read them.
    await caller.wait_output_async(r"All bots finished", timeout=20)
    # sipbot↔sipbot answered call: demand bidirectional RTP — the or-combined
    # assertion passed even when one direction was dead (one-way deaf call).
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, (
        f"Answered call must have bidirectional RTP. Stats: {stats}"
    )

    # Content gate: the caller's mixdown recording must carry the 620 Hz
    # tone end-to-end (caller → agent echo → back). The caller bot exits on
    # its own hangup, which flushes the recording.
    deadline = asyncio.get_event_loop().time() + 15
    resolved = None
    while asyncio.get_event_loop().time() < deadline:
        hits = sorted(record.parent.glob(record.stem + "*.wav"))
        if hits:
            resolved = hits[-1]
            break
        await asyncio.sleep(0.5)
    assert resolved, (
        f"caller mixdown recording never flushed: {record} — audio never "
        "flowed on the queue call"
    )
    samples, sr = read_wav_mono(resolved)
    assert has_audio_content(samples, -40.0), (
        "caller recording silent — queue call carried no audio content"
    )
    start = find_signal_start(samples)
    region = samples[start:min(start + 3 * sr, samples.size)]
    assert region.size >= sr // 2, "not enough non-silent audio"
    rms = compute_rms_db(region)
    assert rms >= -40.0, f"caller recording too quiet ({rms:.1f}dB)"
    dom, _mag = find_dominant_frequency(region, sr, low=200, high=900, step=5)
    assert abs(dom - 620.0) <= 15, (
        f"caller recording dominant {dom:.0f}Hz, want the played 620Hz "
        f"(±15) — queue call audio corrupted or one-way"
    )
    print(f"\n[queue-hold] round-trip audio ok: 620Hz dominant, rms={rms:.1f}dB")


@pytest.mark.asyncio
async def test_queue_fallback_hangup(pbx, sipbot_pool, event_checker):
    """Queue fallback — when the agent rejects (486), the caller gets a
    definitive non-answer, never a media-less zombie call."""
    agent = sipbot_pool.callee(
        host=pbx.host,
        port=15174,
        username="1004",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        reject_code=486,
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1004@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=12,
    )
    ok = await caller.wait_output_async(r"486|487|480|603", timeout=20)
    output = caller.output
    assert ok, (
        f"Expected a rejection (486) or request-terminated for a rejecting "
        f"agent. Output:\n{output[-500:]}"
    )
    assert "200 OK" not in output, (
        f"Rejected call must not be answered. Output:\n{output[-500:]}"
    )


@pytest.mark.asyncio
async def test_queue_csat_config(pbx, api, event_checker):
    """Queue CSAT — GET /cc/queues/{id}/csat endpoint is wired + authed."""
    status, body = await api.raw_request("GET", "/api/cc/queues/support/csat")
    assert status not in (401, 503), f"csat endpoint not reachable: {status}"
    assert status in (200, 404, 422), f"Unexpected csat status {status}: {body!r:.80}"


@pytest.mark.asyncio
async def test_queue_accept_immediately(pbx, sipbot_pool, event_checker):
    """Queue accept_immediately — the caller is answered promptly: 200 OK on
    the SIP side and a bound call_answered webhook within 10s."""
    sipbot_pool.terminate_user("1001")
    agent = sipbot_pool.callee(
        host=pbx.host,
        port=15173,
        username="1001",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    destination = f"sip:1001@{pbx.sip_addr}"
    mark = len(event_checker.webhook.all_events())
    caller = sipbot_pool.caller(
        target=destination,
        username="1001",
        password="123456",
        hangup=5,
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=10)
    assert ok, f"Call not answered immediately. Output:\n{caller.output[-500:]}"

    call_id = await _bind_call_id(event_checker, destination, mark)
    assert call_id, "no matching call_created for the answered call"
    await event_checker.expect_webhook_event(
        "call_answered", call_id=call_id, timeout=10)


@pytest.mark.asyncio
async def test_queue_realtime_stats(pbx, api, event_checker):
    """Queue realtime — query real-time queue statistics."""
    stats = await api.get_realtime_stats()
    assert stats is not None, "get_realtime_stats returned None"


@pytest.mark.asyncio
async def test_queue_dashboard(pbx, api, event_checker):
    """Queue dashboard — query dashboard summary."""
    summary = await api.get_dashboard_summary()
    assert summary is not None, "get_dashboard_summary returned None"


@pytest.mark.asyncio
async def test_queue_callback_config(pbx, api, event_checker):
    """Queue callback — queue config query (callback params reachable)."""
    # Verify the queue config endpoint is reachable; callback config params
    # (callback_offer_after_secs / callback_dtmf_key) ride on this endpoint.
    queues = await api.list_queues()
    if queues is None:
        pytest.skip("CC queue REST requires PhoneAuth JWT (401)")
    # The endpoint wraps the list like the other CC REST resources.
    if isinstance(queues, dict):
        queues = queues.get("data") or []
    assert isinstance(queues, list), f"Expected a list of queues, got {type(queues).__name__}: {queues!r:.120}"
    # CSV L230 domain: the configured dialplan queues must be listed with
    # their strategy and dial targets.
    names = [q.get("name") for q in queues]
    assert any(n == "support" for n in names), f"'support' queue missing from {names}"
    support = next(q for q in queues if q.get("name") == "support")
    assert support.get("strategy_mode") == "sequential", support
    assert any(
        "skill-group:support" in (t.get("uri") or "") for t in (support.get("targets") or [])
    ), f"support queue targets missing skill-group dial: {support}"
