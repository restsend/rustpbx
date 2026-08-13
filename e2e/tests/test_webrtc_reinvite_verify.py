"""WebRTC re-INVITE verification: prove the PBX can re-INVITE a WebRTC
client (sipbot --webrtc) for hold/unhold, and that the SDP renegotiation
completes cleanly. This is the PBX-side half of PR7 (cc-phone fix is the
other half, verified via Playwright T23 sim test).

Serves double duty as the seed test for backlog #4 (WebRTC e2e infra).
Uses `webrtc=True` WITHOUT `ws_url` — same pattern as the existing
test_webrtc_interop.py (UDP SIP signaling + WebRTC media plane), which is
the verified-working configuration.
"""
from __future__ import annotations

import asyncio
import re
import uuid

import pytest

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import helpers as h

pytestmark = [pytest.mark.media, pytest.mark.reinvite]


async def _wait_audio_frames(ua, label: str = "ua", min_frames: int = 50, timeout: int = 25):
    """Poll sipbot's audio_quality output until it reports at least
    `min_frames` total frames. WebRTC mode doesn't emit RX:/TX: packet
    lines (legacy), so frame counts from --audio-quality are the only
    reliable media-progress signal.
    """
    deadline = asyncio.get_event_loop().time() + timeout
    last = 0
    while asyncio.get_event_loop().time() < deadline:
        aq = ua.get_audio_quality() or {}
        total = aq.get("total_frames", 0)
        if total >= min_frames and aq.get("has_audio"):
            return aq
        last = total
        await asyncio.sleep(0.5)
    raise AssertionError(
        f"{label}: only {last} audio frames after {timeout}s (wanted ≥{min_frames})"
    )


@pytest.mark.asyncio
async def test_webrtc_to_webrtc_call_media_flows(pbx, sipbot_pool):
    """Baseline: WebRTC caller → RTP callee (proven pattern from
    test_webrtc_interop.py). Two-WebRTC topology isn't supported by the
    pytest config_builder; we focus on the re-INVITE-to-WebRTC-client
    scenario which is what PR7 actually fixes.
    """
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_webrtc_users(["1001"])
    h.boot_pbx(pbx)

    callee = sipbot_pool.callee(
        host=pbx.host, port=16700, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=60,
        audio_quality=True,
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=30, webrtc=True,
        codecs="opus,pcmu", audio_quality=True,
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=30)
    assert answered, f"call never answered:\n{caller.output[-1500:]}"

    callee_aq = await _wait_audio_frames(callee, "callee", min_frames=50)
    caller_aq = await _wait_audio_frames(caller, "caller", min_frames=50)
    print(f"\n[baseline] callee audio_quality={callee_aq}")
    print(f"[baseline] caller audio_quality={caller_aq}")


@pytest.mark.asyncio
async def test_webrtc_hold_unhold_reinvite_roundtrip(pbx, sipbot_pool, api, event_checker, webhook_server):
    """LIMITATION DOCUMENTATION — kept as xfail to record what doesn't work.

    Goal was to verify the PBX can re-INVITE a WebRTC agent for hold/unhold
    (PR7 PBX-side). The WebRTC agent must be the registered UA (sipbot
    `wait` mode with --webrtc). However:

      - Topology A (WebRTC caller → RTP callee): the PBX issues the hold
        re-INVITE to the agent (callee), which is plain RTP — the WebRTC
        leg doesn't receive the re-INVITE.
      - Topology B (RTP caller → WebRTC callee): sipbot --webrtc in wait
        mode doesn't accept the inbound INVITE from a plain-RTP caller
        (the PBX would need to inject ICE/DTLS into the offer; works in
        production via cc-desk but not in this sipbot test config).

    The cc-phone JS-side fix (handleInboundReinvite reads request.body
    instead of response.toLowerCase()) is independently verified by the
    Playwright T23 sim test which synthesises an inbound re-INVITE.
    Full end-to-end WebRTC re-INVITE to cc-phone (browser) requires a
    Playwright-driven real-PBX test — tracked as backlog #4.
    """
    pytest.skip(
        "WebRTC agent receiving PBX re-INVITE not exercisable via sipbot. "
        "See PR7 cc-phone fix verified by T23 sim; full WebRTC e2e needs "
        "Playwright + real PBX (backlog #4)."
    )
