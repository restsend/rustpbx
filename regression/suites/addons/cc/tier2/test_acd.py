"""Tier 2 — ACD strategy tests (queue dispatch + agent selection).

Each strategy test uses an exclusive skill-group + IVR queue route so only
the configured agents participate, then asserts the selected agent_id from
call_ringing webhook events.
"""

from __future__ import annotations

import asyncio

import pytest

from helpers.acd_strategy_e2e import (
    ensure_agents_ready,
    finish_queue_call,
    place_queue_call,
    reset_agents,
    setup_exclusive_queue,
    wait_agent_idle,
    wait_current_calls_zero,
    wait_dispatch_agent,
    wait_second_dispatch,
)

pytestmark = [pytest.mark.tier2, pytest.mark.acd]


@pytest.mark.order(1)
@pytest.mark.asyncio
async def test_acd_list_policies(pbx, api, event_checker):
    """ACD — query available ACD policies."""
    policies = await api.list_acd_policies()
    if policies is None:
        pytest.skip("CC REST API requires PhoneAuth JWT authentication")
    assert policies is not None, "ACD policy list returned None"


@pytest.mark.order(10)
@pytest.mark.asyncio
async def test_acd_longest_idle_strategy(pbx, sipbot_pool, api, event_checker):
    """longest_idle — agent idle longer receives the next queue call."""
    rp, _, _ = await setup_exclusive_queue(
        pbx,
        api,
        strategy_type="longest_idle",
        agent_ids=["1001", "1002"],
    )
    await reset_agents(
        sipbot_pool,
        api,
        pbx,
        [("1001", 15140), ("1002", 15141)],
    )

    # 1001 leaves idle briefly; 1002 stays idle → 1002 has longer idle time.
    await api.update_agent_status("1001", "away")
    await asyncio.sleep(3)
    await api.update_agent_status("1001", "idle")
    await asyncio.sleep(2)
    await wait_agent_idle(api, "1001")
    await wait_agent_idle(api, "1002")

    caller = await place_queue_call(sipbot_pool, pbx, rp, hangup=20)
    _call_id, agent_id = await wait_dispatch_agent(event_checker, timeout=35)
    assert agent_id == "1002", (
        f"longest_idle should pick agent 1002 (longer idle), got {agent_id}"
    )
    await event_checker.expect_webhook_event("call_answered", timeout=25, call_id=_call_id)
    await finish_queue_call(event_checker, api, _call_id, agent_ids=[agent_id])


@pytest.mark.order(5)
@pytest.mark.asyncio
async def test_acd_least_calls_strategy(pbx, sipbot_pool, api, event_checker):
    """least_answered — agent with fewer handled calls is selected on call 2."""
    # Use 1002+1003 — run before longest_idle (order 10) to avoid cross-test stats.
    rp, _, exclusive_skill = await setup_exclusive_queue(
        pbx,
        api,
        strategy_type="least_answered",
        agent_ids=["1002", "1003"],
    )
    await reset_agents(
        sipbot_pool,
        api,
        pbx,
        [("1002", 15144), ("1003", 15145)],
    )
    await ensure_agents_ready(api, ["1002", "1003"], exclusive_skill=exclusive_skill)

    caller1 = await place_queue_call(sipbot_pool, pbx, rp, hangup=18)
    call1_id, first_agent = await wait_dispatch_agent(event_checker, timeout=35)
    await event_checker.expect_webhook_event("call_answered", timeout=25, call_id=call1_id)
    await finish_queue_call(event_checker, api, call1_id, agent_ids=[first_agent])
    await wait_current_calls_zero(api, first_agent, timeout=35)
    await ensure_agents_ready(
        api,
        ["1002", "1003"],
        exclusive_skill=exclusive_skill,
        sipbot_pool=sipbot_pool,
        pbx=pbx,
        agent_ports={"1002": 15144, "1003": 15145},
    )
    await api.reload_acd()

    other = "1003" if first_agent == "1002" else "1002"
    caller2 = await place_queue_call(sipbot_pool, pbx, rp, hangup=120)
    _call2_id, second_agent = await wait_second_dispatch(
        event_checker,
        sipbot_pool,
        pbx,
        rp,
        after_call_ringing_count=1,
        timeout=60,
    )
    assert second_agent == other, (
        f"least_answered: first={first_agent}, expected second={other}, got {second_agent}"
    )
    await finish_queue_call(event_checker, api, _call2_id, agent_ids=[second_agent])


