"""Tier 3 — IVR edge case tests.

Verifies max retries, unknown key action, step-mode provider,
PlayAndHangup, and IVR API node.
"""

from __future__ import annotations

import asyncio

import pytest

from helpers.config_reload import apply_config

pytestmark = [pytest.mark.tier3, pytest.mark.ivr]


async def _start_step_provider(handler):
    """Start a scripted step-mode provider on an ephemeral port.

    ``handler(body) -> dict`` maps each incoming provider POST to the ActionNode
    JSON response. Returns ``(url, hits, cleanup)`` where ``hits`` collects every
    request body. The provider URL is injected into the IVR TOML so the live
    PBX reaches it.
    """
    from aiohttp import web

    hits: list[dict] = []

    async def handle(request: web.Request) -> web.Response:
        body = await request.json()
        hits.append(body)
        return web.json_response(handler(body))

    app = web.Application()
    app.router.add_post("/step", handle)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 0)
    await site.start()
    port = site._server.sockets[0].getsockname()[1]
    url = f"http://127.0.0.1:{port}/step"

    async def cleanup():
        await runner.cleanup()

    return url, hits, cleanup


def _step_ivr_toml(name: str, provider_url: str) -> str:
    return f"""\
[ivr]
name = "{name}"
ivr_mode = "step"

[ivr.provider]
url = "{provider_url}"
max_retries = 2
retry_delay_ms = 500
timeout_secs = 5
"""


async def _reg_agent(sipbot_pool, pbx, port, username):
    ua = sipbot_pool.callee(
        host=pbx.host,
        port=port,
        username=username,
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)
    return ua


@pytest.mark.asyncio
async def test_ivr_max_retries_hangup(pbx, api, sipbot_pool, event_checker):
    """IVR max_retries — exceeding max retries triggers fallback action."""
    pbx.config_builder.add_ivr(
        "ivr-retries",
        f"""\
[ivr]
name = "ivr-retries"
ivr_mode = "tree"

[ivr.root]
greeting = ""
greeting_text = "Press a key"
timeout_ms = 2000
max_retries = 1
timeout_action = {{ type = "repeat" }}
max_retries_action = {{ type = "hangup" }}
entries = []
""",
    )
    pbx.config_builder.add_route(
        "ivr-retries-route",
        match={"to.user": "ivr-retries"},
        priority=8,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-retries.toml"},
        auto_answer=True,
    )

    await apply_config(pbx, api)
    caller = sipbot_pool.caller(
        target=f"sip:ivr-retries@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=10,
    )
    answered = await caller.wait_output_async(r"200 OK", timeout=15)
    assert answered
    # Let it timeout and retry
    await asyncio.sleep(6)


@pytest.mark.asyncio
async def test_ivr_play_and_hangup(pbx, api, sipbot_pool, event_checker):
    """IVR PlayAndHangup — plays prompt then hangs up."""
    pbx.config_builder.add_ivr(
        "ivr-pah",
        f"""\
[ivr]
name = "ivr-pah"
ivr_mode = "tree"

[ivr.root]
greeting = ""
greeting_text = "This call will now end"
timeout_ms = 3000
max_retries = 0
timeout_action = {{ type = "play_and_hangup", prompt_text = "Goodbye" }}
entries = []
""",
    )
    pbx.config_builder.add_route(
        "ivr-pah-route",
        match={"to.user": "ivr-pah"},
        priority=7,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-pah.toml"},
        auto_answer=True,
    )

    await apply_config(pbx, api)
    caller = sipbot_pool.caller(
        target=f"sip:ivr-pah@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=8,
    )
    answered = await caller.wait_output_async(r"200 OK", timeout=15)
    assert answered


@pytest.mark.asyncio
async def test_ivr_step_mode_provider(pbx, api, sipbot_pool, event_checker):
    """IVR step mode — external provider receives session_start.

    Replaces the prior version that pointed at a dead port (9999) and only
    verified the call survived a provider timeout. Here a live provider returns
    hangup; the call answers and the provider is contacted with session_start.
    """

    def handler(body):
        return {"type": "hangup"}

    url, hits, cleanup = await _start_step_provider(handler)
    try:
        pbx.config_builder.add_ivr("ivr-step", _step_ivr_toml("ivr-step", url))
        pbx.config_builder.add_route(
            "ivr-step-route",
            match={"to.user": "ivr-step"},
            priority=6,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/ivr-step.toml"},
            auto_answer=True,
        )
        await apply_config(pbx, api)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-step@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=6,
        )
        answered = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
        assert answered, f"call not answered:\n{caller.output[-800:]}"
        await asyncio.sleep(2)
        assert hits, "step provider was never called"
        assert (hits[0].get("event") or {}).get("type") == "session_start", (
            f"expected session_start first, got: {hits[0]}"
        )
    finally:
        await cleanup()


