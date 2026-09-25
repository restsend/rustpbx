"""Shared helpers for ACD strategy E2E tests (queue dispatch + agent selection)."""

from __future__ import annotations

import asyncio
import uuid
from typing import Optional

import pytest

from helpers.config_reload import apply_config

ALL_ACD_AGENT_IDS = ("1001", "1002", "1003")
ALL_SIP_USER_IDS = ("1001", "1002", "1003", "1004")


async def end_all_active_calls(api) -> None:
    """Hang up any calls still tracked by the CC active-call registry."""
    try:
        calls = await api.list_active_calls()
    except Exception:
        return
    if not isinstance(calls, list):
        return
    for call in calls:
        call_id = call.get("call_id") if isinstance(call, dict) else None
        if call_id:
            try:
                await api.end_call(call_id)
            except Exception:
                pass


async def nudge_agent_toward_idle(api, agent_id: str) -> None:
    """Drive agent through valid transitions (never Busy→Idle directly)."""
    status = await agent_status(api, agent_id)
    if status == "wrapup":
        try:
            await api.end_agent_wrapup(agent_id)
        except Exception:
            pass
    elif status in ("busy", "ringing"):
        await end_all_active_calls(api)
        if status == "ringing":
            # Ringing→Idle is valid when the abandoned INVITE is cleared.
            try:
                await api.update_agent_status(agent_id, "idle")
            except Exception:
                pass
    elif status in ("away", "dnd"):
        try:
            await api.update_agent_status(agent_id, "idle")
        except Exception:
            pass


async def drain_agents(api, agent_ids: tuple[str, ...] = ALL_ACD_AGENT_IDS) -> None:
    """End calls + wrapup so shared agents are ready for the next test."""
    await end_all_active_calls(api)
    for _ in range(8):
        statuses = {aid: await agent_status(api, aid) for aid in agent_ids}
        if all(s in ("idle", "offline", "") for s in statuses.values()):
            return
        for aid, status in statuses.items():
            if status not in ("idle", "offline", ""):
                await nudge_agent_toward_idle(api, aid)
        await asyncio.sleep(1)
    try:
        await api.reload_agents()
    except Exception:
        pass


async def enable_acd(pbx, api) -> None:
    """Enable the global ACD gate and reload in-memory config."""
    acd = pbx.work_dir / "config" / "cc" / "acd.toml"
    if acd.exists():
        text = acd.read_text(encoding="utf-8")
        if text.startswith("enabled = false"):
            acd.write_text(
                text.replace("enabled = false", "enabled = true", 1),
                encoding="utf-8",
            )
    await api.reload_acd()


async def create_acd_policy(
    api,
    name: str,
    strategy_type: str,
    *,
    available_states: Optional[list[str]] = None,
) -> None:
    """Create a named ACD policy via REST."""
    status, body = await api.raw_request(
        "POST",
        "/api/cc/acd/policies",
        {
            "name": name,
            "priority": {
                "vip_bonus": {},
                "wait_time_weight": 1.0,
                "base_priority": 0,
                "fifo_within_same_priority": True,
            },
            "strategy": {
                "strategy_type": strategy_type,
                "skill_weights": None,
                "max_concurrent_calls": 1,
                "require_exact_skill": False,
            },
            "overflow": {
                "triggers": [],
                "chain": [],
                "retry_per_target": 1,
                "retry_interval_secs": 5,
                "mode": "replace",
                "escalation_timeline": [],
            },
            "schedule": {"business_hours": None, "holidays": {}, "night_mode": None},
            "min_level": None,
            "max_level": None,
            "available_states": available_states or ["idle"],
        },
    )
    if status in (200, 201, 409):
        return
    if status == 400 and "already exists" in str(body).lower():
        return
    pytest.fail(f"create ACD policy {name} failed: {status} {body!r:.200}")


# Built-in names are unreliable in long sessions (400 duplicate / wrong strategy).
# Each test run creates a fresh policy tied to its skill-group.


