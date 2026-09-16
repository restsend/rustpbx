"""CSV 验收测试 — RustPBX 功能清单 L76-L122: IVR

Each test maps to one CSV row. All tests drive the IVR via a real inbound
SIP INVITE from an authenticated sipbot UA: plain RWI originate to
app-route targets (``sip:ivr-test@``) is 407-challenged because the RWI
UAC is not an authenticated endpoint, so originate cannot reach these
routes at all. The server assigns the call_id; tests bind it from the
matching ``call_created`` webhook event (callee == destination URI).

Every test asserts functional effects — specific webhook events with
payload fields (call_answered, ivr_node_entered/exited,
skill_group_call_queued) — not merely "some events arrived".

CSV rows covered:
  L76: IVR 根菜单 root
  L77: IVR 命名子菜单 menus / 菜单按键动作 (transfer)
  L84: 菜单 max_retries_action
  L86: DtmfMenu 按键菜单节点
  L92: Queue 节点转人工队列
  L94: Voicemail 节点转语音留言
  L105: PlayAndHangup 节点
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

from helpers.acd_strategy_e2e import drain_agents, wait_agent_idle

pytestmark = [pytest.mark.acceptance, pytest.mark.ivr]


async def _inbound_to_ivr(
    pbx, sipbot_pool, event_checker, *, dest_user="ivr-test",
    caller_id="1001", hangup=60, timeout=25,
):
    """Authenticated inbound sipbot call to an app-route target.

    Returns (call_id, caller_bot). Fails (never skips) if the INVITE never
    produced a matching call_created webhook event.
    """
    destination = f"sip:{dest_user}@{pbx.sip_addr}"
    mark = len(event_checker.webhook.all_events())
    caller = sipbot_pool.caller(
        target=destination, username=caller_id, password="123456",
        hangup=hangup,
    )
    loop = asyncio.get_event_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        for ev in event_checker.webhook.all_events()[mark:]:
            if ev.event_type != "call_created":
                continue
            payload = ev.payload if isinstance(ev.payload, dict) else {}
            if payload.get("callee") != destination:
                continue
            if caller_id not in (payload.get("caller") or ""):
                continue
            assert ev.call_id, "call_created event without call_id"
            return ev.call_id, caller
        await asyncio.sleep(0.15)
    pytest.fail(
        f"inbound call to {destination} never produced a matching "
        f"call_created within {timeout}s"
    )


@pytest.mark.asyncio
@pytest.mark.csv_line(76)
async def test_csv_L076_ivr_root_menu(pbx, sipbot_pool, event_checker):
    """CSV L76: IVR 根菜单 root — inbound call is answered by the IVR and
    enters the root menu node."""
    call_id, caller = await _inbound_to_ivr(
        pbx, sipbot_pool, event_checker, hangup=15)

    # IVR answered the call: app-routed sessions do NOT emit call_answered
    # (session.rs only emits it when no app is running), so the answered
    # evidence is the root menu starting + the greeting actually playing.
    await event_checker.expect_webhook_payload(
        "ivr_node_entered", {"payload.node_id": "root"},
        call_id=call_id, timeout=20)
    await event_checker.expect_webhook_event(
        "media_play_finished", call_id=call_id, timeout=20)
    event_checker.assert_sip_answered(caller.output, label="ivr-caller")

    # Caller bot auto-hangs up (hangup=15) → the session must terminate the
    # IVR cleanly (call_hangup + ivr_node_exited for root).
    await event_checker.expect_webhook_event(
        "call_hangup", call_id=call_id, timeout=30)
    exited = event_checker.webhook.find("ivr_node_exited")
    assert exited is not None and exited.call_id == call_id, (
        f"ivr_node_exited missing for root menu call {call_id}. "
        f"wh={event_checker.webhook_event_types_for_call(call_id)}"
    )


@pytest.mark.asyncio
@pytest.mark.csv_line(77)
async def test_csv_L077_ivr_menu_transfer_action(pbx, sipbot_pool, event_checker):
    """CSV L77: 菜单按键动作 — DTMF "1" exits root with result_value "1"
    and the mapped transfer action dials the target (1002)."""
    # The transfer target from the ivr-test fixture is 1002; register it so
    # the transfer leg has somewhere to go.
    callee = sipbot_pool.callee(
        host=pbx.host, port=15526, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=25,
    )
    await asyncio.sleep(2)

    call_id, _caller = await _inbound_to_ivr(
        pbx, sipbot_pool, event_checker, hangup=40)
    await event_checker.expect_webhook_payload(
        "ivr_node_entered", {"payload.node_id": "root"},
        call_id=call_id, timeout=20)
    await asyncio.sleep(2)

    await event_checker.rwi.send_dtmf(call_id, "1", 200)

    # Leaving root must be reported with the triggering digit.
    await event_checker.expect_webhook_payload(
        "ivr_node_exited",
        {"payload.node_id": "root", "payload.result_value": "1"},
        call_id=call_id, timeout=20)
    # The transfer action must dial 1002: the bot answers the INVITE.
    ok = await callee.wait_output_async(r"200 OK|Call established", timeout=20)
    assert ok, (
        f"transfer target 1002 never received the INVITE. Output:\n"
        f"{callee.output[-400:]}"
    )


@pytest.mark.asyncio
@pytest.mark.csv_line(86)
async def test_csv_L086_ivr_dtmf_menu(pbx, sipbot_pool, event_checker):
    """CSV L86: DtmfMenu 按键菜单节点 — the pressed digit is delivered to the
    running IVR app and reported as the node exit result.

    Guards the call.send_dtmf injection fix: digits sent via RWI must reach
    the IVR app of an already-established (inbound) call.
    """
    call_id, _caller = await _inbound_to_ivr(
        pbx, sipbot_pool, event_checker, hangup=30)
    await event_checker.expect_webhook_payload(
        "ivr_node_entered", {"payload.node_id": "root"},
        call_id=call_id, timeout=20)
    await asyncio.sleep(2)

    await event_checker.rwi.send_dtmf(call_id, "1", 200)

    await event_checker.expect_webhook_payload(
        "ivr_node_exited",
        {"payload.node_id": "root", "payload.result_value": "1"},
        call_id=call_id, timeout=20)


@pytest.mark.asyncio
@pytest.mark.csv_line(84)
async def test_csv_L084_ivr_max_retries(pbx, sipbot_pool, event_checker):
    """CSV L84: max_retries_action — repeated invalid keys eventually execute
    the configured max_retries_action (hangup) WITHOUT any external hangup.

    The ivr-retry fixture uses max_retries=1 with no greeting/invalid audio,
    so invalid-key cycles are fast: the 2nd invalid key exceeds the budget
    and must hang up. The caller bot's own hangup timer (90s) is far outside
    the 40s assertion window, so the hangup can only come from the IVR.
    """
    call_id, _caller = await _inbound_to_ivr(
        pbx, sipbot_pool, event_checker, dest_user="ivr-retry", hangup=90)
    await event_checker.expect_webhook_payload(
        "ivr_node_entered", {"payload.node_id": "root"},
        call_id=call_id, timeout=20)
    await asyncio.sleep(1)

    for _ in range(3):
        await event_checker.rwi.send_dtmf(call_id, "9", 200)
        await asyncio.sleep(1.5)

    await event_checker.expect_webhook_event(
        "call_hangup", call_id=call_id, timeout=40)
    exited = [
        e for e in event_checker.webhook.events_for_call(call_id)
        if e.event_type == "ivr_node_exited"]
    assert exited, (
        f"ivr_node_exited missing after max_retries hangup for {call_id}. "
        f"wh={event_checker.webhook_event_types_for_call(call_id)}"
    )


@pytest.mark.asyncio
@pytest.mark.csv_line(92)
async def test_csv_L092_ivr_queue_node(pbx, sipbot_pool, api, event_checker):
    """CSV L92: Queue 节点转人工队列 — DTMF "2" routes the call into the
    "support" skill group: queued → agent (1002) ringing → answered."""
    # Register agent 1002 (skills=["support"]) and drive it to a stable
    # Idle state (drain leftover wrapup/busy from earlier tests — a naive
    # status POST gets a 400 on invalid transitions).
    sipbot_pool.callee(
        host=pbx.host, port=15528, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=60,
    )
    await asyncio.sleep(3)
    await drain_agents(api, ("1002",))
    await wait_agent_idle(api, "1002")

    call_id, _caller = await _inbound_to_ivr(
        pbx, sipbot_pool, event_checker, hangup=40)
    await event_checker.expect_webhook_payload(
        "ivr_node_entered", {"payload.node_id": "root"},
        call_id=call_id, timeout=20)
    await asyncio.sleep(2)

    await event_checker.rwi.send_dtmf(call_id, "2", 200)

    # The call enters the support queue…
    await event_checker.expect_webhook_payload(
        "queue_joined", {"payload.queue_id": "support"},
        call_id=call_id, timeout=25)
    # …an idle agent is available, so dispatch is direct (no
    # skill_group_call_queued — that only fires while actually waiting):
    # assigned to the skill-qualified agent…
    await event_checker.expect_webhook_payload(
        "skill_group_agent_assigned", {"payload.agent_id": "1002"},
        call_id=call_id, timeout=40)
    # …ringing…
    await event_checker.expect_webhook_payload(
        "call_ringing", {"payload.agent_id": "1002"},
        call_id=call_id, timeout=25)
    # …and answered.
    await event_checker.expect_webhook_event(
        "call_answered", call_id=call_id, timeout=25)


@pytest.mark.asyncio
@pytest.mark.csv_line(94)
async def test_csv_L094_ivr_voicemail_node(pbx, sipbot_pool, event_checker):
    """CSV L94: Voicemail 节点转语音留言 — DTMF "9" chains the call into the
    voicemail app: the call must stay up (recording), not be hung up."""
    call_id, caller = await _inbound_to_ivr(
        pbx, sipbot_pool, event_checker, dest_user="ivr-vm", hangup=20)
    await event_checker.expect_webhook_payload(
        "ivr_node_entered", {"payload.node_id": "root"},
        call_id=call_id, timeout=20)
    await asyncio.sleep(2)

    await event_checker.rwi.send_dtmf(call_id, "9", 200)

    await event_checker.expect_webhook_payload(
        "ivr_node_exited",
        {"payload.node_id": "root", "payload.result_value": "9"},
        call_id=call_id, timeout=20)
    # The flow completes as a transfer into the voicemail app (mailbox 1002).
    await event_checker.expect_webhook_payload(
        "ivr_flow_completed",
        {"payload.final_result": "transferred",
         "payload.final_routing_target": "1002"},
        call_id=call_id, timeout=20)
    # The voicemail app takes the media: no immediate server hangup.
    await event_checker.expect_no_webhook_event("call_hangup", wait=5)
    # Caller bot auto-hangs up (hangup=20) → voicemail session terminates.
    await event_checker.expect_webhook_event(
        "call_hangup", call_id=call_id, timeout=40)


@pytest.mark.asyncio
@pytest.mark.csv_line(105)
async def test_csv_L105_ivr_play_and_hangup(pbx, sipbot_pool, event_checker):
    """CSV L105: PlayAndHangup 节点 — on DTMF timeout the ivr-pah menu plays
    its prompt and hangs up FROM THE SERVER side.

    The caller bot's own hangup timer (90s) is far outside the 40s assertion
    window, so an earlier call_hangup can only come from the IVR action.
    """
    call_id, _caller = await _inbound_to_ivr(
        pbx, sipbot_pool, event_checker, dest_user="ivr-pah", hangup=90)
    await event_checker.expect_webhook_payload(
        "ivr_node_entered", {"payload.node_id": "root"},
        call_id=call_id, timeout=20)

    await event_checker.expect_webhook_event(
        "call_hangup", call_id=call_id, timeout=40)
    exited = [
        e for e in event_checker.webhook.events_for_call(call_id)
        if e.event_type == "ivr_node_exited"]
    assert exited, (
        f"ivr_node_exited missing after PlayAndHangup for {call_id}. "
        f"wh={event_checker.webhook_event_types_for_call(call_id)}"
    )
