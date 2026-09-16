"""WebRTC early-media gate regression E2E test.

Ports the Rust regression guards:

  * `test_caller_gate_regression.rs::test_webrtc_rtp_caller_gate_opens_on_same_sdp_200ok`
    — after 183 Session Progress + SDP followed by a 200 OK carrying the same
    SDP, the caller's media gate must be open so RTP actually flows.
  * `test_183_early_media_regression.rs::test_early_media_183_then_same_sdp_200ok_rtp_flow`
    — the B→A direction was silent (0 RTP packets) before the fix; after the
    fix, the caller receives early-media RTP on the 183 and continues after the
    200 OK.

An inbound trunk with `ringback.ring` sends 183 early media (the ringback tone)
while the callee rings, then a 200 OK when the callee answers — exactly the
183-then-200-OK sequence the regression targets. Driving it with a real WebRTC
(sipbot `--webrtc`) caller exercises the DTLS-SRTP gate path: if the gate fails
to open, the caller hears nothing and `has_audio` stays false.

The plain-RTP half of this regression is already covered by
`test_trunk_ringback.py::test_trunk_ringback_ring_183_early_media`.
"""

from __future__ import annotations

import asyncio

import pytest

import helpers as h

pytestmark = [pytest.mark.reinvite]


async def _wait_audio_frames(ua, label: str, min_frames: int = 100, timeout: float = 25):
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        aq = ua.get_audio_quality()
        if aq and aq.get("total_frames", 0) >= min_frames and aq.get("has_audio"):
            return aq
        await asyncio.sleep(0.5)
    aq = ua.get_audio_quality()
    raise AssertionError(
        f"{label}: no audio frames after {timeout}s — aq={aq}\n{ua.output[-1500:]}"
    )


@pytest.mark.asyncio
async def test_webrtc_183_early_media_then_200ok_gate_opens(pbx_config, pbx, sipbot_pool):
    """WebRTC caller through an inbound trunk hears the 183 early-media ringback
    tone, then the callee answers (200 OK) and audio keeps flowing — proving the
    caller media gate is open after the 183→200-OK sequence."""
    callee_port = h.ua_port(15410)
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.media_proxy = "all"
    pbx_config.add_trunk(
        "webrtc-gate-trunk", dest=f"127.0.0.1:{callee_port}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
        ringback={"ring": "tone://440,3000"},
    )
    h.boot_pbx(pbx)

    callee = sipbot_pool.callee(
        host=pbx.host, port=callee_port, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(callee)

    # External WebRTC caller (From domain differs from the PBX realm → inbound
    # trunk call). WebRTC transport is auto-detected from the offer SDP.
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="external", password="123456",
        from_uri="sip:external@trunk.example.com", hangup=10,
        webrtc=True, codecs="opus,pcmu", audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=30), (
        f"WebRTC trunk call not answered:\n{caller.output[-1500:]}"
    )

    # The caller must have received real early-media / post-answer audio.
    aq = await _wait_audio_frames(caller, "WebRTC caller")
    assert aq["has_audio"], f"WebRTC caller RX has no audio (gate closed?): {aq}"
    assert aq["silence_ratio"] < 1.0, f"WebRTC caller RX all silence: {aq}"
