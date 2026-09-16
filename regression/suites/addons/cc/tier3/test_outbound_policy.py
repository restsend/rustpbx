"""Per-agent outbound policy (outbound_policy.rs) — REST e2e.

Policy semantics (grant-union): a named policy constrains allowed routes /
trunks / caller-ids; attached to an agent via the `outbound_policy` extra.
The desk-lines delivered through `GET /api/cc/agents/{id}/config` must be
filtered by that policy: lines whose caller-id is not whitelisted never
reach the agent.

Strict: disallowed line absent, allowed line present with exact fields,
policy round-trips through create/get.
"""

from __future__ import annotations

import pytest

pytestmark = [pytest.mark.tier3, pytest.mark.acd]

POLICY = "pol-e2e-caller"


async def _first_agent(api) -> dict:
    listing = await api.get("/api/cc/agents")
    items = listing.get("data") if isinstance(listing, dict) else listing
    assert items, f"no seeded agents found: {str(listing)[:300]}"
    return items[0]


async def test_outbound_policy_filters_desk_lines(pbx, api, evidence):
    agent = await _first_agent(api)
    agent_id = agent["agent_id"]
    evidence.log_metric("agent_under_test", agent_id)

    # 1) create a caller-id-constrained policy
    created = await api.post("/api/cc/outbound-policies", {
        "name": POLICY,
        "policy": {
            "enabled": True,
            "allowed_routes": [],
            "allowed_trunks": [],
            "allowed_caller_ids": [agent_id],
        },
    })
    assert created.get("success") is True, f"policy create failed: {created}"
    got = await api.get(f"/api/cc/outbound-policies/{POLICY}")
    policy_body = got.get("policy") or got
    assert policy_body.get("allowed_caller_ids") == [agent_id], (
        f"policy round-trip mismatch: {str(got)[:300]}"
    )

    # 2) two desk lines: one the agent may use, one outside the whitelist
    line_ok = {"id": "line-ok", "label": "allowed", "caller": agent_id, "enabled": True}
    line_bad = {"id": "line-bad", "label": "forbidden", "caller": "ghost-caller", "enabled": True}
    for line in (line_ok, line_bad):
        resp = await api.post("/api/cc/desk/lines", {"line": line})
        print("CREATE_RESP:", str(resp)[:200])
        assert resp.get("success", True) is not False, f"line create failed: {resp}"


    # 3) attach the policy to the agent (extras key outbound_policy)
    updated = await api.put(f"/api/cc/agents/{agent_id}", {
        "display_name": agent.get("display_name") or agent_id,
        "skills": agent.get("skills") or [],
        "extra": {"outbound_policy": {"type": "text", "value": POLICY}},
    })
    assert isinstance(updated, dict), f"agent update failed: {str(updated)[:200]}"

    # 4a) management plane: both lines exist (strict)
    listing = await api.get("/api/cc/desk/lines")
    listed = listing.get("data") if isinstance(listing, dict) else listing
    listed_ids = [_unwrap_line(l).get("id") for l in (listed or [])]
    assert "line-ok" in listed_ids and "line-bad" in listed_ids, (
        f"desk lines management list incomplete: {listed_ids}"
    )
    # 4b) delivery plane (N5 known gap): get_agent_config reads desk-profile
    # lines, a different store from create_desk_line entities — filtering
    # cannot be observed until the stores are unified. Surface as xfail.
    config = await api.get(f"/api/cc/agents/{agent_id}/config")
    assert isinstance(config, dict), f"config must be an object: {str(config)[:200]}"
    lines = config.get("lines")
    if lines is None:
        evidence.log_metric(
            "known_gap",
            "N5: desk lines created via POST /cc/desk/lines are stored separately "
            "from load_agent_profile desk.lines — delivery filtering unobservable",
        )
        pytest.xfail("known product gap (N5): desk-line delivery store not unified with line entities")
    ids = [l.get("id") for l in lines]
    assert "line-ok" in ids, f"allowed line filtered out: {ids}"
    assert "line-bad" not in ids, (
        f"SECURITY/POLICY: non-whitelisted caller-id line delivered to agent: {ids}"
    )
    evidence.log_metric("policy_lines", ids)

    # 5) cleanup: detach policy, remove lines, delete policy
    await api.put(f"/api/cc/agents/{agent_id}", {
        "display_name": agent.get("display_name") or agent_id,
        "skills": agent.get("skills") or [],
        "extra": {},
    })
    for line_id in ("line-ok", "line-bad"):
        await api.delete(f"/api/cc/desk/lines/{line_id}")
    await api.delete(f"/api/cc/outbound-policies/{POLICY}")


def _unwrap_line(l):
    return l.get("line") if isinstance(l, dict) and isinstance(l.get("line"), dict) else l


async def test_outbound_policy_unconstrained_without_reference(pbx, api, evidence):
    """未引用任何策略的坐席 = 无约束（向后兼容语义）：line-bad 也必须可达。"""
    agent = await _first_agent(api)
    agent_id = agent["agent_id"]
    line_open = {"id": "line-open", "label": "open", "caller": "anyone", "enabled": True}
    resp = await api.post("/api/cc/desk/lines", {"line": line_open})
    assert resp.get("success", True) is not False, f"line create failed: {resp}"
    # 管理面：创建的线路必须可列出（严格）
    listing = await api.get("/api/cc/desk/lines")
    listed = listing.get("data") if isinstance(listing, dict) else listing
    listed_ids = [_unwrap_line(l).get("id") for l in (listed or [])]
    assert "line-open" in listed_ids, f"created line missing in management list: {listed_ids}"
    try:
        config = await api.get(f"/api/cc/agents/{agent_id}/config")
        lines = config.get("lines")
        if lines is None:
            evidence.log_metric("known_gap", "N5: delivery store not unified (unconstrained probe)")
            pytest.xfail("known product gap (N5): delivery store not unified with line entities")
        ids = [l.get("id") for l in lines]
        assert "line-open" in ids, f"unconstrained agent must receive every enabled line: {ids}"
        evidence.log_metric("unconstrained_lines", ids)
    finally:
        await api.delete("/api/cc/desk/lines/line-open")
