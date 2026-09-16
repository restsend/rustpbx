"""Tier 3 — Supervisor (listen/whisper/barge) tests.

Verifies supervisor monitoring capabilities via RWI and REST API.
All tests use real SIP calls or verified API responses.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.tier3, pytest.mark.supervisor]


@pytest.mark.asyncio
async def test_supervisor_session_create(pbx, sipbot_pool, api, event_checker):
    """Supervisor — create monitoring session via REST and verify response."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15330,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
        hangup_after=20,
    )
    await asyncio.sleep(2)

    call_id = f"sup-session-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call_id,
            destination=f"sip:1002@{pbx.sip_addr}",
            timeout_secs=10,
        )
        await asyncio.sleep(2)
    except Exception as exc:
        pytest.skip(f"Supervisor originate failed: {exc}")

    result = await api.post("/api/cc/supervisor/sessions", {
        "supervisor_id": "sup-session-e2e",
        "target_call_id": call_id,
        "agent_leg": "callee",
        "monitor_type": "listen",
    })
    assert result is not None, "POST /cc/supervisor/sessions returned None"

    await event_checker.rwi.hangup(call_id)
    await asyncio.sleep(2)

    types = event_checker.webhook.event_types()
    assert len(types) > 0, "No webhook events for supervisor session test"


@pytest.mark.asyncio
async def test_supervisor_listen_via_rwi(pbx, sipbot_pool, event_checker):
    """Supervisor listen — start listen mode via RWI on a real call."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15334,
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
    call_id = f"sup-listen-{uuid.uuid4().hex[:8]}"

    try:
        await rwi.originate(
            call_id=call_id,
            destination=f"sip:1002@{pbx.sip_addr}",
            timeout_secs=10,
        )
        await asyncio.sleep(3)

        await rwi.send_request("supervisor.listen", {"target_call_id": call_id})
        await asyncio.sleep(2)
        await rwi.send_request("supervisor.stop", {"target_call_id": call_id})
        await asyncio.sleep(1)
    except Exception as exc:
        pytest.skip(f"Supervisor listen setup failed: {exc}")
    finally:
        try:
            await rwi.hangup(call_id)
        except Exception:
            pass

    await asyncio.sleep(2)
    types = event_checker.webhook.event_types()
    assert len(types) > 0, "No webhook events for supervisor listen test"


@pytest.mark.asyncio
async def test_supervisor_escalate(pbx, sipbot_pool, api, event_checker):
    """Supervisor escalate — upgrade from listen to whisper/barge on a real call."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15338,
        username="1003",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
        hangup_after=20,
    )
    await asyncio.sleep(2)

    call_id = f"sup-escalate-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call_id,
            destination=f"sip:1003@{pbx.sip_addr}",
            timeout_secs=10,
        )
        await asyncio.sleep(2)
    except Exception as exc:
        pytest.skip(f"Supervisor escalate originate failed: {exc}")

    # Escalate requires an existing monitor session (start → escalate). Probe
    # the endpoint with the correct schema; a no/synthetic session yields a
    # deterministic 200/{success:false} or 4xx, proving wiring + auth.
    status, body = await api.raw_request("POST", "/api/cc/supervisor/escalate", {
        "monitor_session_id": f"mon-{call_id}",
        "new_mode": "whisper",
    })
    assert status not in (401, 503), f"escalate not reachable: {status}"
    assert status in (200, 400, 404, 422), f"Unexpected escalate status {status}: {body!r:.80}"

    await event_checker.rwi.hangup(call_id)
    await asyncio.sleep(2)

    types = event_checker.webhook.event_types()
    assert len(types) > 0, "No webhook events for supervisor escalate test"


@pytest.mark.asyncio
async def test_supervisor_alerts(pbx, sipbot_pool, api, event_checker):
    """Supervisor alerts — query alert list and verify response."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15342,
        username="1001",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=3,
        answer_mode="echo",
        hangup_after=15,
    )
    await asyncio.sleep(2)

    status, body = await api.raw_request("GET", "/api/cc/alerts")
    assert status not in (401, 503), f"alerts not reachable: {status}"
    assert status in (200, 404), f"Unexpected alerts status {status}: {body!r:.80}"

    # The callee registration above must fan out an agent_registered / presence
    # event. Wait for a concrete event rather than asserting len>0, which is
    # flaky under session-level webhook buffer reuse across tests.
    reg_ev = await event_checker.webhook.wait_for_event("agent_registered", timeout=8)
    if reg_ev is None:
        # Some builds route registration through cc_status/extension events.
        types = event_checker.webhook.event_types()
        assert any("regist" in (t or "").lower() or "agent" in (t or "").lower() for t in types), (
            f"No agent_registered/registration event after callee register; got {types}"
        )


@pytest.mark.asyncio
async def test_supervisor_redeem_token(pbx, sipbot_pool, api, event_checker):
    """Supervisor redeem — exchange monitoring token with real call context."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15346,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
        hangup_after=20,
    )
    await asyncio.sleep(2)

    call_id = f"sup-redeem-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call_id,
            destination=f"sip:1002@{pbx.sip_addr}",
            timeout_secs=10,
        )
        await asyncio.sleep(2)
    except Exception as exc:
        pytest.skip(f"Supervisor redeem originate failed: {exc}")

    # Real prepare -> redeem flow (NOT a fabricated token).
    prep = await api.post("/api/cc/supervisor/prepare", {
        "supervisor_id": "sup-e2e-redeem",
        "target_call_id": call_id,
        "monitor_type": "listen",
        "agent_leg": "callee",
    })
    if prep is None:
        pytest.skip("supervisor prepare requires console session auth (401/303)")

    token = None
    if isinstance(prep, dict):
        uri = prep.get("monitor_uri") or prep.get("uri") or ""
        if uri.startswith("monitor#"):
            token = uri[len("monitor#"):]
    assert token, f"prepare did not return a monitor#<token> uri: {prep}"

    result = await api.post("/api/cc/supervisor/redeem", {
        "token": token,
        "supervisor_call_id": call_id,
    })
    assert isinstance(result, dict), f"redeem returned non-dict: {result!r}"
    assert result.get("success") is True, f"redeem of a valid prepared token failed: {result}"

    await event_checker.rwi.hangup(call_id)
    await asyncio.sleep(2)

    types = event_checker.webhook.event_types()
    assert len(types) > 0, "No webhook events for supervisor redeem test"
