"""CC quick-route feature codes (`*81<sg-id>` / `*82<ivr-name>`) — SIP e2e.

Zero-configuration dialing: no route rules and no queue definitions exist for
these targets — the `CcQuickRouteInspector` (post-route fallback) must route
`*81support` into the skill group's ACD and `*82<ivr-name>` into the IVR app.
Also covers the desk-delivery enrichment: pinned quick contacts for typed
targets are delivered with the current feature code and filled labels.
"""

from __future__ import annotations

import asyncio
import json

import pytest
import pytest_asyncio

import helpers as h

pytestmark = [pytest.mark.tier3]

API_BASE = "/api/cc"


@pytest_asyncio.fixture
async def cc_api(pbx, webhook_server):
    """CC session with a FILE database (extras shared console ↔ addon)."""
    pbx.config_builder.database_url = f"sqlite://{pbx.work_dir}/cc-quick-route.db?mode=rwc"
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


async def test_feature_code_dials_skill_group_acd(pbx, webhook_server, cc_api, sipbot_pool, evidence):
    """`*81support` with NO route/queue config → ACD dispatches to the
    registered support agent."""
    agent = sipbot_pool.callee(
        host=pbx.host,
        port=15100,
        username="1001",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    assert agent, "agent bot failed to start"
    await asyncio.sleep(2)
    webhook_server.receiver.clear()

    caller = sipbot_pool.caller(
        target=f"sip:*81support@{pbx.sip_addr}",
        username="1002",
        password="123456",
        hangup=8,
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, (
        "feature code *81support must reach the skill group ACD (agent answers). "
        f"Output:\n{caller.output[-500:]}"
    )
    finished = await caller.wait_output_async(r"All bots finished", timeout=20)
    assert finished, f"caller bot did not finish cleanly. Output:\n{caller.output[-300:]}"

    # ACD dispatch evidence: the call entered the skill-group queue and a
    # support agent was assigned (webhook events, mirrored from RWI).
    deadline = asyncio.get_event_loop().time() + 10
    assigned = []
    while asyncio.get_event_loop().time() < deadline:
        assigned = [
            e for e in webhook_server.receiver.all_events()
            if e.event_type == "skill_group_agent_assigned"
        ]
        if assigned:
            break
        await asyncio.sleep(0.5)
    assert assigned, (
        "skill_group_agent_assigned event missing — feature code did not dispatch. "
        f"Events: {[e.event_type for e in webhook_server.receiver.all_events()]}"
    )
    payload = assigned[0].payload
    evidence.log_metric("quick_route_sg", {
        "agent": payload.get("agent_id"),
        "skill_group": payload.get("skill_group_id"),
        "events": [e.event_type for e in webhook_server.receiver.all_events()],
    })


async def test_feature_code_dials_ivr(pbx, cc_api, sipbot_pool, evidence):
    """`*82<ivr-name>` with NO route config → the IVR app answers."""
    caller = sipbot_pool.caller(
        target=f"sip:*82ivr-test@{pbx.sip_addr}",
        username="1002",
        password="123456",
        hangup=8,
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, (
        "feature code *82ivr-test must start the IVR app (auto-answered). "
        f"Output:\n{caller.output[-500:]}"
    )
    evidence.log_metric("quick_route_ivr", "answered")


async def test_unknown_feature_code_target_fails(pbx, cc_api, sipbot_pool, evidence):
    """`*81<unknown>` falls through the inspector → normal offline handling
    (4xx), never a silent hang."""
    caller = sipbot_pool.caller(
        target=f"sip:*81no-such-group@{pbx.sip_addr}",
        username="1002",
        password="123456",
        hangup=8,
    )
    rejected = await caller.wait_output_async(r"480|404|486|603|500", timeout=15)
    assert rejected, (
        f"unknown feature-code target must be rejected with 4xx. Output:\n{caller.output[-500:]}"
    )
    evidence.log_metric("unknown_target", "rejected")


async def test_quick_contacts_delivered_with_feature_codes(cc_api, evidence):
    """Pinned skill-group / IVR quick contacts are delivered with the current
    feature-code dial targets and filled labels (restsend-call contract)."""
    agent_id = "1001"
    payload = {
        "skill_group_id": "support",
        "display_name": "售后支持组",
        "skills_required": ["support"],
        "extra": {
            "desk": {
                "type": "text",
                "value": json.dumps({
                    "transfer": {
                        "quickContacts": [
                            {"agentId": "support", "type": "skill_group"},
                            {"agentId": "ivr-test", "type": "ivr"},
                            {"agentId": "1002", "label": "同事李四", "number": "1002"},
                        ]
                    }
                }),
            }
        },
    }
    resp = await cc_api.put(f"{API_BASE}/skill-groups/support", payload)
    assert resp.get("success") is True or resp.get("skill_group_id"), f"sg update failed: {resp}"

    config = await cc_api.get(f"{API_BASE}/agents/{agent_id}/config")
    desk = config.get("desk") if isinstance(config.get("desk"), dict) else {}
    qc = ((desk.get("transfer") or {}).get("quickContacts")) or []
    by_id = {c.get("agentId"): c for c in qc}

    sg = by_id.get("support")
    assert sg, f"skill-group quick contact missing: {qc}"
    assert sg.get("number") == "*81support", f"feature code not generated: {sg}"
    assert sg.get("label") == "售后支持组", f"label not filled: {sg}"
    assert sg.get("type") == "skill_group", f"type missing: {sg}"

    ivr = by_id.get("ivr-test")
    assert ivr, f"ivr quick contact missing: {qc}"
    assert ivr.get("number") == "*82ivr-test", f"ivr feature code not generated: {ivr}"
    assert ivr.get("label") == "ivr-test", f"ivr label fallback missing: {ivr}"

    agent_row = by_id.get("1002")
    assert agent_row and agent_row.get("number") == "1002", f"agent row altered: {agent_row}"
    evidence.log_metric("delivered_qc", [c.get("number") for c in qc])
