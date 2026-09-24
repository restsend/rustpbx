"""CC quick-route via SIP REFER + outbound-permission enforcement — SIP e2e.

P5: an agent's phone sends an in-dialog REFER with Refer-To `*81support` /
`*82main` — the call transfers into the skill-group ACD / IVR app with zero
route/queue configuration (the REFER hand-off resolves the feature code to an
internal transfer target before any dialing).

P6: outbound-permission enforcement — an agent bound to an outbound policy
can only dial the policy's line prefixes / destination prefixes; violations
are rejected with 403 on the INVITE path (a forward route for the prefix
gives the policy a trunk-bound outcome to inspect).
"""

from __future__ import annotations

import asyncio
import json
import pathlib

import pytest
import pytest_asyncio

import helpers as h
from helpers.restsend_agent import RestsendAgent

pytestmark = [pytest.mark.tier3]

API_BASE = "/api/cc"


def pbx_work_dir() -> pathlib.Path:
    from helpers.pbx_server import find_project_root
    return find_project_root(pathlib.Path(__file__)) / "target" / "e2e-cc-regression"


@pytest_asyncio.fixture
async def cc_api(pbx, webhook_server):
    pbx.config_builder.database_url = f"sqlite://{pbx.work_dir}/cc-qr-transfer.db?mode=rwc"
    h.boot_pbx(pbx, webhook_url=webhook_server.url)

    import aiohttp
    from helpers.pbx_server import PbxApiClient

    session = aiohttp.ClientSession()
    client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
    assert await client.ensure_console_auth(), "console superuser auth failed"
    seed = await client.seed_default_agents()
    assert seed.get("agents", 0) >= 1, f"agent seeding failed: {seed}"
    yield client
    await session.close()


