"""CSV 回归 — L193: ACD 路由策略 external（外部路由系统决策，支持路由动作）.

覆盖：
  1. external 策略按外部系统返回的坐席顺序派发（dispatch_reason 带 external 前缀）
  2. external 超时/不可达时降级到 fallback_strategy（本地 longest_idle）
  3. transfer 路由动作：外部系统指示转接目标，拨号重定向到该目标
"""

from __future__ import annotations

import asyncio
import json
import uuid

import pytest
from aiohttp import web

pytestmark = [pytest.mark.acceptance, pytest.mark.acd]


class ExternalRouterMock:
    """Stand-in for the external routing system (CSV L193 '调用webhook')."""

    def __init__(self, host: str = "127.0.0.1"):
        self.host = host
        self.port = 0
        self.runner: web.AppRunner | None = None
        # Configurable decision: None => 500 (forces fallback); otherwise the
        # JSON body to return.
        self.decision: dict | None = {"action": "agents", "params": {"agents": []}}
        self.requests: list[dict] = []

    async def start(self):
        app = web.Application()
        app.router.add_post("/decide", self._decide)
        self.runner = web.AppRunner(app)
        await self.runner.setup()
        site = web.TCPSite(self.runner, self.host, 0)
        await site.start()
        self.port = self.runner.addresses[0][1]
        return self

    async def stop(self):
        if self.runner:
            await self.runner.cleanup()

    @property
    def url(self) -> str:
        return f"http://{self.host}:{self.port}/decide"

    async def _decide(self, request: web.Request) -> web.Response:
        body = await request.json()
        self.requests.append(body)
        if self.decision is None:
            return web.Response(status=500, text="router down")
        return web.json_response(self.decision)


async def _enable_acd(pbx, api):
    """Flip the PBX's `{generated_dir}/cc/acd.toml` `enabled = true` (repo default
    is false — the whole ACD gate). The file lives under the PBX process's
    working directory (pbx.work_dir), NOT the pytest cwd."""
    acd = pbx.work_dir / "config" / "cc" / "acd.toml"
    if acd.exists():
        text = acd.read_text(encoding="utf-8")
        if text.startswith("enabled = false"):
            acd.write_text(
                text.replace("enabled = false", "enabled = true", 1),
                encoding="utf-8",
            )
    await api.reload_acd()


