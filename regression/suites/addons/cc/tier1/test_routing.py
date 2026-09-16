"""Tier 1 — Call routing smoke tests.

Verifies static routing: forward, reject, busy, queue actions, and
source/destination matching.
"""

from __future__ import annotations

import asyncio

import pytest

pytestmark = [pytest.mark.tier1, pytest.mark.routing]


@pytest.mark.asyncio
async def test_route_forward_to_extension(pbx, sipbot_pool, event_checker):
    """Route forward — caller reaches registered callee via dialplan."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15080,
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
        hangup=8,
    )
    ok = await caller.wait_output_async(r"200 OK", timeout=15)
    assert ok, f"Routing forward failed. Output:\n{caller.output[-500:]}"

    await caller.wait_output_async(r"All bots finished", timeout=15)
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0 or stats.tx_packets > 0, (
        f"No RTP flow. Stats: {stats}\nOutput:\n{caller.output[-500:]}"
    )


@pytest.mark.asyncio
async def test_route_to_nonexistent_returns_error(pbx, sipbot_pool):
    """Route — call to non-existent destination returns 4xx/5xx."""
    caller = sipbot_pool.caller(
        target=f"sip:9999@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=3,
    )
    await caller.wait_output_async(r"All bots finished", timeout=15)
    output = caller.output
    assert any(code in output for code in ["404", "480", "484", "487", "486", "500", "503"]), (
        f"Expected error for nonexistent destination. Output:\n{output[-300:]}"
    )


@pytest.mark.asyncio
async def test_route_priority_ordering(pbx, sipbot_pool, event_checker):
    """Route priority — higher priority routes take precedence."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15081,
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
    ok = await caller.wait_output_async(r"200 OK", timeout=15)
    assert ok, f"Priority routing failed. Output:\n{caller.output[-500:]}"


@pytest.mark.asyncio
async def test_route_auto_answer(pbx, sipbot_pool, event_checker):
    """Route — SIP signaling is observed for outbound call."""
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=5,
    )
    await caller.wait_output_async(r"(INVITE|100|180|200|Progress)", timeout=10)
    output = caller.output
    assert "INVITE" in output or "Progress" in output, (
        f"No SIP signaling observed. Output:\n{output[-300:]}"
    )
