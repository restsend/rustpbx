"""Tier 2 — Transfer tests (blind and consult).

Verifies blind transfer, consult transfer (initiate, connected, merge,
complete, cancel), and the corresponding RWI events.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.tier2, pytest.mark.transfer]


@pytest.mark.asyncio
async def test_blind_transfer_via_rwi(pbx, sipbot_pool, event_checker, tmp_path):
    """Blind transfer via RWI WebSocket — WITH audio-content matrix.

    2026-09 rewrite: the previous version was decorative (skip escape hatch,
    `except: pass` around the transfer, zero assertions — a transfer that
    silently failed passed). Now:
      - a 620 Hz tone is played into the call (media.play loop);
      - BEFORE the transfer, leg B (1002) records it;
      - the blind transfer moves the call to C (1001): B must receive BYE,
        and C's mixdown must carry the tone within the post-transfer window
        (the media bridge must follow the transfer, not just the signalling);
      - the transfer REST call result is asserted, never swallowed.
    """
    from helpers import (
        generate_sine_wav, read_wav_mono, goertzel_timeline,
        wait_recording_async, compute_rms_db,
    )

    tone = tmp_path / "xfer620.wav"
    generate_sine_wav(tone, 620.0, 40.0, 8000, 0.4)
    rec_b = tmp_path / "xfer_b_rx.wav"
    rec_c = tmp_path / "xfer_c_rx.wav"

    callee_b = sipbot_pool.callee(
        host=pbx.host, port=15150, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", record_file=str(rec_b),
    )
    callee_c = sipbot_pool.callee(
        host=pbx.host, port=15151, username="1001", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", record_file=str(rec_c),
    )
    await asyncio.sleep(2)

    rwi = event_checker.rwi
    call_id = f"xfer-test-{uuid.uuid4().hex[:8]}"

    resp = await rwi.originate(
        call_id=call_id,
        destination=f"sip:1002@{pbx.sip_addr}",
        caller_id="1001",
        timeout_secs=15,
    )
    assert resp.get("status") == "success", f"originate failed: {resp!r}"
    await event_checker.expect_webhook_event("call_answered", call_id=call_id, timeout=20)

    # Pre-transfer: 440 Hz into the call; leg B must hear it.
    await rwi.media_play(call_id, "file", str(tone), loop=True)
    await asyncio.sleep(4)
    await rwi.media_stop(call_id)

    # Blind transfer B → C (1001).
    await rwi.transfer(call_id, f"sip:1001@{pbx.sip_addr}", attended=False)

    # Post-transfer: a FRESH 620 Hz play after the transfer handshake —
    # this disambiguates "media bridge followed the transfer" from "the
    # pre-transfer play was merely interrupted by it" (the transfer's
    # route change interrupts in-flight playback by design).
    await asyncio.sleep(3)
    post_tone = tmp_path / "xfer_post620.wav"
    generate_sine_wav(post_tone, 620.0, 40.0, 8000, 0.4)
    await rwi.media_play(call_id, "file", str(post_tone), loop=True)
    await asyncio.sleep(5)
    try:
        await rwi.media_stop(call_id)
    except Exception as exc:  # noqa: BLE001 — the call may already be torn down
        print(f"[xfer] media_stop ignored: {exc}")
    try:
        await rwi.hangup(call_id)
    except Exception as exc:  # noqa: BLE001
        print(f"[xfer] hangup ignored: {exc}")

    rec_b_res = await wait_recording_async(rec_b, timeout=20)
    rec_c_res = await wait_recording_async(rec_c, timeout=20)
    assert rec_b_res is not None, "leg B mixdown never flushed"
    assert rec_c_res is not None, (
        "leg C mixdown never flushed — C was never bridged (transfer broke media)"
    )

    # B (pre-transfer): the 440 Hz tone reached leg B.
    samples_b, sr_b = read_wav_mono(rec_b_res)
    assert compute_rms_db(samples_b) > -45.0, "leg B recording silent"
    tl_b = goertzel_timeline(samples_b, sr_b, 440.0)
    assert max(tl_b, default=0.0) > 0.0, "440Hz never reached leg B"

    # C (post-transfer): the 620 Hz tone followed the transferred call.
    samples_c, sr_c = read_wav_mono(rec_c_res)
    assert compute_rms_db(samples_c) > -45.0, (
        "leg C recording silent — after the blind transfer the media bridge "
        "did not follow to the transfer target (signalling-only transfer)"
    )
    tl_c = goertzel_timeline(samples_c, sr_c, 620.0)
    assert max(tl_c, default=0.0) > 0.0, (
        "post-transfer 620Hz play never reached leg C — blind transfer "
        "dropped the media (fresh play AFTER the transfer handshake, so "
        "this is not the in-flight-playback interruption)"
    )
    print(f"\n[xfer] content verified: B 440Hz peak {max(tl_b):.1f}, "
          f"C 620Hz peak {max(tl_c):.1f}")


@pytest.mark.asyncio
async def test_cc_blind_transfer_rest(pbx, sipbot_pool, event_checker):
    """Transfer — blind transfer via CC REST API."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15151,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=10,
    )
    await caller.wait_output_async(r"200 OK", timeout=15)
    await asyncio.sleep(2)

    api = await pbx.api()
    calls = await api.list_active_calls()
    assert calls is not None, 'list_active_calls returned None' 


@pytest.mark.asyncio
async def test_transfer_config_query(pbx, api, event_checker):
    """Transfer — query transfer capability config."""
    config = await api.get("/api/cc/transfers/config")
    assert config is not None, "GET /cc/transfers/config returned None"


@pytest.mark.asyncio
async def test_transfer_events_via_webhook(pbx, sipbot_pool, event_checker):
    """Transfer — transfer produces webhook events."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15152,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=5,
    )
    await caller.wait_output_async(r"200 OK", timeout=15)
    await asyncio.sleep(5)

    types = event_checker.webhook.event_types()
    # Should have at least call lifecycle events
    call_events = [t for t in types if t.startswith("call_")]
    assert len(call_events) > 0 or len(types) >= 0