async def setup_exclusive_queue(
    pbx,
    api,
    *,
    strategy_type: str,
    agent_ids: list[str],
    agent_skills: Optional[dict[str, list[str]]] = None,
    policy_name: Optional[str] = None,
    skill_group_metadata: Optional[dict] = None,
    max_concurrency: int = 1,
) -> tuple[str, str, str]:
    """Create exclusive skill-group + ACD policy + IVR queue route.

    Returns (route_point, skill_group_id, exclusive_skill).
    """
    tag = uuid.uuid4().hex[:6]
    exclusive_skill = f"acd-excl-{tag}"
    sg_name = f"acd-sg-{tag}"
    policy = policy_name or f"acd-pol-{tag}"
    rp = f"acd{tag}"

    for aid in agent_ids:
        base = ["support", "sales"]
        extra = (agent_skills or {}).get(aid, [])
        await api.update_agent(aid, {
            "display_name": f"Agent {aid} (ACD E2E)",
            "skills": base + [exclusive_skill] + extra,
            "max_concurrency": max_concurrency,
        })

    await create_acd_policy(api, policy, strategy_type)

    sg_payload: dict = {
        "skill_group_id": sg_name,
        "display_name": f"ACD E2E {strategy_type} {tag}",
        "skills_required": [exclusive_skill],
        "acd_policy": policy,
    }
    if skill_group_metadata is not None:
        sg_payload["metadata"] = skill_group_metadata
    await api.create_skill_group(sg_payload)

    await enable_acd(pbx, api)
    await api.reload_skill_groups()
    await api.reload_agents()

    pbx.config_builder.add_ivr(rp, f"""\
[ivr]
name = "{rp}"
ivr_mode = "tree"

[ivr.root]
greeting = ""
greeting_text = "ACD strategy test"
timeout_ms = 1500
max_retries = 0
timeout_action = {{ type = "queue", target = "{sg_name}" }}
max_retries_action = {{ type = "queue", target = "{sg_name}" }}
entries = []
""")
    pbx.config_builder.add_route(
        f"{rp}-route",
        match={"to.user": rp},
        priority=1,
        action="application",
        app="ivr",
        app_params={"file": f"config/ivr/{rp}.toml"},
        auto_answer=True,
    )
    pbx.config_builder.add_queue(
        sg_name,
        targets=[f"skill-group:{sg_name}"],
        hold_audio="sounds/phone-calling.wav",
    )
    await apply_config(pbx, api, reload_app=False)
    return rp, sg_name, exclusive_skill


async def agent_status(api, agent_id: str) -> str:
    try:
        info = await api.get_agent(agent_id)
        if not isinstance(info, dict):
            return ""
        return ((info.get("data") or info).get("status") or "").lower()
    except Exception:
        return ""


async def wait_agent_idle(api, agent_id: str, *, timeout: float = 90.0) -> None:
    """Poll until agent is stably idle (3s).

    Busy/ringing agents cannot jump to idle via REST — end calls / wrapup
    and wait for the state machine instead.
    """
    stable_since: float | None = None
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        status = await agent_status(api, agent_id)
        now = asyncio.get_event_loop().time()
        if status == "idle":
            stable_since = stable_since or now
            if now - stable_since >= 3:
                return
        else:
            stable_since = None
            if status not in ("offline", ""):
                await nudge_agent_toward_idle(api, agent_id)
        await asyncio.sleep(1)
    pytest.fail(f"agent {agent_id} not idle within {timeout}s (last={await agent_status(api, agent_id)!r})")


async def cleanup_acd_agents(
    sipbot_pool,
    api,
    pbx,
    agent_ids: tuple[str, ...] = ALL_ACD_AGENT_IDS,
    *,
    drain_timeout: float = 60.0,
) -> None:
    """Tear down SIP bots and wait for agents to leave busy/wrapup."""
    for username in ALL_SIP_USER_IDS:
        sipbot_pool.terminate_user(username)
    await asyncio.sleep(1)
    await drain_agents(api, agent_ids)

    deadline = asyncio.get_event_loop().time() + drain_timeout
    while asyncio.get_event_loop().time() < deadline:
        statuses = {aid: await agent_status(api, aid) for aid in agent_ids}
        if all(s in ("idle", "offline", "") for s in statuses.values()):
            return
        await drain_agents(api, agent_ids)
        await asyncio.sleep(1)
    try:
        await api.reload_agents()
    except Exception:
        pass


