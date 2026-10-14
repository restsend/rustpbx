"""CC realtime monitor e2e — prd dual-node cluster (在线坐席 / 排队计数).

Regressions covered (2026-10-13 prd 验收):

1. 在线/空闲坐席计数不对 — the Monitor's `agents_by_status` merged the local
   registry with shared presence rows using instance-id ownership only. When
   two nodes share an instance id (host-network containers on one host) or a
   replica simply went stale, a peer node's live agent showed as
   offline/idle from an outdated registry copy (空闲坐席 0 with agents
   logged in). The monitor must agree with the shared `cc_agent_presence`
   row on BOTH nodes.

2. 进线显示通话中、实际在排队 / 排队数量不对 — a call waiting in the ACD
   queue must appear as state "queued" (排队中) in the Inbound (进线) view
   on BOTH nodes, with `summary.waiting` counting it exactly once. A
   cross-node caller (entering via the AGENT node while reading the monitor
   on the CALL node) is the regression: the queued state used to depend on
   data only the owning node had.

Agents used: 1003 (+ caller identity 1004) — 1001/1002 are driven by the
concurrent live desk client on this deployment and MUST NOT be touched.
"""

from __future__ import annotations

import asyncio
import os
import random
import time

import pytest

from helpers.prd_cluster import PrdCluster, PrdError, PrdRestsendAgent  # noqa: F401

pytestmark = [pytest.mark.cluster, pytest.mark.acd, pytest.mark.slow]

if os.environ.get("RUSTPBX_PRD_E2E", "1") != "1":
    pytest.skip("RUSTPBX_PRD_E2E=0 — prd cluster tests disabled",
                allow_module_level=True)

GROUP_ID = "mon-e2e-q"
ROUTE_BASE = "7181"  # route point 71811
AGENT_ID = "1003"
CALLER_USER = "1004"

# Unique, never-reused local port per UA: the locator keys a REGISTER by
# contact, and a second REGISTER from the SAME contact while a lease is live
# is a "refresh" — the registrar bridge skips refreshes, so a reused port
# would leave the agent Offline until the old lease expires (10 min with the
# harness's expires=600). Randomised (not sequential) so a previous test run
# can never collide with this one.
def _ua_port() -> int:
    return random.randint(30000, 39000)


async def spawn_ua(user: str) -> PrdRestsendAgent:
    """Register one restsend UA on the AGENT node with a never-reused port.

    The session-scoped `fleet` fixture pkill(1)s restsend-cli at setup —
    that would kill the operator's LIVE desk session running on this host,
    so this suite owns its UAs explicitly instead.
    """
    ua = PrdRestsendAgent(user, _ua_port())
    await ua.start()
    return ua


# ────────────────────────────────────────────────────────────────────────────
# monitor readers
# ────────────────────────────────────────────────────────────────────────────

def monitor(prd: PrdCluster) -> dict[str, dict]:
    """Snapshot the monitor on both nodes at (nearly) the same moment."""
    return {n.name: n.get("/cc/monitor")["data"] for n in prd.nodes}


def caller_rows(snapshot: dict, caller: str) -> list[dict]:
    """Active-call rows for this suite's caller — by caller identity OR by
    queue attribution to the exclusive group (remote cluster_sessions rows
    do not always carry caller/callee)."""
    rows = snapshot.get("active_calls") or []
    return [
        r for r in rows
        if caller in str(r.get("caller") or "")
        or (r.get("route") or {}).get("queue_id") == GROUP_ID
    ]


def queued_for_call(snapshot: dict, call_id: str) -> dict | None:
    for q in snapshot.get("queued_calls") or []:
        if q.get("call_id") == call_id:
            return q
    return None


def agent_entry(snapshot: dict, agent_id: str) -> dict | None:
    for a in snapshot.get("agents") or []:
        if a.get("agent_id") == agent_id:
            return a
    return None


def group_waiting(snapshot: dict, group_id: str) -> int | None:
    for g in snapshot.get("skill_groups") or []:
        if g.get("skill_group_id") == group_id:
            return int(g.get("waiting") or 0)
    return None


# ────────────────────────────────────────────────────────────────────────────
# 1. 在线坐席计数
# ────────────────────────────────────────────────────────────────────────────

