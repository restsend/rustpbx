"""Tier 3 — ACD agent capacity: max_concurrency enforcement + short wrapup recovery."""

from __future__ import annotations

import asyncio

import pytest

from helpers.acd_strategy_e2e import (
    ensure_agents_ready,
    finish_queue_call,
    place_queue_call,
    reset_agents,
    setup_exclusive_queue,
    wait_current_calls_zero,
    wait_dispatch_agent,
    wait_second_dispatch,
)

pytestmark = [pytest.mark.tier3, pytest.mark.acd]


@pytest.mark.order(50)
@pytest.mark.asyncio
async def test_agent_max_concurrency_blocks_second_dispatch(pbx, sipbot_pool, api, event_checker):
    """max_concurrency=1 — second queue call must not ring while first call is active."""
    rp, _, exclusive_skill = await setup_exclusive_queue(
        pbx,
        api,
        strategy_type="longest_idle",
        agent_ids=["1003"],
        max_concurrency=1,
    )
    await reset_agents(sipbot_pool, api, pbx, [("1003", 15220)])
    await ensure_agents_ready(api, ["1003"], exclusive_skill=exclusive_skill)

    caller1 = await place_queue_call(sipbot_pool, pbx, rp, hangup=45)
    call1_id, agent1 = await wait_dispatch_agent(event_checker, timeout=35)
    assert agent1 == "1003"
    await event_checker.expect_webhook_event("call_answered", timeout=25, call_id=call1_id)

    # Second caller enters queue while agent is still on call1.
    caller2 = await place_queue_call(sipbot_pool, pbx, rp, hangup=120)
    await asyncio.sleep(6)
    ring_for_call2 = [
        e
        for e in event_checker.webhook.events
        if e.event_type == "call_ringing" and e.call_id != call1_id
    ]
    assert not ring_for_call2, (
        "max_concurrency=1: agent must not receive second ring while first call active. "
        f"rings={[(e.call_id, e.payload.get('agent_id')) for e in ring_for_call2]}"
    )

    await finish_queue_call(event_checker, api, call1_id, agent_ids=["1003"])
    await wait_current_calls_zero(api, "1003", timeout=35)
    await ensure_agents_ready(
        api,
        ["1003"],
        exclusive_skill=exclusive_skill,
        sipbot_pool=sipbot_pool,
        pbx=pbx,
        agent_ports={"1003": 15220},
    )
    await api.reload_acd()

    # After first call ends, queued caller2 should eventually dispatch.
    _call2_id, agent2 = await wait_second_dispatch(
        event_checker,
        sipbot_pool,
        pbx,
        rp,
        after_call_ringing_count=1,
        timeout=45,
    )
    assert agent2 == "1003", f"second call should dispatch to 1003, got {agent2}"


@pytest.mark.order(60)
@pytest.mark.asyncio
async def test_short_wrapup_releases_capacity_for_second_call(pbx, sipbot_pool, api, event_checker):
    """wrapup_time_secs=2 — after call end, current_calls=0 and next call can dispatch."""
    rp, _, exclusive_skill = await setup_exclusive_queue(
        pbx,
        api,
        strategy_type="longest_idle",
        agent_ids=["1001"],
        skill_group_metadata={"wrapup_time_secs": 2},
        max_concurrency=1,
    )
    await reset_agents(sipbot_pool, api, pbx, [("1001", 15221)])
    await ensure_agents_ready(api, ["1001"], exclusive_skill=exclusive_skill)

    caller1 = await place_queue_call(sipbot_pool, pbx, rp, hangup=10)
    call1_id, _ = await wait_dispatch_agent(event_checker, timeout=35)
    await event_checker.expect_webhook_event("call_answered", timeout=25, call_id=call1_id)
    await finish_queue_call(event_checker, api, call1_id, agent_ids=["1001"])

    await wait_current_calls_zero(api, "1001", timeout=35)

    caller2 = await place_queue_call(sipbot_pool, pbx, rp, hangup=12)
    call2_id, agent2 = await wait_dispatch_agent(event_checker, timeout=35)
    assert agent2 == "1001", f"second call should dispatch after capacity release, got {agent2}"
    await finish_queue_call(event_checker, api, call2_id, agent_ids=["1001"])
