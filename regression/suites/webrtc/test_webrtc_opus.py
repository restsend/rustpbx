"""WebRTC caller (opus) -> plain-RTP callee (opus): same-codec Opus relay.

Verifies that the rustpbx media bridge relays Opus↔Opus (fast-path rewrite)
when both the WebRTC caller and the plain-RTP callee negotiate opus, and that
bidirectional audio flows end-to-end.
"""

from __future__ import annotations

import asyncio
import re

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
        f"{label}: no audio frames after {timeout}s - aq={aq}\n{ua.output[-1200:]}"
    )


def _negotiated_codec(ua) -> str:
    """Last codec sipbot reported (e.g. `codec: OPUS`, `Negotiated Codec: OPUS`)."""
    matches = re.findall(r"(?:codec|Codec):\s*([A-Za-z0-9/]+)", ua.output)
    return matches[-1] if matches else ""


@pytest.mark.asyncio
async def test_webrtc_to_rtp_opus_relay(pbx, sipbot_pool):
    """WebRTC caller (opus) -> RTP callee (opus): opus<->opus relay audio."""
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_webrtc_users(["1001"])
    h.boot_pbx(pbx)

    callee = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15300), username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True, codecs="opus",
    )
    await h.wait_registered(callee)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=10, webrtc=True, audio_quality=True, codecs="opus,pcmu",
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"WebRTC call not answered:\n{caller.output[-1500:]}"

    # The plain-RTP callee must have negotiated opus (not fallen back to pcmu).
    codec = _negotiated_codec(callee)
    assert codec.upper() == "OPUS", (
        f"callee did not negotiate OPUS (got {codec!r}):\n{callee.output[-2000:]}"
    )

    callee_aq = await _wait_audio_frames(callee, "callee")
    silence_ratio = callee_aq.get("silence_ratio", 1.0)
    assert silence_ratio < 0.55, (
        f"callee RX mostly silence (opus bridge not delivering): {callee_aq}"
    )
    assert callee_aq["avg_rms"] > 20.0, f"callee RX RMS too low: {callee_aq}"

    caller_aq = await _wait_audio_frames(caller, "caller")
    assert caller_aq["has_audio"], f"caller (WebRTC) RX has no audio: {caller_aq}"

    if pbx.log_file_path and pbx.log_file_path.exists():
        log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace")
        assert "fast-path relay activated" in log and "codec=Opus" in log, (
            f"bridge did not run Opus fast-path relay:\n{log[-3000:]}"
        )
