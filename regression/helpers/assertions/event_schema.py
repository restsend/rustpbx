"""Strict RWI/webhook event assertions: type registry + payload schema checks.

Event wire format (verified against examples/rwi_events_*.jsonl captures and
src/rwi/webhook.rs): each event carries sequence/timestamp/call_id/event_type
plus a payload object. This module fails on missing/empty values instead of
silently matching.
"""

from __future__ import annotations

from typing import Optional

from .guards import require, require_non_empty, require_keys

# Events that are call-scoped: must carry a non-empty call_id.
CALL_SCOPED = {
    "call_created", "call_ringing", "call_answered", "call_hangup",
    "call_transferred", "call_hold_started", "call_hold_stopped",
    "media_hold_started", "media_hold_stopped", "media_play_started",
    "media_play_stopped", "record_started", "record_stopped", "record_paused",
    "record_resumed", "record_end", "recording_metadata_available",
    "queue_joined", "queue_left", "queue_agent_assigned",
    "skill_group_agent_assigned", "agent_state_changed", "agent_registered",
    "agent_unregistered", "ivr_node_entered", "ivr_node_exited",
    "ivr_flow_completed", "conference_joined", "conference_left",
    "conference_created", "conference_destroyed", "call_app_started",
    "call_app_stopped", "dtmf_received",
}

# Per-type required payload extras (declared from observed emissions; extend as
# suites demand). Validated when the event type matches.
TYPE_EXTRAS: dict[str, tuple[str, ...]] = {
    "call_transferred": ("transfer_target_type",),
}

# Canonical ordering constraints used by assert_event_flow().
LIFECYCLE_ORDER = ("call_created", "call_ringing", "call_answered", "call_hangup")

# `call_answered` is session-scoped: exactly one per call_id, with NO `leg_id`
# in the payload (leg transitions no longer emit their own answered events).
# Suites must not wait for per-leg answered events.


def event_type(ev) -> str:
    if isinstance(ev, str):
        return ev
    require(ev, "event object")
    for key in ("event_type", "type", "eventType"):
        v = ev.get(key)
        if v:
            return str(v)
    raise AssertionError(f"[event] no event type field in {ev!r:.200}")


def event_call_id(ev) -> Optional[str]:
    if isinstance(ev, dict):
        for key in ("call_id", "callId"):
            v = ev.get(key)
            if v:
                return str(v)
    return None


def validate_event(ev, *, expect_type: Optional[str] = None, expect_call_id: Optional[str] = None) -> dict:
    """Validate one event dict: type non-empty, call_id present for call-scoped
    events, payload keys per TYPE_EXTRAS. Returns the (possibly unwrapped) event."""
    ev = ev.get("event") if isinstance(ev, dict) and isinstance(ev.get("event"), dict) and "event_type" not in ev else ev
    require(ev, "event")
    if not isinstance(ev, dict):
        raise AssertionError(f"[event] event is not a dict: {ev!r:.200}")
    etype = event_type(ev)
    require_non_empty(etype, "event_type")
    if expect_type is not None and etype != expect_type:
        raise AssertionError(f"[event] expected type {expect_type!r}, got {etype!r}")
    cid = event_call_id(ev)
    if etype in CALL_SCOPED:
        require_non_empty(cid, f"call_id for {etype}", "call-scoped events must carry a call id")
    if expect_call_id is not None and cid is not None and cid != expect_call_id:
        raise AssertionError(f"[event] {etype}: call_id {cid!r} != expected {expect_call_id!r}")
    extras = TYPE_EXTRAS.get(etype)
    if extras:
        payload = ev.get("payload", ev)
        require_keys(payload, extras, f"{etype} payload")
    return ev


def assert_event_flow(
    events,
    expected_flow,
    *,
    call_id: Optional[str] = None,
    allow_interleaved: bool = True,
) -> list:
    """Strict ordered subsequence match (like EventChecker.expect_webhook_sequence,
    but validates each event via validate_event and returns normalized events).

    expected_flow: list of event type strings that must appear IN ORDER.
    allow_interleaved: other event types may interleave (default). If False the
    flow must match exactly (no extras between).
    """
    require_non_empty(events, "event list", "no events captured — check webhook/RWI wiring")
    normalized = [validate_event(e, expect_call_id=call_id) for e in events]
    types = [event_type(e) for e in normalized]

    idx = 0
    for want in expected_flow:
        while idx < len(types) and types[idx] != want:
            if not allow_interleaved:
                raise AssertionError(
                    f"[event-flow] strict mismatch at position {idx}: expected {want!r}, got {types[idx]!r}\n"
                    f"flow so far: {types[: idx + 5]}"
                )
            idx += 1
        if idx >= len(types):
            raise AssertionError(
                f"[event-flow] expected {want!r} in order but exhausted events. "
                f"got: {types}"
            )
        idx += 1

    if not allow_interleaved and len(types) != len(expected_flow):
        raise AssertionError(
            f"[event-flow] strict flow length {len(types)} != expected {len(expected_flow)}: {types}"
        )
    return normalized


def assert_no_ghost_events(events, forbidden_types, *, what: str = "") -> None:
    """Assert none of *forbidden_types* appeared (e.g. 'call abandoned' race)."""
    seen = [event_type(e) for e in events if event_type(e) in set(forbidden_types)]
    if seen:
        raise AssertionError(f"[event]{(' ' + what) if what else ''} forbidden events present: {seen}")


# Call-timeline contract: cross-event checks over one call's event stream
# (shared by the pytest suites and regression/scripts/rwi_timeline_check.py).

