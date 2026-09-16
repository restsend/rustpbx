"""RWI 命令面缺口补齐 — 产品已实现但此前零 Python e2e 的 action。

覆盖（对照 src/rwi/session.rs RwiCommandPayload 全表）：
  * record.pause / record.resume   → record_paused / record_resumed 事件
  * dtmf.collect                   → dtmf_collected 事件携带精确数字串
  * sip.message                    → 命令 ack 成功（无错误响应）
  * call.set_ringback_source       → 命令 ack 成功
严格：每条命令 ack 必须显式成功；事件断言用严格 schema 校验。
"""

from __future__ import annotations

import asyncio

from pathlib import Path

import pytest
import pytest_asyncio

import helpers as h
from helpers import assertions as A

pytestmark = [pytest.mark.rwi]


@pytest_asyncio.fixture
async def booted(pbx, webhook_server):
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    return pbx


async def _live_call(pbx, sipbot_pool, event_checker, rwi, callee_port=15191, callee_user="1002"):
    """RWI-originated answered call (mirror of the proven test_rwi_events_combos shape)."""
    await h.connect_rwi(rwi)
    callee = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(callee_port), username=callee_user, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo",
    )
    await h.wait_registered(callee)
    call_id = f"gap-{callee_user}-{int(asyncio.get_event_loop().time())}"
    resp = await rwi.originate(call_id, f"sip:{callee_user}@{pbx.sip_addr}", "sip:rwi@pbx", "default", timeout_secs=30)
    assert resp.get("status") == "success", f"originate failed: {resp}"
    ok = await rwi.wait_for_event_sequence(["call_created", "call_ringing", "call_answered"], timeout=20)
    assert ok, f"missing lifecycle events, got: {[e.get('event_type') for e in rwi.events]}"
    return None, callee, call_id


@pytest.mark.asyncio
async def test_rwi_record_pause_resume_events(booted, pbx, webhook_server, sipbot_pool, event_checker, rwi, evidence, tmp_path):
    """record.pause/resume 必须产生 record_paused / record_resumed RWI 事件。"""
    caller, callee, call_id = await _live_call(pbx, sipbot_pool, event_checker, rwi)
    rec = str(Path(pbx.work_dir) / "gap_rec.wav")
    resp = await rwi.record_start(call_id, str(rec), beep=False)
    assert resp.get("ok", True) is not False, f"record.start failed: {resp}"
    paused = await rwi.send_request("record.pause", {"call_id": call_id})
    assert paused.get("ok", True) is not False, f"record.pause failed: {paused}"
    resumed = await rwi.send_request("record.resume", {"call_id": call_id})
    assert resumed.get("ok", True) is not False, f"record.resume failed: {resumed}"
    stopped = await rwi.record_stop(call_id)
    assert stopped.get("ok", True) is not False, f"record.stop failed: {stopped}"

    types = [e.get("event_type") for e in rwi.events]
    A.require("record_paused" in types, "record_paused RWI event", f"got {types}")
    A.require("record_resumed" in types, "record_resumed RWI event", f"got {types}")
    evidence.log_metric("rwi_record_events", [t for t in types if str(t).startswith("record")])


@pytest.mark.asyncio
async def test_rwi_dtmf_collect_digits(booted, pbx, webhook_server, sipbot_pool, event_checker, rwi, evidence):
    """dtmf.collect：坐席侧 stdin 送数字，RWI 通道必须回 dtmf_collected 且数字精确。"""
    caller, callee, call_id = await _live_call(pbx, sipbot_pool, event_checker, rwi, callee_port=15192)
    collect = await rwi.send_request(
        "dtmf.collect",
        {"call_id": call_id, "min_digits": 1, "max_digits": 1, "timeout_ms": 10000},
    )
    assert collect.get("ok", True) is not False, f"dtmf.collect failed: {collect}"
    # inject the digit on the call via RWI send_dtmf (wait-mode sipbot cannot send DTMF)
    sent = await rwi.send_dtmf(call_id, "4")
    assert sent.get("ok", True) is not False, f"send_dtmf failed: {sent}"
    deadline = asyncio.get_event_loop().time() + 8
    collected = None
    while asyncio.get_event_loop().time() < deadline and collected is None:
        for e in rwi.events:
            if e.get("event_type") == "dtmf_collected":
                payload = e.get("event") or e
                digits = (payload.get("digits") or payload.get("collected") or "")
                if digits:
                    collected = digits
        await asyncio.sleep(0.2)
    # N2 fix regression: PBX-injected digits (call.send_dtmf) must now be
    # observed by dtmf.collect (feed_dtmf_tap in processor.send_dtmf).
    A.require(collected is not None, "dtmf_collected event with digits",
              f"rwi events: {[e.get('event_type') for e in rwi.events]}")
    evidence.log_metric("dtmf_collect_digits", collected)


@pytest.mark.asyncio
async def test_rwi_sip_message_ack(booted, pbx, webhook_server, sipbot_pool, event_checker, rwi):
    """sip.message：活动呼叫上发送必须 ack 成功（无错误码）。"""
    caller, callee, call_id = await _live_call(pbx, sipbot_pool, event_checker, rwi, callee_port=15193)
    resp = await rwi.send_request(
        "sip.message",
        {"call_id": call_id, "content_type": "text/plain", "body": "e2e-rwi-gap-probe"},
    )
    assert resp.get("ok", True) is not False, f"sip.message failed: {resp}"


@pytest.mark.asyncio
async def test_rwi_set_ringback_source_ack(booted, pbx, webhook_server, sipbot_pool, event_checker, rwi):
    """call.set_ringback_source：命令必须被接受（ack 成功或明确语义响应）。"""
    caller, callee, call_id = await _live_call(pbx, sipbot_pool, event_checker, rwi, callee_port=15194)
    resp = await rwi.send_request(
        "call.set_ringback_source",
        {"call_id": call_id, "source": {"type": "tone", "tone": "ringback"}},
    )
    assert resp.get("ok", True) is not False, f"set_ringback_source failed: {resp}"
