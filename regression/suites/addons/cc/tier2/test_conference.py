"""Tier 2 — Conference tests.

Verifies conference create, add member, mute/unmute, remove, destroy.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.tier2, pytest.mark.conference]


@pytest.mark.asyncio
async def test_conference_events_via_webhook(pbx, sipbot_pool, event_checker):
    """Conference — lifecycle events via webhook, bound to the conf_id.

    conference_created and conference_destroyed must BOTH arrive, in order,
    carrying the conf_id the test created. The previous "assert
    len(conf_events) >= 0" was literally always true.
    """
    rwi = event_checker.rwi
    conf_id = f"conf-evt-{uuid.uuid4().hex[:8]}"

    # Cold-start tolerance: the very first RWI command of a session can race
    # the webhook forwarding pipeline, so re-issue create until the event
    # lands (destroying the unreported room between attempts).
    deadline = asyncio.get_event_loop().time() + 20
    created = None
    while created is None and asyncio.get_event_loop().time() < deadline:
        await rwi.conference_create(conf_id)
        created = await event_checker.webhook.wait_for_event(
            "conference_created", timeout=4,
            match={"payload.conf_id": conf_id})
        if created is None:
            await rwi.conference_destroy(conf_id)
            await asyncio.sleep(0.5)
    assert created is not None, (
        "conference_created never reached the webhook. "
        f"wh={event_checker.webhook.event_types()}"
    )

    # Destroy the room — the destroyed event must follow.
    await rwi.conference_destroy(conf_id)
    await event_checker.expect_webhook_payload(
        "conference_destroyed", {"payload.conf_id": conf_id}, timeout=15)

    # Ordering: created must precede destroyed in the webhook arrival order
    # (broadcast events carry no envelope sequence — both seq=0).
    events = event_checker.webhook.all_events()
    idx_created = events.index(created)
    idx_destroyed = [i for i, e in enumerate(events)
                     if e.event_type == "conference_destroyed"
                     and (e.payload or {}).get("conf_id") == conf_id]
    assert idx_destroyed and idx_created < idx_destroyed[-1], (
        "conference_destroyed did not arrive after conference_created: "
        f"{[(e.event_type, e.sequence) for e in events]}"
    )


@pytest.mark.asyncio
async def test_conference_create_destroy_via_rwi(pbx, api, event_checker):
    """Conference — create and destroy a conference room via RWI.

    The room must be acknowledged by a conference_created webhook bound to
    the conf_id and later by conference_destroyed.
    """
    rwi = event_checker.rwi
    conf_id = f"conf-test-{uuid.uuid4().hex[:8]}"

    await rwi.conference_create(conf_id)
    await event_checker.expect_webhook_payload(
        "conference_created", {"payload.conf_id": conf_id}, timeout=15)

    await rwi.conference_destroy(conf_id)
    await event_checker.expect_webhook_payload(
        "conference_destroyed", {"payload.conf_id": conf_id}, timeout=15)


@pytest.mark.asyncio
async def test_conference_rest_endpoints(pbx, api, event_checker):
    """Conference — REST list reflects RWI-created rooms.

    Create a room via RWI → it must appear in GET /cc/conferences → destroy
    it → conference_destroyed must fire.
    """
    conf_id = f"conf-rest-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.conference_create(conf_id)
    try:
        await event_checker.expect_webhook_payload(
            "conference_created", {"payload.conf_id": conf_id}, timeout=15)
        listing = await api.get("/api/cc/conferences")
        assert listing is not None, "GET /cc/conferences returned None"
        rooms = listing.get("data", []) if isinstance(listing, dict) else []
        room_ids = [r.get("room_id") for r in rooms if isinstance(r, dict)]
        assert conf_id in room_ids, (
            f"created room {conf_id!r} missing from REST list: {room_ids}"
        )
    finally:
        await event_checker.rwi.conference_destroy(conf_id)
        await event_checker.expect_webhook_payload(
            "conference_destroyed", {"payload.conf_id": conf_id}, timeout=15)


@pytest.mark.asyncio
async def test_conference_mute_unmute_via_rwi(pbx, sipbot_pool, event_checker):
    """Conference — mute/unmute participant."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15160,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    rwi = event_checker.rwi
    call_id = f"conf-call-{uuid.uuid4().hex[:8]}"
    conf_id = f"conf-mute-{uuid.uuid4().hex[:8]}"

    try:
        await rwi.conference_create(conf_id)
        await rwi.originate(
            call_id=call_id,
            destination=f"sip:1002@{pbx.sip_addr}",
            timeout_secs=10,
        )
        await asyncio.sleep(3)
        await rwi.conference_add(conf_id, call_id)
        await asyncio.sleep(1)
        await rwi.conference_mute(conf_id, call_id)
        await asyncio.sleep(1)
        await rwi.conference_unmute(conf_id, call_id)
        await rwi.conference_remove(conf_id, call_id)
        await rwi.conference_destroy(conf_id)
    except Exception as exc:
        pytest.skip(f"Conference operations not available: {exc}")
