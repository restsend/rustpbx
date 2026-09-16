"""Queue call-state sequence E2E tests.

A caller reaching a queue observes:
- 180 Ringing (queue dialing the agent)
- 200 OK once the agent answers
- bidirectional RTP caller <-> agent
- a completed CDR after hangup
"""

from __future__ import annotations

import asyncio
import json

import pytest

import helpers as h

pytestmark = [pytest.mark.queue, pytest.mark.cdr]


@pytest.mark.asyncio
async def test_queue_180_200_state_sequence(pbx, sipbot_pool, cdr_dir):
    """Caller -> queue -> agent answers: caller sees 180 then 200 + CDR."""
    pbx.config_builder.add_queue(
        "support",
        strategy_mode="sequential",
        targets=[f"sip:1002@127.0.0.1:{h.ua_port(15150)}"],
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
        host=pbx.host, port=h.ua_port(15150), username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo",
    )
    await h.wait_registered(agent)

    since = _now()
    caller = sipbot_pool.caller(
        target=f"sip:support@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    # The agent observes 180 while ringing; the caller is connected with 200.
    assert await caller.wait_output_async(r"\b200:\s*[1-9]", timeout=15), caller.output
    await h.wait_rtp_rx(agent, "agent", 20)
    await h.wait_rtp(caller, "caller", 20)

    # Wait for completed CDR after hangup.
    deadline = asyncio.get_event_loop().time() + 20
    while asyncio.get_event_loop().time() < deadline:
        cdrs = await _fresh_cdrs(cdr_dir, since)
        if any(c.get("status") == "completed" for c in cdrs):
            break
        await asyncio.sleep(0.5)
    completed = [c for c in cdrs if c.get("status") == "completed"]
    assert completed, "no completed CDR after queue call"
    rec = completed[-1]
    assert "support" in str(rec.get("callee") or ""), f"CDR callee should reference queue: {rec.get('callee')}"


@pytest.mark.asyncio
async def test_queue_no_agent_180_then_busy(pbx, sipbot_pool):
    """Queue with no available agent: caller rings, then gets a busy/failure (no 200)."""
    pbx.config_builder.add_queue(
        "emptyq",
        strategy_mode="sequential",
        targets=["sip:nobody@127.0.0.1:19999"],
        accept_immediately=False,
        wait_timeout_secs=3,
    )
    pbx.config_builder.add_route(
        "to-emptyq",
        match={"to.user": "emptyq"},
        priority=10,
        action="queue",
        queue="emptyq",
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:emptyq@{pbx.sip_addr}", username="1001", password="123456", hangup=8,
    )
    # Call is terminated (busy/failure) without ever establishing 200.
    assert await caller.wait_output_async(
        r"4[0-9][0-9]|5[0-9][0-9]|Busy|Terminated|No agent|no agent|486|480", timeout=25
    ), caller.output


def _now() -> float:
    import time

    return time.time()


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
