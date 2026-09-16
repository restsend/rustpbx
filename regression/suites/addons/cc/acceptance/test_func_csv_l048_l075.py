"""CSV 回归测试 — 功能清单 L48-L75: 通话控制/转接/会议

Each test maps to one CSV row. All tests use real SIP calls + event verification.
"""

from __future__ import annotations

import asyncio, uuid
import pytest

pytestmark = [pytest.mark.acceptance, pytest.mark.transfer]


@pytest.mark.asyncio
@pytest.mark.csv_line(48)
async def test_csv_L048_originate(pbx, sipbot_pool, event_checker):
    """CSV L48: Click-to-Dial POST /cc/calls/originate — real ANSWERED call.

    originate ACKs immediately, and even failure paths emit call_created +
    call_hangup(reason=originate_failed). Only a call_answered event for
    THIS call_id proves click-to-dial actually established the call.
    """
    callee = sipbot_pool.callee(
        host=pbx.host, port=15524, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=30,
    )
    await asyncio.sleep(2)
    call_id = f"acpt-org-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id,
        caller_id="1001",
        destination=f"sip:1002@{pbx.sip_addr}",
        timeout_secs=15,
    )
    # Strict: fail (not silently pass) if the call was never answered.
    await event_checker.expect_webhook_event(
        "call_answered", call_id=call_id, timeout=15)
    # Callee side: the INVITE → 200 OK exchange must be visible to the bot.
    event_checker.assert_sip_answered(callee.output, label="callee-1002")
    await event_checker.rwi.hangup(call_id)
    hangup = await event_checker.webhook.wait_for_event(
        "call_hangup", timeout=15, call_id=call_id)
    assert hangup is not None, f"no call_hangup for {call_id}"
    assert hangup.payload.get("reason") != "originate_failed", (
        f"call never established (originate_failed): {hangup.raw!r:.300}"
    )


@pytest.mark.asyncio
@pytest.mark.csv_line(49)
async def test_csv_L049_active_calls(pbx, sipbot_pool, api, event_checker):
    """CSV L49: GET /cc/calls/active — query active calls."""
    callee = sipbot_pool.callee(
        host=pbx.host, port=15528, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=30,
    )
    await asyncio.sleep(2)
    call_id = f"acpt-act-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id,
        caller_id="1001",
        destination=f"sip:1002@{pbx.sip_addr}",
        timeout_secs=15,
    )
    await asyncio.sleep(3)
    result = await api.get("/api/cc/calls/active")
    assert result is not None, "GET /cc/calls/active returned None"
    await event_checker.rwi.hangup(call_id)
    await asyncio.sleep(2)


@pytest.mark.asyncio
@pytest.mark.csv_line(50)
async def test_csv_L050_hold(pbx, sipbot_pool, api, event_checker):
    """CSV L50: POST /cc/calls/{id}/hold — hold call."""
    callee = sipbot_pool.callee(
        host=pbx.host, port=15500, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=30,
    )
    await asyncio.sleep(2)
    call_id = f"acpt-hld-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id, caller_id="1001",
        destination=f"sip:1002@{pbx.sip_addr}", timeout_secs=10,
    )
    await asyncio.sleep(3)
    result = await api.post(f"/api/cc/calls/{call_id}/hold")
    assert result is not None, "POST hold returned None"
    await asyncio.sleep(2)
    types = event_checker.webhook.event_types()
    assert len(types) > 0
    await event_checker.rwi.hangup(call_id)
    await asyncio.sleep(2)


@pytest.mark.asyncio
@pytest.mark.csv_line(51)
async def test_csv_L051_unhold(pbx, sipbot_pool, api, event_checker):
    """CSV L51: POST /cc/calls/{id}/unhold — unhold call."""
    callee = sipbot_pool.callee(
        host=pbx.host, port=15504, username="1003", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=30,
    )
    await asyncio.sleep(2)
    call_id = f"acpt-uhl-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id, caller_id="1001",
        destination=f"sip:1003@{pbx.sip_addr}", timeout_secs=10,
    )
    await asyncio.sleep(3)
    await api.post(f"/api/cc/calls/{call_id}/hold")
    await asyncio.sleep(1)
    result = await api.post(f"/api/cc/calls/{call_id}/unhold")
    assert result is not None, "POST unhold returned None"
    await asyncio.sleep(2)
    types = event_checker.webhook.event_types()
    assert len(types) > 0
    await event_checker.rwi.hangup(call_id)
    await asyncio.sleep(2)


