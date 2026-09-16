"""Tier 1 — Agent presence and state transition tests.

Each status transition is verified three ways:
  1. REST ack — update_agent_status must not return None (401/404 surface
     as None via the api helper);
  2. read-back — GET /cc/agents/{id} must report the new status;
  3. event — an agent_state_changed webhook carrying agent_id + to_status.

Note: the console API is authenticated by the session fixture; a None
response is a failure, not a skip.
"""

from __future__ import annotations

import asyncio

import pytest

pytestmark = [pytest.mark.tier1, pytest.mark.presence]


def _readback_status(info) -> str:
    data = (info or {}).get("data") or info or {}
    return (data.get("status") or "").lower()


@pytest.mark.asyncio
async def test_agent_initial_status(pbx, api, event_checker):
    """GET /cc/agents/1001 returns the agent with id + status fields."""
    info = await api.get_agent("1001")
    assert info is not None, "GET agent 1001 returned None (auth/route broken)"
    data = (info or {}).get("data") or info or {}
    assert data.get("agent_id") or data.get("id"), (
        f"agent payload missing agent_id: {info!r:.200}"
    )
    assert "status" in data, f"agent payload missing status: {info!r:.200}"


async def _set_and_verify_status(api, event_checker, status, agent_id="1001"):
    # NOTE: the event_checker fixture clears the webhook buffer before each
    # test, so any matching event in the buffer is THIS transition's — no
    # occurrence arithmetic needed.
    match = {"payload.agent_id": agent_id, "payload.to_status": status}
    resp = await api.update_agent_status(agent_id, status)
    assert resp is not None, (
        f"POST status {status!r} returned None (401/404) — console auth broken"
    )
    info = await api.get_agent(agent_id)
    assert info is not None, f"GET agent {agent_id} returned None"
    readback = _readback_status(info)
    assert readback == status, (
        f"status read-back mismatch: set {status!r}, GET reports {readback!r}"
    )
    await event_checker.expect_webhook_payload(
        "agent_state_changed", match, timeout=10)


@pytest.mark.asyncio
async def test_agent_status_idle(pbx, api, event_checker):
    await _set_and_verify_status(api, event_checker, "idle")


@pytest.mark.asyncio
async def test_agent_status_away(pbx, api, event_checker):
    await _set_and_verify_status(api, event_checker, "away")


@pytest.mark.asyncio
async def test_agent_status_dnd(pbx, api, event_checker):
    await _set_and_verify_status(api, event_checker, "dnd")


@pytest.mark.asyncio
async def test_agent_status_offline(pbx, api, event_checker):
    await _set_and_verify_status(api, event_checker, "offline")


@pytest.mark.asyncio
async def test_agent_status_custom(pbx, api, event_checker):
    resp = await api.update_agent_status("1001", "custom:lunch")
    assert resp is not None, "POST custom status returned None"
    info = await api.get_agent("1001")
    assert info is not None, "GET agent returned None"
    readback = _readback_status(info)
    # The product normalizes "custom:lunch" to "away:lunch" (custom reason
    # rides on the away base state) — pin THAT contract.
    assert readback in ("custom:lunch", "away:lunch"), (
        f"custom status read-back mismatch: set 'custom:lunch', "
        f"GET reports {readback!r}"
    )
    await event_checker.expect_webhook_payload(
        "agent_state_changed",
        {"payload.agent_id": "1001"},
        timeout=10,
    )


@pytest.mark.asyncio
async def test_agent_phone_config(pbx, api, event_checker):
    """GET /cc/phone/config returns a non-empty config object."""
    config = await api.get_phone_config()
    assert config is not None, "GET phone config returned None"
    data = (config or {}).get("data") or config or {}
    assert isinstance(data, dict) and data, (
        f"phone config is empty: {config!r:.200}"
    )


@pytest.mark.asyncio
async def test_agent_breaks_query(pbx, api, event_checker):
    """GET /cc/agents/1001/breaks returns the agent's break/usage stats.

    The endpoint answers with the agent_id and cumulative counters
    (total_break_secs / total_wrapup_secs / total_calls / total_talk_secs).
    """
    breaks = await api.get_agent_breaks("1001")
    assert breaks is not None, "GET agent breaks returned None"
    data = breaks.get("data", breaks) if isinstance(breaks, dict) else breaks
    assert isinstance(data, dict), (
        f"breaks payload has unexpected shape: {breaks!r:.200}"
    )
    assert data.get("agent_id") == "1001", (
        f"breaks payload missing agent_id: {data!r:.200}"
    )
    assert "total_break_secs" in data, (
        f"breaks payload missing total_break_secs: {data!r:.200}"
    )


@pytest.mark.asyncio
async def test_presence_webhook_emitted(pbx, sipbot_pool, api, event_checker):
    """SIP registration must produce an agent_registered webhook bound to
    the registering extension.

    agent_registered fires on the offline→registered transition, so force
    the agent offline first; registering then must emit the event.
    """
    try:
        await api.update_agent_status("1001", "offline")
    except Exception:
        # away/wrapup → offline is not a valid transition; go via idle.
        await api.update_agent_status("1001", "idle")
        await api.update_agent_status("1001", "offline")
    mark = len(event_checker.webhook.all_events())
    agent = sipbot_pool.callee(
        host=pbx.host,
        port=15150,
        username="1001",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
    )
    ok = await agent.wait_output_async(r"Registered|200 OK", timeout=20)
    assert ok, f"SIP registration failed. Output:\n{agent.output[-400:]}"

    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        for ev in event_checker.webhook.all_events()[mark:]:
            if ev.event_type != "agent_registered":
                continue
            payload = ev.payload if isinstance(ev.payload, dict) else {}
            if payload.get("agent_id") == "1001" or (
                payload.get("agent_extension") == "1001"
            ):
                return
        await asyncio.sleep(0.2)
    pytest.fail(
        "agent_registered event for 1001 never arrived after SIP "
        f"registration. wh={event_checker.webhook.event_types()[-15:]}"
    )
