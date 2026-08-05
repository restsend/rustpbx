"""WebRTC cross-transport smoke test.

Verifies that a sipbot caller with --webrtc can establish a call through
rustpbx to a plain RTP callee, exercising the RTP↔WebRTC bridge.

This is a smoke test: it verifies call setup + RTP flow. Deep audio-quality
verification (frequency analysis) requires the WebRTC recording pipeline
to be finalized.
"""

from __future__ import annotations

import asyncio

import pytest

import helpers as h

pytestmark = [pytest.mark.media]


@pytest.mark.xfail(reason=(
    "ICE connectivity incomplete: PBX bridge activates (fast-path relay with "
    "PCMU, strip_a_to_b=true) but PBX ICE agent logs 'awaiting inbound TCP' "
    "and never receives STUN checks from sipbot. Likely a sipbot ICE "
    "implementation or PBX ICE config issue (UDP vs TCP)."
))
@pytest.mark.asyncio
async def test_webrtc_to_rtp_call_connects(pbx, sipbot_pool):
    """WebRTC caller → RTP callee through rustpbx media proxy."""
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_webrtc_users(["1001"])
    h.boot_pbx(pbx)

    callee = sipbot_pool.callee(
        host=pbx.host, port=15300, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=10, webrtc=True, audio_quality=True,
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"WebRTC call not answered:\n{caller.output[-1500:]}"

    await h.wait_rtp(callee, "callee", 20)

    stats = callee.get_rtp_stats()
    assert stats.has_rx or stats.has_tx, (
        f"no RTP at callee after WebRTC→RTP bridge: {stats}\n"
        f"caller output tail:\n{caller.output[-800:]}"
    )
