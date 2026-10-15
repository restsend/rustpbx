"""ACD cluster e2e — 客户端意外掉线，及时清理状态 (dual node).

Scenario 5 from the 2026-10-15 prd acceptance request.

Two client-death shapes are exercised with the REAL production client
(restsend-cli) killed with no BYE/unregister:

  1. ``test_ws_client_drop_cleans_state_promptly`` — WS transport died
     (browser/cc-phone crash). The server must observe the dropped transport
     and clear the agent to ``offline`` on BOTH nodes + the shared presence
     row QUICKLY (no lingering idle scheduling), and a queued call must never
     be dispatched to the dead extension.

  2. ``test_udp_client_death_expiry_cleans_state`` — UDP client killed with no
     transport signal; the registration lease lapses and the locator sweep +
     presence layer must still reclaim the agent to ``offline``.

These use agent 1004 with dedicated UAs (never the shared session fleet), so a
killed client cannot bleed into other suites.
"""

from __future__ import annotations

import asyncio
import os
import random

import pytest

from helpers.prd_cluster import PrdCluster, PrdError, PrdRestsendAgent  # noqa: F401

pytestmark = [pytest.mark.cluster, pytest.mark.acd]

if os.environ.get("RUSTPBX_PRD_E2E", "1") != "1":
    pytest.skip("RUSTPBX_PRD_E2E=0 — prd cluster tests disabled",
                allow_module_level=True)


GROUP_WS = "e2e-drop1"
ROUTE_WS = "7431"  # caller dials 74311
GROUP_UDP = "e2e-drop2"
ROUTE_UDP = "7432"  # caller dials 74321
AGENT = "1004"


def _ua_port() -> int:
    return random.randint(30000, 39000)


def monitor(prd: PrdCluster) -> dict[str, dict]:
    return {n.name: n.get("/cc/monitor")["data"] for n in prd.nodes}


def summary(snap: dict, key: str) -> int:
    return int((snap.get("summary") or {}).get(key) or 0)


def presence_status(prd: PrdCluster, agent_id: str) -> str:
    rows = prd.sql(
        f"SELECT status FROM cc_agent_presence WHERE agent_id='{agent_id}'"
    )
    return rows[1][0] if len(rows) > 1 else ""


def _agent_entry(snap: dict, agent_id: str) -> dict:
    for a in snap.get("agents") or []:
        if a.get("agent_id") == agent_id:
            return a
    return {}


async def _spawn(user: str, *, transport=None, expires: int = 600) -> PrdRestsendAgent:
    ua = PrdRestsendAgent(user, _ua_port(), transport=transport, expires=expires)
    try:
        await ua.start()
    except PrdError:
        print(f"\n[diag] {user} spawn/register failed; "
              f"stderr={ua.agent.stderr_text()[-800:]}")
        raise
    return ua


async def test_ws_client_drop_cleans_state_promptly(prd, pool, topology_factory):
    topo = topology_factory(
        tag="drop1",
        groups=[{"id": GROUP_WS, "max_wait_secs": 120, "sla_target_secs": 30}],
        agents=[{"id": AGENT, "groups": [GROUP_WS], "max_concurrency": 1}],
        route_base=ROUTE_WS,
    )
    prd.drain([AGENT])

    ua = await _spawn(AGENT, transport="ws", expires=60)
    try:
        prd.assert_agent_status_both_nodes(AGENT, "idle", timeout=45)
        # outlive the locator's unregister grace so the transport-close is not
        # ignored as a rapid reconnect
        await asyncio.sleep(6)

        # ── kill the client with NO goodbye ──────────────────────────────
        await ua.kill()

        prd.wait_for(
            lambda: all(n.agent_status(AGENT) == "offline" for n in prd.nodes),
            timeout=30,
            what="agent forced offline on both nodes after WS transport drop",
        )
        prd.wait_for(
            lambda: presence_status(prd, AGENT) == "offline",
            timeout=15,
            what="shared presence row offline after WS transport drop",
        )
        for name, snap in monitor(prd).items():
            entry = _agent_entry(snap, AGENT)
            assert entry.get("status") in (None, "offline"), (
                f"{name}: monitor still shows {AGENT}={entry.get('status')!r} "
                f"(dead client must not stay schedulable)"
            )

        # a queued call must NOT be dispatched to the dead extension
        caller = pool.caller(
            target=topo.target_uri(GROUP_WS, entry="call"),
            username="1004", password="123456", hangup=20,
            addr=f"0.0.0.0:{prd.next_caller_port()}",
        )
        ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
        assert ok, f"caller did not enter the queue\n{str(caller.output)[-800:]}"
        prd.wait_for(
            lambda: all(summary(s, "waiting") >= 1 for s in monitor(prd).values()),
            timeout=15, what="call waits (no live candidate)",
        )
        for name, snap in monitor(prd).items():
            assert not [r for r in (snap.get("ringing_agents") or [])], (
                f"{name}: a ringing agent exists though the only agent is dead: "
                f"{snap.get('ringing_agents')}"
            )
        prd.call_node.end_all_active_calls()
    finally:
        ua.agent.kill()  # idempotent; the client is already dead


async def test_udp_client_death_expiry_cleans_state(prd, pool, topology_factory):
    topo = topology_factory(
        tag="drop2",
        groups=[{"id": GROUP_UDP, "max_wait_secs": 120, "sla_target_secs": 30}],
        agents=[{"id": AGENT, "groups": [GROUP_UDP], "max_concurrency": 1}],
        route_base=ROUTE_UDP,
    )
    prd.drain([AGENT])

    # short client lease so the binding lapses off a hard kill (no transport
    # signal over UDP): expires + locator grace + sweep.
    ua = await _spawn(AGENT, transport=None, expires=15)
    try:
        prd.assert_agent_status_both_nodes(AGENT, "idle", timeout=45)
        await asyncio.sleep(3)

        await ua.kill()

        prd.wait_for(
            lambda: all(n.agent_status(AGENT) == "offline" for n in prd.nodes),
            timeout=120,
            what="agent reclaimed offline after UDP lease expiry",
        )
        prd.wait_for(
            lambda: presence_status(prd, AGENT) == "offline",
            timeout=20,
            what="shared presence row offline after UDP lease expiry",
        )
    finally:
        ua.agent.kill()  # idempotent; the client is already dead