def _register_callee(sipbot_pool, pbx, username: str, port: int):
    bot = sipbot_pool.callee(
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
    assert bot, f"{username} bot failed to start"
    return bot


def _wait_webhook(webhook_server, event_type: str, predicate=None, timeout: float = 20.0):
    async def poll():
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while loop.time() < deadline:
            for e in webhook_server.receiver.all_events():
                if e.event_type == event_type and (predicate is None or predicate(e)):
                    return e
            await asyncio.sleep(0.4)
        return None
    return poll


async def test_refer_transfers_into_skill_group_acd(pbx, webhook_server, cc_api, sipbot_pool, evidence):
    """In-dialog REFER to `sip:*81support@…` → blind transfer dials the
    feature code → quick-route resolver → ACD dispatch."""
    # 1001 = the transfer recipient (answers the initial call); 1003 = idle
    # support agent the ACD dispatches to after the transfer.
    _register_callee(sipbot_pool, pbx, "1001", 15110)
    _register_callee(sipbot_pool, pbx, "1003", 15113)
    await asyncio.sleep(2)
    webhook_server.receiver.clear()

    agent = RestsendAgent(pbx, "1002", local_port=25121)
    await agent.start()
    try:
        assert await agent.register(expires=120), "1002 REGISTER failed"
        await agent.publish_idle()
        await agent.cmd({"cmd": "sip_call",
                         "remote_uri": f"sip:1001@{pbx.sip_addr}"})
        connected = await agent.wait_event(
            "state_changed", predicate=lambda e: e.get("name") == "connected", timeout=15)
        assert connected, f"call to 1001 not connected:\n{agent.stderr_text()[-400:]}"

        # Blind-transfer the call into the support skill group.
        await agent.cmd({"cmd": "sip_refer",
                         "refer_to": f"sip:*81support@{pbx.sip_addr}"})

        ev = await _wait_webhook(webhook_server, "skill_group_agent_assigned",
                                 predicate=lambda e: (
                                     (e.payload or {}).get("skill_group_id") == "support"
                                 ), timeout=20)()
        assert ev, (
            "transferred call must enter the support ACD. Events: "
            f"{webhook_server.receiver.event_types()}"
        )
        evidence.log_metric("refer_sg", {"agent": (ev.payload or {}).get("agent_id")})
    finally:
        await agent.stop()


async def test_refer_transfers_into_ivr(pbx, webhook_server, cc_api, sipbot_pool, evidence):
    """In-dialog REFER to `sip:*82ivr-test@…` → the IVR app runs the
    transferred call."""
    _register_callee(sipbot_pool, pbx, "1001", 15111)
    await asyncio.sleep(2)
    webhook_server.receiver.clear()

    agent = RestsendAgent(pbx, "1002", local_port=25122)
    await agent.start()
    try:
        assert await agent.register(expires=120), "1002 REGISTER failed"
        await agent.publish_idle()
        await agent.cmd({"cmd": "sip_call",
                         "remote_uri": f"sip:1001@{pbx.sip_addr}"})
        connected = await agent.wait_event(
            "state_changed", predicate=lambda e: e.get("name") == "connected", timeout=15)
        assert connected, f"call to 1001 not connected:\n{agent.stderr_text()[-400:]}"

        await agent.cmd({"cmd": "sip_refer",
                         "refer_to": f"sip:*82ivr-test@{pbx.sip_addr}"})

        ivr_ev = await _wait_webhook(webhook_server, "ivr_node_entered", timeout=20)()
        assert ivr_ev, (
            "transferred call must execute the IVR flow. Events: "
            f"{webhook_server.receiver.event_types()}"
        )
        completed = await _wait_webhook(webhook_server, "ivr_flow_completed", timeout=25)()
        assert completed, "IVR flow should run to completion. Events: " + str(
            webhook_server.receiver.event_types()
        )
        evidence.log_metric("refer_ivr", "ivr flow executed")
    finally:
        await agent.stop()


# ── P6: outbound-permission enforcement ─────────────────────────────────────


@pytest_asyncio.fixture
async def perm_pbx(webhook_server):
    """Dedicated PBX (own ports + file DB) that boots WITH a forward route
    for the policy-governed prefix 9 — route config is boot-time only, so
    the permission test needs its own instance (the session pbx may already
    be running and cannot pick up added routes)."""
    import socket as _socket

    from helpers.pbx_server import PbxServer

    def free_port():
        s = _socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        return port

    server = PbxServer(
        host="127.0.0.1",
        sip_port=free_port(),
        http_port=free_port(),
        rwi_token="perm-token",
        work_dir=pbx_work_dir() / "cc-perm",
    )
    server.config_builder.database_url = (
        f"sqlite://{server.work_dir}/cc-perm.db?mode=rwc"
    )
    # Outbound route for the policy-governed prefix 9 — gives the policy
    # enforcement a Forward (trunk-bound) outcome to inspect. The fake trunk
    # destination never completes, which is fine: policy denial (403) happens
    # before dialing.
    server.config_builder.add_route(
        "perm-outbound",
        match={"to.user": "^9"},
        priority=5,
        action="forward",
        dest="sip:fake-trunk.invalid:5060",
    )
    h.boot_pbx(server, webhook_url=webhook_server.url)
    yield server
    server.stop()


def sipbot_pool_dial(pbx, number: str, port: int):
    """Standalone sipbot caller (no pool fixture — the perm_pbx fixture owns
    its own server)."""
    from helpers.sipbot import SipBotProcess

    p = SipBotProcess(name=f"caller-{number}")
    p.start_caller(
        target=f"sip:{number}@{pbx.sip_addr}",
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        hangup=6,
        addr=f"127.0.0.1:{port}",
    )
    return p


async def test_outbound_policy_blocks_and_allows(perm_pbx, webhook_server, evidence):
    """Agent 1002 bound to a lines policy: destination-prefix violations are
    rejected with 403 on the INVITE; permitted destinations pass the policy
    (and fail later at routing with a non-403 code — the fake trunk never
    completes)."""
    import aiohttp
    from helpers.pbx_server import PbxApiClient

    session = aiohttp.ClientSession()
    try:
        cc_api = PbxApiClient(session, perm_pbx.http_url, perm_pbx.rwi_token)
        assert await cc_api.ensure_console_auth(), "console auth failed"
        seed = await cc_api.seed_default_agents()
        assert seed.get("agents", 0) >= 1, f"agent seeding failed: {seed}"

        policy = {
            "name": "perm-policy",
            "policy": {
                "enabled": True,
                "lines": [
                    {"id": "mobile", "label": "Mobile", "prefix": "9", "caller": "self",
                     "default": True, "allowed_prefixes": ["1"]},
                    {"id": "ld", "label": "LD", "prefix": "0", "caller": "02188886666",
                     "allowed_prefixes": ["0"]},
                ],
            },
            "agents": ["1002"],
        }
        resp = await cc_api.post(f"{API_BASE}/outbound-policies", policy)
        assert resp.get("success") is True, f"policy create failed: {resp}"

        # Delivery: the bound agent's desk.lines ARE the policy rules.
        config = await cc_api.get(f"{API_BASE}/agents/1002/config")
        desk = config.get("desk") if isinstance(config.get("desk"), dict) else {}
        lines = desk.get("lines") or []
        assert [l.get("id") for l in lines] == ["mobile", "ld"], f"policy lines missing: {lines}"

        # 1) Violation: prefix 9 matches the line, destination "00211" is
        #    outside allowed_prefixes ["1"] → 403 on the INVITE.
        viol = sipbot_pool_dial(perm_pbx, "900211", 25441)
        got = await viol.wait_output_async(r"403", timeout=15)
        assert got, f"out-of-policy destination must be 403. Output:\n{viol.output[-500:]}"
        evidence.log_metric("denied", "900211 → 403")

        # 2) Within policy: prefix 9 + destination 138… (starts with "1")
        #    passes the policy — failure later is plain routing (NOT 403).
        allowed = sipbot_pool_dial(perm_pbx, "91380013800", 25442)
        failed = await allowed.wait_output_async(r"404|480|486|500|503|408", timeout=15)
        out = allowed.output[-800:]
        assert failed or "All bots finished" in out, (
            f"permitted destination should fail at routing (no trunk), got:\n{out}"
        )
        assert "403" not in out, f"permitted destination must NOT be 403:\n{out}"
        evidence.log_metric("allowed", "91380013800 → non-403 failure (no trunk)")
    finally:
        await session.close()
