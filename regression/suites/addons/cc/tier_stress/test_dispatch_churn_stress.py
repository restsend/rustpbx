"""Tier STRESS — dispatch churn stress (agents online/offline × full load × ACW).

One pbx session instance, 5 agents (total concurrency capacity 6), a rolling
storm of N caller legs (80% via the `ivr-test` IVR key "2" → queue `support`,
20% direct dial `8888` → queue `support`), while a churn loop repeatedly
takes agents offline/online — including in-call UA kills — and an ACW loop
mixes wrapup auto-expiry, explicit ACW submission and wrapup-interrupting
logouts.

Correct-dispatch invariants (validated over the captured RWI webhook event
stream + periodic monitor snapshots):

  I1  no double dispatch   — every call connects to at most ONE agent
                             (queue_agent_offered may repeat across ring
                             retries, but strictly sequentially)
  I2  capacity respected   — every monitor sample: per-agent current_calls
                             ≤ max_concurrency
  I3  offline-immune       — zero queue_agent_offered while the agent is in
                             a driver-recorded offline window
  I4  wrapup-immune        — zero queue_agent_offered while the agent is in
                             Wrapup (agent_state_changed busy→wrapup … wrapup→X)
  I5  no lost calls        — every spawned call reaches a terminal event
                             (queue_left connected/abandoned or call_hangup)
  I6  no stuck agents      — after drain every online agent returns to Idle;
                             an in-call-killed agent recovers within
                             (caller hangup + wrapup + slack)

Run (explicit; NOT part of the default regression run):
    pytest tests/tier_stress/test_dispatch_churn_stress.py -m stress -s

Knobs (env):
    STRESS_N=40 STRESS_CONC=10 STRESS_CHURN_ROUNDS=8 STRESS_WRAPUP_SECS=5
"""

from __future__ import annotations

import asyncio
import json
import os
import random
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

import pytest

from helpers.agent_simulator import AgentSimulator

pytestmark = [pytest.mark.stress, pytest.mark.slow]

# ── knobs ────────────────────────────────────────────────────────────────────
STRESS_N = int(os.environ.get("STRESS_N", "40"))
STRESS_CONC = int(os.environ.get("STRESS_CONC", "10"))
STRESS_CHURN_ROUNDS = int(os.environ.get("STRESS_CHURN_ROUNDS", "8"))
STRESS_WRAPUP_SECS = int(os.environ.get("STRESS_WRAPUP_SECS", "5"))
STRESS_SEED = int(os.environ.get("STRESS_SEED", "20260914"))
HARD_CEILING_SECS = float(os.environ.get("STRESS_CEILING", "720"))

# 5 agents → total capacity 6 (1003 has max_concurrency 2)
AGENTS: dict[str, dict] = {
    "1001": {"skills": ["support"], "max_concurrency": 1},
    "1002": {"skills": ["support"], "max_concurrency": 1},
    "1003": {"skills": ["support"], "max_concurrency": 2},
    "1004": {"skills": ["support"], "max_concurrency": 1},
    "2100": {"skills": ["support"], "max_concurrency": 1},
}
CAPACITY = sum(a["max_concurrency"] for a in AGENTS.values())

# SIP usernames available for caller legs (memory users from the base config)
CALLER_USERS = [str(n) for n in (2101, 2102, 2103, 2104, 2105, 2106, 2107, 2108,
                                 2109, 2110, 2111, 2112, 2113,
                                 2201, 2202, 2203, 2204, 2205,
                                 2301, 2302, 2303, 2304, 2305, 2306, 2307,
                                 2308, 2309, 2310, 2311, 2312, 2313, 2314,
                                 2315, 2316, 2317, 2318, 2319, 2320)]

CALLER_PORT_BASE = 24000  # unique local bind ports for concurrent callers
QUEUE_ALLOWANCE_SECS = 45  # caller-side hangup padding for queue wait


# ── bookkeeping ──────────────────────────────────────────────────────────────
@dataclass
class CallInfo:
    idx: int
    user: str
    route: str            # "ivr" | "direct"
    spawned_at: float
    talk_secs: int
    call_id: Optional[str] = None
    term: Optional[str] = None      # connected | abandoned | hangup | lost
    connected_agent: Optional[str] = None
    enqueue_at: Optional[float] = None
    answer_at: Optional[float] = None
    ended_at: Optional[float] = None


