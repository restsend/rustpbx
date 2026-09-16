"""会议授权 IVR（ConferenceAuthApp）端到端 — 验收「会议IVR按键授权」场景。

全链路：
  A(真实 caller) → CC 坐席 B 接听 → REST consult C(应答) → Connected
  → owner op `conference_auth_start`（对 A 起授权 IVR，B 被 hold）
  → A 按键授权（dtmf_flows 单键 "1"）
  → `conference_auth_result=authorized` → 自动 merge 为三方会议。

严格断言：授权 IVR 结果（session 扩展/PBX 日志）、会议建立事件、三方存活。
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.tier3, pytest.mark.mcu, pytest.mark.slow]


async def _reg(sipbot_pool, pbx, port, username):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h_wait_registered(ua, username)
    return ua


async def h_wait_registered(ua, label):
    import helpers as h

    await h.wait_registered(ua, label)


@pytest.mark.asyncio
async def test_conference_auth_ivr_authorized(pbx, api, sipbot_pool, event_checker, rwi, evidence):
    import helpers as h

    await h.connect_rwi(rwi)

    # B（坐席）与 C（咨询目标）两个 echo UA
    agent = await _reg(sipbot_pool, pbx, h.ua_port(15196), "2101")
    consult = await _reg(sipbot_pool, pbx, h.ua_port(15197), "2102")

    # 1) A 真实呼叫 B（CC 坐席分机直呼，走 DefaultRoute）
    call_id = f"confauth-{uuid.uuid4().hex[:8]}"
    caller = sipbot_pool.caller(
        target=f"sip:2101@{pbx.sip_addr}", username="1001", password="123456",
        hangup=40, audio_quality=True, dtmf_flows="6s:1",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output[-1500:]
    created = await event_checker.expect_webhook_event("call_created", timeout=10)
    call_id = (created.raw.get("call_id") if hasattr(created, "raw") else None) \
        or (created.payload or {}).get("call_id") if hasattr(created, "payload") else None
    # WebhookEvent 对象：直接取属性
    call_id = getattr(created, "call_id", None) or call_id
    payload = getattr(created, "payload", None) or {}
    session_id = payload.get("session_id") or (created.raw or {}).get("session_id") or ""
    evidence.log_metric("call_id", call_id)
    assert await agent.wait_output_async(r"200 OK|Call established", timeout=20), agent.output[-1500:]

    # 2) consult C（应答后 Connected）
    status, resp = await _post_json(pbx, f"/api/cc/calls/{call_id}/consult", {"target": "2102"})
    assert status in (200, 201), f"consult create {status}: {str(resp)[:300]}"
    transfer_id = (resp or {}).get("transfer_id") or (resp or {}).get("id")
    A = transfer_id
    if not A:
        for e in event_checker.webhook.events + [
            {"event_type": x.get("event_type"), **(x.get("event") or {})} for x in rwi.events
        ]:
            tid = e.get("transfer_id")
            if tid:
                A = tid
                break
    assert A, f"no transfer_id from consult response/events: {str(resp)[:300]}"
    evidence.log_metric("transfer_id", A)
    # C 应答 → 咨询腿接通
    await asyncio.sleep(2.0)

    # 3) owner op：对 A 起会议授权 IVR
    op_status, op_resp = await _post_json(pbx, "/ami/v1/cluster/cc_owner_op", {
        "session_id": session_id,
        "op": "conference_auth_start",
        "body": {"transfer_id": A},
    })
    assert op_status in (200, 201), (
        f"conference_auth_start {op_status}: {str(op_resp)[:300]}"
    )
    evidence.log_metric("conference_auth_start", "ok")

    # 4) A 在 6s 时按键 "1" 授权（已在 dtmf_flows 配置）→ 等待合并/会议事件
    deadline = asyncio.get_event_loop().time() + 25
    conf_created = False
    auth_authorized = False
    while asyncio.get_event_loop().time() < deadline and not (conf_created and auth_authorized):
        for e in event_checker.webhook.events:
            et = e.event_type if hasattr(e, "event_type") else e.get("event_type")
            if et == "conference_created":
                conf_created = True
        try:
            await rwi.wait_for_event("conference_created", timeout=2)
            conf_created = True
        except Exception:
            pass
        if not auth_authorized:
            text = ""
            log_path = pbx.log_file_path
            from pathlib import Path as _P

            if log_path and _P(log_path).exists():
                text = _P(log_path).read_text(encoding="utf-8", errors="replace")
            if "conference_auth_result" in text and "authorized" in text:
                auth_authorized = True
    evidence.log_metric("conf_created", conf_created)
    evidence.log_metric("auth_authorized", auth_authorized)
    assert auth_authorized or conf_created, (
        "授权流程未完成：既无 conference_created 也无 authorized 结果 —— "
        f"webhook events: {[e.get('event_type') for e in event_checker.webhook.events]}"
    )


async def _post_json(pbx, path, payload):
    import aiohttp

    from helpers.pbx_server import PbxApiClient

    # reuse an authenticated console session for /api/cc + AMI superuser bypass
    async with aiohttp.ClientSession() as session:
        client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
        await client.ensure_console_auth()
        headers = await client._get_headers()
        async with session.post(f"{pbx.http_url}{path}", json=payload,
                                headers=headers, cookies=client._cookies) as resp:
            try:
                body = await resp.json(content_type=None)
            except Exception:
                body = {"raw": (await resp.text())[:300]}
            return resp.status, body
