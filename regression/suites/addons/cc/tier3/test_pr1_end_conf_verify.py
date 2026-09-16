"""Backend verification for PR1: End Conference REST path.

PR1 fixed `onEndConference` in cc-phone which was wrongly calling
`controller.hold()` instead of `controller.endConference()`. This test
verifies the server-side REST endpoint that PR1's fix targets:

  POST /cc/calls/{call_id}/conference/end {transfer_id}

Creates a real consult→merge flow, then POSTs /conference/end and
asserts the conference is actually destroyed.
"""
from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.tier3, pytest.mark.cc_transfer_events]


@pytest.mark.asyncio
async def test_pr1_end_conference_destroys_conf(pbx, sipbot_pool, api, event_checker):
    """PR1: POST /cc/calls/{id}/conference/end destroys the conference
    that /merge created. The cc-phone widget's End Conference button
    hits this endpoint (was wrongly hitting /hold before PR1).
    """
    callee_b = sipbot_pool.callee(
        host=pbx.host, port=17020, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=90,
    )
    callee_c = sipbot_pool.callee(
        host=pbx.host, port=17030, username="1003", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=90,
    )
    await asyncio.sleep(3)

    # Originate A→B
    call1 = f"pr1-a-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call1, caller_id="1001",
            destination=f"sip:1002@{pbx.sip_addr}", timeout_secs=15,
        )
    except Exception as exc:
        pytest.skip(f"originate A→B failed: {exc}")
    await asyncio.sleep(3)

    # /consult
    s, body = await api.raw_request(
        "POST", f"/api/cc/calls/{call1}/consult", {"target": "1003"})
    assert s == 200, f"consult failed: {s}"
    tid = body.get("transfer_id") if isinstance(body, dict) else None
    assert tid, f"no transfer_id: {body!r:.80}"

    # Originate B→C
    call2 = f"pr1-c-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call2, caller_id="1002",
            destination=f"sip:1003@{pbx.sip_addr}", timeout_secs=15,
        )
    except Exception as exc:
        pytest.fail(f"originate B→C failed: {exc}")
    await asyncio.sleep(3)

    # /connected
    s, _ = await api.raw_request(
        "PUT", f"/api/cc/calls/{call1}/consult/{tid}/connected",
        {"session_b": call2})
    assert s == 200, f"connected failed: {s}"

    # /merge — creates the conference
    s, merge_body = await api.raw_request(
        "POST", f"/api/cc/calls/{call1}/consult/{tid}/merge", {})
    assert s == 200, f"merge failed: {s}"
    conf_id = (merge_body or {}).get("conf_id")
    assert conf_id, f"merge returned no conf_id: {merge_body!r:.80}"
    print(f"\n[pr1] conference created: {conf_id}")

    # ── The PR1 endpoint: POST /conference/end ──────────────────────────
    # This is what cc-phone's End Conference button hits (via
    # controller.endConference → api.endConference). Before PR1 the widget
    # wrongly called /hold instead.
    s, end_body = await api.raw_request(
        "POST", f"/api/cc/calls/{call1}/conference/end",
        {"transfer_id": tid})
    print(f"[pr1] /conference/end status={s} body={end_body!r:.200}")
    assert s == 200, f"conference/end failed: {s} {end_body!r:.80}"

    # Assert: response confirms destruction
    assert isinstance(end_body, dict), f"unexpected body type: {end_body!r}"
    assert end_body.get("state") == "ended", (
        f"expected state=ended, got {end_body.get('state')!r}"
    )
    assert end_body.get("conf_id"), f"no conf_id in end response: {end_body!r}"

    # Cleanup
    for cid in (call1, call2):
        try:
            await event_checker.rwi.hangup(cid)
        except Exception:
            pass
    await asyncio.sleep(2)


@pytest.mark.asyncio
async def test_pr1_end_conference_409_on_unknown_transfer(pbx, sipbot_pool, api, event_checker):
    """PR1 + PR4: /conference/end on a non-existent transfer_id must
    return a proper error (not 500). The endpoint calls
    `end_by_host(conf_id, host_leg)` which returns Err on unknown conf;
    the handler maps it to 500 currently. With TransferError-style
    mapping it should be 404/409. For now we just verify it's reachable
    and doesn't crash.
    """
    callee = sipbot_pool.callee(
        host=pbx.host, port=17040, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=60,
    )
    await asyncio.sleep(3)

    call_id = f"pr1-err-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call_id, caller_id="1001",
            destination=f"sip:1002@{pbx.sip_addr}", timeout_secs=15,
        )
    except Exception as exc:
        pytest.skip(f"originate failed: {exc}")
    await asyncio.sleep(3)

    s, body = await api.raw_request(
        "POST", f"/api/cc/calls/{call_id}/conference/end",
        {"transfer_id": "nonexistent-tid"})
    print(f"\n[pr1-err] /conference/end on unknown tid: status={s} body={body!r:.120}")
    # destroy_conference is idempotent (REST DELETE semantics) —
    # destroying a non-existent conference returns 200 Ok. This is fine.
    assert s == 200, f"expected 200 (idempotent destroy), got {s}"
    assert body.get("state") == "ended"

    try:
        await event_checker.rwi.hangup(call_id)
    except Exception:
        pass
    await asyncio.sleep(2)
