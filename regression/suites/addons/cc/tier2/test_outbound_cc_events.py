"""Tier 2 — Agent outbound (click-to-call) cc_* webhook events.

Covers the CTI originate path (``POST /api/cc/calls/originate``): an
agent-attributed outbound call must emit the full ``call_ringing`` →
``call_answered`` → ``call_hangup`` chain through the RWI webhook, mirroring
the inbound queue-dispatch flow. ``call_ringing`` carries ``early_media`` so
consumers can distinguish a plain 180 ringback from 183/SDP early media.
"""

from __future__ import annotations

import asyncio

import pytest

pytestmark = [pytest.mark.tier2, pytest.mark.webhook]


@pytest.mark.asyncio
async def test_cti_originate_emits_cc_event_chain(pbx, sipbot_pool, api, event_checker):
    """CTI click-to-call — agent 1001 originates to a customer endpoint.

    The originate dials its first leg direct-to-callee, so the destination
    points at the customer sipbot's own socket (host:port). The agent is the
    caller (1001); the 180/200 the customer leg produces must surface as
    call_ringing / call_answered attributed to agent 1001.

    The customer endpoint uses a NON-agent username: when both parties are
    registered agents (agent-to-agent assist), attribution deliberately goes
    to the served callee instead (see rwi/processor.rs originate metadata).
    """
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15901,
        username="customer01",
        password="123456",
        # No registration needed (and none possible — customer01 is not a
        # PBX user): the originate dials the endpoint's socket directly.
        register=False,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
        hangup_after=60,
    )
    # Without REGISTER there is no ready line — give the socket a moment.
    await asyncio.sleep(1)

    resp = await api.originate(
        {
            "agent_id": "1001",
            "target": f"sip:customer01@{pbx.host}:15901",
            "timeout": 20,
        }
    )
    assert resp and resp.get("call_id"), f"originate failed: {resp!r}"
    call_id = resp["call_id"]

    # 1) call_ringing — customer leg 180 Ringing, attributed to agent 1001,
    #    carrying the early_media discriminator.
    ring = await event_checker.expect_webhook_event(
        "call_ringing", timeout=30, call_id=call_id
    )
    assert ring.payload.get("agent_id") == "1001", (
        f"call_ringing must attribute the originating agent, got: {ring.payload!r:.200}"
    )
    assert "early_media" in ring.payload, (
        f"call_ringing must carry the early_media flag, got: {ring.payload!r:.200}"
    )

    # 2) call_answered — customer answers (200 OK).
    answered = await event_checker.expect_webhook_event(
        "call_answered", timeout=30, call_id=call_id
    )
    assert answered.payload.get("agent_id") == "1001", (
        f"call_answered must attribute the originating agent, got: {answered.payload!r:.200}"
    )

    # 3) call_hangup — teardown still emits the (pre-existing) hangup event.
    await event_checker.rwi.hangup(call_id)
    ended = await event_checker.expect_webhook_event(
        "call_hangup", timeout=30, call_id=call_id
    )
    assert ended.payload.get("agent_id") == "1001"
