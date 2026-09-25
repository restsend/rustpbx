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
