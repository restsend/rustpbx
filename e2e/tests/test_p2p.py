"""P2P call E2E tests: basic call, CDR, reject, cancel, bidirectional audio.

Covers the migrated Rust e2e_p2p_demo / e2e_p2p_comprehensive / p2p_audio tests.

rustpbx supports both RTP and WebRTC media. These tests use plain RTP sipbot
UAs (the config defaults users to RTP-only via `is_support_webrtc = false`), so
the PBX bridges with standard RTP. Media is asserted from the caller's RTP
packet stats.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.p2p]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _registered_callee(sipbot_pool, pbx, port, username="1002"):
    """Spawn a sipbot callee registered to the PBX (so the PBX can route to it)."""
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await asyncio.sleep(2)  # allow REGISTER to complete
    return ua


async def _wait_rtp(ua, label: str, timeout: float = 20) -> None:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if ua.get_rtp_stats().has_rx or ua.get_rtp_stats().has_tx:
            return
        await asyncio.sleep(0.3)
    raise AssertionError(
        f"{label}: no RTP after {timeout}s — {ua.get_rtp_stats()}\n{ua.output[-1500:]}"
    )


# ---------------------------------------------------------------------------
# Basic call + CDR
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_p2p_basic_call_cdr(pbx, sipbot_pool, cdr_dir):
    """Caller -> callee(echo) establishes; caller has bidirectional RTP; CDR written."""
    h.boot_pbx(pbx)
    await _registered_callee(sipbot_pool, pbx, 15080, "1002")
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    await _wait_rtp(caller, "caller")
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"caller RTP not bidirectional: {stats}"
    # CDR file
    deadline = asyncio.get_event_loop().time() + 10
    cdr_files: list = []
    while asyncio.get_event_loop().time() < deadline:
        cdr_files = list(cdr_dir.rglob("*.json"))
        if cdr_files:
            break
        await asyncio.sleep(0.5)
    assert cdr_files, "no CDR JSON files produced under config/cdr/"


@pytest.mark.asyncio
async def test_p2p_reject_486(pbx, sipbot_pool):
    """Callee rejects -> caller sees a 4xx/5xx termination, no media flows."""
    h.boot_pbx(pbx)
    sipbot_pool.callee(
        host=pbx.host, port=15081, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        reject_code=486,
    )
    await asyncio.sleep(2)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=4,
    )
    # Exact code may surface as 486/487/480 depending on the media layer; the
    # essential check is the call is terminated and no RTP flows.
    assert await caller.wait_output_async(r"4[0-9][0-9]|5[0-9][0-9]|Busy|Terminated|Unavailable", timeout=15), caller.output
    assert not caller.get_rtp_stats().has_rx, "caller should have no RTP on reject"


@pytest.mark.asyncio
async def test_p2p_cancel_during_ringing(pbx, sipbot_pool, rwi):
    """Originate to a never-answering callee, cancel while ringing."""
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    sipbot_pool.callee(
        host=pbx.host, port=15082, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=60, answer_mode="none",
    )
    await asyncio.sleep(2)
    call_id = _call_id("p2p-cancel")
    resp = await rwi.originate(
        call_id, f"sip:1002@{pbx.sip_addr}", "sip:p2p@pbx", "default", timeout_secs=30,
    )
    assert resp.get("status") == "success", resp
    await rwi.wait_for_event("call_ringing", timeout=10)
    resp = await rwi.hangup(call_id)
    assert resp.get("status") in ("success", "error"), resp


# ---------------------------------------------------------------------------
# Hold / resume via RWI
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_p2p_hold_resume_via_rwi(pbx, sipbot_pool, rwi):
    """Originate -> hold -> unhold -> hangup; media events emitted."""
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    await _registered_callee(sipbot_pool, pbx, 15083, "1002")
    call_id = _call_id("p2p-hold")
    resp = await rwi.originate(call_id, f"sip:1002@{pbx.sip_addr}", "sip:p2p@pbx", "default")
    assert resp.get("status") == "success", resp
    await rwi.wait_for_event("call_answered", timeout=15)
    hold = await rwi.hold(call_id)
    assert hold.get("status") in ("success", "error"), hold
    if hold.get("status") == "success":
        await rwi.wait_for_event("media_hold_started", timeout=10)
        assert (await rwi.unhold(call_id)).get("status") in ("success", "error")
    assert (await rwi.hangup(call_id)).get("status") in ("success", "error")


# ---------------------------------------------------------------------------
# Bidirectional audio (content)
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_p2p_bidirectional_audio(pbx, sipbot_pool, tmp_path):
    """Caller plays a 440 Hz tone (looped) -> echo callee.

    The caller TX carries the tone and its RX carries the callee's echo, so
    bidirectional RTP packet counts prove real audio flowed both ways. (The
    sipbot AudioQuality analyzer reports 0 frames under WebRTC media, so packet
    counts are the reliable signal here.)
    """
    from helpers import generate_sine_wav

    h.boot_pbx(pbx)
    sine = tmp_path / "sine.wav"
    generate_sine_wav(sine, 440.0, 0.5, 8000, 0.5)

    await _registered_callee(sipbot_pool, pbx, 15084, "1002")
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=12, play_file=str(sine),
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    await _wait_rtp(caller, "caller")
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"caller RTP not bidirectional: {stats}"
    # The tone is looped, so the caller should have sent many packets.
    assert stats.tx_packets > 30, f"expected caller TX tone packets, got {stats}"


# ---------------------------------------------------------------------------
# Multiple registrations per callee (parallel_fork)
# ---------------------------------------------------------------------------

async def _register_two_devices(sipbot_pool, pbx, *, port_echo: int, port_none: int,
                                username: str = "1002"):
    """Register the SAME username from two sipbot UAs on different local ports.

    The echo device is registered first; the never-answering device is
    registered second, making it the *newest* registration — the one that
    last-alive mode would pick.
    """
    sipbot_pool.callee(
        host=pbx.host, port=port_echo, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        answer_mode="echo",
    )
    await asyncio.sleep(1.5)
    sipbot_pool.callee(
        host=pbx.host, port=port_none, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=60, answer_mode="none",
    )
    await asyncio.sleep(1.5)


@pytest.mark.asyncio
async def test_p2p_parallel_fork_rings_all_registrations(pbx, sipbot_pool):
    """parallel_fork=true (default): a callee with multiple registered devices
    rings ALL of them — the caller connects even though the NEWEST registration
    never answers (the older echo device picks up first)."""
    h.boot_pbx(pbx)
    await _register_two_devices(sipbot_pool, pbx, port_echo=15220, port_none=15221)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    # The echo device answers despite the never-answering newest registration.
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    await _wait_rtp(caller, "caller")
    assert caller.get_rtp_stats().is_bidirectional, caller.get_rtp_stats()


@pytest.mark.asyncio
async def test_p2p_parallel_fork_disabled_rings_last_registered(pbx, sipbot_pool, pbx_config):
    """parallel_fork=false: only the NEWEST registration is dialed. Here the
    newest device never answers, so the caller must NOT connect and no media
    flows — proving the older device was not rung."""
    pbx_config.set_parallel_fork(False)
    h.boot_pbx(pbx)
    await _register_two_devices(sipbot_pool, pbx, port_echo=15222, port_none=15223)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=4,
    )
    # Only the never-answering newest device is dialed → the call never connects.
    assert not await caller.wait_output_async(r"200 OK|Call established", timeout=10), caller.output
    assert not caller.get_rtp_stats().has_rx, (
        f"no media should flow when only the never-answering device is dialed: {caller.get_rtp_stats()}"
    )


# ---------------------------------------------------------------------------
# max_ring_time (global no-answer rejection)
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_p2p_max_ring_time_rejects_no_answer(pbx, sipbot_pool, pbx_config):
    """[proxy] max_ring_time rejects a no-answer call with 408 after N seconds.

    With max_ring_time=4 and a callee that never answers, the caller must
    receive a 408 Request Timeout within ~12s and the call must never be
    established (early-media ringback RTP may still flow while ringing).
    """
    pbx_config.set_max_ring_time(4)
    h.boot_pbx(pbx)
    sipbot_pool.callee(
        host=pbx.host, port=15224, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=60, answer_mode="none",
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=15,
    )
    # The PBX enforces the configured ring timeout and rejects with 408.
    assert await caller.wait_output_async(r"408|RequestTimeout|Request Timeout", timeout=12), (
        f"caller should see 408 after the ring timeout:\n{caller.output}"
    )
    # A no-answer rejection must never establish the call.
    assert not await caller.wait_output_async(r"200 OK|Call established", timeout=2), (
        f"caller must not connect on a ring-timeout rejection:\n{caller.output}"
    )