@pytest.mark.asyncio
async def test_ivr_api_node(pbx, api, sipbot_pool, event_checker):
    """IVR Api action (step mode) — provider returns an `api` node.

    `api` is a step-mode-only action (tree mode rejects it), so the prior
    tree-mode wiring never executed it. Here the provider returns an api node
    targeting a mock HTTP endpoint, which must be hit by the PBX.
    """
    from aiohttp import web

    api_hits: list[dict] = []

    async def _start_api_target():
        async def handle(request: web.Request) -> web.Response:
            api_hits.append({"method": request.method})
            return web.json_response({"ok": True})

        app = web.Application()
        app.router.add_get("/", handle)
        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]

        async def cleanup():
            await runner.cleanup()

        return f"http://127.0.0.1:{port}/", cleanup

    api_url, api_cleanup = await _start_api_target()

    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {"type": "api", "url": api_url, "method": "GET", "timeout": 5}
        return {"type": "hangup"}

    url, _hits, cleanup = await _start_step_provider(handler)
    try:
        pbx.config_builder.add_ivr("ivr-api", _step_ivr_toml("ivr-api", url))
        pbx.config_builder.add_route(
            "ivr-api-route",
            match={"to.user": "ivr-api"},
            priority=5,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/ivr-api.toml"},
            auto_answer=True,
        )
        await apply_config(pbx, api)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-api@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=8,
        )
        answered = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
        assert answered, f"call not answered:\n{caller.output[-800:]}"
        # The Api action must fire its HTTP request to the mock endpoint.
        await asyncio.sleep(3)
        assert api_hits, f"Api action did not reach its URL: {api_hits}"
    finally:
        await cleanup()
        await api_cleanup()


@pytest.mark.asyncio
async def test_ivr_unknown_key_action(pbx, api, sipbot_pool, event_checker):
    """IVR unknown_key_action — invalid key triggers fallback."""
    pbx.config_builder.add_ivr(
        "ivr-unknown",
        f"""\
[ivr]
name = "ivr-unknown"
ivr_mode = "tree"

[ivr.root]
greeting = ""
greeting_text = "Press 1 or 2"
timeout_ms = 5000
max_retries = 3
unknown_key_action = {{ type = "repeat" }}

[[ivr.root.entries]]
key = "1"
label = "Option 1"

[ivr.root.entries.action]
type = "hangup"
""",
    )
    pbx.config_builder.add_route(
        "ivr-unknown-route",
        match={"to.user": "ivr-unknown"},
        priority=4,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-unknown.toml"},
        auto_answer=True,
    )

    await apply_config(pbx, api)
    caller = sipbot_pool.caller(
        target=f"sip:ivr-unknown@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=6,
    )
    answered = await caller.wait_output_async(r"200 OK", timeout=15)
    assert answered


@pytest.mark.asyncio
async def test_ivr_jump_ivr(pbx, api, sipbot_pool, event_checker):
    """IVR JumpIvr action (step mode) — jump to another IVR route point.

    `jump_ivr` is step-mode-only (tree mode rejects it). The provider returns a
    jump_ivr node → PBX transfers to ``toivr:ivr-target`` → the target tree-mode
    IVR times out (max_retries=0) and transfers to callee 1002.
    """

    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {"type": "jump_ivr", "route_point": "ivr-target"}
        return {"type": "hangup"}

    url, _hits, cleanup = await _start_step_provider(handler)

    # Target IVR: empty greeting → immediate DTMF wait → timeout → transfer.
    pbx.config_builder.add_ivr(
        "ivr-target",
        """\
[ivr]
name = "ivr-target"
ivr_mode = "tree"

[ivr.root]
greeting = ""
greeting_text = ""
timeout_ms = 1000
max_retries = 0
max_retries_action = { type = "transfer", target = "1002" }
entries = []
""",
    )
    pbx.config_builder.add_route(
        "ivr-target-route",
        match={"to.user": "ivr-target"},
        priority=4,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-target.toml"},
        auto_answer=True,
    )
    pbx.config_builder.add_ivr("ivr-jump", _step_ivr_toml("ivr-jump", url))
    pbx.config_builder.add_route(
        "ivr-jump-route",
        match={"to.user": "ivr-jump"},
        priority=3,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-jump.toml"},
        auto_answer=True,
    )

    await apply_config(pbx, api)

    callee = await _reg_agent(sipbot_pool, pbx, 15132, "1002")
    try:
        caller = sipbot_pool.caller(
            target=f"sip:ivr-jump@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=18,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
        # jump_ivr → ivr-target → timeout → transfer to 1002.
        assert await callee.wait_output_async(r"200 OK|Call established", timeout=25), (
            f"callee 1002 never received the jumped call:\n{callee.output[-1500:]}"
        )
    finally:
        await cleanup()


@pytest.mark.asyncio
async def test_ivr_route_to_agent(pbx, api, sipbot_pool, event_checker):
    """IVR RouteToAgent action (step mode) — route to a registered agent.

    `route_to_agent` is step-mode-only (tree mode rejects it). The provider
    returns a route_to_agent node → PBX transfers to target 1002 (setting
    route_node_value/channel_code for downstream routing) → agent answers.
    """
    agent = await _reg_agent(sipbot_pool, pbx, 15133, "1002")

    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {
                "type": "route_to_agent",
                "target": "1002",
                "skill_group_id": "support",
                "channel_code": "web",
            }
        return {"type": "hangup"}

    url, _hits, cleanup = await _start_step_provider(handler)
    try:
        pbx.config_builder.add_ivr("ivr-rta", _step_ivr_toml("ivr-rta", url))
        pbx.config_builder.add_route(
            "ivr-rta-route",
            match={"to.user": "ivr-rta"},
            priority=2,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/ivr-rta.toml"},
            auto_answer=True,
        )
        # reload_app=False: the full restart would wipe agent 1002's SIP
        # registration (registered above), so the routed INVITE would find
        # no contact and never reach the agent.
        await apply_config(pbx, api, reload_app=False)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-rta@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=14,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
        # route_to_agent transferred to 1002 → agent answers.
        assert await agent.wait_output_async(r"200 OK|Call established", timeout=25), (
            f"agent 1002 never received the routed call:\n{agent.output[-1500:]}"
        )
    finally:
        await cleanup()
