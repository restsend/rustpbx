"""RWI webhook event contracts — REAL end-to-end payloads.

Drives three REAL flows (sipbot caller → PBX queue → CC agent, RWI blind
transfer) and dumps the RAW webhook stream each flow produces, then asserts
the three event contracts fixed in this change:

  A. agent-hangup dedup — an agent leg teardown is reported by exactly ONE
     ``call_hangup{leg_role:"agent", reason:"UasBye"}``; the session-scoped
     hangup carries no leg_role (before the fix: a second, contradictory
     ``leg_role:"agent", reason:"callee"`` event raced it).
  B. skill-group re-entry — after an agent miss (ring timeout → PBX cancel)
     the call keeps queuing on the next candidate and emits a SECOND
     ``queue_joined`` carrying ``attempt: 2`` (before the fix: silent).
  C. transfer attribution clear — a blind transfer to a plain extension
     hands the call to its next stage: the replacement's ring/answer
     events carry NO agent role and NO ``agent_id`` enrichment, and the
     departing agent is reported once by its own leg hangup (before the
     fix: the stale agent role rode every later event).

Every test prints the full event stream (pytest -s) and stores it under
``regression/artifacts/rwi_event_contracts/``.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.acceptance, pytest.mark.queue, pytest.mark.transfer]

logger = logging.getLogger(__name__)

ARTIFACT_DIR = (
    Path(__file__).resolve().parents[5] / "regression" / "artifacts" / "rwi_event_contracts"
)


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------


async def _ensure_group(api, group_id: str, skills: list[str]) -> None:
    body = {
        "skill_group_id": group_id,
        "display_name": f"RWI contract {group_id}",
        "skills_required": skills,
        "overflow_groups": [],
        "sla_target_secs": 30,
        "max_wait_secs": 120,
    }
    try:
        await api.create_skill_group(body)
    except Exception as exc:  # noqa: BLE001 — 409 duplicate is fine
        if "409" not in str(exc) and "400" not in str(exc):
            raise


async def _wait_agent_base_status(api, agent_id: str, base: str, timeout: float = 15.0):
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    observed = None
    while loop.time() < deadline:
        try:
            agent = await api.get_agent(agent_id)
            observed = (agent or {}).get("status")
            if observed and observed.split(":")[0] == base:
                return observed
        except Exception:  # noqa: BLE001 — transient
            pass
        await asyncio.sleep(0.4)
    return observed


async def _register_agent(
    pbx, sipbot_pool, api, username: str, *, skills: list[str],
    answer_mode: str = "echo", hangup_after=None, reject_code=None, port=None,
) -> None:
    """Create the CC agent + register its sipbot extension, wait for Idle."""
    try:
        await api.create_agent({
            "agent_id": username,
            "display_name": f"RWI contract {username}",
            "skills": skills,
            "max_concurrency": 1,
            "role": "agent",
        })
    except Exception as exc:  # noqa: BLE001 — duplicate fine
        if "409" not in str(exc) and "400" not in str(exc):
            raise
    try:
        await api.end_agent_wrapup(username)
    except Exception:  # noqa: BLE001
        pass
    try:
        await api.update_agent_status(username, "offline")
    except Exception:  # noqa: BLE001
        pass
    sipbot_pool.terminate_user(username)
    sipbot_pool.callee(
        host=pbx.host,
        port=port or (17200 + (int(username) % 500)),
        username=username,
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode=answer_mode,
        hangup_after=hangup_after,
        reject_code=reject_code,
    )
    observed = await _wait_agent_base_status(api, username, "idle")
    if not observed or observed.split(":")[0] != "idle":
        # Coerce once (stale wrapup timer), then require.
        try:
            await api.update_agent_status(username, "idle")
        except Exception:  # noqa: BLE001
            pass
        observed = await _wait_agent_base_status(api, username, "idle", timeout=15.0)
    assert observed and observed.split(":")[0] == "idle", (
        f"agent {username} never reached Idle (got {observed})"
    )


async def _register_extension(pbx, sipbot_pool, username: str, *, answer_mode="echo"):
    """Plain registered SIP extension (NO CC agent record behind it)."""
    sipbot_pool.terminate_user(username)
    ua = sipbot_pool.callee(
        host=pbx.host,
        port=17200 + (int(username) % 500),
        username=username,
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode=answer_mode,
    )
    await h.wait_registered(ua, f"extension {username}")
    return ua


def _events_for(webhook, call_id: str, since: int = 0) -> list:
    return [
        ev for ev in list(webhook.events)[since:]
        if ev.call_id == call_id
    ]


def _require_exact_counts(events, expected: dict[str, int]) -> None:
    """Each listed event type must appear EXACTLY `n` times in the call's
    webhook stream — 不能多，也不能少."""
    counts: dict[str, int] = {}
    for ev in events:
        counts[ev.event_type] = counts.get(ev.event_type, 0) + 1
    for etype, n in expected.items():
        assert counts.get(etype, 0) == n, (
            f"{etype}: expected exactly {n}, got {counts.get(etype, 0)} — "
            f"timeline: {[e.event_type for e in events]}"
        )


def _require_order(events, markers: list[tuple[str, object]]) -> None:
    """The (event_type, payload-match) markers must appear in exactly this
    relative order — each search starts AFTER the previous marker's position
    (a later marker can never match an earlier event), and each matches
    exactly one event."""
    start = 0
    positions: list[int] = []
    for etype, match in markers:
        pos = next(
            (
                i for i, ev in enumerate(events)
                if i >= start and ev.event_type == etype and (match is None or match(ev.payload))
            ),
            None,
        )
        assert pos is not None, (
            f"expected one {etype} (matching {match}) at/after position {start} — "
            f"timeline: {[(i, e.event_type, {k: v for k, v in e.payload.items() if k in ('leg_id', 'leg_role', 'agent_id', 'reason', 'attempt')}) for i, e in enumerate(events)]}"
        )
        positions.append(pos)
        start = pos + 1


def _dump(events, label: str) -> Path:
    """Print + persist the RAW webhook stream — the real consumer view."""
    ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
    out = ARTIFACT_DIR / f"{label}.jsonl"
    lines = []
    for i, ev in enumerate(events):
        line = json.dumps(
            {
                "seq": i,
                "event_type": ev.event_type,
                "call_id": ev.call_id,
                "payload": ev.payload,
            },
            ensure_ascii=False,
        )
        lines.append(line)
        print(f"  [{i:>2}] {ev.event_type:<26} {line[len(line) // 3:]}")
    out.write_text("\n".join(lines) + "\n")
    print(f"  == raw stream saved: {out}")
    return out


async def _wait_event(webhook, types, *, call_id=None, match=None, timeout=25.0, since=0):
    """Wait for ONE event matching type / call_id / payload-match.

    ``match`` compares values that ARE present; an absent key never matches
    (events omit null fields — match on the key's absence explicitly instead).
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    wanted = set(types)
    while True:
        for ev in list(webhook.events)[since:]:
            if ev.event_type not in wanted:
                continue
            if call_id and ev.call_id != call_id:
                continue
            if match:
                node = ev.payload
                ok = True
                for path, expected in match.items():
                    for part in path.replace("payload.", "", 1).split("."):
                        if not isinstance(node, dict) or part not in node:
                            ok = False
                            break
                        node = node[part]
                    if not ok or node != expected:
                        ok = False
                        break
                if not ok:
                    continue
            return ev
        if loop.time() >= deadline:
            return None
        await asyncio.sleep(0.25)


# ---------------------------------------------------------------------------
# A. agent-hangup dedup
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_contract_a_agent_hangup_single_leg_role_event(pbx, sipbot_pool, api, event_checker):
    """Queue call answered by agent 2305; the AGENT hangs up (sipbot BYE).
    Exactly one call_hangup{leg_role:"agent", reason:"UasBye"}; the
    session-scoped hangup carries no leg_role."""
    webhook = event_checker.webhook
    await _ensure_group(api, "base-grp", ["base"])
    await _register_agent(pbx, sipbot_pool, api, "2305", skills=["base"], hangup_after=8)

    baseline = len(list(webhook.events))
    caller = sipbot_pool.caller(
        target=f"sip:8899@{pbx.sip_addr}", username="2315", password="123456", hangup=120,
    )
    connected = await _wait_event(
        webhook, ["queue_agent_connected"],
        match={"payload.agent_id": "2305"}, timeout=30, since=baseline,
    )
    assert connected is not None, "agent 2305 never answered the queue call"
    call_id = connected.call_id

    # Agent hangs up after `hangup_after` seconds of talk time (real BYE).
    hangups = await _wait_event(
        webhook, ["call_hangup"], call_id=call_id, timeout=30,
        since=connected.sequence,
    )
    assert hangups is not None, "no call_hangup after the agent hung up"
    await asyncio.sleep(3)  # let the session-scoped event land too
    caller.terminate()

    events = _events_for(webhook, call_id)
    print(f"\n== contract A: {len(events)} webhook events for call {call_id}")
    _dump(events, "contract_a_agent_hangup")

    # ── exact counts — 不能多，也不能少 ──
    _require_exact_counts(events, {
        "call_created": 1,
        "queue_joined": 1,
        "queue_agent_offered": 1,
        "queue_agent_connected": 1,
        "call_hangup": 2,          # exactly: the agent leg + the session
    })

    # ── exact payloads ──
    hangup_events = [ev for ev in events if ev.event_type == "call_hangup"]
    leg_hangups = [ev for ev in hangup_events if ev.payload.get("leg_id") is not None]
    session_hangups = [ev for ev in hangup_events if ev.payload.get("leg_id") is None]
    assert len(leg_hangups) == 1 and len(session_hangups) == 1, (
        f"exactly one leg-scoped + one session-scoped hangup, got: "
        f"{[e.payload for e in hangup_events]}"
    )

    leg = leg_hangups[0].payload
    assert leg["leg_role"] == "agent", f"agent leg teardown: {leg}"
    assert leg["reason"] == "UasBye", f"the AGENT's BYE (UasBye): {leg}"
    assert leg["hangup_by"] == "agent", f"agent initiator: {leg}"
    assert isinstance(leg.get("duration_secs"), int) and leg["duration_secs"] >= 1, (
        f"established dialog carries talk time: {leg}"
    )
    assert leg.get("agent_id") == "2305", f"agent attribution: {leg}"

    session = session_hangups[0].payload
    assert "leg_role" not in session, (
        f"session hangup must NOT re-claim the reported leg role: {session}"
    )
    assert "leg_id" not in session
    assert session["reason"] == "callee", f"session teardown reason: {session}"

    # ── exact order — the strict lifecycle. NOTE: `queue_joined` vs
    #    `call_created` race their webhook delivery BY DESIGN (the join is
    #    broadcast before agent resolution; the two are distinct keys, see
    #    webhook.rs) — each is asserted exactly once above, their relative
    #    order is NOT part of the contract. From the agent leg's ring on,
    #    the order is strict, and there are EXACTLY two hangups. ──
    # ── exact order — the contract-owned chains, strictly. Two chains that
    #    run on DIFFERENT async emitters are each strict INTERNALLY; their
    #    interleaving (call_answered vs queue_agent_connected) is not a
    #    delivery-order contract. ──
    _require_order(events, [
        ("call_ringing", lambda p: p.get("leg_role") == "agent"),
        ("queue_agent_offered", lambda p: p.get("agent_id") == "2305"),
        ("queue_agent_connected", lambda p: p.get("agent_id") == "2305"),
    ])
    _require_order(events, [
        ("call_answered", lambda p: p.get("leg_role") == "agent"),
        ("call_hangup", lambda p: p.get("leg_role") == "agent" and p.get("reason") == "UasBye"),
        ("call_hangup", lambda p: p.get("leg_id") is None),
    ])

    # roster hygiene for later scenarios
    try:
        await api.update_agent_status("2305", "offline")
    except Exception:  # noqa: BLE001
        pass


# ---------------------------------------------------------------------------
# B. skill-group re-entry
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_contract_b_agent_miss_reentry_emits_joined_attempt(pbx, sipbot_pool, api, event_checker):
    """ring-grp re-entry, deterministic: the group holds ONLY the rejecting
    agent 2311 (486 — the sipbot no-answer shape used across the queue
    suites) when the call arrives. The dispatch misses → the call KEEPS
    queuing and announces the re-entry with queue_joined{attempt:2}; agent
    2312 is registered only AFTER the miss, so the next candidate that
    answers is unambiguous."""
    webhook = event_checker.webhook
    await _ensure_group(api, "ring-grp", ["ring"])
    # 2311 always rejects (486). 2312 is registered but parked AWAY —
    # away blocks dispatch, so the FIRST dispatch can only hit 2311.
    await _register_agent(
        pbx, sipbot_pool, api, "2311", skills=["ring"], reject_code=486,
    )
    await _register_agent(pbx, sipbot_pool, api, "2312", skills=["ring"])
    await api.update_agent_status("2312", "away")
    await _wait_agent_base_status(api, "2312", "away")

    baseline = len(list(webhook.events))
    caller = sipbot_pool.caller(
        target=f"sip:8890@{pbx.sip_addr}", username="2315", password="123456", hangup=180,
    )
    miss = await _wait_event(
        webhook, ["queue_agent_rejected"],
        match={"payload.agent_id": "2311"}, timeout=90, since=baseline,
    )
    assert miss is not None, "2311's 486 never produced queue_agent_rejected"
    call_id = miss.call_id

    # NOW release 2312 from break: the re-entry round dials a real candidate.
    await api.update_agent_status("2312", "idle")
    await _wait_agent_base_status(api, "2312", "idle")

    rejoin = await _wait_event(
        webhook, ["queue_joined"], call_id=call_id, timeout=30, since=miss.sequence,
    )
    assert rejoin is not None, "no re-entry queue_joined after the miss"
    connected = await _wait_event(
        webhook, ["queue_agent_connected"],
        match={"payload.agent_id": "2312"}, call_id=call_id, timeout=90, since=baseline,
    )
    assert connected is not None, "2312 never answered after the re-entry"
    caller.terminate()

    events = _events_for(webhook, call_id)
    print(f"\n== contract B: {len(events)} webhook events for call {call_id}")
    _dump(events, "contract_b_reentry")

    # ── exact conservation — 不能多，也不能少 ──
    # 2311 rejects EVERY dispatch while 2312 is still coming out of away, so
    # the wait-retention loop may re-dial it more than once. The CONTRACT is
    # count conservation: EVERY miss (rejected + its leg hangup, one pair per
    # attempt, all attributed to 2311) is followed by EXACTLY ONE re-entry
    # join, and the attempt counters form the strict sequence 2..N+1 — no
    # missing joins (the reported gap), no extra ones.
    rejects = [ev for ev in events if ev.event_type == "queue_agent_rejected"]
    joins = [ev for ev in events if ev.event_type == "queue_joined"]
    miss_hangups = [
        ev for ev in events
        if ev.event_type == "call_hangup" and "486" in (ev.payload.get("reason") or "")
    ]
    assert len(rejects) >= 1, "at least one 2311 miss expected"
    assert all(r.payload.get("agent_id") == "2311" for r in rejects), (
        f"every miss belongs to 2311: {[r.payload for r in rejects]}"
    )
    assert len(miss_hangups) == len(rejects), (
        f"each miss is one leg teardown: {[h.payload for h in miss_hangups]} vs "
        f"{[r.payload for r in rejects]}"
    )
    assert len(joins) == len(rejects) + 1, (
        f"EXACTLY one re-entry join per miss (misses={len(rejects)}), got "
        f"{len(joins)} joins: {[j.payload for j in joins]}"
    )
    # first join: wire-compatible (no attempt). Re-entries: strict 2..N+1.
    assert "attempt" not in joins[0].payload, (
        f"first join keeps the wire-compatible payload: {joins[0].payload}"
    )
    for n, join in enumerate(joins[1:], start=2):
        assert join.payload.get("attempt") == n, (
            f"re-entry #{n - 1} must carry attempt={n}: {join.payload}"
        )
        assert join.payload.get("queue_id") == "ring-grp", (
            f"re-entry stays in the SAME queue: {join.payload}"
        )
        assert join.payload.get("skill_groups") == ["ring-grp"], join.payload

    # ── exact payloads ──
    rejected = rejects[0]
    assert rejected.payload.get("leg_role") == "agent", rejected.payload
    miss_hangup = miss_hangups[0]
    assert miss_hangup.payload.get("leg_id") == rejected.payload.get("leg_id"), (
        f"the miss hangup is the rejected candidate's own leg: {miss_hangup.payload}"
    )
    assert miss_hangup.payload.get("leg_role") == "agent", miss_hangup.payload

    connected = next(ev for ev in events if ev.event_type == "queue_agent_connected")
    assert connected.payload.get("agent_id") == "2312", connected.payload
    _require_exact_counts(events, {"queue_agent_connected": 1})

    # ── exact order — join → the miss leg teardown → rejected → re-entry
    #    join → next offer → connect (the REAL webhook order; intermediate
    #    miss/re-entry pairs may sit between the first join and the final
    #    connect, each already covered by the conservation checks above) ──
    _require_order(events, [
        ("queue_joined", lambda p: "attempt" not in p),
        ("call_hangup", lambda p: "486" in (p.get("reason") or "")),
        ("queue_agent_rejected", lambda p: p.get("agent_id") == "2311"),
        ("queue_joined", lambda p: p.get("attempt") == 2),
    ])
    _require_order(events, [
        ("queue_agent_offered", lambda p: p.get("agent_id") == "2312"),
        ("queue_agent_connected", lambda p: p.get("agent_id") == "2312"),
    ])

    # roster hygiene
    for aid in ("2311", "2312"):
        try:
            await api.update_agent_status(aid, "offline")
        except Exception:  # noqa: BLE001
            pass


# ---------------------------------------------------------------------------
# C. transfer attribution clear
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_contract_c_transfer_to_extension_clears_agent_role(
    pbx, sipbot_pool, api, event_checker, rwi,
):
    """Queue call answered by 2313, blind-transferred (RWI call.transfer) to
    the plain extension 2314. The replacement's events carry no agent role
    and no agent_id enrichment; the departing agent is reported once by its
    own leg hangup."""
    webhook = event_checker.webhook
    await _ensure_group(api, "base-grp", ["base"])
    await _register_agent(pbx, sipbot_pool, api, "2313", skills=["base"])
    await _register_extension(pbx, sipbot_pool, "2314")

    baseline = len(list(webhook.events))
    caller = sipbot_pool.caller(
        target=f"sip:8899@{pbx.sip_addr}", username="2315", password="123456", hangup=120,
    )
    connected = await _wait_event(
        webhook, ["queue_agent_connected"],
        match={"payload.agent_id": "2313"}, timeout=30, since=baseline,
    )
    assert connected is not None, "agent 2313 never answered the queue call"
    call_id = connected.call_id
    await asyncio.sleep(1.5)  # settle the bridge

    resp = await rwi.transfer(call_id, "2314")
    assert str(resp.get("status", "")).lower() == "success", f"transfer rejected: {resp}"

    transferred = await _wait_event(
        webhook, ["call_transferred"], call_id=call_id, timeout=25, since=baseline,
    )
    assert transferred is not None, "no call_transferred event"
    answered = await _wait_event(
        webhook, ["call_answered"], call_id=call_id,
        timeout=30, since=transferred.sequence,
    )
    assert answered is not None, "transfer target never answered"
    # End the call and wait for the SESSION-scoped hangup — leg_id ABSENT
    # (the field is not serialized when None, so match on absence).
    await rwi.hangup(call_id)
    caller.terminate()
    async def _wait_session_hangup(timeout: float = 30.0):
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while True:
            for ev in list(webhook.events)[transferred.sequence:]:
                if (ev.call_id == call_id and ev.event_type == "call_hangup"
                        and "leg_id" not in ev.payload):
                    return ev
            if loop.time() >= deadline:
                return None
            await asyncio.sleep(0.25)
    final = await _wait_session_hangup()
    assert final is not None, "session-scoped call_hangup never arrived"
    await asyncio.sleep(2)  # freeze the stream — nothing may land after this

    events = _events_for(webhook, call_id)
    print(f"\n== contract C: {len(events)} webhook events for call {call_id}")
    _dump(events, "contract_c_transfer")

    transferred_idx = events.index(transferred)
    after = events[transferred_idx + 1:]

    # ── exact counts — 不能多，也不能少 ──
    _require_exact_counts(events, {
        "call_transferred": 1,
        "call_transfer_accepted": 1,
        "queue_agent_connected": 1,
        # the queue stage is over: NO further queue-agent lifecycle events
        # may fire on the transferred call.
        "queue_agent_offered": 1,
        "call_hangup": 2,            # exactly: departing agent leg + session
    })
    assert not [
        ev for ev in after if ev.event_type.startswith("queue_agent")
    ], f"no queue-agent lifecycle may follow the transfer: {[e.payload for e in after]}"

    # ── exact payloads ──
    # The replacement stage: no agent pin, positional fallback EXACTLY.
    replacement_stage = [
        ev for ev in after
        if ev.event_type in ("call_ringing", "call_answered")
        and ev.payload.get("leg_id") is not None
    ]
    assert replacement_stage, (
        f"no replacement-stage events: {[e.payload for e in after]}"
    )
    for ev in replacement_stage:
        assert ev.payload.get("leg_role") == "callee", (
            f"replacement leg carries the documented positional role: {ev.payload}"
        )
        assert "agent_id" not in ev.payload, (
            f"replacement event must not carry agent_id: {ev.payload}"
        )

    # The departing agent leg: reported EXACTLY once, by its own teardown.
    agent_leg_hangups = [
        ev for ev in after
        if ev.event_type == "call_hangup" and ev.payload.get("leg_role") == "agent"
    ]
    assert len(agent_leg_hangups) == 1, (
        f"exactly one departing-agent hangup, got: {[e.payload for e in agent_leg_hangups]}"
    )
    depart = agent_leg_hangups[0].payload
    assert depart.get("reason") == "UacBye", depart
    assert depart.get("hangup_by") == "system", depart
    assert depart.get("agent_id") == "2313", depart

    # The final session hangup: NO leg_role (the leg teardown was reported
    # by the departing agent's own hangup) — it MAY still carry agent_id:
    # the CC on_call_ended hook re-attributes the call for CDR / recording
    # metadata. The contract is about the STAGE events, not the post-call
    # attribution record.
    assert "leg_id" not in final.payload and "leg_role" not in final.payload, final.payload

    # ── exact order — transfer → replacement answers → agent leg leaves →
    #    session ends; and NOTHING agent-attributed after the agent leg. ──
    _require_order(events, [
        ("call_transfer_accepted", None),
        ("call_transferred", None),
        ("call_answered", lambda p: p.get("leg_id") is not None
         and "agent_id" not in p),
        ("call_hangup", lambda p: p.get("leg_role") == "agent"),
        ("call_hangup", lambda p: p.get("leg_id") is None),
    ])
    last_agent_pos = events.index(agent_leg_hangups[0])
    for ev in events[last_agent_pos + 1:]:
        payload = ev.payload
        assert payload.get("leg_role") != "agent", (
            f"post-transfer event must not carry the agent role: {payload}"
        )
        # agent_id enrichment on the SESSION-scoped terminal record is the
        # CC hook's post-call attribution — allowed. Every STAGE event
        # (leg-scoped, ringing/answered of the replacement) must be clean.
        if payload.get("leg_id") is not None or ev.event_type in (
            "call_ringing", "call_answered", "queue_agent_offered",
            "queue_agent_connected",
        ):
            assert not payload.get("agent_id"), (
                f"post-transfer stage event must not carry agent_id: {payload}"
            )

    # 3. The replacement rang/answered WITHOUT the agent pin — the b2bua
    #    path re-attaches the "callee" slot, the customer-anchored path dials
    #    an independent "transfer-*" leg; either way no agent role, and the
    #    positional fallback ("callee") is the documented non-CC contract.
    replacement_stage = [
        ev for ev in after
        if ev.event_type in ("call_ringing", "call_answered")
        and ev.payload.get("leg_id") is not None
    ]
    for ev in replacement_stage:
        assert ev.payload.get("leg_role") != "agent", (
            f"transfer target event must not claim the agent role: {ev.payload}"
        )
        assert not ev.payload.get("agent_id"), (
            f"transfer target event must not carry agent_id: {ev.payload}"
        )

    # roster hygiene
    try:
        await api.update_agent_status("2313", "offline")
    except Exception:  # noqa: BLE001
        pass