async def _setup_external_policy(pbx, api, router_url: str, fallback: str = "longest_idle"):
    """Create the exclusive skill-group + queue + external ACD policy.

    Returns the route point to dial.
    """
    tag = uuid.uuid4().hex[:6]
    skill = f"ext-skill-{tag}"
    sg = f"ext-sg-{tag}"
    policy = f"ext-policy-{tag}"

    await api.update_agent("1001", {
        "display_name": "Agent 1001 (Regression)",
        "skills": ["support", "sales", skill],
    })
    # ACD policy with the external callback.
    await api.raw_request("POST", "/api/cc/acd/policies", {
        "name": policy,
        "priority": {
            "vip_bonus": {}, "wait_time_weight": 1.0,
            "base_priority": 0, "fifo_within_same_priority": True,
        },
        "strategy": {
            "strategy_type": "external",
            "skill_weights": None,
            "max_concurrent_calls": 1,
            "require_exact_skill": False,
        },
        "external": {
            "url": router_url,
            "method": "POST",
            "timeout_secs": 3,
            "fallback_strategy": fallback,
        },
        "overflow": {
            "triggers": [], "chain": [],
            "retry_per_target": 1, "retry_interval_secs": 5,
            "mode": "replace", "escalation_timeline": [],
        },
        "schedule": {"business_hours": None, "holidays": {}, "night_mode": None},
        "min_level": None,
        "max_level": None,
        "available_states": ["idle"],
    })
    await api.create_skill_group({
        "skill_group_id": sg,
        "display_name": f"ext sg {tag}",
        "skills_required": [skill],
        "acd_policy": policy,
    })
    # The ACD engine keeps an in-memory config snapshot: creating the policy
    # via REST writes the file, but the snapshot only picks it up on reload.
    # Also flip the global `enabled` gate (repo default is false).
    await _enable_acd(pbx, api)
    await api.reload_skill_groups()

    rp = f"ext{tag}"
    pbx.config_builder.add_ivr(rp, f"""\
[ivr]
name = "{rp}"
ivr_mode = "tree"

[ivr.root]
greeting = ""
greeting_text = "external routing"
timeout_ms = 1500
max_retries = 0
timeout_action = {{ type = "queue", target = "{sg}" }}
max_retries_action = {{ type = "queue", target = "{sg}" }}
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
        sg, targets=[f"skill-group:{sg}"], hold_audio="sounds/phone-calling.wav",
    )
    from helpers.config_reload import apply_config
    await apply_config(pbx, api, reload_app=False)
    return rp


async def _drain_agent(pbx, api, timeout: float = 70.0) -> None:
    """Post-call hygiene: some flows leave agent 1001 Busy without a wrapup
    transition (e.g. caller-side attribution on the external-transfer path);
    later suites (presence) cannot force busy->idle. End any active call the
    agent holds, then wait out busy/ringing/wrapup so the agent is reusable.
    """
    async def _agent_status() -> str:
        status, body = await api.raw_request("GET", "/api/cc/agents/1001")
        return ((body or {}).get("status") or "").lower() if status == 200 else ""

    st = await _agent_status()
    if "busy" in st or "ringing" in st:
        # End the agent's active call(s): the BYE triggers wrapup -> idle.
        try:
            status, body = await api.raw_request("GET", "/api/cc/calls")
            calls = (body or {}).get("data") if isinstance(body, dict) else body
            for call in (calls or []):
                if not isinstance(call, dict):
                    continue
                if str(call.get("agent_id", "")) == "1001" or "1001" in str(
                    call.get("caller", "")
                ):
                    cid = call.get("call_id")
                    if cid:
                        await api.raw_request(
                            "POST", f"/api/cc/calls/{cid}/end", {})
        except Exception:
            pass

    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        st = await _agent_status()
        if st in ("idle", "offline", ""):
            return
        await asyncio.sleep(1)


async def _wait_stable_idle(api, timeout: float = 45.0) -> None:
    deadline = asyncio.get_event_loop().time() + timeout
    stable_since = None
    while asyncio.get_event_loop().time() < deadline:
        status, body = await api.raw_request("GET", "/api/cc/agents/1001")
        st = ((body or {}).get("status") or "").lower() if status == 200 else ""
        now = asyncio.get_event_loop().time()
        if st == "idle":
            stable_since = stable_since or now
            if now - stable_since >= 4:
                return
        else:
            stable_since = None
            if "ringing" in st or "busy" in st:
                # Ringing reverts to idle after the ~20s ring timeout; forcing
                # a transition is rejected by the state machine. Just wait.
                pass
            elif st and "offline" not in st:
                # wrapup/away/dnd — actively reset
                try:
                    await api.update_agent_status("1001", "idle")
                except Exception:
                    pass
        await asyncio.sleep(1)


@pytest.mark.asyncio
@pytest.mark.csv_line(193)
async def test_csv_L193_acd_external_agents_order(pbx, sipbot_pool, api, event_checker):
    """External routing: the external system's agent order drives dispatch."""
    router = await ExternalRouterMock().start()
    try:
        router.decision = {"action": "agents", "params": {"agents": ["1001"]}}
        rp = await _setup_external_policy(pbx, api, router.url)

        sipbot_pool.terminate_user("1001")
        agent = sipbot_pool.callee(
            host=pbx.host, port=15480, username="1001", password="123456",
            register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=1, answer_mode="echo", hangup_after=60,
        )
        await asyncio.sleep(3)
        await _wait_stable_idle(api)

        # Full-suite isolation: stale 1001 bots' unregister burst can flip the
        # agent Offline at the dispatch instant; re-establish idle and retry
        # the call once when the first attempt doesn't connect.
        answered = False
        for attempt in range(2):
            caller = sipbot_pool.caller(
                target=f"sip:{rp}@{pbx.sip_addr}",
                username="1002", password="123456", hangup=25,
            )
            ok = await caller.wait_output_async(r"INVITE|200 OK", timeout=20)
            assert ok, f"external-routed call produced no signaling: {caller.output[-300:]}"
            if event_checker.webhook.find("call_answered") is not None:
                answered = True
                break
            deadline = asyncio.get_event_loop().time() + 25
            while asyncio.get_event_loop().time() < deadline:
                if event_checker.webhook.find("call_answered") is not None:
                    answered = True
                    break
                await asyncio.sleep(1)
            if answered:
                break
            await _wait_stable_idle(api)
        assert answered, (
            f"external-routed call never answered; events="
            f"{event_checker.webhook.event_types()}"
        )

        # The external router must have been consulted with full context.
        assert router.requests, "external router was never called"
        payload = router.requests[-1]
        assert payload.get("queue_id"), f"router payload missing queue_id: {payload}"
        assert any(
            c.get("agent_id") == "1001" for c in payload.get("candidates", [])
        ), f"1001 missing from candidates: {payload}"

        assigned = event_checker.webhook.find("skill_group_agent_assigned")
        assert assigned is not None, "skill_group_agent_assigned never arrived"
        reason = (assigned.payload or {}).get("dispatch_reason", "")
        assert reason.startswith("external"), (
            f"dispatch_reason must carry the external provenance, got {reason!r}"
        )
    finally:
        await router.stop()
        await _drain_agent(pbx, api)


@pytest.mark.asyncio
@pytest.mark.csv_line(193)
async def test_csv_L193_acd_external_fallback_on_error(pbx, sipbot_pool, api, event_checker):
    """External routing degraded: router 500 → local fallback strategy dispatches."""
    router = await ExternalRouterMock().start()
    try:
        router.decision = None  # 500 → degrade
        rp = await _setup_external_policy(pbx, api, router.url, fallback="longest_idle")

        sipbot_pool.terminate_user("1001")
        agent = sipbot_pool.callee(
            host=pbx.host, port=15481, username="1001", password="123456",
            register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=1, answer_mode="echo", hangup_after=60,
        )
        await asyncio.sleep(3)
        await _wait_stable_idle(api)

        # Full-suite isolation retry (see agents_order test).
        answered = False
        for attempt in range(2):
            caller = sipbot_pool.caller(
                target=f"sip:{rp}@{pbx.sip_addr}",
                username="1002", password="123456", hangup=25,
            )
            ok = await caller.wait_output_async(r"INVITE|200 OK", timeout=20)
            assert ok, f"fallback-routed call produced no signaling: {caller.output[-300:]}"
            deadline = asyncio.get_event_loop().time() + 25
            while asyncio.get_event_loop().time() < deadline:
                if event_checker.webhook.find("call_answered") is not None:
                    answered = True
                    break
                await asyncio.sleep(1)
            if answered:
                break
            await _wait_stable_idle(api)
        assert answered, (
            f"fallback-routed call never answered; events="
            f"{event_checker.webhook.event_types()}"
        )

        # The assignment event rides the same async webhook pipeline as
        # call_answered and can land milliseconds later — poll briefly.
        assigned = None
        deadline = asyncio.get_event_loop().time() + 10
        while asyncio.get_event_loop().time() < deadline:
            assigned = event_checker.webhook.find("skill_group_agent_assigned")
            if assigned is not None:
                break
            await asyncio.sleep(0.5)
        assert assigned is not None, (
            f"fallback dispatch produced no assignment event; events="
            f"{event_checker.webhook.event_types()}"
        )
        reason = (assigned.payload or {}).get("dispatch_reason", "")
        assert reason.startswith("fallback") or reason.startswith("status_aware_fallback"), (
            f"degraded dispatch must be attributed to the fallback path, got {reason!r}"
        )
    finally:
        await router.stop()
        await _drain_agent(pbx, api)


@pytest.mark.asyncio
@pytest.mark.csv_line(193)
async def test_csv_L193_acd_external_transfer_action(pbx, sipbot_pool, api, event_checker):
    """External routing action: transfer → the dial redirects to the target."""
    router = await ExternalRouterMock().start()
    try:
        router.decision = {
            "action": "transfer",
            "params": {"target": f"sip:1002@{pbx.sip_addr}"},
        }
        rp = await _setup_external_policy(pbx, api, router.url)

        sipbot_pool.terminate_user("1002")
        target_bot = sipbot_pool.callee(
            host=pbx.host, port=15482, username="1002", password="123456",
            register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=1, answer_mode="echo", hangup_after=60,
        )
        await asyncio.sleep(3)

        # Caller 1004 is NOT a CC agent: keeps the caller-side cc-event
        # attribution away from agent 1001 (the transfer target is 1002).
        caller = sipbot_pool.caller(
            target=f"sip:{rp}@{pbx.sip_addr}",
            username="1004", password="123456", hangup=25,
        )
        ok = await caller.wait_output_async(r"INVITE|200 OK", timeout=20)
        assert ok, f"transfer-action call produced no signaling: {caller.output[-300:]}"

        # The transfer target (1002) must receive the INVITE driven by the
        # external system's decision.
        got_invte = await target_bot.wait_output_async(r"INVITE|ringing|200 OK", timeout=25)
        assert got_invte, (
            f"transfer target 1002 never rang: {target_bot.output[-300:]}"
        )
    finally:
        await router.stop()
        await _drain_agent(pbx, api)