async def test_monitor_agent_counts_match_presence_cluster_wide(prd, pool, topology_factory):
    """Agent logged in on one node must count as idle/online in BOTH nodes'
    monitor — matching the shared presence row, not a stale replica.

    Phases: offline (baseline 0) → idle (SIP register) → dnd → offline.
    At each phase both nodes' `agents_by_status` must agree with the
    authoritative per-node agent status API and the shared DB row.
    """
    topology_factory(
        tag="mon1",
        groups=[{"id": GROUP_ID, "max_wait_secs": 120, "sla_target_secs": 30}],
        agents=[{"id": AGENT_ID, "groups": [GROUP_ID], "max_concurrency": 1}],
        route_base=ROUTE_BASE,
    )
    prd.set_agent_status(AGENT_ID, "offline")
    prd.assert_agent_status_both_nodes(AGENT_ID, "offline", timeout=20)
    # The offline baseline must SETTLE before the UA registers — a late REST
    # write would overwrite the UA's idle and the phase assertion would race.
    await asyncio.sleep(2)

    agent = await spawn_ua(AGENT_ID)
    try:
        prd.assert_agent_status_both_nodes(AGENT_ID, "idle", timeout=45)
    except PrdError:
        print(f"\n[diag] UA reg={agent.agent.has_event('sip_reg_state')} "
              f"events={agent.agent.events[-4:]} "
              f"stderr={agent.agent.stderr_text()[-300:]}")
        raise

    def idle_count(snap: dict) -> int:
        return int(snap["summary"]["agents_by_status"].get("idle") or 0)

    # idle phase — both monitors must show the agent idle, count >= 1
    deadline = time.monotonic() + 10
    last = {}
    while time.monotonic() < deadline:
        last = monitor(prd)
        if all(
            (agent_entry(s, AGENT_ID) or {}).get("status") == "idle"
            and idle_count(s) >= 1
            for s in last.values()
        ):
            break
        await asyncio.sleep(0.5)
    for name, snap in last.items():
        entry = agent_entry(snap, AGENT_ID) or {}
        assert entry.get("status") == "idle", (
            f"{name}: monitor shows {AGENT_ID}={entry.get('status')!r}, "
            f"expected idle (在线坐席计数错误复现)"
        )
        assert idle_count(snap) >= 1, (
            f"{name}: agents_by_status.idle={idle_count(snap)} with {AGENT_ID} idle "
            f"(空闲坐席0复现); snapshot={snap['summary']}"
        )

    # dnd phase — leaves the online pool's idle count
    prd.set_agent_status(AGENT_ID, "dnd")
    prd.wait_for(
        lambda: all(
            (agent_entry(s, AGENT_ID) or {}).get("status") == "dnd"
            for s in monitor(prd).values()
        ),
        timeout=10, what="dnd visible on both monitors",
    )

    # offline again — idle count must drop back on BOTH nodes
    prd.set_agent_status(AGENT_ID, "offline")
    prd.wait_for(
        lambda: all(
            (agent_entry(s, AGENT_ID) or {}).get("status") in ("offline", None)
            and idle_count(s) == 0
            for s in monitor(prd).values()
        ),
        timeout=12, what="idle back to 0 on both monitors",
    )

    await agent.stop()


# ────────────────────────────────────────────────────────────────────────────
# 2. 进线排队状态 + 排队数量
# ────────────────────────────────────────────────────────────────────────────