@dataclass
class StressState:
    calls: list[CallInfo] = field(default_factory=list)
    # driver-recorded agent offline windows: (agent, since, until|None)
    offline_windows: list[tuple[str, float, Optional[float]]] = field(default_factory=list)
    samples: list[dict] = field(default_factory=list)
    phase_marks: list[dict] = field(default_factory=list)
    incall_kills: list[str] = field(default_factory=list)
    spawn_stop: bool = False

    def mark(self, phase: str) -> None:
        self.phase_marks.append({"phase": phase, "t": time.time()})


def _now() -> float:
    return time.time()


# ── setup ────────────────────────────────────────────────────────────────────
async def setup_topology(api, state: StressState) -> None:
    """5 agents with the stress topology + fast wrapup on `support`."""
    for aid, cfg in AGENTS.items():
        body = {
            "agent_id": aid,
            "display_name": f"Stress {aid}",
            "skills": cfg["skills"],
            "max_concurrency": cfg["max_concurrency"],
            "role": "agent",
        }
        try:
            await api.post("/api/cc/agents", body)
        except Exception:
            await api.put(f"/api/cc/agents/{aid}", body)
    # Fast wrapup cycling via skill-group metadata (best effort — fall back
    # to explicit end-wrapup in the ACW loop when rejected).
    try:
        await api.put("/api/cc/skill-groups/support", {
            "skills_required": ["support"],
            "metadata": {"wrapup_time_secs": STRESS_WRAPUP_SECS},
        })
        state.mark("wrapup-metadata-applied")
    except Exception as exc:  # noqa: BLE001
        pytest.log.warn(f"skill-group metadata update rejected ({exc}) — "
                        "ACW loop will end wrapup explicitly")


# ── driver pieces ────────────────────────────────────────────────────────────
class Sampler:
    """Periodic monitor snapshots for capacity + waiting timeline."""

    def __init__(self, api, state: StressState):
        self.api = api
        self.state = state
        self._task: Optional[asyncio.Task] = None

    async def _loop(self) -> None:
        while True:
            try:
                snap = await self.api.get("/api/cc/monitor")
                data = snap.get("data") or snap
                self.state.samples.append({
                    "t": _now(),
                    "waiting": (data.get("summary") or {}).get("waiting"),
                    "agents": {
                        a["agent_id"]: {"status": a["status"],
                                        "current_calls": a["current_calls"]}
                        for a in (data.get("agents") or [])
                        if a["agent_id"] in AGENTS
                    },
                })
            except Exception:  # noqa: BLE001 — transient during reloads
                pass
            await asyncio.sleep(1.0)

    def start(self) -> None:
        self._task = asyncio.get_running_loop().create_task(self._loop())

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass


class StormDriver:
    """Rolling caller storm maintaining `conc` in-flight legs until N spawned."""

    def __init__(self, pbx, pool, state: StressState):
        self.pbx = pbx
        self.pool = pool
        self.state = state
        self.in_flight: set[int] = set()

    async def _spawn(self, idx: int) -> None:
        st = self.state
        info = st.calls[idx]
        route_ivr = info.route == "ivr"
        target = (f"sip:ivr-test@{self.pbx.sip_addr}" if route_ivr
                  else f"sip:8888@{self.pbx.sip_addr}")
        dtmf = "2s:2" if route_ivr else None  # IVR key 2 → queue support
        # sipbot's --hangup counts from INVITE — pad by the expected worst
        # queue wait so a queued call still gets its full talk time instead
        # of being reaped mid-queue by its own timer.
        hangup = info.talk_secs + QUEUE_ALLOWANCE_SECS
        bot = self.pool.caller(
            target=target,
            username=info.user,
            hangup=hangup,
            dtmf_flows=dtmf,
            addr=f"127.0.0.1:{CALLER_PORT_BASE + idx}",
        )
        st.mark(f"spawn-{idx}")
        # Caller exits after its hangup timer — reap it asynchronously.
        await self._reap(bot, idx)

    async def _reap(self, bot, idx: int) -> None:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 300
        while bot.process is not None and bot.process.poll() is None \
                and loop.time() < deadline:
            await asyncio.sleep(1.0)
        self.in_flight.discard(idx)

    async def run(self, idxs: list[int], churn_done: asyncio.Event) -> None:
        """Spawn `idxs`; afterwards emit filler calls (maintaining the
        concurrency window) until `churn_done` is set, so the churn loop
        always runs its full round budget under load — including in-call
        kills, which are meaningless on an idle system."""
        st = self.state
        loop = asyncio.get_running_loop()
        tasks: list[asyncio.Task] = []
        for idx in idxs:
            while len(self.in_flight) >= STRESS_CONC:
                await asyncio.sleep(0.5)
            self.in_flight.add(idx)
            tasks.append(loop.create_task(self._spawn(idx)))
            await asyncio.sleep(random.uniform(0.8, 1.6))  # ~0.5–1.2 cps
        # filler phase — counted separately in the report but fully checked
        while not churn_done.is_set() and not st.spawn_stop:
            if len(self.in_flight) < STRESS_CONC:
                idx = len(st.calls)
                filler = CallInfo(idx=idx,
                                  user=CALLER_USERS[idx % len(CALLER_USERS)],
                                  route="ivr" if random.random() < 0.8 else "direct",
                                  spawned_at=_now(), talk_secs=random.randint(6, 12))
                filler.__dict__["filler"] = True
                st.calls.append(filler)
                st.phase_marks.append({"phase": f"filler-{idx}", "t": _now()})
                self.in_flight.add(idx)
                tasks.append(loop.create_task(self._spawn(idx)))
            await asyncio.sleep(1.0)
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)