# Normalized event shape: `event_type` plus optional `payload` (fields may
# also sit flat on the event).
def _field(ev: dict, key: str):
    payload = ev.get("payload")
    if isinstance(payload, dict) and payload.get(key) is not None:
        return payload[key]
    return ev.get(key)


def _has_leg(ev: dict) -> bool:
    """True when the event is leg-scoped (carries a non-null leg_id)."""
    return _field(ev, "leg_id") is not None


def check_call_timeline(
    events,
    *,
    call_id: Optional[str] = None,
    agent_id: Optional[str] = None,
    require_agent: Optional[bool] = None,
) -> tuple[list[str], list[str]]:
    """Cross-event contract over one call's event stream.

    Returns ``(violations, warnings)``; raises nothing. Checks lifecycle
    shape (single call_created/call_answered), queue-agent attribution on
    ringing/offered/connected/answered/hangup, flow ordering, and
    attribution stability.
    """
    violations: list[str] = []
    warnings: list[str] = []

    if not events:
        return ([f"no events to check{f' for {call_id}' if call_id else ''}"], [])

    types = [str(event_type(e)) for e in events]

    # ── lifecycle shape ────────────────────────────────────────────────
    if types[0] != "call_created":
        warnings.append(
            f"timeline does not start with call_created (first={types[0]!r}) "
            "— capture may be truncated"
        )
    if "call_created" in types and types.count("call_created") > 1:
        violations.append(f"call_created emitted {types.count('call_created')}x (expected 1)")
    answered = [e for e in events if str(event_type(e)) == "call_answered"]
    if len(answered) > 1:
        violations.append(
            f"call_answered emitted {len(answered)}x (expected exactly 1 — "
            "session-scoped, no per-leg answered events)"
        )
    for ev in answered:
        if _has_leg(ev):
            violations.append(
                f"call_answered must be session-scoped (no leg_id), got leg_id="
                f"{_field(ev, 'leg_id')!r}"
            )

    # ── attribution contract ───────────────────────────────────────────
    queue_context = "queue_joined" in types or "skill_group_agent_assigned" in types
    active = require_agent if require_agent is not None else (queue_context or agent_id is not None)

    # Expected agent value: explicit, or the first attributed value seen on
    # a type that MUST carry it (order: offered → ringings → answered …).
    expected_agent = agent_id
    if active and expected_agent is None:
        for probe in ("queue_agent_offered", "queue_agent_connected", "call_answered"):
            for ev in events:
                if str(event_type(ev)) == probe and _field(ev, "agent_id"):
                    expected_agent = str(_field(ev, "agent_id"))
                    break
            if expected_agent:
                break

    # The authoritative agent must never CHANGE mid-call.
    if expected_agent:
        for ev in events:
            aid = _field(ev, "agent_id")
            if aid is not None and str(aid) != str(expected_agent):
                violations.append(
                    f"agent attribution changed mid-call: {event_type(ev)} carries "
                    f"agent_id={aid!r}, expected {expected_agent!r}"
                )

    def _attribution_lacking(etype: str) -> list:
        out = []
        for ev in events:
            if str(event_type(ev)) != etype:
                continue
            aid = _field(ev, "agent_id")
            scope = "leg-level" if _has_leg(ev) else "session-level"
            if not active:
                continue
            if aid is None or str(aid) == "":
                out.append(
                    f"{etype} ({scope}) is missing agent_id"
                    + (f" (expected {expected_agent!r})" if expected_agent else "")
                )
            elif expected_agent and str(aid) != str(expected_agent):
                out.append(f"{etype} ({scope}) carries agent_id={aid!r}, expected {expected_agent!r}")
        return out

    if active:
        # Every call_ringing — leg- and session-scoped — must carry agent_id.
        violations.extend(_attribution_lacking("call_ringing"))
        violations.extend(_attribution_lacking("call_answered"))
        violations.extend(_attribution_lacking("call_hangup"))
        ringings = [e for e in events if str(event_type(e)) == "call_ringing"]
        if len(ringings) < 2:
            warnings.append(
                f"expected >=2 call_ringing (leg-level + session-level), got {len(ringings)}"
            )

    # ── queue flow ordering ────────────────────────────────────────────
    def _first_index(etype: str) -> Optional[int]:
        return next((i for i, t in enumerate(types) if t == etype), None)

    i_offered = _first_index("queue_agent_offered")
    i_connected = _first_index("queue_agent_connected")
    i_first_ring = _first_index("call_ringing")
    if i_offered is not None and i_connected is not None and i_offered > i_connected:
        violations.append("queue_agent_offered arrived AFTER queue_agent_connected")
    if i_first_ring is not None and i_offered is not None and i_first_ring > i_offered:
        violations.append("call_ringing arrived AFTER queue_agent_offered")

    return (violations, warnings)


def assert_call_timeline(
    events,
    *,
    call_id: Optional[str] = None,
    agent_id: Optional[str] = None,
    require_agent: Optional[bool] = None,
    what: str = "",
) -> tuple[list[str], list[str]]:
    """Pytest-facing wrapper: raise on the first contract violation."""
    violations, warnings = check_call_timeline(
        events, call_id=call_id, agent_id=agent_id, require_agent=require_agent
    )
    label = f" for {call_id}" if call_id else ""
    if violations:
        raise AssertionError(
            f"[timeline]{(' ' + what) if what else ''}{label}: "
            f"{len(violations)} contract violation(s):\n  - " + "\n  - ".join(violations)
        )
    return (violations, warnings)