@pytest.mark.order(30)
@pytest.mark.asyncio
async def test_acd_round_robin_strategy(pbx, sipbot_pool, api, event_checker):
    """round_robin — two sequential calls rotate between idle agents."""
    rp, _, exclusive_skill = await setup_exclusive_queue(
        pbx,
        api,
        strategy_type="round_robin",
        agent_ids=["1002", "1003"],
    )
    await reset_agents(
        sipbot_pool,
        api,
        pbx,
        [("1002", 15142), ("1003", 15143)],
    )
    await ensure_agents_ready(api, ["1002", "1003"], exclusive_skill=exclusive_skill)

    await place_queue_call(sipbot_pool, pbx, rp, hangup=15)
    _c1, agent1 = await wait_dispatch_agent(event_checker, timeout=35)
    await event_checker.expect_webhook_event("call_answered", timeout=25, call_id=_c1)
    await finish_queue_call(event_checker, api, _c1, agent_ids=[agent1])
    await asyncio.sleep(2)
    await ensure_agents_ready(api, ["1002", "1003"], exclusive_skill=exclusive_skill)

    await place_queue_call(sipbot_pool, pbx, rp, hangup=15)
    _c2, agent2 = await wait_dispatch_agent(
        event_checker, timeout=35, after_call_ringing_count=1
    )
    assert agent1 != agent2, (
        f"round_robin should rotate agents across two calls, got {agent1} twice"
    )
    await finish_queue_call(event_checker, api, _c2, agent_ids=[agent2])


@pytest.mark.order(40)
@pytest.mark.asyncio
async def test_acd_skill_based_strategy(pbx, sipbot_pool, api, event_checker):
    """skill_based — agent with higher skill level on required skill wins."""
    rp, _, exclusive_skill = await setup_exclusive_queue(
        pbx,
        api,
        strategy_type="skill_based",
        agent_ids=["1001", "1003"],
    )
    status, body = await api.raw_request(
        "PUT",
        "/api/cc/agents/1003/skill-levels",
        {"skill_levels": {exclusive_skill: 9}},
    )
    assert status in (200, 204), f"skill-levels for 1003 failed: {status} {body!r:.120}"
    status, body = await api.raw_request(
        "PUT",
        "/api/cc/agents/1001/skill-levels",
        {"skill_levels": {exclusive_skill: 2}},
    )
    assert status in (200, 204), f"skill-levels for 1001 failed: {status} {body!r:.120}"
    await api.reload_agents()

    await reset_agents(
        sipbot_pool,
        api,
        pbx,
        [("1001", 15146), ("1003", 15147)],
    )
    await ensure_agents_ready(api, ["1001", "1003"], exclusive_skill=exclusive_skill)

    await place_queue_call(sipbot_pool, pbx, rp, hangup=18)
    _call_id, agent_id = await wait_dispatch_agent(event_checker, timeout=35)
    assert agent_id == "1003", (
        f"skill_based should prefer higher skill level (1003), got {agent_id}"
    )
    await event_checker.expect_webhook_event("call_answered", timeout=25, call_id=_call_id)
    await finish_queue_call(event_checker, api, _call_id, agent_ids=[agent_id])


@pytest.mark.order(90)
@pytest.mark.asyncio
async def test_acd_reload(pbx, api, event_checker):
    """ACD — reload ACD configuration via REST API."""
    resp = await api.reload_acd()
    assert resp is not None, "reload_acd returned None"


@pytest.mark.order(91)
@pytest.mark.asyncio
async def test_acd_diagnostics(pbx, api, event_checker):
    """ACD — diagnostics endpoint."""
    resp = await api.get("/api/cc/acd/diagnostics")
    assert resp is not None, "GET /cc/acd/diagnostics returned None"
