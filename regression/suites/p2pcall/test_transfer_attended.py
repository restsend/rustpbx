"""Attended (consult) transfer E2E over real SIP via RWI.

Flow (see src/rwi/transfer.rs):
  1. A (sipbot caller) calls B (registered echo callee); session talking.
  2. `call.transfer.attended {call_id: <session>, target: C}` → the caller leg
     (A) is held, a consultation id is returned (RWI does NOT dial C).
  3. Client originates the consult call to C (echo callee).
  4. `call.transfer.complete` → A is bridged to C, B leg released,
     `call_transferred` fires (observed on the webhook bus — transfer events
     are Owner-dispatched, and A's session has no RWI owner).

The inbound session's call_id is discovered via `session.list_calls`.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.p2p]


def _cid(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _reg(sipbot_pool, pbx, port: int, username: str, **kw):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", **kw,
    )
    await h.wait_registered(ua)
    return ua


async def _find_session_call(rwi, caller: str, callee: str,
                             timeout: float = 15) -> str:
    """Locate the live session created by caller→callee via list_calls."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        resp = await rwi.list_calls()
        data = (resp.get("data") or [])
        if isinstance(data, dict):
            data = data.get("calls") or []
        for entry in data:
            blob = str(entry)
            if caller in blob and callee in blob:
                cid = entry.get("call_id") or entry.get("session_id") or entry.get("id") or ""
                if cid:
                    return cid
        await asyncio.sleep(0.3)
    raise AssertionError(f"no live call {caller}→{callee} in list_calls: {resp}")



async def _wait_caller_audio(ua, label: str, min_frames: int = 50, timeout: float = 15):
    """Call-mode UAs only print packet counters in the final summary, so use
    the periodic AudioQuality frames as the mid-call media signal."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        aq = ua.get_audio_quality()
        if aq and aq.get("total_frames", 0) >= min_frames:
            return
        await asyncio.sleep(0.5)
    raise AssertionError(f"{label}: no audio frames: {ua.get_audio_quality()}")


@pytest.mark.asyncio
async def test_attended_transfer_bridges_a_to_c(pbx, sipbot_pool, rwi,
                                                webhook_server, webhook_session):
    """A↔B talking → attended transfer to C → A ends up bridged with C and B
    is released; the transferred event fires and A keeps bidirectional RTP."""
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    await h.connect_rwi(rwi)

    await _reg(sipbot_pool, pbx, h.ua_port(15502), "1002")
    await _reg(sipbot_pool, pbx, h.ua_port(15503), "1003", audio_quality=True)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=25, audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), (
        caller.output
    )
    await _wait_caller_audio(caller, "A before transfer")

    session_id = await _find_session_call(rwi, "1001", "1002")

    # Phase 1: attended transfer holds A and returns a consultation id.
    att = await rwi.transfer_attended(session_id, f"sip:1003@{pbx.sip_addr}")
    assert att.get("status") == "success", att
    consult_id = (att.get("data") or {}).get("consultation_call_id") or ""
    assert consult_id, f"no consultation_call_id in response: {att}"

    # Phase 2: dial C ourselves (the RWI layer does not originate).
    consult_call = _cid("consult")
    r2 = await rwi.originate(
        consult_call, f"sip:1003@{pbx.sip_addr}", "sip:consult@pbx", "default"
    )
    assert r2.get("status") == "success", r2
    await asyncio.sleep(1.5)

    # Phase 3: complete — A bridges to C.
    comp = await rwi.transfer_complete(session_id, consult_id)
    assert comp.get("status") == "success", comp

    transferred = await webhook_session.wait_for_event("call_transferred", timeout=15)
    assert transferred, (
        f"no call_transferred webhook event: {webhook_session.event_types()[-20:]}"
    )

    # A keeps talking (now to C): RTP still flows from A's view once the call
    # ends (call-mode stats print in the final summary).
    deadline = asyncio.get_event_loop().time() + 30
    while asyncio.get_event_loop().time() < deadline:
        if not caller.is_alive:
            break
        await asyncio.sleep(0.5)
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, (
        f"A's media did not flow across the transfer: {stats}"
    )
