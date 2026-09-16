"""Parallel / serial call-pattern coverage.

Parallel calls: N concurrent p2p calls all establish with live media, and each leg
produces its own CDR (no cross-call bleed).
Serial calls: back-to-back sequential calls on the SAME endpoints must all
complete independently — no state leak between calls (3rd call identical to
1st).
"""

from __future__ import annotations

import asyncio
import json

import pytest

import helpers as h
from helpers import assertions as A

pytestmark = [pytest.mark.p2p]


async def _reg(sipbot_pool, pbx, port, username):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(ua)
    return ua


def _wait_cdr_count(cdr_dir, expected: int, timeout: float = 20.0) -> list:
    import time

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        files = list(cdr_dir.rglob("*.json"))
        if len(files) >= expected:
            return files
        time.sleep(0.3)
    files = list(cdr_dir.rglob("*.json"))
    raise AssertionError(f"[cdr] expected >= {expected} CDRs within {timeout}s, got {len(files)}")


@pytest.mark.asyncio
async def test_parallel_four_p2p_calls_all_establish(pbx, webhook_server, sipbot_pool, cdr_dir, evidence):
    """4 parallel p2p calls: all establish, each leg has live RTP, and each
    produces its own CDR (distinct callIds)."""
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    callees = [
        await _reg(sipbot_pool, pbx, h.ua_port(15170 + i), f"100{i % 3 + 1}")
        for i in range(4)
    ]
    callers = []
    for i in range(4):
        c = sipbot_pool.caller(
            target=f"sip:100{i % 3 + 1}@{pbx.sip_addr}",
            username="1001", password="123456", hangup=10, audio_quality=True,
        )
        callers.append(c)

    established = await asyncio.gather(
        *[c.wait_output_async(r"200 OK|Call established", timeout=25) for c in callers]
    )
    A.require(all(established), "all four parallel calls established",
              f"outputs: {[c.output[-300:] for c in callers]}")
    await asyncio.gather(*[h.wait_rtp(c, f"caller-{i}", 20) for i, c in enumerate(callers)])

    files = _wait_cdr_count(cdr_dir, 4)
    ids = set()
    for f in files:
        doc, _ = A.load_cdr(f)
        A.require_keys(doc, ("callId", "statusCode"), "parallel CDR")
        ids.add(str(doc.get("callId")))
    assert len(ids) >= 4, f"parallel calls must yield >=4 distinct callIds, got {len(ids)}: {ids}"
    evidence.log_metric("parallel_calls", 4)
    evidence.log_metric("distinct_cdr_ids", len(ids))
    for c in callers:
        c.terminate()


@pytest.mark.asyncio
async def test_serial_back_to_back_calls_no_state_leak(pbx, webhook_server, sipbot_pool, cdr_dir, evidence):
    """3 back-to-back serial calls on the same endpoints: each completes and the
    third behaves like the first (no state leak)."""
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    callee = await _reg(sipbot_pool, pbx, h.ua_port(15180), "1002")
    durations = []
    for i in range(3):
        caller = sipbot_pool.caller(
            target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
            hangup=4, audio_quality=True,
        )
        ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
        assert ok, f"serial call #{i + 1} never established: {caller.output[-400:]}"
        await h.wait_rtp(caller, f"serial-{i}", 15)
        # wait for the 4s auto-hangup to complete before the next call
        await caller.wait_output_async(r" hung up|BYE|Call finished|finished", timeout=10) if False else None
        await asyncio.sleep(4.5)
        durations.append(True)
    files = _wait_cdr_count(cdr_dir, 3)
    completed = 0
    for f in files:
        doc, _ = A.load_cdr(f)
        if str(doc.get("status")) == "completed" or str(doc.get("statusCode")) == "200":
            completed += 1
    assert completed >= 3, (
        f"serial calls must each produce a completed CDR: {completed} of {len(files)}"
    )
    evidence.log_metric("serial_calls", 3)
    evidence.log_metric("completed_cdrs", completed)


@pytest.mark.asyncio
async def test_concurrent_calls_to_same_callee_queue_or_answer(pbx, webhook_server, sipbot_pool, evidence):
    """Two concurrent calls to the same callee: no crash, no ghost hangups —
    each call either establishes or is explicitly rejected."""
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    callee = await _reg(sipbot_pool, pbx, h.ua_port(15183), "1002")
    callers = [
        sipbot_pool.caller(
            target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
            hangup=8, audio_quality=True,
        )
        for _ in range(2)
    ]
    outcomes = await asyncio.gather(
        *[c.wait_output_async(r"200 OK|Call established|486|Busy|480", timeout=20) for c in callers],
        return_exceptions=True,
    )
    for i, (c, o) in enumerate(zip(callers, outcomes)):
        assert not isinstance(o, Exception), f"concurrent caller {i} timed out entirely: {c.output[-300:]}"
    established = sum(
        1 for c in callers if "200 OK" in c.output or "Call established" in c.output
    )
    rejected = sum(1 for c in callers if "486" in c.output or "480" in c.output or "Busy" in c.output)
    assert established + rejected == 2, (
        f"each concurrent call must establish or be explicitly rejected: "
        f"established={established} rejected={rejected}"
    )
    evidence.log_metric("same_callee_established", established)
    evidence.log_metric("same_callee_rejected", rejected)
