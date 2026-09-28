"""CC quick-route feature codes (`*81<sg-id>` / `*82<ivr-name>` /
`*83<room-id>`) — SIP e2e.

Zero-configuration dialing: no route rules and no queue definitions exist for
these targets — the `CcQuickRouteInspector` (post-route fallback) must route
`*81support` into the skill group's ACD, `*82<ivr-name>` into the IVR app and
`*83<room-id>` into a (created-on-demand) conference room. Also covers the
desk-delivery enrichment: pinned quick contacts for typed targets are
delivered with the current feature code and filled labels.
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


async def test_feature_code_joins_conference_and_room_auto_ends(pbx, cc_api,
                                                                sipbot_pool,
                                                                evidence):
    """`*83<room-id>` with NO route config → the conference app answers and
    the caller lands in the created-on-demand room (visible as a live target
    in /transfer-targets). After the last participant leaves the room is
    auto-destroyed — 无人自动结束."""
    room_id = "room-e2e"
    caller = sipbot_pool.caller(
        target=f"sip:*83{room_id}@{pbx.sip_addr}",
        username="1002",
        password="123456",
        hangup=6,
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, (
        f"feature code *83{room_id} must start the conference app (auto-answered). "
        f"Output:\n{caller.output[-500:]}"
    )

    # While the call is up, the room shows up as a live transfer-target
    # candidate with its current feature-code number.
    async def _room_present():
        resp = await cc_api.get(f"{API_BASE}/transfer-targets")
        confs = resp.get("conferences") or []
        return next((c for c in confs if c.get("id") == room_id), None)

    deadline = asyncio.get_event_loop().time() + 10
    entry = None
    while asyncio.get_event_loop().time() < deadline:
        entry = await _room_present()
        if entry:
            break
        await asyncio.sleep(0.5)
    assert entry, (
        f"live room '{room_id}' missing from /transfer-targets. "
        f"Output:\n{caller.output[-300:]}"
    )
    assert entry.get("number") == f"*83{room_id}", f"feature code not generated: {entry}"
    evidence.log_metric("quick_route_conference", {
        "room": room_id,
        "participants": entry.get("participants"),
    })

    # Caller hangs up (hangup=6) → the last participant leaves → the room is
    # auto-destroyed (ConferenceManager empty-room check, not the watchdog).
    finished = await caller.wait_output_async(r"All bots finished", timeout=20)
    assert finished, f"caller bot did not finish cleanly. Output:\n{caller.output[-300:]}"

    deadline = asyncio.get_event_loop().time() + 10
    gone = False
    while asyncio.get_event_loop().time() < deadline:
        if (await _room_present()) is None:
            gone = True
            break
        await asyncio.sleep(0.5)
    assert gone, (
        f"room '{room_id}' must be auto-destroyed once everyone left "
        "(无人自动结束)."
    )
    evidence.log_metric("conference_auto_end", f"{room_id} destroyed after last leave")


async def test_bare_conference_code_joins_personal_room(pbx, cc_api, sipbot_pool,
                                                        evidence):
    """Bare `*83` from internal ext 1002 joins the caller's personal meet
    room `1002` (identical to `*831002`) — the room is created on demand; a
    colleague dialing `*831002` lands in the SAME room; once everyone leaves
    the room is auto-destroyed."""
    room_id = "1002"

    owner = sipbot_pool.caller(
        target=f"sip:*83@{pbx.sip_addr}",
        username="1002",
        password="123456",
        hangup=14,
    )
    ok = await owner.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, (
        "bare feature code *83 must answer into the caller's personal room. "
        f"Output:\n{owner.output[-500:]}"
    )

    async def _room_entry():
        resp = await cc_api.get(f"{API_BASE}/transfer-targets")
        confs = resp.get("conferences") or []
        return next((c for c in confs if c.get("id") == room_id), None)

    deadline = asyncio.get_event_loop().time() + 10
    entry = None
    while asyncio.get_event_loop().time() < deadline:
        entry = await _room_entry()
        if entry:
            break
        await asyncio.sleep(0.5)
    assert entry, f"personal room '{room_id}' missing from /transfer-targets"
    assert entry.get("number") == f"*83{room_id}", f"feature code mismatch: {entry}"

    # A colleague explicitly dials `*831002` and joins the SAME room.
    joiner = sipbot_pool.caller(
        target=f"sip:*831002@{pbx.sip_addr}",
        username="1003",
        password="123456",
        hangup=8,
    )
    ok2 = await joiner.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok2, f"colleague *831002 must join room {room_id}. Output:\n{joiner.output[-500:]}"

    deadline = asyncio.get_event_loop().time() + 8
    participants = entry.get("participants")
    while asyncio.get_event_loop().time() < deadline:
        entry = await _room_entry()
        if entry and entry.get("participants") == 2:
            participants = 2
            break
        await asyncio.sleep(0.4)
    assert participants == 2, (
        f"both callers must be in room '{room_id}' (participants={participants})"
    )
    evidence.log_metric("bare_conference", {"room": room_id, "participants": 2})

    finished_owner = await owner.wait_output_async(r"All bots finished", timeout=25)
    finished_joiner = await joiner.wait_output_async(r"All bots finished", timeout=25)
    assert finished_owner and finished_joiner, (
        f"bots did not finish cleanly.\nowner:{owner.output[-300:]}\n"
        f"joiner:{joiner.output[-300:]}"
    )

    deadline = asyncio.get_event_loop().time() + 10
    gone = False
    while asyncio.get_event_loop().time() < deadline:
        if (await _room_entry()) is None:
            gone = True
            break
        await asyncio.sleep(0.5)
    assert gone, f"personal room '{room_id}' must auto-destroy after everyone left"
    evidence.log_metric("bare_conference_auto_end", f"{room_id} destroyed")


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
                            {"agentId": "standup", "type": "conference"},
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

    conf = by_id.get("standup")
    assert conf, f"conference quick contact missing: {qc}"
    assert conf.get("number") == "*83standup", f"conference feature code not generated: {conf}"
    assert conf.get("label") == "standup", f"conference label fallback missing: {conf}"

    agent_row = by_id.get("1002")
    assert agent_row and agent_row.get("number") == "1002", f"agent row altered: {agent_row}"
    evidence.log_metric("delivered_qc", [c.get("number") for c in qc])


async def test_quick_contacts_merge_manual_targets(cc_api, evidence):
    """Group + agent quickContacts merge: union deduped per target with the
    agent's own entry winning; manually pinned extension numbers (which may
    not exist in the system) and SIP URIs are delivered with their dial
    `number` untouched by the feature-code enrichment."""
    group_payload = {
        "skill_group_id": "support",
        "display_name": "售后支持组",
        "skills_required": ["support"],
        "extra": {
            "desk": {
                "type": "text",
                "value": json.dumps({
                    "transfer": {
                        "quickContacts": [
                            {"agentId": "1002", "label": "前台", "number": "1002"},
                            {"agentId": "2001", "label": "Helpdesk", "number": "2001", "type": "extension"},
                        ]
                    }
                }),
            }
        },
    }
    resp = await cc_api.put(f"{API_BASE}/skill-groups/support", group_payload)
    assert resp.get("success") is True or resp.get("skill_group_id"), f"sg update failed: {resp}"

    agent = await cc_api.get(f"{API_BASE}/agents/1001")
    skills = agent.get("skills")
    if isinstance(skills, dict):
        skills = skills.get("list") or ["support", "sales"]
    resp = await cc_api.put(f"{API_BASE}/agents/1001", {
        "display_name": agent.get("display_name") or "1001",
        "skills": skills or ["support", "sales"],
        "extra": {
            "desk": {
                "type": "text",
                "value": json.dumps({
                    "transfer": {
                        "quickContacts": [
                            # Same (agent, 1002) key as the group entry — wins.
                            {"agentId": "1002", "label": "李四", "number": "1002"},
                            # Same extension id, different kind → distinct key.
                            {"agentId": "2001", "label": "Helpdesk-L2", "number": "2002", "type": "extension"},
                            # Manual SIP URI — dialed verbatim.
                            {"agentId": "sip:bob@example.com", "label": "Bob URI",
                             "number": "sip:bob@example.com", "type": "sip"},
                        ]
                    }
                }),
            }
        },
    })
    assert resp.get("success") is True or resp.get("agent_id"), f"agent update failed: {resp}"

    config = await cc_api.get(f"{API_BASE}/agents/1001/config")
    desk = config.get("desk") if isinstance(config.get("desk"), dict) else {}
    qc = ((desk.get("transfer") or {}).get("quickContacts")) or []
    by_key = {(c.get("type") or "agent", c.get("agentId")): c for c in qc}

    assert len(qc) == len(by_key), f"duplicate quick contacts delivered: {qc}"
    assert len(qc) == 3, f"unexpected merge result: {qc}"

    deduped = by_key[("agent", "1002")]
    assert deduped.get("label") == "李四", f"agent entry must win the dedup: {deduped}"

    ext = by_key[("extension", "2001")]
    assert ext.get("number") == "2002", f"manual extension altered: {ext}"
    assert ext.get("label") == "Helpdesk-L2", f"agent extension entry must win: {ext}"

    sip = by_key[("sip", "sip:bob@example.com")]
    assert sip.get("number") == "sip:bob@example.com", f"sip uri altered: {sip}"
    evidence.log_metric("merged_qc", [f"{t}:{i}" for t, i in by_key])
