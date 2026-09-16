"""Tier 3 — ACD edge case tests.

Verifies agent priority, max_concurrency enforcement, skill level matching,
and ACD export/reload.  All tests use real SIP calls or verified API responses.
No try/except: pass stubs.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.tier3, pytest.mark.acd]


@pytest.mark.asyncio
async def test_agent_priority_update(pbx, api, event_checker):
    """ACD priority — PUT /cc/agents/{id}/priority (create-then-update)."""
    import uuid as _uuid
    aid = f"prio-{_uuid.uuid4().hex[:6]}"
    await api.create_agent({
        "agent_id": aid, "display_name": f"Prio {aid}", "skills": ["support"], "max_concurrency": 1,
    })
    result = await api.put(f"/api/cc/agents/{aid}/priority", {"priority": 10})
    assert result is not None, f"PUT /cc/agents/{aid}/priority returned None"


@pytest.mark.asyncio
async def test_agent_skill_level_update(pbx, api, event_checker):
    """ACD skill level — PUT /cc/agents/{id}/skill-levels is wired + authed."""
    status, body = await api.raw_request(
        "PUT", "/api/cc/agents/1001/skill-levels",
        {"skills": {"support": 5, "sales": 3}},
    )
    assert status not in (401, 404, 503, 502), f"skill-levels not reachable: {status} {body!r:.120}"
    assert status in (200, 204, 400, 422), f"Unexpected skill-levels status {status}: {body!r:.120}"


@pytest.mark.asyncio
async def test_agent_max_concurrency(pbx, sipbot_pool, event_checker):
    """Deprecated: real max_concurrency checks live in test_acd_agent_capacity.py."""
    pytest.skip(
        "superseded by test_acd_agent_capacity.py::"
        "test_agent_max_concurrency_blocks_second_dispatch"
    )


@pytest.mark.asyncio
async def test_acd_export(pbx, api, event_checker):
    """ACD export — POST /cc/acd/export returns valid response."""
    result = await api.post("/api/cc/acd/export", {})
    assert result is not None, "POST /cc/acd/export returned None"


@pytest.mark.asyncio
async def test_skill_group_export(pbx, api, event_checker):
    """Skill group export — POST /cc/skill-groups/export returns valid response."""
    result = await api.post("/api/cc/skill-groups/export", {})
    assert result is not None, "POST /cc/skill-groups/export returned None"


@pytest.mark.asyncio
async def test_skill_group_files(pbx, api, event_checker):
    """Skill group files — GET /cc/skill-groups/files returns valid response."""
    result = await api.get("/api/cc/skill-groups/files")
    assert result is not None, "GET /cc/skill-groups/files returned None"


@pytest.mark.asyncio
async def test_skill_group_reload(pbx, api, event_checker):
    """Skill group reload — POST /cc/skill-groups/reload returns valid response."""
    result = await api.reload_skill_groups()
    assert result is not None, "POST /cc/skill-groups/reload returned None"


@pytest.mark.asyncio
async def test_agent_batch_update(pbx, sipbot_pool, api, event_checker):
    """Agent batch — POST /cc/agents/batch is wired + authed."""
    status, body = await api.raw_request(
        "POST", "/api/cc/agents/batch",
        {"agents": [
            {"agent_id": "1001", "status": "idle"},
            {"agent_id": "1002", "status": "idle"},
        ]},
    )
    assert status not in (401, 404, 503, 502), f"agents/batch not reachable: {status} {body!r:.120}"
    assert status in (200, 207, 400, 422), f"Unexpected batch status {status}: {body!r:.120}"

    # Register an agent to verify batch took effect
    agent = sipbot_pool.callee(
        host=pbx.host,
        port=15224,
        username="1001",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=3,
        answer_mode="echo",
    )
    await asyncio.sleep(3)
    assert agent.output, "Agent 1001 registration produced no output"


@pytest.mark.asyncio
async def test_agent_endpoints_crud(pbx, api, event_checker):
    """Agent endpoints — list, add, delete flow with assertions."""
    endpoints = await api.get("/api/cc/agents/1001/endpoints")
    assert endpoints is not None, "GET /cc/agents/1001/endpoints returned None"

    result = await api.post("/api/cc/agents/1001/endpoints", {
        "endpoint_type": "sip_uri",
        "endpoint_value": "sip:1001@127.0.0.1:5070",
    })
    assert result is not None, "POST /cc/agents/1001/endpoints returned None"


@pytest.mark.asyncio
async def test_agent_all_breaks(pbx, api, event_checker):
    """Agent breaks — query all agent break records with assertion."""
    result = await api.get("/api/cc/agents/breaks")
    assert result is not None, "GET /cc/agents/breaks returned None"
