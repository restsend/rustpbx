"""WebRTC cross-transport E2E test.

Verifies that a sipbot caller with --webrtc can establish a call through
rustpbx to a plain RTP callee, exercising the RTP↔WebRTC bridge.

Media is asserted via sipbot AudioQuality (frames + silence_frames), because
the legacy RTP `RX:/TX:` packet stats are not emitted in WebRTC mode. The
AudioQuality analyzer reads actual decoded audio frames from the media tap,
so `has_audio=true` proves bidirectional media flow.
"""

from __future__ import annotations

import asyncio

import pytest

import helpers as h

pytestmark = [pytest.mark.media]


async def _wait_audio_frames(ua, label: str, min_frames: int = 100, timeout: float = 20):
    """Wait until the UA's AudioQuality shows received non-silent audio frames."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        aq = ua.get_audio_quality()
        if aq and aq.get("total_frames", 0) >= min_frames and aq.get("has_audio"):
            return aq
        await asyncio.sleep(0.5)
    aq = ua.get_audio_quality()
    raise AssertionError(
        f"{label}: no audio frames after {timeout}s — aq={aq}\n{ua.output[-1200:]}"
    )


@pytest.mark.asyncio
async def test_webrtc_to_rtp_call_audio(pbx, sipbot_pool):
    """WebRTC caller → RTP callee: bidirectional audio flows through the bridge."""
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_webrtc_users(["1001"])
    h.boot_pbx(pbx)

    callee = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15300), username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(callee)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=10, webrtc=True, audio_quality=True, codecs="opus,pcmu",
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"WebRTC call not answered:\n{caller.output[-1500:]}"

    callee_aq = await _wait_audio_frames(callee, "callee")
    assert callee_aq["silence_frames"] == 0, (
        f"callee RX all silence (WebRTC→RTP bridge not delivering): {callee_aq}"
    )
    assert callee_aq["avg_rms"] > 20.0, (
        f"callee RX RMS too low: {callee_aq}"
    )

    caller_aq = await _wait_audio_frames(caller, "caller")
    assert caller_aq["has_audio"], f"caller (WebRTC) RX has no audio: {caller_aq}"
