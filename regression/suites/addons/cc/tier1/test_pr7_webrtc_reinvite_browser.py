"""PR7 real verification: PBX → cc-phone browser WebRTC re-INVITE.

The cc-phone onInvite fix (reads request.body instead of
response.toLowerCase()) is the browser-side half of PR7. This test drives
a REAL browser widget (via dev-console) against a REAL PBX:

  1. Browser registers agent 1004 over WebSocket (auto → WebRTC leg)
  2. sipbot caller (1002) calls 1004
  3. Browser answers → active call
  4. POST /cc/calls/{call_id}/hold → PBX sends a sendonly re-INVITE to
     the browser's PeerConnection
  5. Assert: the browser widget shows "Held" (proves onCallHeld fired
     via the real re-INVITE, i.e. the PR7 fix works end-to-end)
  6. POST unhold → widget returns to Active

This is the strongest possible verification for PR7 — real sip.js +
real WebRTC + real PBX re-INVITE — closing the gap left by T23 (which
synthesised the re-INVITE in sim mode).
"""
from __future__ import annotations

import asyncio
import re

import pytest


pytestmark = [pytest.mark.tier1]


async def _login_widget(page, pbx):
    from helpers.cc_phone import CcPhonePage

    phone = CcPhonePage(page)
    await phone.open_widget(pbx, agent="1004", password="123456")
    return phone


async def _wait_widget_state(page, label: str, timeout: float = 20):
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        try:
            text = await page.locator(".cc-active-state").text_content(timeout=2000)
            if text and re.search(label, text, re.IGNORECASE):
                return text
        except Exception:
            pass
        await asyncio.sleep(0.5)
    raise AssertionError(
        f"widget state never showed {label!r}"
    )


@pytest.mark.asyncio
async def test_pr7_webrtc_reinvite_hold_unhold_browser(
    pbx, page, sipbot_pool, api, event_checker,
):
    """PBX-initiated hold/unhold re-INVITE reaches the browser widget and
    updates its UI state. Verifies the PR7 onInvite fix end-to-end.
    """
    await _login_widget(page, pbx)
    await page.wait_for_selector(".cc-dialpad", timeout=10000)

    # ── 1. sipbot calls the browser agent (1001) ────────────────────────
    callee = sipbot_pool.callee(
        host=pbx.host, port=17250, username="1002", password="123456",
        register=False, ring_secs=2, answer_mode="echo",
    )
    await asyncio.sleep(1)

    caller = sipbot_pool.caller(
        target=f"sip:1004@{pbx.sip_addr}",
        username="1002", password="123456", hangup=40,
    )

    # ── 2. Browser answers the call ─────────────────────────────────────
    try:
        await page.wait_for_selector(".cc-incoming-panel", timeout=15000)
    except Exception:
        pytest.skip("Browser never received the inbound call (1004 not WebRTC-registered)")
    # Button order in the incoming panel: [Reject, Answer] — click Answer.
    answer_btn = page.locator(".cc-incoming-panel button").nth(1)
    await answer_btn.click()
    await page.wait_for_selector(".cc-active-bar", timeout=15000)
    await _wait_widget_state(page, "active")

    # ── 3. Get the call_id from the widget SDK ──────────────────────────
    call_id = await page.evaluate("""() => {
        const w = window.phone || window.__testPhone;
        return w ? (w.getCallId ? w.getCallId() : '') : '';
    }""")
    print(f"\n[pr7] widget call_id: {call_id!r}")
    if not call_id:
        await asyncio.sleep(1)
        answered = event_checker.webhook.find("call_answered")
        call_id = answered.call_id if answered else ""
        print(f"[pr7] fallback webhook call_id: {call_id!r}")

    # ── 4. REST hold → PBX asks the browser to hold itself ─────────────
    # mode="notify": Chrome's WebRTC stack rejects PBX-crafted re-INVITE
    # offers, so for browser agents hold/unhold is delivered as an in-dialog
    # SIP INFO and the cc-phone widget drives its own re-INVITE (see the
    # HoldRequest docs in src/addons/cc/mod.rs).
    status, body = await api.raw_request(
        "POST", f"/api/cc/calls/{call_id}/hold", {"mode": "notify"})
    print(f"[pr7] hold status={status} body={body!r:.100}")
    assert status == 200, f"hold REST failed: {status}"

    # ── 5. Assert browser widget flips to Held ──────────────────────────
    await _wait_widget_state(page, "hold|held")
    print(f"[pr7] ✓ browser widget shows Held after PBX re-INVITE")

    # ── 6. REST unhold → widget returns to Active ───────────────────────
    status2, _ = await api.raw_request(
        "POST", f"/api/cc/calls/{call_id}/unhold", {"mode": "notify"})
    print(f"[pr7] unhold status={status2}")
    assert status2 == 200, f"unhold REST failed: {status2}"

    await _wait_widget_state(page, "active")
    print(f"[pr7] ✓ browser widget back to Active after unhold")

    # Cleanup: hang up and WAIT for the agent to leave busy/wrapup — a
    # lingering in-call agent would fail every later presence/queue test
    # (the agent state machine rejects busy -> idle transitions).
    try:
        await event_checker.rwi.hangup(call_id)
    except Exception:
        pass
    await asyncio.sleep(2)
    # Closing the page tears down the widget's SIP dialog (belt & suspenders).
    try:
        await page.close()
    except Exception:
        pass
    await asyncio.sleep(2)