async def test_monitor_queued_call_state_and_waiting_count(prd, pool, topology_factory):
    """A call waiting in the queue shows 排队中 (state=queued) in the Inbound
    view and counts exactly once in `summary.waiting` — on BOTH nodes,
    including the cross-node case (caller enters via the AGENT node, monitor
    read on the CALL node)."""
    topology_factory(
        tag="mon2",
        groups=[{"id": GROUP_ID, "max_wait_secs": 120, "sla_target_secs": 30}],
        agents=[{"id": AGENT_ID, "groups": [GROUP_ID], "max_concurrency": 1}],
        route_base=ROUTE_BASE,
    )
    # nobody available → the caller must WAIT in queue. Drain FIRST (it
    # coerces agents to idle/offline), then pin the agent DND so the queue
    # cannot dispatch the waiting call.
    prd.drain([AGENT_ID])
    prd.set_agent_status(AGENT_ID, "dnd")
    prd.wait_for(
        lambda: all(n.agent_status(AGENT_ID) == "dnd" for n in prd.nodes),
        timeout=15, what="agent dnd on both nodes before queueing",
    )

    for entry in ("agent", "call"):  # cross-node first, then same-node
        caller = pool.caller(
            target=f"sip:{ROUTE_BASE}1@{prd.call_sip if entry == 'call' else prd.agent_sip}",
            username=CALLER_USER, password="123456",
            hangup=15,  # bot BYEs gracefully → call_hangup → queue dequeue
            addr=f"0.0.0.0:{prd.next_caller_port()}",
        )
        ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
        assert ok, f"[{entry}] caller did not enter the queue\n{str(caller.output)[-800:]}"

        # wait for the call to be QUEUED per the monitors. First sight can
        # briefly render 通话中 (the cluster_sessions row is written at session
        # birth, the shared queue row lands when the queue app registers the
        # wait — sub-second apart); allow ≤5s to converge, then hold the
        # strict queued-state invariants.
        deadline = time.monotonic() + 5
        last: dict[str, dict] = {}
        while time.monotonic() < deadline:
            last = monitor(prd)
            if all(
                caller_rows(s, CALLER_USER)
                and all(r.get("state") == "queued" for r in caller_rows(s, CALLER_USER))
                for s in last.values()
            ):
                break
            await asyncio.sleep(0.4)
        assert last, "no monitor snapshot"

        for name, snap in last.items():
            rows = caller_rows(snap, CALLER_USER)
            assert rows, (
                f"[{entry}] {name}: waiting call absent from 进线 view; "
                f"active_calls={snap.get('active_calls')}"
            )
            for r in rows:
                assert r.get("state") == "queued", (
                    f"[{entry}] {name}: caller leg still shows state={r.get('state')!r} "
                    f"(通话中) >5s after entering the queue — 进线排队状态错误; row={r}"
                )
            waiting = int(snap["summary"].get("waiting") or 0)
            queued_rows = snap.get("queued_calls") or []
            assert waiting >= 1, (
                f"[{entry}] {name}: summary.waiting={waiting} with a call in queue "
                f"(排队数量错误); queued={queued_rows}"
            )
            assert int(snap["summary"].get("dispatching") or 0) == 0, (
                f"[{entry}] {name}: nobody is being rung yet; dispatching must be 0; "
                f"summary={snap['summary']}"
            )
            for q in queued_rows:
                assert q.get("linked") is not False, (
                    f"[{entry}] {name}: waiting row not linked to a skill group: {q}"
                )
            assert len(queued_rows) == waiting, (
                f"[{entry}] {name}: queued_calls rows={len(queued_rows)} != "
                f"summary.waiting={waiting}"
            )
            gw = group_waiting(snap, GROUP_ID)
            assert gw == 1, (
                f"[{entry}] {name}: skill group {GROUP_ID} waiting={gw}, expected 1"
            )

        # every queued row must have its 进线 counterpart queued too
        for name, snap in last.items():
            for q in snap.get("queued_calls") or []:
                match = [r for r in snap.get("active_calls") or []
                         if r.get("call_id") == q.get("call_id")]
                if match:
                    assert match[0].get("state") == "queued", (
                        f"[{entry}] {name}: queued call {q.get('call_id')} renders "
                        f"as {match[0].get('state')!r} in 进线"
                    )

        # Graceful BYE via the bot's hangup timer (terminate() is a hard kill
        # with no BYE — the queue row then lingers until the max-wait
        # fallback instead of the call_hangup dequeue).
        await asyncio.sleep(17)  # hangup=15 → BYE lands
        prd.wait_for(
            lambda: all(
                not (s.get("queued_calls")) for s in monitor(prd).values()
            ),
            timeout=30, what=f"[{entry}] queue drained after caller hangup",
        )
        pool.terminate_all()


# ────────────────────────────────────────────────────────────────────────────
# 3. dispatch (ringing) phase — monitor keeps 排队 attribution + ringing agent
# ────────────────────────────────────────────────────────────────────────────