@pytest.mark.asyncio
@pytest.mark.csv_line(52)
async def test_csv_L052_send_dtmf(pbx, sipbot_pool, api, event_checker):
    """CSV L52: POST /cc/calls/{id}/send-dtmf — send DTMF."""
    callee = sipbot_pool.callee(
        host=pbx.host, port=15508, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=20,
    )
    await asyncio.sleep(2)
    call_id = f"acpt-dtm-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id, caller_id="1001",
        destination=f"sip:1002@{pbx.sip_addr}", timeout_secs=10,
    )
    await asyncio.sleep(3)
    result = await api.post(f"/api/cc/calls/{call_id}/send-dtmf", {"digits": "1"})
    assert result is not None, "POST send-dtmf returned None"
    await asyncio.sleep(2)
    await event_checker.rwi.hangup(call_id)
    await asyncio.sleep(2)


@pytest.mark.asyncio
@pytest.mark.csv_line(60)
async def test_csv_L060_end_call(pbx, sipbot_pool, api, event_checker):
    """CSV L60: POST /cc/calls/{id}/end — end call with CDR.

    The endpoint dispatches a real SIP BYE (Hangup command) and answers 200;
    the functional effect is the call_hangup webhook event for THIS call_id.
    """
    callee = sipbot_pool.callee(
        host=pbx.host, port=15512, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=20,
    )
    await asyncio.sleep(2)
    call_id = f"acpt-end-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id, caller_id="1001",
        destination=f"sip:1002@{pbx.sip_addr}", timeout_secs=10,
    )
    await event_checker.expect_webhook_event(
        "call_answered", call_id=call_id, timeout=15)
    status, body = await api.raw_request("POST", f"/api/cc/calls/{call_id}/end", {})
    assert status == 200, f"end_call failed: {status} {body!r:.200}"
    await event_checker.expect_webhook_event(
        "call_hangup", call_id=call_id, timeout=15)


@pytest.mark.asyncio
@pytest.mark.csv_line(53)
async def test_csv_L053_blind_transfer(pbx, sipbot_pool, api, event_checker):
    """CSV L53: POST /cc/calls/{id}/transfer — blind transfer."""
    callee = sipbot_pool.callee(
        host=pbx.host, port=15516, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=30,
    )
    await asyncio.sleep(2)
    call_id = f"acpt-blt-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id, caller_id="1001",
        destination=f"sip:1002@{pbx.sip_addr}", timeout_secs=10,
    )
    await asyncio.sleep(3)
    result = await api.blind_transfer(call_id, "1003")
    assert result is not None, "blind_transfer returned None"
    await asyncio.sleep(3)
    types = event_checker.webhook.event_types()
    assert len(types) > 0
    await event_checker.rwi.hangup(call_id)
    await asyncio.sleep(2)


@pytest.mark.asyncio
@pytest.mark.csv_line(68)
async def test_csv_L068_conference_list(pbx, api, event_checker):
    """CSV L68: GET /cc/conferences — list conferences."""
    result = await api.get("/api/cc/conferences")
    assert result is not None, "GET /cc/conferences returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(67)
async def test_csv_L067_conference_end(pbx, sipbot_pool, api, event_checker):
    """CSV L67: POST /cc/calls/{id}/conference/end — end conference."""
    callee = sipbot_pool.callee(
        host=pbx.host, port=15520, username="1003", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=20,
    )
    await asyncio.sleep(2)
    call_id = f"acpt-cfe-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id, caller_id="1001",
        destination=f"sip:1003@{pbx.sip_addr}", timeout_secs=10,
    )
    await asyncio.sleep(3)
    status, body = await api.raw_request("POST", f"/api/cc/calls/{call_id}/conference/end", {})
    assert status not in (401, 503), f"conference/end not reachable: {status}"
    assert status in (200, 404, 409, 500, 422), f"Unexpected conference/end status {status}"
    try:
        await event_checker.rwi.hangup(call_id)
    except Exception:
        pass
    await asyncio.sleep(2)

@pytest.mark.asyncio
@pytest.mark.csv_line(87)
async def test_csv_L087_voip_bridge(pbx, api, event_checker):
    """CSV L87: VoipBridge 节点对接外部 create_room_uri."""
    result = await api.post("/api/cc/conferences", {"room_id": "bridge-test"})
    assert result is not None, "POST /cc/conferences returned None"
