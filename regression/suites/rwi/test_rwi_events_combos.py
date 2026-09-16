"""RWI event sequence + CDR correlation E2E tests.

Verifies that a call's full lifecycle emits the expected RWI event sequence
(call_created -> call_ringing -> call_answered -> call_hangup) and that the
CDR's callId matches the RWI-originated call_id (data consistency).
"""

from __future__ import annotations

import asyncio
import json
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.p2p, pytest.mark.cdr]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def _time_now() -> float:
    import time

    return time.time()


async def _reg_callee(sipbot_pool, pbx, port, username="1002"):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo",
    )
    await h.wait_registered(ua)
    return ua


async def _fresh_cdrs(cdr_dir, since):
    recs = []
    for f in cdr_dir.rglob("*.json"):
        if f.stat().st_mtime < since:
            continue
        try:
            data = json.loads(f.read_text())
            body = data.get("record") if isinstance(data, dict) and "record" in data else data
            recs.append(body if isinstance(body, dict) else data)
        except Exception:
            pass
    return recs


@pytest.mark.asyncio
async def test_rwi_event_sequence_and_cdr_correlation(pbx, sipbot_pool, rwi, cdr_dir):
    """RWI-originated call emits full event sequence; CDR callId matches call_id."""
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    await _reg_callee(sipbot_pool, pbx, h.ua_port(15140))

    call_id = _call_id("rwi-seq")
    since = _time_now()
    resp = await rwi.originate(
        call_id, f"sip:1002@{pbx.sip_addr}", "sip:rwi@pbx", "default", timeout_secs=30,
    )
    assert resp.get("status") == "success", resp

    # Full lifecycle event sequence.
    ok = await rwi.wait_for_event_sequence(
        ["call_created", "call_ringing", "call_answered"], timeout=20
    )
    assert ok, f"missing lifecycle events, got: {[e.get('event_type') for e in rwi.events]}"

    # Clean up: hang up the call (RWI drives the BYE), expect call_hangup.
    await rwi.hangup(call_id)
    ok = await rwi.wait_for_event("call_hangup", timeout=15)
    assert ok, f"missing call_hangup event: {[e.get('event_type') for e in rwi.events]}"

    # CDR correlation: a CDR must exist whose callId contains our call_id.
    deadline = asyncio.get_event_loop().time() + 15
    cdrs: list[dict] = []
    while asyncio.get_event_loop().time() < deadline:
        cdrs = await _fresh_cdrs(cdr_dir, since)
        if cdrs:
            break
        await asyncio.sleep(0.5)
    assert cdrs, "no CDR produced"
    matched = [c for c in cdrs if call_id in str(c.get("callId") or c.get("call_id") or "")]
    assert matched, f"no CDR correlated to call_id {call_id}: {cdrs}"


@pytest.mark.asyncio
async def test_rwi_call_answer_then_hangup_events(pbx, sipbot_pool, rwi):
    """Answered call emits call_answered, then hangup emits call_hangup with the call."""
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    await _reg_callee(sipbot_pool, pbx, h.ua_port(15141))

    call_id = _call_id("rwi-ans")
    resp = await rwi.originate(
        call_id, f"sip:1002@{pbx.sip_addr}", "sip:rwi@pbx", "default", timeout_secs=30,
    )
    assert resp.get("status") == "success", resp

    answered = await rwi.wait_for_event("call_answered", timeout=20)
    assert answered, f"call should answer: {[e.get('event_type') for e in rwi.events]}"

    await rwi.hangup(call_id)
    ended = await rwi.wait_for_event("call_hangup", timeout=15)
    assert ended, f"call should hangup: {[e.get('event_type') for e in rwi.events]}"