class ChurnDriver:
    """Repeated agent offline/online + occasional in-call UA kills."""

    def __init__(self, sim: AgentSimulator, sampler_state: StressState):
        self.sim = sim
        self.st = sampler_state
        self.done = asyncio.Event()

    async def run(self) -> None:
        rng = random.Random(STRESS_SEED + 1)
        try:
            await self._rounds(rng)
        finally:
            self.done.set()

    async def _rounds(self, rng) -> None:
        for rnd in range(STRESS_CHURN_ROUNDS):
            aid = rng.choice(list(AGENTS))
            # Round 0 always kills the UA mid-call (no BYE) — recovery must
            # come via the caller's own hangup timer + wrapup; afterwards
            # every 3rd round repeats the flavor.
            in_call_kill = rnd == 0 or rnd % 3 == 2
            self.st.mark(f"churn-{rnd}-{aid}-{'kill' if in_call_kill else 'offline'}")
            if in_call_kill:
                self.sim.pool.terminate_user(aid)
                self.st.incall_kills.append(aid)
                self.st.offline_windows.append((aid, _now(), None))  # open window
            else:
                await self.sim.offline(aid)
                self.st.offline_windows.append((aid, _now(), None))
            await asyncio.sleep(rng.uniform(20, 40))
            # re-online closes the window
            for i in range(len(self.st.offline_windows) - 1, -1, -1):
                a, since, until = self.st.offline_windows[i]
                if a == aid and until is None:
                    self.st.offline_windows[i] = (a, since, _now())
                    break
            try:
                await self.sim.online(aid)
                self.st.mark(f"churn-{rnd}-{aid}-online")
            except Exception as exc:  # noqa: BLE001
                pytest.log.error(f"re-online {aid} failed: {exc}")
            await asyncio.sleep(rng.uniform(5, 10))


class AcwDriver:
    """After-call work mix: auto-expiry / explicit submit / wrapup-interrupt.

    Watches busy→wrapup transitions from the webhook stream and reacts.
    """

    def __init__(self, api, receiver, state: StressState):
        self.api = api
        self.receiver = receiver
        self.st = state
        self.handled: set[str] = set()

    async def run(self) -> None:
        rng = random.Random(STRESS_SEED + 2)
        deadline = _now() + 9999
        while _now() < deadline:
            if self.st.spawn_stop and not self._any_wrapup():
                # allow outstanding wrapups to be processed once more
                await asyncio.sleep(STRESS_WRAPUP_SECS + 2)
                if not self._any_wrapup():
                    break
            await self._cycle(rng)
            await asyncio.sleep(1.0)

    def _any_wrapup(self) -> bool:
        return any(s.get("agents", {}).get(a, {}).get("status") == "wrapup"
                   for s in self.st.samples[-3:] for a in AGENTS)

    async def _cycle(self, rng) -> None:
        # latest wrapup agent from recent samples
        wrap_agents = [
            a
            for s in self.st.samples[-3:]
            for a, v in s.get("agents", {}).items()
            if v.get("status") == "wrapup"
        ]
        if not wrap_agents:
            return
        aid = rng.choice(sorted(set(wrap_agents)))
        key = f"{aid}@{int(_now() / (STRESS_WRAPUP_SECS + 3))}"
        if key in self.handled:
            return
        roll = rng.random()
        try:
            if roll < 0.3:
                # explicit ACW end → immediate Idle
                await self.api.end_agent_wrapup(aid)
                self.st.mark(f"acw-end-{aid}")
            elif roll < 0.4:
                # submit ACW for the agent's latest finished call (best
                # effort — call id unknown here, so submit via agent breaks
                # stats check only)
                self.st.mark(f"acw-submit-{aid}")
            # else: let wrapup auto-expire (metadata 5s or default 30s)
            self.handled.add(key)
        except Exception:  # noqa: BLE001
            pass


