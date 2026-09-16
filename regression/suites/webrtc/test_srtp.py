"""SRTP (SDES) E2E — sipbot --srtp both legs through the PBX.

rustpbx classifies an offer with RTP/SAVP (or a=crypto without ICE/DTLS) as
TransportMode::Srtp and mirrors SDES-SRTP onto the outbound leg ("secure in →
secure out"). Both UAs negotiate crypto and exchange SRTP packets; audio
content is verified via sipbot's AudioQuality analyzer.
"""

from __future__ import annotations

import pytest

import helpers as h

pytestmark = [pytest.mark.media]


@pytest.mark.asyncio
@pytest.mark.xfail(
    reason="sipbot 0.2.56 --srtp does not interoperate even UA↔UA directly: a "
           "two-sipbot direct call with --srtp on both ends exchanges "
           "signaling but zero RTP (no a=crypto in the offers). Until the "
           "test UA's SDES works, the PBX-side mirror cannot be validated "
           "end-to-end; rustpbx's transport classification is covered by "
           "unit tests (sip_session.rs test_sdp_transport_mode_*).",
    strict=True,
)
async def test_srtp_sdes_both_legs(pbx, sipbot_pool):
    """PCMU caller with --srtp → registered callee with --srtp: the call
    establishes and bidirectional SRTP audio flows with real content.

    sipbot neither logs crypto keywords nor can we read the negotiated SDP
    from its output, so the assertion is behavioral: with SDES on both ends,
    media only flows if the PBX mirrored SRTP correctly — a plain-RTP
    fallback yields auth failures and zero decodable audio."""
    import asyncio

    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)

    callee = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15512), username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", srtp=True, audio_quality=True,
    )
    await h.wait_registered(callee)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=8, srtp=True, audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), (
        caller.output
    )

    async def _wait_done(ua, timeout=20):
        end = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < end:
            if not ua.is_alive:
                return
            await asyncio.sleep(0.3)

    await _wait_done(caller)
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"SRTP media not bidirectional: {stats}"
    assert stats.rx_packets > 50 and stats.tx_packets > 50, (
        f"too little SRTP media flowed: {stats}"
    )
    aq = caller.get_audio_quality()
    assert aq and aq.get("has_audio"), f"caller audio silent: {aq}"
    aq_c = callee.get_audio_quality()
    assert aq_c and aq_c.get("has_audio"), f"callee audio silent: {aq_c}"
