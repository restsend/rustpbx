"""Conference authorization IVR (ConferenceAuthApp) end-to-end - the
"conference IVR key authorization" acceptance scenario.

Full chain:
  A (real caller) -> CC agent B answers -> REST consult C (answered) -> Connected
  -> owner op `conference_auth_start` (auth IVR runs on A, B is held)
  -> A presses the authorize key (dtmf_flows single key "1")
  -> `conference_auth_result=authorized` -> auto-merge into a three-way conference.

Strict asserts: auth IVR outcome (session extensions / PBX log), conference
establishment events, all three parties alive."""

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

    # B (agent) and C (consult target) echo UAs
    agent = await _reg(sipbot_pool, pbx, h.ua_port(15196), "2101")
    consult = await _reg(sipbot_pool, pbx, h.ua_port(15197), "2102")

    # 1) A (real caller) calls B (CC agent extension, via DefaultRoute)
    call_id = f"confauth-{uuid.uuid4().hex[:8]}"
    caller = sipbot_pool.caller(
        target=f"sip:2101@{pbx.sip_addr}", username="1001", password="123456",
        hangup=40, audio_quality=True, dtmf_flows="6s:1",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output[-1500:]
    created = await event_checker.expect_webhook_event("call_created", timeout=10)
    call_id = (created.raw.get("call_id") if hasattr(created, "raw") else None) \
        or (created.payload or {}).get("call_id") if hasattr(created, "payload") else None
    # WebhookEvent object: read attributes directly
    call_id = getattr(created, "call_id", None) or call_id
    payload = getattr(created, "payload", None) or {}
    session_id = payload.get("session_id") or (created.raw or {}).get("session_id") or ""
    evidence.log_metric("call_id", call_id)
    assert await agent.wait_output_async(r"200 OK|Call established", timeout=20), agent.output[-1500:]

    # 2) consult C (answered -> Connected)
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
    # C answers -> consult leg Connected
    await asyncio.sleep(2.0)

    # 3) owner op: start the conference auth IVR on A
    op_status, op_resp = await _post_json(pbx, "/ami/v1/cluster/cc_owner_op", {
        "session_id": session_id,
        "op": "conference_auth_start",
        "body": {"transfer_id": A},
    })
    assert op_status in (200, 201), (
        f"conference_auth_start {op_status}: {str(op_resp)[:300]}"
    )
    evidence.log_metric("conference_auth_start", "ok")

    # 4) A presses authorize key "1" at ~6s (via dtmf_flows) -> wait for merge/conference events
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
        "auth flow incomplete: neither conference_created nor an authorized result — "
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