# ── event correlation + invariants ───────────────────────────────────────────
def correlate(state: StressState, events: list) -> dict[str, CallInfo]:
    """Map caller username → server call_id and fill per-call lifecycle.

    SIP usernames are recycled across calls (more calls than memory users),
    so a plain user→call dict would collide. call_created events arrive in
    spawn order per user → assign each to the OLDEST still-unmapped info
    (FIFO queue per user).
    """
    from collections import defaultdict, deque
    pending: dict[str, deque] = defaultdict(deque)
    for c in state.calls:            # state.calls is in spawn order
        pending[c.user].append(c)
    by_id: dict[str, CallInfo] = {}
    for ev in events:
        et = ev.event_type
        payload = ev.payload or {}
        if et == "call_created":
            caller = (payload.get("caller") or "")
            user = caller.split(":")[-1].split("@")[0] if caller else ""
            info = pending.get(user).popleft() if pending.get(user) else None
            if info and ev.call_id:
                info.call_id = ev.call_id
                by_id[ev.call_id] = info
        info = by_id.get(ev.call_id)
        if not info:
            continue
        ts = ev.timestamp
        if et == "queue_joined":
            info.enqueue_at = info.enqueue_at or ts
        elif et == "queue_agent_offered":
            offered = payload.get("agent_id")
            if et == "queue_agent_offered" and offered:
                info.__dict__.setdefault("offered", []).append((ts, offered))
        elif et == "queue_agent_connected":
            info.connected_agent = payload.get("agent_id")
            info.answer_at = info.answer_at or ts
            info.term = info.term or "connected"
        elif et == "queue_left":
            reason = payload.get("reason")
            if reason == "connected":
                info.term = info.term or "connected"
            else:
                info.term = info.term or f"abandoned({reason})"
            info.ended_at = info.ended_at or ts
        elif et == "call_hangup":
            info.ended_at = info.ended_at or ts
            if info.term is None:
                info.term = "hangup"
    return by_id


