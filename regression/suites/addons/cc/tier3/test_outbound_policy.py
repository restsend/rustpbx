"""Per-agent outbound policy (outbound_policy.rs) — REST e2e.

Storage note: desk lines / policies live in `cc_entity_extras` of the APP
database. With the default `sqlite::memory:` the console connection and the
addon connection see different databases, so these tests boot a file-backed
DB (mirroring production) before creating resources.

Strict: policy create/get round-trip; desk lines management list; delivery
filtering via `GET /api/cc/agents/{id}/config` — non-whitelisted caller-id
lines must never reach the agent; no policy reference = unconstrained.
"""

from __future__ import annotations

import pytest
import pytest_asyncio

import helpers as h
from helpers import assertions as A

pytestmark = [pytest.mark.tier3]

POLICY = "pol-e2e-caller"
API_BASE = "/api/cc"


@pytest_asyncio.fixture
async def cc_api(pbx, webhook_server):
    """CC session with a FILE database so entity-extras writes are shared
    between the console and the addon connections."""
    pbx.config_builder.database_url = f"sqlite://{pbx.work_dir}/cc-policy-e2e.db?mode=rwc"
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


def _unwrap(item):
    return item.get("line") if isinstance(item, dict) and isinstance(item.get("line"), dict) else (item or {})


async def test_outbound_policy_filters_desk_lines(cc_api, evidence):
    agent_id = "1001"
    # 0) baseline: clean slate for our line ids
    for lid in ("line-ok", "line-bad"):
        try:
            await cc_api.delete(f"{API_BASE}/desk/lines/{lid}")
        except Exception:
            pass

    # 1) create a caller-id-constrained policy (round-trip strict)
    created = await cc_api.post(f"{API_BASE}/outbound-policies", {
        "name": POLICY,
        "policy": {"enabled": True, "allowed_caller_ids": [agent_id]},
    })
    assert created.get("success") is True, f"policy create failed: {created}"
    got = await cc_api.get(f"{API_BASE}/outbound-policies/{POLICY}")
    body = got.get("policy") or got
    assert (body.get("allowed_caller_ids") or []) == [agent_id], (
        f"policy round-trip mismatch: {str(got)[:300]}"
    )

    # 2) two desk lines: whitelisted caller vs outside the whitelist
    for line in ({"id": "line-ok", "label": "allowed", "caller": agent_id, "enabled": True},
                 {"id": "line-bad", "label": "forbidden", "caller": "ghost-caller", "enabled": True}):
        resp = await cc_api.post(f"{API_BASE}/desk/lines", {"line": line})
        assert resp.get("success") is True, f"line create failed: {resp}"

    # 3) management plane: both lines listed (strict)
    listing = await cc_api.get(f"{API_BASE}/desk/lines")
    listed = listing.get("data") if isinstance(listing, dict) else listing
    listed_ids = [_unwrap(l).get("id") for l in (listed or [])]
    assert {"line-ok", "line-bad"} <= set(listed_ids), f"management list incomplete: {listed_ids}"

    # 4) attach the policy to the agent
    agent = await cc_api.get(f"{API_BASE}/agents/{agent_id}")
    await cc_api.put(f"{API_BASE}/agents/{agent_id}", {
        "display_name": agent.get("display_name") or agent_id,
        "skills": ["support"],
        "extra": {"outbound_policy": {"type": "text", "value": POLICY}},
    })

    # 5) delivery: config.lines carries ONLY the whitelisted line
    config = await cc_api.get(f"{API_BASE}/agents/{agent_id}/config")
    assert isinstance(config, dict), f"config must be an object: {str(config)[:200]}"
    desk_obj = config.get("desk") if isinstance(config.get("desk"), dict) else {}
    lines = config.get("lines") or desk_obj.get("lines")
    assert isinstance(lines, list), (
        f"N5 regression: config.lines missing — delivery store still not unified? "
        f"config keys: {sorted(config.keys())}"
    )
    ids = [l.get("id") for l in lines]
    assert "line-ok" in ids, f"allowed line filtered out: {ids}"
    assert "line-bad" not in ids, (
        f"POLICY: non-whitelisted caller-id line delivered to constrained agent: {ids}"
    )
    evidence.log_metric("policy_lines", ids)

    # 6) cleanup
    await cc_api.put(f"{API_BASE}/agents/{agent_id}", {
        "display_name": agent.get("display_name") or agent_id,
        "skills": ["support"],
        "extra": {},
    })
    for lid in ("line-ok", "line-bad"):
        await cc_api.delete(f"{API_BASE}/desk/lines/{lid}")
    await cc_api.delete(f"{API_BASE}/outbound-policies/{POLICY}")


async def test_outbound_policy_unconstrained_without_reference(cc_api, evidence):
    """未引用策略的坐席 = 无约束（向后兼容）：任何 caller-id 的线路都可达。"""
    agent_id = "1002"
    line_open = {"id": "line-open", "label": "open", "caller": "anyone", "enabled": True}
    resp = await cc_api.post(f"{API_BASE}/desk/lines", {"line": line_open})
    assert resp.get("success") is True, f"line create failed: {resp}"
    try:
        config = await cc_api.get(f"{API_BASE}/agents/{agent_id}/config")
        desk_obj = config.get("desk") if isinstance(config.get("desk"), dict) else {}
        lines = config.get("lines") or desk_obj.get("lines")
        assert isinstance(lines, list), "N5 regression: config.lines missing"
        ids = [l.get("id") for l in lines]
        assert "line-open" in ids, f"unconstrained agent must receive all lines: {ids}"
        evidence.log_metric("unconstrained_lines", ids)
    finally:
        await cc_api.delete(f"{API_BASE}/desk/lines/line-open")
