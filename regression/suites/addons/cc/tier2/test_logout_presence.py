"""Tier 2 — Agent logout / unregister presence recovery.

Regression coverage for "agent logged out but the CC panel still shows
idle": the ungraceful exit path (browser killed / network dropped, no
REGISTER Expires=0) must be picked up by the registrar expiry sweeper and
reflected in the panel list (GET /api/cc/agents) without manual
intervention.
"""

from __future__ import annotations

import asyncio

import pytest

pytestmark = [pytest.mark.tier2, pytest.mark.presence]


async def _list_agent_status(api, agent_id):
    agents = await api.list_agents()
    if agents is None:
        pytest.skip("CC REST API requires console authentication")
    for a in agents.get("data", []):
        if a.get("agent_id") == agent_id:
            return a.get("status")
    return None


@pytest.mark.asyncio
async def test_ungraceful_logout_recovered_by_sweeper(pbx, sipbot_pool, api):
    """Agent UA dies without unregister → sweeper marks it offline in the panel."""
    agent_id = "1001"

    # Register via SIP: the registrar bridge flips the agent to idle.
    sipbot_pool.callee(
        host=pbx.host,
        port=15150,
        username=agent_id,
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
    )

    # The panel list must show the agent idle while registered.
    deadline = asyncio.get_event_loop().time() + 20
    status = None
    while asyncio.get_event_loop().time() < deadline:
        status = await _list_agent_status(api, agent_id)
        if status == "idle":
            break
        await asyncio.sleep(1)
    assert status == "idle", (
        f"agent should be listed as idle while registered, got {status!r}"
    )

    # Ungraceful logout: kill the UA without REGISTER Expires=0 (equivalent
    # to closing the browser tab). The expiry sweeper must mark the agent
    # offline once the registration expires (max_registrar_expires 50s +
    # expiry grace), and the panel list must reflect it.
    sipbot_pool.terminate_all()

    deadline = asyncio.get_event_loop().time() + 150
    while asyncio.get_event_loop().time() < deadline:
        status = await _list_agent_status(api, agent_id)
        if status == "offline":
            break
        await asyncio.sleep(2)
    assert status == "offline", (
        "panel must show offline after an ungraceful logout "
        f"(last status: {status!r})"
    )