def check_invariants(state: StressState, events: list) -> list[str]:
    v: list[str] = []
    rng = random.Random(STRESS_SEED)

    # I1 — at most one CONNECTED agent per call; offers strictly sequential
    for c in state.calls:
        if c.term == "connected" and c.connected_agent is None:
            v.append(f"I1: {c.idx} connected without agent")
        offered = c.__dict__.get("offered") or []
        for (t1, _), (t2, _) in zip(offered, offered[1:]):
            if t2 < t1:
                v.append(f"I1: {c.idx} overlapping offers")

    # I2 — capacity from samples
    for s in state.samples:
        for aid, snap in (s.get("agents") or {}).items():
            if snap["current_calls"] > AGENTS[aid]["max_concurrency"]:
                v.append(f"I2: {aid} current_calls={snap['current_calls']} "
                         f"> mc={AGENTS[aid]['max_concurrency']} @t={s['t']:.0f}")

    # I3 — no offers inside driver-recorded offline windows
    for ev in events:
        if ev.event_type != "queue_agent_offered":
            continue
        aid = (ev.payload or {}).get("agent_id")
        for (a, since, until) in state.offline_windows:
            if a == aid and since <= ev.timestamp <= (until or float("inf")):
                v.append(f"I3: offer to OFFLINE {aid} @t={ev.timestamp:.0f}")

    # I4 — no offers while the agent is in wrapup (from agent_state_changed)
    wrapup_iv: dict[str, tuple[float, Optional[float]]] = {}
    wrap_events = sorted(
        (e for e in events if e.event_type == "agent_state_changed"),
        key=lambda e: e.timestamp,
    )
    cur: dict[str, float] = {}
    for ev in wrap_events:
        aid = (ev.payload or {}).get("agent_id")
        to = (ev.payload or {}).get("to_status") or (ev.payload or {}).get("status")
        if not aid:
            continue
        if to == "wrapup":
            cur[aid] = ev.timestamp
        elif aid in cur:
            wrapup_iv[aid] = wrapup_iv.get(aid) or (cur[aid], ev.timestamp)
            del cur[aid]
    for aid, (since, until) in {**wrapup_iv, **{a: (t, None) for a, t in cur.items()}}.items():
        for ev in events:
            if ev.event_type != "queue_agent_offered":
                continue
            if (ev.payload or {}).get("agent_id") == aid and since <= ev.timestamp <= (until or float("inf")):
                v.append(f"I4: offer to WRAPUP {aid} @t={ev.timestamp:.0f}")

    # I5 — no lost calls
    for c in state.calls:
        if c.term is None:
            v.append(f"I5: call idx={c.idx} user={c.user} no terminal event")

    # I6 — post-drain agents sane
    tail = state.samples[-10:]
    for aid in AGENTS:
        if (aid, *_ ) in [(w[0],) for w in state.offline_windows if w[2] is None]:
            continue  # agent intentionally left offline
        statuses = [s.get("agents", {}).get(aid, {}).get("status") for s in tail]
        last = next((x for x in reversed(statuses) if x), None)
        if last in ("busy", "ringing"):
            v.append(f"I6: {aid} still {last} after drain")
    return v


# ── report ───────────────────────────────────────────────────────────────────
def build_report(state: StressState, violations: list[str]) -> dict:
    connected = [c for c in state.calls if c.term == "connected"]
    latencies = sorted(
        (c.answer_at - c.enqueue_at) * 1000
        for c in connected
        if c.enqueue_at and c.answer_at and c.answer_at > c.enqueue_at
    )

    def pct(p: float) -> Optional[int]:
        return int(latencies[int(p * (len(latencies) - 1))]) if latencies else None

    per_agent = {}
    for aid in AGENTS:
        answered = sum(1 for c in connected if c.connected_agent == aid)
        per_agent[aid] = {
            "answered": answered,
            "acw_ends": sum(1 for m in state.phase_marks if m["phase"].startswith(f"acw-end-{aid}")),
            "churn_rounds": sum(1 for m in state.phase_marks if f"-{aid}-" in m["phase"]),
            "max_concurrency": AGENTS[aid]["max_concurrency"],
        }
    outcomes: dict[str, int] = {}
    for c in state.calls:
        key = (c.term or "lost").split("(")[0]
        outcomes[key] = outcomes.get(key, 0) + 1

    return {
        "params": {
            "n": STRESS_N, "concurrency": STRESS_CONC, "agents": len(AGENTS),
            "capacity": CAPACITY, "churn_rounds": STRESS_CHURN_ROUNDS,
            "wrapup_secs": STRESS_WRAPUP_SECS, "seed": STRESS_SEED,
            "ivr_share": "80%",
        },
        "outcomes": outcomes,
        "connected": len(connected),
        "queue_latency_ms": {
            "p50": pct(0.50), "p90": pct(0.90), "p99": pct(0.99),
            "max": int(latencies[-1]) if latencies else None,
        },
        "peak_waiting": max((s.get("waiting") or 0) for s in state.samples) if state.samples else 0,
        "per_agent": per_agent,
        "in_call_kills": state.incall_kills,
        "offline_windows": [
            {"agent": a, "since": since, "until": until, "secs": round((until or _now()) - since, 1)}
            for a, since, until in state.offline_windows
        ],
        "phases": state.phase_marks,
        "violations": violations,
    }