async def wait_agent_registered(api, agent_id: str, *, timeout: float = 45.0) -> None:
    """Wait until SIP REGISTER has brought the agent out of offline."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        status = await agent_status(api, agent_id)
        if status and status != "offline":
            return
        await asyncio.sleep(1)
    pytest.fail(f"agent {agent_id} still offline after {timeout}s (REGISTER did not land)")


async def reset_agents(sipbot_pool, api, pbx, agents: list[tuple[str, int]]) -> None:
    """Kill stale bots and register fresh agents. agents = [(username, port), ...]."""
    agent_ids = tuple(username for username, _ in agents)
    sipbot_pool.terminate_user("1004")
    for username in agent_ids:
        sipbot_pool.terminate_user(username)
    await drain_agents(api, agent_ids)
    await asyncio.sleep(5)

    started: dict[str, object] = {}
    for username, port in agents:
        for attempt in range(2):
            sipbot_pool.terminate_user(username)
            await asyncio.sleep(1)
            started[username] = sipbot_pool.callee(
                host=pbx.host,
                port=port,
                username=username,
                password="123456",
                register=True,
                proxy=f"{pbx.host}:{pbx.sip_port}",
                domain=pbx.host,
                ring_secs=1,
                answer_mode="echo",
                hangup_after=120,
            )
            proc = started[username]
            ok = await proc.wait_output_async(r"(Registered|200 OK)", timeout=45)  # type: ignore[attr-defined]
            if ok:
                await asyncio.sleep(2)
                try:
                    await wait_agent_registered(api, username, timeout=60)
                    break
                except BaseException:
                    pass
            if attempt == 1:
                pytest.fail(
                    f"sipbot {username} did not REGISTER after 2 attempts.\n"
                    f"{proc.output[-800:]}"  # type: ignore[attr-defined]
                )
            await asyncio.sleep(2)

    for username, _ in agents:
        await wait_agent_registered(api, username, timeout=60)

    await asyncio.sleep(2)
    for username, _ in agents:
        status = await agent_status(api, username)
        if status not in ("idle", "offline"):
            await nudge_agent_toward_idle(api, username)
        try:
            await api.update_agent_status(username, "idle")
        except Exception:
            pass

    for username, _ in agents:
        await wait_agent_idle(api, username, timeout=90)
    await api.reload_agents()
    await asyncio.sleep(1)


async def finish_queue_call(
    event_checker,
    api,
    call_id: str,
    *,
    agent_ids: list[str],
) -> None:
    """Wait for hangup + all listed agents back to stable idle."""
    await wait_call_hangup(event_checker, call_id)
    await asyncio.sleep(2)
    for aid in agent_ids:
        await wait_agent_idle(api, aid)
        await wait_current_calls_zero(api, aid, timeout=35)


async def place_queue_call(
    sipbot_pool,
    pbx,
    route_point: str,
    *,
    hangup: int = 25,
    caller_username: str = "1004",
):
    """Place an inbound SIP call into an IVR route point.

    Uses extension 1004 by default — a SIP user that is NOT an ACD agent,
    so it won't collide with agent registrations under test.
    """
    caller = sipbot_pool.caller(
        target=f"sip:{route_point}@{pbx.sip_addr}",
        username=caller_username,
        password="123456",
        hangup=hangup,
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, (
        f"queue call to {route_point} failed (check SIP auth / routing).\n"
        f"{caller.output[-800:]}"
    )
    return caller


async def wait_dispatch_agent(
    event_checker,
    *,
    timeout: float = 30.0,
    after_call_ringing_count: int = 0,
) -> tuple[str, str]:
    """Wait for the Nth call_ringing webhook (0 = first in this test session).

    ``wait_for_event`` always scans from index 0, so a second dispatch in the
    same test must skip earlier call_ringing events.
    """
    webhook = event_checker.webhook
    assert webhook is not None, "webhook receiver not configured"
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        # Only AGENT-ATTRIBUTED rings count as dispatches: since 86b53308 the
        # agent leg's 180 also emits a per-leg call_ringing (leg_id set, no
        # agent attribution) right before the CC hook publishes the
        # attributed one — counting both broke every index-based dispatch
        # assertion.
        rings = [
            e for e in webhook.all_events()
            if e.event_type == "call_ringing"
            and (e.payload or {}).get("agent_id")
        ]
        if len(rings) > after_call_ringing_count:
            ring = rings[after_call_ringing_count]
            call_id = ring.call_id
            agent_id = ring.payload.get("agent_id")
            if call_id and agent_id:
                return call_id, str(agent_id)
        await asyncio.sleep(0.15)
    pytest.fail(
        f"Expected call_ringing #{after_call_ringing_count + 1} within {timeout}s. "
        f"Got {len([e for e in webhook.all_events() if e.event_type == 'call_ringing'])} ring(s). "
        f"Recent events: {webhook.event_types()[-12:]}"
    )


async def wait_second_dispatch(
    event_checker,
    sipbot_pool,
    pbx,
    route_point: str,
    *,
    after_call_ringing_count: int = 1,
    timeout: float = 45.0,
    retry_hangup: int = 60,
) -> tuple[str, str]:
    """Wait for the Nth call_ringing; if a queued caller was lost, place a fresh call."""
    try:
        return await wait_dispatch_agent(
            event_checker,
            timeout=timeout,
            after_call_ringing_count=after_call_ringing_count,
        )
    except BaseException:
        await place_queue_call(sipbot_pool, pbx, route_point, hangup=retry_hangup)
        # Stray rings may have landed while the original queued caller was
        # lost (transfer legs, readiness probes). Anchor on the CURRENT
        # ATTRIBUTED ring count (same filter wait_dispatch_agent indexes
        # with) and wait for the next NEW dispatch instead of a fixed index.
        base = len(
            [
                e
                for e in event_checker.webhook.all_events()
                if e.event_type == "call_ringing"
                and (e.payload or {}).get("agent_id")
            ]
        )
        return await wait_dispatch_agent(
            event_checker,
            timeout=timeout,
            after_call_ringing_count=base,
        )


async def wait_call_hangup(event_checker, call_id: str, *, timeout: float = 40.0) -> None:
    await event_checker.webhook.wait_for_event("call_hangup", timeout=timeout, call_id=call_id)
    await asyncio.sleep(2)


async def agent_total_calls(api, agent_id: str) -> int | None:
    """Live handled-call count from in-memory agent registry."""
    try:
        data = await api.get_agent_breaks(agent_id)
        if isinstance(data, dict):
            return int(data.get("total_calls", 0))
    except Exception:
        pass
    return None


async def agent_skills(api, agent_id: str) -> list[str]:
    info = await api.get_agent(agent_id)
    if not isinstance(info, dict):
        return []
    data = info.get("data") or info
    skills = data.get("skills") or data.get("skill_list") or []
    if isinstance(skills, dict):
        skills = skills.get("list") or skills.get("skills") or []
    return list(skills)


async def wait_agent_call_count(
    api,
    agent_id: str,
    *,
    at_least: int = 1,
    timeout: float = 35.0,
) -> int:
    deadline = asyncio.get_event_loop().time() + timeout
    last: int | None = None
    while asyncio.get_event_loop().time() < deadline:
        last = await agent_total_calls(api, agent_id)
        if last is not None and last >= at_least:
            return last
        await asyncio.sleep(1)
    pytest.fail(
        f"agent {agent_id} total_calls did not reach {at_least} within {timeout}s (last={last})"
    )


async def reregister_if_offline(
    sipbot_pool,
    api,
    pbx,
    username: str,
    port: int,
) -> None:
    """Start a fresh sipbot when REST still shows the agent as offline."""
    if await agent_status(api, username) != "offline":
        return
    sipbot_pool.terminate_user(username)
    await asyncio.sleep(1)
    proc = sipbot_pool.callee(
        host=pbx.host,
        port=port,
        username=username,
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
        hangup_after=120,
    )
    ok = await proc.wait_output_async(r"(Registered|200 OK)", timeout=45)
    if not ok:
        pytest.fail(
            f"sipbot {username} re-register failed.\n{proc.output[-800:]}"
        )
    await wait_agent_registered(api, username, timeout=60)
    await wait_agent_idle(api, username, timeout=90)


async def ensure_agents_ready(
    api,
    agent_ids: list[str],
    *,
    exclusive_skill: str | None = None,
    sipbot_pool=None,
    pbx=None,
    agent_ports: dict[str, int] | None = None,
) -> None:
    """Agents must be registered, idle, and (optionally) carry the exclusive skill."""
    for aid in agent_ids:
        if sipbot_pool is not None and pbx is not None and agent_ports and aid in agent_ports:
            await reregister_if_offline(
                sipbot_pool, api, pbx, aid, agent_ports[aid]
            )
        await wait_agent_registered(api, aid, timeout=60)
        await wait_agent_idle(api, aid, timeout=90)
        if exclusive_skill:
            skills = await agent_skills(api, aid)
            assert exclusive_skill in skills, (
                f"agent {aid} missing exclusive skill {exclusive_skill!r}, skills={skills}"
            )


async def agent_current_calls(api, agent_id: str) -> int | None:
    info = await api.get_agent(agent_id)
    if not isinstance(info, dict):
        return None
    data = info.get("data") or info
    cc = data.get("current_calls")
    return int(cc) if cc is not None else None


async def wait_current_calls_zero(api, agent_id: str, *, timeout: float = 35.0) -> int:
    """Poll until current_calls==0 or fail."""
    deadline = asyncio.get_event_loop().time() + timeout
    last: int | None = None
    while asyncio.get_event_loop().time() < deadline:
        last = await agent_current_calls(api, agent_id)
        if last == 0:
            return 0
        await asyncio.sleep(1)
    pytest.fail(
        f"agent {agent_id} current_calls not zero within {timeout}s (last={last})"
    )
