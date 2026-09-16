"""Shared helpers for the cc/checklist.md regression suites (§3.2.1.2 / §3.2.3).

Strictness contract: every helper here either CONFIRMS the expected state or
raises / returns the observed value for the test to assert on — a silent pass
is a bug.
"""

from __future__ import annotations

import asyncio
import logging
import uuid
from typing import Optional

logger = logging.getLogger(__name__)

# Dedicated agents for the checklist suites (2000-range never collides with
# the shared 1001-1003 regression agents).
AGENT_PORT_BASE = 17200

# Queue route numbers wired in conftest (match {"to.user": number}).
QUEUE_NUMBERS = {
    "ovf-prime": "8891",
    "ovf-backup": "8892",
    "fb-grp": "8893",
    "mute-grp": "8894",
    "order-grp": "8895",
    "xfer-sg": "8896",
    "batch-sg": "8897",
    "hot-grp": "8898",
    "base-grp": "8899",
    "ring-grp": "8890",
    "loop-sg": "8889",
}

# Events that carry payload.call_id and mark a queue call's progress.
QUEUE_CALL_EVENTS = (
    "skill_group_call_queued",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_connected",
    "call_answered",
)


def _payload_match(ev, match: Optional[dict]) -> bool:
    """Check ``{"payload.<field>": value}`` style match against an event."""
    if not match:
        return True
    for path, expected in (match or {}).items():
        node = ev.payload
        for part in path.replace("payload.", "", 1).split("."):
            if not isinstance(node, dict) or part not in node:
                return False
            node = node[part]
        if node != expected:
            return False
    return True


async def wait_for_call_event(
    event_checker,
    types,
    *,
    timeout: float = 20.0,
    match: Optional[dict] = None,
    min_index: int = 0,
    poll: float = 0.25,
):
    """Wait for the first webhook event whose type ∈ ``types`` (optionally
    matching payload fields), scanning only events appended after list index
    ``min_index`` — so stale events from earlier calls/tests are never
    captured.

    Returns the WebhookEvent or None — callers assert (strict).
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    wanted = set(types)
    while True:
        events = list(event_checker.webhook.events)
        for ev in events[max(0, min_index):]:
            if ev.event_type not in wanted:
                continue
            if not _payload_match(ev, match):
                continue
            return ev
        if loop.time() >= deadline:
            return None
        await asyncio.sleep(poll)


def events_index(event_checker) -> int:
    """Current number of captured webhook events (append-only baseline)."""
    return len(list(event_checker.webhook.events))


async def wait_agent_status(
    api,
    agent_id: str,
    expected_base: str,
    *,
    timeout: float = 15.0,
    poll: float = 0.4,
) -> str:
    """Poll GET /cc/agents/{id} until the status BASE name equals expected.

    Returns the last full status string ("wrapup:call-1", "idle", ...).
    Never raises on mismatch — the caller asserts (strict, with context).
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    observed: Optional[str] = None
    while True:
        try:
            agent = await api.get_agent(agent_id)
            observed = (agent or {}).get("status")
        except Exception:  # noqa: BLE001 — transient 4xx/5xx during transitions
            observed = None
        if observed is not None and observed.split(":")[0] == expected_base:
            return observed
        if loop.time() >= deadline:
            return observed or ""
        await asyncio.sleep(poll)


async def assert_agent_status(api, agent_id: str, expected_base: str, *, timeout: float = 15.0) -> str:
    """wait_agent_status + hard assert — for setup/teardown that MUST hold."""
    observed = await wait_agent_status(api, agent_id, expected_base, timeout=timeout)
    assert observed.split(":")[0] == expected_base, (
        f"agent {agent_id} never reached {expected_base!r} within {timeout}s "
        f"(last status: {observed!r})"
    )
    return observed


async def register_agent(
    pbx,
    sipbot_pool,
    api,
    username: str,
    *,
    skills: Optional[list[str]] = None,
    answer_mode: str = "echo",
    ring_secs: int = 1,
    hangup_after: Optional[int] = 240,
    wait_idle: bool = True,
) -> None:
    """Create (idempotent) + optionally re-skill + register a checklist agent.

    Registers a sipbot extension and waits until the registrar bridge made
    the CC agent Idle (strict — setup failures must fail loudly).
    """
    agent = {
        "agent_id": username,
        "display_name": f"Checklist {username}",
        "skills": skills or [],
        "max_concurrency": 1,
        "role": "agent",
    }
    try:
        await api.create_agent(agent)
    except Exception as exc:  # noqa: BLE001 — 409 duplicate is fine
        if "409" not in str(exc) and "400" not in str(exc):
            raise
    if skills is not None:
        try:
            await api.update_agent(username, {
                "display_name": f"Checklist {username}",
                "skills": skills,
            })
        except Exception as exc:  # noqa: BLE001
            logger.warning("update_agent skills for %s failed: %s", username, exc)

    port = AGENT_PORT_BASE + (int(username) % 500 if username.isdigit() else 0)
    sipbot_pool.terminate_user(username)

    # Clear stale wrapup/break from a previous test's call so registration
    # can actually reach Idle (the bridge won't touch a non-Offline agent).
    try:
        await api.end_agent_wrapup(username)
    except Exception:  # noqa: BLE001
        pass
    try:
        await api.update_agent_status(username, "offline")
    except Exception:  # noqa: BLE001
        pass

    sipbot_pool.callee(
        host=pbx.host,
        port=port,
        username=username,
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=ring_secs,
        answer_mode=answer_mode,
        hangup_after=hangup_after,
    )
    if wait_idle:
        observed = await wait_agent_status(api, username, "idle", timeout=10)
        if observed.split(":")[0] != "idle":
            # Stale wrapup timer or lingering break — coerce once, strictly.
            try:
                await api.end_agent_wrapup(username)
            except Exception:  # noqa: BLE001
                pass
            try:
                await api.update_agent_status(username, "idle")
            except Exception:  # noqa: BLE001
                pass
            await assert_agent_status(api, username, "idle", timeout=15.0)


