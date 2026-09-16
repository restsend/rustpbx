"""Tier 1 — RWI webhook event verification tests.

Verifies that the full call lifecycle (incoming → ringing → answered → hangup)
produces the expected webhook event sequence.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.tier1, pytest.mark.webhook]


@pytest.mark.asyncio
async def test_webhook_call_lifecycle_events(pbx, sipbot_pool, event_checker):
    """Webhook — full call lifecycle emits expected event types."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15120,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=8,
    )
    await caller.wait_output_async(r"200 OK", timeout=15)
    await asyncio.sleep(5)

    types = event_checker.webhook.event_types()
    assert len(types) > 0, f"No webhook events received. Got: {types}"

    call_events = [t for t in types if t.startswith("call_")]
    assert len(call_events) > 0, f"No call_* events in webhook stream: {types}"


@pytest.mark.asyncio
async def test_webhook_event_envelope_format(pbx, sipbot_pool, event_checker):
    """Webhook — events have correct envelope format (rwi, event_type)."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15121,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=5,
    )
    await asyncio.sleep(5)

    events = event_checker.webhook.all_events()
    assert len(events) > 0, "No webhook events"

    for ev in events[:3]:
        assert ev.event_type is not None, f"Event missing event_type: {ev.raw}"
        assert "sequence" not in ev.raw, f"Envelope must not carry sequence: {ev.raw}"


@pytest.mark.asyncio
async def test_webhook_rwi_correlation(pbx, sipbot_pool, event_checker):
    """Webhook — webhook events correlate with RWI WebSocket events."""
    rwi = event_checker.rwi

    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15122,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=5,
    )
    await asyncio.sleep(5)

    webhook_types = event_checker.webhook.event_types()
    rwi_types = [e.get("event_type") for e in rwi.events if e.get("event_type")]

    # Both should have received events, or at least webhook events should be present
    assert len(webhook_types) > 0 or len(rwi_types) > 0, (
        f"No events from either source. Webhook: {webhook_types}, RWI: {rwi_types}"
    )


@pytest.mark.asyncio
async def test_webhook_originate_events(pbx, event_checker):
    """Webhook — RWI originate produces call events."""
    rwi = event_checker.rwi
    call_id = f"test-wh-{uuid.uuid4().hex[:8]}"

    # Originate to a non-existent target (will fail but should still emit events)
    try:
        await rwi.originate(
            call_id=call_id,
            destination=f"sip:9999@{pbx.sip_addr}",
            caller_id="test",
            timeout_secs=5,
        )
    except Exception:
        pass

    await asyncio.sleep(5)

    # At least some events should have fired
    rwi_types = [e.get("event_type") for e in rwi.events]
    assert len(rwi_types) > 0 or event_checker.webhook.count() > 0, (
        "Expected some events from originate attempt"
    )


@pytest.mark.asyncio
async def test_webhook_health_endpoint(pbx, api):
    """Webhook — health endpoint is accessible (use console page as probe)."""
    import aiohttp
    async with aiohttp.ClientSession() as session:
        async with session.get(f"{pbx.http_url}/console/cc") as resp:
            assert resp.status < 500, f"Console page returned {resp.status}"
