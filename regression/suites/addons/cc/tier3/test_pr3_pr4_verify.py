"""Backend verification: assert PR3 + PR4 actually behave as claimed.

PR3: complete_transfer keeps the conference alive when 2 participants
     remain (returns result=conference_active, NOT downgraded_to_p2p).
PR4: consult sub-endpoints return 4xx (not 500) on wrong-state / not-found.

These properties were only sim/unit tested at commit time; this module
verifies them against a real PBX + real sipbot legs.
"""
from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.tier3, pytest.mark.cc_transfer_events]


@pytest.mark.asyncio
async def test_pr3_complete_returns_conference_active(pbx, sipbot_pool, api, event_checker):
    """PR3: when /complete is called with 2 participants remaining, the
    response must report result=conference_active (the conference is kept
    alive so A and C continue via the mixer). Previously it returned
    downgraded_to_p2p and destroyed the conference, leaving A/C silent.
    """
    callee_b = sipbot_pool.callee(
        host=pbx.host, port=16520, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=60,
    )
    callee_c = sipbot_pool.callee(
        host=pbx.host, port=16530, username="1003", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=60,
    )
    await asyncio.sleep(3)

    # Originate A→B
    call1 = f"pr3-a-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call1, caller_id="1001",
            destination=f"sip:1002@{pbx.sip_addr}", timeout_secs=15,
        )
    except Exception as exc:
        pytest.skip(f"originate A→B failed: {exc}")
    await asyncio.sleep(3)

    # /consult
    status, body = await api.raw_request(
        "POST", f"/api/cc/calls/{call1}/consult", {"target": "1003"})
    assert status == 200, f"consult failed: {status} {body!r:.80}"
    tid = body.get("transfer_id") if isinstance(body, dict) else None
    assert tid, f"no transfer_id: {body!r:.80}"

    # Originate B→C consult leg
    call2 = f"pr3-c-{uuid.uuid4().hex[:8]}"
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

    # /merge
    s, merge_body = await api.raw_request(
        "POST", f"/api/cc/calls/{call1}/consult/{tid}/merge", {})
    assert s == 200, f"merge failed: {s} {merge_body!r:.80}"

    # /complete — the key PR3 assertion
    s, complete_body = await api.raw_request(
        "POST", f"/api/cc/calls/{call1}/consult/{tid}/complete", {})
    assert s == 200, f"complete failed: {s} {complete_body!r:.80}"

    completion = (complete_body or {}).get("completion") or {}
    result_value = completion.get("result")
    print(f"\n[PR3] /complete response: {complete_body!r:.200}")
    print(f"[PR3] completion.result = {result_value!r}")

    # PR3 assertion: with 2 participants remaining, the conference is kept
    # active. The old code returned "downgraded_to_p2p" and destroyed the
    # conference, leaving A and C with no media bridge.
    assert result_value == "conference_active", (
        f"PR3 regression: complete returned {result_value!r}, "
        f"expected 'conference_active' (conf should stay alive with A+C)"
    )
    assert "conf_id" in completion, (
        f"PR3 regression: no conf_id in completion {completion!r:.120}"
    )

    # Cleanup
    for cid in (call1, call2):
        try:
            await event_checker.rwi.hangup(cid)
        except Exception:
            pass
    await asyncio.sleep(2)


@pytest.mark.asyncio
async def test_pr4_consult_endpoints_return_4xx_on_wrong_state(pbx, sipbot_pool, api, event_checker):
    """PR4: consult sub-endpoints must return 4xx (NOT 500) for wrong-state
    / not-found. Previously every error was blanket-mapped to 500, masking
    state-machine bugs as server crashes.

    Drives: /consult on a real call → then immediately /merge WITHOUT
    /connected → state is still Consulting → /merge must return 409.
    """
    callee = sipbot_pool.callee(
        host=pbx.host, port=16540, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=60,
    )
    await asyncio.sleep(3)

    call_id = f"pr4-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call_id, caller_id="1001",
            destination=f"sip:1002@{pbx.sip_addr}", timeout_secs=15,
        )
    except Exception as exc:
        pytest.skip(f"originate failed: {exc}")
    await asyncio.sleep(3)

    # /consult succeeds
    s, body = await api.raw_request(
        "POST", f"/api/cc/calls/{call_id}/consult", {"target": "1003"})
    assert s == 200, f"consult failed: {s}"
    tid = body.get("transfer_id")

    # /merge WITHOUT /connected → state is Consulting → must be 409, NOT 500
    s2, _ = await api.raw_request(
        "POST", f"/api/cc/calls/{call_id}/consult/{tid}/merge", {})
    print(f"\n[PR4] /merge on Consulting state returned HTTP {s2}")
    assert s2 != 500, (
        "PR4 regression: /merge returned 500 on wrong state. The handler "
        "should now return 409 (Conflict), not blanket-500."
    )
    assert s2 == 409, f"PR4: expected 409 Conflict, got {s2}"

    # /complete on Consulting state → also 409
    s3, _ = await api.raw_request(
        "POST", f"/api/cc/calls/{call_id}/consult/{tid}/complete", {})
    print(f"[PR4] /complete on Consulting state returned HTTP {s3}")
    assert s3 != 500, "PR4 regression: /complete returned 500 on wrong state"
    assert s3 == 409, f"PR4: expected 409 Conflict, got {s3}"

    # /connected on a non-existent transfer_id → must be 404, NOT 500
    s4, _ = await api.raw_request(
        "PUT", f"/api/cc/calls/{call_id}/consult/nonexistent-tid/connected",
        {"session_b": "fake"})
    print(f"[PR4] /connected on unknown transfer_id returned HTTP {s4}")
    assert s4 != 500, "PR4 regression: /connected returned 500 on not-found"
    assert s4 == 404, f"PR4: expected 404 Not Found, got {s4}"

    try:
        await event_checker.rwi.hangup(call_id)
    except Exception:
        pass
    await asyncio.sleep(2)