async def ensure_skill_group(api, group_id: str, skills: list[str], **extra) -> None:
    """Create the skill group if absent (idempotent). Extra kwargs pass through
    (overflow_groups, max_wait_secs, acd_policy, ...)."""
    body = {
        "skill_group_id": group_id,
        "display_name": f"Checklist {group_id}",
        "skills_required": skills,
        "overflow_groups": extra.pop("overflow_groups", []),
        "sla_target_secs": extra.pop("sla_target_secs", 30),
        "max_wait_secs": extra.pop("max_wait_secs", 90),
        **extra,
    }
    try:
        await api.create_skill_group(body)
    except Exception as exc:  # noqa: BLE001 — 409 duplicate is fine
        if "409" not in str(exc) and "400" not in str(exc):
            raise


async def dial_queue(pbx, sipbot_pool, group: str, *, caller_user: str = "1001",
                     hangup: int = 90):
    """Place a REAL caller call into the group's queue via its route number.

    The route (conftest) maps ``sip:<number>@realm`` → action queue → the
    queue app → skill-group dispatch. Returns the caller bot handle.
    """
    number = QUEUE_NUMBERS[group]
    return sipbot_pool.caller(
        target=f"sip:{number}@{pbx.sip_addr}",
        username=caller_user,
        password="123456",
        hangup=hangup,
    )


async def dial_queue_and_track(event_checker, pbx, sipbot_pool, group: str,
                               *, caller_user: str = "1001", timeout: float = 25.0):
    """Dial the queue route and discover the PBX call_id from the first
    queue-lifecycle event arriving AFTER the dial (index-baselined, so stale
    events from earlier calls/tests are never captured).
    Returns ``(caller, call_id)`` — call_id is None if no queue event arrived
    (caller asserts on the reason)."""
    baseline = len(list(event_checker.webhook.events))
    caller = await dial_queue(pbx, sipbot_pool, group, caller_user=caller_user)
    ev = await wait_for_call_event(
        event_checker, QUEUE_CALL_EVENTS, timeout=timeout, min_index=baseline)
    return caller, (ev.call_id if ev else None)


async def hangup_quietly(event_checker, *call_ids) -> None:
    for call_id in call_ids:
        try:
            await event_checker.rwi.hangup(call_id)
        except Exception:  # noqa: BLE001 — teardown best-effort
            pass


async def hangup_all_active(api) -> None:
    """Best-effort: BYE every active call so no leftover leg keeps an agent
    Busy into the next test."""
    try:
        calls = await api.list_active_calls()
    except Exception:  # noqa: BLE001
        return
    if isinstance(calls, dict):
        calls = calls.get("data") or calls.get("calls") or []
    for c in calls or []:
        cid = (c or {}).get("call_id")
        if cid:
            try:
                await api.end_call(cid)
            except Exception:  # noqa: BLE001
                pass


async def take_offline(api, *agent_ids: str, delete: bool = False) -> None:
    """Best-effort teardown: hang up every active call and force agents
    Offline so they never leak into a later test's candidate set (stale idle
    bots are the #1 flake source). Pass ``delete=True`` for final module
    cleanup only — deleting and recreating an agent mid-suite races the ACD
    dispatch, so it must not happen between tests that share the roster."""
    await hangup_all_active(api)
    for agent_id in agent_ids:
        try:
            await api.update_agent_status(agent_id, "offline")
        except Exception:  # noqa: BLE001
            try:
                await api.end_agent_wrapup(agent_id)
                await api.update_agent_status(agent_id, "offline")
            except Exception:  # noqa: BLE001
                pass
    for agent_id in agent_ids:
        try:
            observed = await wait_agent_status(api, agent_id, "offline", timeout=6)
            if observed.split(":")[0] != "offline":
                logger.warning("agent %s still %r after offline", agent_id, observed)
        except Exception:  # noqa: BLE001
            pass
    if delete:
        for agent_id in agent_ids:
            try:
                await api.delete_agent(agent_id)
            except Exception:  # noqa: BLE001
                pass


def new_call_id(prefix: str) -> str:
    return f"ck-{prefix}-{uuid.uuid4().hex[:8]}"
