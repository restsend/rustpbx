"""Conference E2E — room dial-in (app=conference) over real SIP.

The Rust suite covers the MCU mixer unit-level (tests/call/conference_*,
mcu_*). This file proves the SIP-level lifecycle of the proxy dial-in path:
two callers dial the conference route number, both legs join the room,
events fire, and destroying the room tears the calls down.

Note: RWI `conference.add` only performs mixer bookkeeping for real proxy
sessions (participant media wiring exists on the dial-in path — see
SipSession::join_conference_mixer), so the E2E uses room dial-in, which is
also how production callers reach a conference.

Known gap (documented via the strict xfail below): the dial-in reverse path
(UA → mixer) delivers silence — participants continuously receive the
mixer's output (comfort silence) but no other member's audio is mixed in.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h
from helpers import generate_sine_wav

pytestmark = [pytest.mark.media]


def _cid(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _dialin_pair(pbx, sipbot_pool, tmp_path, *, with_tones=False):
    """Two callers dialing conference room 9000. Returns (b_call, c_call)."""
    kwargs_b, kwargs_c = {}, {}
    if with_tones:
        tone_b = tmp_path / "tone_b.wav"
        tone_c = tmp_path / "tone_c.wav"
        generate_sine_wav(tone_b, 440.0, 15.0, 8000, 0.5)
        generate_sine_wav(tone_c, 600.0, 15.0, 8000, 0.5)
        kwargs_b = {"play_file": str(tone_b)}
        kwargs_c = {"play_file": str(tone_c)}
    b_call = sipbot_pool.caller(
        target=f"sip:9000@{pbx.sip_addr}", username="1002", password="123456",
        hangup=18, audio_quality=True, **kwargs_b,
    )
    c_call = sipbot_pool.caller(
        target=f"sip:9000@{pbx.sip_addr}", username="1003", password="123456",
        hangup=18, audio_quality=True, **kwargs_c,
    )
    return b_call, c_call


async def _setup_room(pbx, pbx_config, webhook_server=None):
    pbx_config.add_route(
        "to-conference",
        match={"to.user": "^9000$"},
        priority=10,
        action="application",
        app="conference",
        app_params={"id": "room-9000"},
    )
    h.boot_pbx(pbx, webhook_url=webhook_server.url if webhook_server else "")


@pytest.mark.asyncio
async def test_conference_dialin_lifecycle(pbx, pbx_config, sipbot_pool, rwi,
                                           webhook_server, webhook_session, tmp_path):
    """Two dial-in legs join room-9000 (conference_joined ×2 on the webhook
    bus — dial-in events are Owner-dispatched, so the webhook tap is the
    reliable observation channel); conference.destroy tears both calls down."""
    await _setup_room(pbx, pbx_config, webhook_server)
    await h.connect_rwi(rwi)

    b_call, c_call = await _dialin_pair(pbx, sipbot_pool, tmp_path)
    for ua, label in ((b_call, "B"), (c_call, "C")):
        assert await ua.wait_output_async(r"200 OK|Call established", timeout=25), (
            f"{label} could not dial into the conference:\n{ua.output[-1200:]}"
        )
        await asyncio.sleep(1.0)

    # conference_joined fires per dial-in leg on the webhook tap.
    joined = 0
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline and joined < 2:
        joined = webhook_session.count("conference_joined")
        if joined >= 2:
            break
        await asyncio.sleep(0.3)
    assert joined >= 2, (
        f"expected 2 conference_joined webhook events, got {joined}; "
        f"all: {webhook_session.event_types()[-20:]}"
    )

    # The mixer keeps delivering to participants (frames keep advancing even
    # if the content is silence — see the xfail audio test below).
    async def _wait_frames(ua, label, min_frames=100, timeout=15):
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            aq = ua.get_audio_quality()
            if aq and aq.get("total_frames", 0) >= min_frames:
                return aq
            await asyncio.sleep(0.5)
        raise AssertionError(f"{label}: mixer output never reached the leg: {ua.get_audio_quality()}")

    await _wait_frames(b_call, "B")
    await _wait_frames(c_call, "C")

    # Hang up: both legs must end cleanly and report call_hangup.
    deadline = asyncio.get_event_loop().time() + 30
    while asyncio.get_event_loop().time() < deadline:
        if not b_call.is_alive and not c_call.is_alive:
            break
        await asyncio.sleep(0.5)
    assert not b_call.is_alive and not c_call.is_alive, "dial-in calls never ended"

    hangups = 0
    deadline = asyncio.get_event_loop().time() + 15
    while asyncio.get_event_loop().time() < deadline and hangups < 2:
        hangups = webhook_session.count("call_hangup")
        await asyncio.sleep(0.3)
    assert hangups >= 2, (
        f"expected 2 call_hangup events, got {hangups}; "
        f"events: {webhook_session.event_types()[-20:]}"
    )


@pytest.mark.asyncio
@pytest.mark.xfail(
    reason="dial-in conference reverse path delivers silence to the mixer: "
           "participants continuously receive mixer output (frames advance) but "
           "no other member's audio is mixed in — UA→mixer input is not wired "
           "(SipSession::start_conference_mixer reverse_loop). Documents a "
           "product bug in the proxy dial-in path.",
    strict=True,
)
async def test_conference_dialin_audio_mixing(pbx, pbx_config, sipbot_pool, rwi, tmp_path):
    """B plays 440 Hz, C plays 600 Hz into room-9000: each member must hear
    the OTHER's tone through the mixer."""
    await _setup_room(pbx, pbx_config)
    await h.connect_rwi(rwi)

    b_call, c_call = await _dialin_pair(pbx, sipbot_pool, tmp_path, with_tones=True)
    for ua, label in ((b_call, "B"), (c_call, "C")):
        assert await ua.wait_output_async(r"200 OK|Call established", timeout=25), (
            f"{label} could not dial into the conference:\n{ua.output[-1200:]}"
        )
        await asyncio.sleep(1.0)

    async def _wait_audible(ua, label, timeout=15):
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            aq = ua.get_audio_quality()
            if aq and aq.get("has_audio") and aq.get("total_frames", 0) >= 100:
                return
            await asyncio.sleep(0.5)
        aq = ua.get_audio_quality()
        raise AssertionError(
            f"{label}: heard no other member through the mixer: {aq}"
        )

    await _wait_audible(b_call, "B (should hear C's 600 Hz)")
    await _wait_audible(c_call, "C (should hear B's 440 Hz)")
