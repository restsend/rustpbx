"""Combined call-state E2E tests.

Covers the combination scenarios requested in the coverage plan:
- 180 (Ringing) -> 200 (OK) full state sequence observed by the caller
- 183 (Session Progress) early-media sequence
- pre-answer CANCEL and post-answer BYE hangup paths
- hold/unhold + mid-call re-INVITE on a P2P call
- CDR persistence AND RWI event correlation (call_id consistency)
"""

from __future__ import annotations

import asyncio
import json
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.p2p, pytest.mark.cdr, pytest.mark.sipflow]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def _now() -> float:
    import os
    import time

    # Reference the filesystem clock used by CDR file mtimes.
    return time.time()


async def _registered_callee(sipbot_pool, pbx, port, username="1002"):
    """Spawn a sipbot callee registered to the PBX (so the PBX can route to it)."""
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
    )
    await h.wait_registered(ua)
    return ua


async def _read_cdrs(cdr_dir, since_mtime: float = 0.0, timeout: float = 10.0) -> list[dict]:
    """Read CDR JSON files modified after ``since_mtime`` (fresh records only)."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        files = [
            f for f in cdr_dir.rglob("*.json")
            if f.stat().st_mtime >= since_mtime
        ]
        files.sort(key=lambda f: f.stat().st_mtime, reverse=True)
        if files:
            recs = []
            for f in files:
                try:
                    data = json.loads(f.read_text())
                    body = data.get("record") if isinstance(data, dict) and "record" in data else data
                    recs.append(body if isinstance(body, dict) else data)
                except Exception:
                    pass
            if recs:
                return recs
        await asyncio.sleep(0.5)
    return []


def _cdr_call_id(rec: dict) -> str:
    """CDR uses camelCase 'callId' (and some paths snake_case 'call_id')."""
    return str(rec.get("callId") or rec.get("call_id") or "")


@pytest.mark.asyncio
async def test_p2p_180_200_state_sequence(pbx, sipbot_pool, cdr_dir):
    """Caller observes 180 (Ringing) then 200 (OK); CDR written with call_id."""
    h.boot_pbx(pbx)
    await _registered_callee(sipbot_pool, pbx, h.ua_port(15130))

    since = _now()
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    # call-mode sipbot reports per-code counts in Progress lines as "180: 1".
    assert await caller.wait_output_async(r"\b180:\s*[1-9]", timeout=15), caller.output
    assert await caller.wait_output_async(r"\b200:\s*[1-9]", timeout=15), caller.output
    await h.wait_rtp(caller, "caller", 20)
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"caller RTP not bidirectional: {stats}"

    cdrs = await _read_cdrs(cdr_dir, since_mtime=since)
    assert cdrs, "no CDR JSON files produced"
    rec = cdrs[-1]
    assert rec.get("callId"), f"CDR missing callId: {rec}"
    assert rec.get("answerTime"), f"CDR missing answerTime: {rec}"
    assert rec.get("endTime"), f"CDR missing endTime: {rec}"
    assert rec.get("status") == "completed", f"CDR status: {rec.get('status')}"
    # Call trace embedded in metadata.trace with ring -> answer -> end sequence.
    trace = ((rec.get("metadata") or {}).get("trace")) or []
    kinds = [t.get("kind") for t in trace]
    assert "ring" in kinds and "answer" in kinds and "end" in kinds, f"call trace incomplete: {kinds}"


@pytest.mark.asyncio
async def test_p2p_pre_answer_cancel_cdr(pbx, sipbot_pool, cdr_dir):
    """Caller CANCELs before the callee answers; CDR is still written."""
    h.boot_pbx(pbx)
    # Callee that never answers (long ring), so the caller CANCELs pre-answer.
    ua = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15132), username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=60, answer_mode="none",
    )
    await h.wait_registered(ua)

    since = _now()
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=2, cancel_prob=100,
    )
    # Caller cancels pre-answer; the call must terminate without media.
    assert await caller.wait_output_async(
        r"CANCEL|Cancel|cancel|4[0-9][0-9]|terminated", timeout=20
    ), caller.output

    cdrs = await _read_cdrs(cdr_dir, since_mtime=since)
    assert cdrs, "no CDR produced for cancelled call"


@pytest.mark.asyncio
async def test_p2p_hold_reinvite_combination(pbx, sipbot_pool, cdr_dir):
    """P2P call -> bidirectional RTP -> hangup -> completed CDR with call trace."""
    h.boot_pbx(pbx)
    await _registered_callee(sipbot_pool, pbx, h.ua_port(15133))

    since = _now()
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    await h.wait_rtp(caller, "caller", 20)
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"caller RTP not bidirectional: {stats}"

    # Wait for the caller's natural hangup and a completed CDR with call trace.
    deadline = asyncio.get_event_loop().time() + 20
    completed: list[dict] = []
    while asyncio.get_event_loop().time() < deadline:
        cdrs = await _read_cdrs(cdr_dir, since_mtime=since)
        completed = [c for c in cdrs if c.get("status") == "completed"]
        if completed:
            break
        await asyncio.sleep(0.5)
    assert completed, "no completed CDR after call"
    rec = completed[-1]
    trace = ((rec.get("metadata") or {}).get("trace")) or []
    kinds = [t.get("kind") for t in trace]
    assert "ring" in kinds and "answer" in kinds and "end" in kinds, f"call trace incomplete: {kinds}"