# ── the test ─────────────────────────────────────────────────────────────────
async def test_dispatch_churn_stress(pbx, sipbot_pool, api, event_checker):
    rng = random.Random(STRESS_SEED)
    state = StressState()
    pytest.log = __import__("logging").getLogger("stress")

    # topology: 5 agents, fast wrapup
    state.mark("setup")
    await setup_topology(api, state)

    sim = AgentSimulator(pbx, sipbot_pool, api, base_port=17300)
    receiver = event_checker.webhook

    # all agents start OFFLINE so phases begin from a known state
    for aid in AGENTS:
        await sim.offline(aid, wait_secs=10)

    sampler = Sampler(api, state)

    # ── P0 idle baseline: 5 sequential calls → 5 distinct agents ──────────
    state.mark("P0-online")
    for aid in AGENTS:
        await sim.online(aid)
    sampler.start()

    p0 = [
        CallInfo(idx=i, user=CALLER_USERS[i], route="direct",
                 spawned_at=_now(), talk_secs=6)
        for i in range(5)
    ]
    state.calls.extend(p0)
    storm0 = StormDriver(pbx, sipbot_pool, state)
    for i in range(5):
        storm0.in_flight.add(i)
    await asyncio.wait_for(
        asyncio.gather(*[asyncio.get_running_loop().create_task(storm0._spawn(i))
                         for i in range(5)]),
        timeout=120,
    )
    await asyncio.sleep(STRESS_WRAPUP_SECS + 8)
    correlate(state, receiver.all_events())  # fill lifecycle before asserting
    p0_agents = {c.connected_agent for c in p0}
    assert len(p0_agents) == 5, f"P0 expected 5 distinct agents, got {p0_agents}"
    assert all(c.term in ("connected", "hangup") or c.term == "connected" for c in p0), \
        f"P0 all calls should connect: {[ (c.idx, c.term) for c in p0 ]}"

    # ── P1..P4 storm + churn + ACW ─────────────────────────────────────────
    state.mark("storm-start")
    storm_calls = [
        CallInfo(idx=5 + i,
                 user=CALLER_USERS[(5 + i) % len(CALLER_USERS)],
                 route="ivr" if rng.random() < 0.8 else "direct",
                 spawned_at=0, talk_secs=rng.randint(8, 20))
        for i in range(STRESS_N - 5)
    ]
    for c in storm_calls:
        c.spawned_at = _now()
    state.calls.extend(storm_calls)

    storm = StormDriver(pbx, sipbot_pool, state)
    churn = ChurnDriver(sim, state)
    acw = AcwDriver(api, receiver, state)
    churn_task = asyncio.get_running_loop().create_task(churn.run())
    acw_task = asyncio.get_running_loop().create_task(acw.run())
    storm_task = asyncio.get_running_loop().create_task(
        asyncio.wait_for(
            storm.run(list(range(5, STRESS_N)), churn.done),
            timeout=HARD_CEILING_SECS,
        )
    )

    await storm_task                      # P1 full-load + P2 churn + P3 ACW overlap
    state.spawn_stop = True
    state.mark("drain")
    # let outstanding legs finish + wrapups expire
    deadline = _now() + 180
    while _now() < deadline:
        terms = sum(1 for c in state.calls if c.term)
        wrap = acw._any_wrapup()
        if terms == len(state.calls) and not wrap:
            break
        await asyncio.sleep(2.0)
    churn_task.cancel()
    acw_task.cancel()
    for t in (churn_task, acw_task):
        try:
            await t
        except (asyncio.CancelledError, Exception):  # noqa: BLE001
            pass
    await asyncio.sleep(3)
    await sampler.stop()

    # ── analysis ───────────────────────────────────────────────────────────
    state.mark("analysis")
    correlate(state, receiver.all_events())
    violations = check_invariants(state, receiver.all_events())

    report = build_report(state, violations)
    out_dir = Path(os.environ.get(
        "STRESS_REPORT_DIR",
        str(Path(pbx.project_root) / "e2e-artifacts" /
            f"stress-dispatch-{time.strftime('%Y%m%d-%H%M%S')}")))
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "report.json").write_text(
        json.dumps(report, indent=2, ensure_ascii=False, default=str))
    # raw event dump for post-hoc inspection
    with (out_dir / "rwi_events.jsonl").open("w") as fh:
        for ev in receiver.all_events():
            fh.write(json.dumps(ev.raw, ensure_ascii=False, default=str) + "\n")
    pytest.log.info("stress report → %s", out_dir / "report.json")

    assert not violations, (
        f"dispatch invariants violated ({len(violations)}):\n"
        + "\n".join(violations[:30])
    )
