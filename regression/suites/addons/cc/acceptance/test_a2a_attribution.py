"""Acceptance — A2A (agent-to-agent) call attribution.

When one registered agent calls another, the call must be attributed to the
CALLEE (the agent who answers), never to the caller: the dialing session
resolves the destination agent and plants ``resolved_agent_id`` (see
SipSession resolve_custom_targets), so call_answered / call_hangup and the
cc_calls CDR row must carry agent_id == callee.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.acceptance, pytest.mark.queue]


@pytest.mark.asyncio
async def test_a2a_originate_attributes_callee(pbx, sipbot_pool, api, event_checker):
    """RWI originate agent 1001 → agent 1002: attribution == callee (1002)."""
    callee = sipbot_pool.callee(
        host=pbx.host, port=15536, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=25,
    )
    await asyncio.sleep(2)

    call_id = f"a2a-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id,
        caller_id="1001",
        destination=f"sip:1002@{pbx.sip_addr}",
        timeout_secs=15,
    )

    # The answering agent is 1002 (callee) — attribution must never name the
    # caller (1001).
    await event_checker.expect_webhook_payload(
        "call_answered", {"payload.agent_id": "1002"},
        call_id=call_id, timeout=20)

    await event_checker.rwi.hangup(call_id)
    hangup = await event_checker.expect_webhook_payload(
        "call_hangup", {"payload.agent_id": "1002"},
        call_id=call_id, timeout=20)
    assert hangup.payload.get("agent_id") == "1002"

    # No cc lifecycle event may attribute this call to the caller.
    offenders = [
        e for e in event_checker.webhook.events_for_call(call_id)
        if (e.event_type or "").startswith("cc_")
        and (e.payload or {}).get("agent_id") == "1001"
    ]
    assert not offenders, (
        f"call attributed to CALLER 1001 (must be callee 1002): "
        f"{[(e.event_type, e.payload) for e in offenders][:3]}"
    )

    # CDR row must attribute the call to 1002.
    detail = await api.get(f"/api/cc/calls/{call_id}")
    assert detail is not None, f"GET /cc/calls/{call_id} returned None"
    cdr = detail.get("data", detail) if isinstance(detail, dict) else {}
    agent_attr = cdr.get("agent_id") or cdr.get("agentId")
    if agent_attr:
        assert str(agent_attr) == "1002", (
            f"CDR attributed to {agent_attr!r}, want callee 1002. CDR: {cdr!r:.200}"
        )
