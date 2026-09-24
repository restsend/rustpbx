"""Per-agent outbound policy (outbound_policy.rs) — REST e2e.

Storage note: desk lines / policies live in `cc_entity_extras` of the APP
database. With the default `sqlite::memory:` the console connection and the
addon connection see different databases, so these tests boot a file-backed
DB (mirroring production) before creating resources.

Strict: policy create/get round-trip (with rules + reverse bindings); the
bound agent's `desk.lines[]` IS the policy's rules (restsend-call contract:
`{id,label,prefix,caller,default}` + extra pass-through keys, camelCased);
a bound policy with zero rules delivers an empty line list; an agent with no
policy reference keeps the global desk lines.
"""

from __future__ import annotations

import pytest
import pytest_asyncio

import helpers as h

pytestmark = [pytest.mark.tier3]

POLICY = "pol-e2e-lines"
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


def _agent_lines(config):
    desk_obj = config.get("desk") if isinstance(config.get("desk"), dict) else {}
    lines = config.get("lines") or desk_obj.get("lines")
    assert isinstance(lines, list), (
        f"N5 regression: config.lines missing — config keys: {sorted(config.keys())}"
    )
    return lines


async def test_policy_lines_replace_agent_delivery(cc_api, evidence):
    """Bound policy: the agent receives the policy's rules as desk.lines[]
    (restsend-call contract) — global lines no longer reach the agent, extra
    pass-through keys ride along camelCased, exactly one default line."""
    agent_id = "1001"
    global_line = {"id": "line-global", "label": "global", "caller": "anyone", "enabled": True}
    resp = await cc_api.post(f"{API_BASE}/desk/lines", {"line": global_line})
    assert resp.get("success") is True, f"global line create failed: {resp}"

    lines = [
        {
            "id": "line-mobile",
            "label": "Mobile",
            "prefix": "9",
            "caller": "self",
            "default": True,
            "allowed_prefixes": ["1"],
            "extra": {"channel_no": "8"},
        },
        {"id": "line-ld", "label": "Long distance", "prefix": "0", "caller": "02188886666"},
    ]
    created = await cc_api.post(f"{API_BASE}/outbound-policies", {
        "name": POLICY,
        "policy": {"enabled": True, "lines": lines},
        "agents": [agent_id],
    })
    assert created.get("success") is True, f"policy create failed: {created}"

    # Round-trip: rules + reverse binding visible on the policy resource.
    got = await cc_api.get(f"{API_BASE}/outbound-policies/{POLICY}")
    body = got.get("policy") or {}
    got_ids = [l.get("id") for l in (body.get("lines") or [])]
    assert got_ids == ["line-mobile", "line-ld"], f"policy rules round-trip mismatch: {str(got)[:300]}"
    assert (got.get("agents") or []) == [agent_id], f"reverse binding missing: {str(got)[:300]}"

    try:
        config = await cc_api.get(f"{API_BASE}/agents/{agent_id}/config")
        delivered = _agent_lines(config)
        by_id = {l.get("id"): l for l in delivered}
        assert set(by_id) == {"line-mobile", "line-ld"}, (
            f"bound agent must receive exactly the policy rules, got: {[l.get('id') for l in delivered]}"
        )
        mobile = by_id["line-mobile"]
        assert mobile.get("prefix") == "9" and mobile.get("caller") == "self", str(mobile)
        assert mobile.get("default") is True, f"default flag lost: {str(mobile)}"
        assert mobile.get("channelNo") == "8", (
            f"custom pass-through key not delivered camelCased: {str(mobile)}"
        )
        assert "line-global" not in by_id, "global line must NOT reach a policy-bound agent"
        evidence.log_metric("bound_lines", [l.get("id") for l in delivered])
    finally:
        await cc_api.delete(f"{API_BASE}/outbound-policies/{POLICY}")

    # Unbound again (delete strips the reference): global lines are back.
    config = await cc_api.get(f"{API_BASE}/agents/{agent_id}/config")
    delivered = _agent_lines(config)
    ids = [l.get("id") for l in delivered]
    assert "line-global" in ids, f"unbound agent must see global lines again: {ids}"
    assert "line-mobile" not in ids, f"deleted policy still delivers rules: {ids}"
    await cc_api.delete(f"{API_BASE}/desk/lines/line-global")


async def test_policy_without_rules_denies_outbound_delivery(cc_api, evidence):
    """Bound policy with zero rules → empty desk.lines[] (client single-line
    mode) — outbound is denied end-to-end."""
    agent_id = "1002"
    created = await cc_api.post(f"{API_BASE}/outbound-policies", {
        "name": f"{POLICY}-blocked",
        "policy": {"enabled": True, "lines": []},
        "agents": [agent_id],
    })
    assert created.get("success") is True, f"policy create failed: {created}"
    try:
        config = await cc_api.get(f"{API_BASE}/agents/{agent_id}/config")
        delivered = _agent_lines(config)
        assert delivered == [], f"zero-rule policy must deliver no lines: {delivered}"
        evidence.log_metric("blocked_lines", delivered)
    finally:
        await cc_api.delete(f"{API_BASE}/outbound-policies/{POLICY}-blocked")


async def test_outbound_policy_unconstrained_without_reference(cc_api, evidence):
    """An agent with no policy reference = unconstrained (backwards-compatible):
    the global desk lines are delivered unchanged."""
    agent_id = "1002"
    line_open = {"id": "line-open", "label": "open", "caller": "anyone", "enabled": True}
    resp = await cc_api.post(f"{API_BASE}/desk/lines", {"line": line_open})
    assert resp.get("success") is True, f"line create failed: {resp}"
    try:
        config = await cc_api.get(f"{API_BASE}/agents/{agent_id}/config")
        lines = _agent_lines(config)
        ids = [l.get("id") for l in lines]
        assert "line-open" in ids, f"unconstrained agent must receive global lines: {ids}"
        evidence.log_metric("unconstrained_lines", ids)
    finally:
        await cc_api.delete(f"{API_BASE}/desk/lines/line-open")


async def test_policy_options_return_binding_candidates(cc_api, evidence):
    """Editor options expose skill groups + agents (routes/trunks candidates
    are gone)."""
    options = await cc_api.get(f"{API_BASE}/outbound-policies/options")
    assert isinstance(options, dict), f"options must be an object: {str(options)[:200]}"
    assert isinstance(options.get("skill_groups"), list), "skill_groups candidates missing"
    assert isinstance(options.get("agents"), list) and options["agents"], "agents candidates missing"
    assert "routes" not in options and "trunks" not in options, (
        "legacy routes/trunks candidates must be removed"
    )
    evidence.log_metric("options_agents", len(options["agents"]))