async def test_monitor_dispatch_ringing_state(prd, pool, topology_factory):
    """While the queue dispatch rings an agent, the Inbound row must keep
    queue attribution (queued/dispatching — NOT 通话中) and the ringing agent
    must be visible on both nodes."""
    topology_factory(
        tag="mon3",
        groups=[{"id": GROUP_ID, "max_wait_secs": 120, "sla_target_secs": 30}],
        agents=[{"id": AGENT_ID, "groups": [GROUP_ID], "max_concurrency": 1}],
        route_base=ROUTE_BASE,
    )
    prd.drain([AGENT_ID])
    agent = await spawn_ua(AGENT_ID)
    prd.assert_agent_status_both_nodes(AGENT_ID, "idle", timeout=45)

    caller = pool.caller(
        target=f"sip:{ROUTE_BASE}1@{prd.agent_sip}",
        username=CALLER_USER, password="123456",
        hangup=60,
        addr=f"0.0.0.0:{prd.next_caller_port()}",
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, f"caller did not enter the queue\n{str(caller.output)[-800:]}"

    ring = await agent.wait_ring(timeout=30)
    assert ring is not None, "agent never rang — dispatch failed"

    # During dispatch the shared row is CLAIMED: the call is 振铃中 — NOT
    # 排队中 (an agent has been assigned) and NOT 通话中 (not connected yet).
    # It must count under `dispatching`, leaving `waiting` at 0. Allow ≤5s to
    # converge, then hold.
    deadline = time.monotonic() + 5
    last: dict[str, dict] = {}
    while time.monotonic() < deadline:
        last = monitor(prd)
        if all(
            any(r.get("agent_id") == AGENT_ID for r in (s.get("ringing_agents") or []))
            and all(r.get("state") == "ringing" for r in caller_rows(s, CALLER_USER))
            and int(s["summary"].get("dispatching") or 0) == 1
            for s in last.values()
        ):
            break
        await asyncio.sleep(0.4)
    for name, snap in last.items():
        ringing = [r for r in (snap.get("ringing_agents") or [])
                   if r.get("agent_id") == AGENT_ID]
        assert ringing, f"{name}: ringing agent {AGENT_ID} missing; summary={snap['summary']}"
        rows = caller_rows(snap, CALLER_USER)
        assert rows, f"{name}: inbound row vanished while ringing"
        for r in rows:
            assert r.get("state") == "ringing", (
                f"{name}: dispatching call shows state={r.get('state')!r}, expected "
                f"ringing (振铃中, assigned) — not 排队中/通话中; row={r}"
            )
        assert int(snap["summary"].get("dispatching") or 0) == 1, (
            f"{name}: summary.dispatching != 1 during ring; summary={snap['summary']}"
        )
        assert int(snap["summary"].get("waiting") or 0) == 0, (
            f"{name}: a ringing (assigned) call must NOT count as 排队中; "
            f"summary={snap['summary']}"
        )

    # answer → talking is now CORRECT; the owning node must show the call as
    # active (talking), and NO node may keep labelling it queued.
    # (Known gap, pre-existing: after connect the peer node's 进线 view drops
    # the remote session row entirely — cluster_sessions loses the caller leg
    # on the non-owning node at connect. Follow-up, not a queued-state bug.)
    assert await agent.answer_pending(), "agent answer did not connect"
    prd.wait_for(
        lambda: (
            any(
                r.get("state") == "active"
                for r in caller_rows(prd.agent_node.get("/cc/monitor")["data"], CALLER_USER)
            )
            and all(
                all(r.get("state") != "queued" for r in caller_rows(s, CALLER_USER))
                for s in monitor(prd).values()
            )
        ),
        timeout=15,
        what="answered call shown as active on the owning node, queued nowhere",
    )

    prd.call_node.end_all_active_calls()
    prd.agent_node.end_all_active_calls()
    await agent.hangup()
    pool.terminate_all()
    prd.wait_for(
        lambda: all(not (s.get("queued_calls")) for s in monitor(prd).values()),
        timeout=20, what="queue drained",
    )
    await agent.stop()
