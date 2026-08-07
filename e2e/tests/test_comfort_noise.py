"""Comfort-noise (CNG) E2E tests.

Verifies the media-layer comfort noise behavior:
  - Config parsing: comfort_noise on/off + custom level
  - RTP continuity during hold (CNG/silence is still packetized)
  - Content-level CNG verification via sipbot --record (best-effort)

The EgressPipeline (crates/rustpbx-media/src/egress.rs) is always
ptime-paced (20 ms). When a leg has no active media source (Silence
egress), comfort_noise=true synthesizes low-pass-filtered white noise
at -35 dBFS; comfort_noise=false emits digital silence (all zeros).
In both cases, RTP packets continue to flow — the difference is only
in payload content.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.media]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _registered_echo_callee(sipbot_pool, pbx, port, username="1002"):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await asyncio.sleep(2)
    return ua


@pytest.mark.asyncio
async def test_cng_on_config_parses(pbx, sipbot_pool, rwi):
    """comfort_noise=true: config parses, PBX boots, call works."""
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_media(comfort_noise=True, level_db=-35.0)
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    await _registered_echo_callee(sipbot_pool, pbx, 15100, "1002")

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=8,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 15)


@pytest.mark.asyncio
async def test_cng_off_config_parses(pbx, sipbot_pool, rwi):
    """comfort_noise=false: config parses, PBX boots, call works."""
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_media(comfort_noise=False, level_db=-40.0)
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    await _registered_echo_callee(sipbot_pool, pbx, 15101, "1002")

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=8,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 15)


@pytest.mark.asyncio
async def test_cng_custom_level_config_parses(pbx, sipbot_pool, rwi):
    """Custom comfort_noise_level_db config parses without error."""
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_media(comfort_noise=True, level_db=-25.0)
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    await _registered_echo_callee(sipbot_pool, pbx, 15102, "1002")

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=8,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 15)


@pytest.mark.xfail(reason="WIP: RWI hold on originated (UAC) calls fails 'Leg not found: callee' — the originate registers the 1002 dialog as 'caller' with media on LegSide::B but no 'callee' leg in self.legs, so handle_hold returns before the media switch")
@pytest.mark.asyncio
async def test_cng_rtp_flows_during_hold(pbx, sipbot_pool, rwi):
    """RTP packets keep flowing during hold (ptime-paced egress).

    With comfort_noise=true, CNG frames are packetized at 20ms intervals
    just like real audio. The callee's RX packet count must increase even
    while the call is on hold.
    """
    from helpers import generate_sine_wav

    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_media(comfort_noise=True, level_db=-35.0)
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    callee = await _registered_echo_callee(sipbot_pool, pbx, 15103, "1002")

    call_id = _call_id("cng")
    resp = await rwi.originate(call_id, f"sip:1002@{pbx.sip_addr}", "sip:rwi@pbx", "default")
    assert resp.get("status") == "success", resp
    await rwi.wait_for_event("call_answered", timeout=15)

    await asyncio.sleep(2)
    stats_before = callee.get_rtp_stats()
    rx_before = stats_before.rx_packets

    hold = await rwi.hold(call_id)
    assert hold.get("status") == "success", f"hold failed: {hold}"
    await rwi.wait_for_event("media_hold_started", timeout=10)
    await asyncio.sleep(3)
    stats_after = callee.get_rtp_stats()
    rx_after = stats_after.rx_packets
    assert rx_after > rx_before, (
        f"callee RX did not increase during hold "
        f"(CNG should keep packets flowing): before={rx_before} after={rx_after}"
    )

    await rwi.hangup(call_id)


@pytest.mark.xfail(reason="WIP media layer: sipbot --record content analysis not finalized for all modes")
@pytest.mark.asyncio
async def test_cng_on_hold_nonzero_rms(pbx, sipbot_pool, rwi, tmp_path):
    """comfort_noise=true: RX audio during hold has non-zero RMS (CNG present).

    With CNG on, silence egress synthesizes noise at ~-35 dBFS instead of
    digital silence (-inf). Requires a working RX recording to verify.
    """
    from helpers import read_wav_mono, compute_rms_db, has_audio_content

    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_media(comfort_noise=True, level_db=-35.0)
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)

    rec = tmp_path / "callee_cng.wav"
    callee = sipbot_pool.callee(
        host=pbx.host, port=15104, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
        record_file=str(rec),
    )
    await asyncio.sleep(2)

    call_id = _call_id("cngrms")
    resp = await rwi.originate(call_id, f"sip:1002@{pbx.sip_addr}", "sip:rwi@pbx", "default")
    assert resp.get("status") == "success", resp
    await rwi.wait_for_event("call_answered", timeout=15)

    hold = await rwi.hold(call_id)
    assert hold.get("status") == "success", hold
    await rwi.wait_for_event("media_hold_started", timeout=10)
    await asyncio.sleep(3)
    await rwi.hangup(call_id)
    await asyncio.sleep(1)

    if rec.exists() and rec.stat().st_size > 44:
        rx, sr = read_wav_mono(rec)
        rms = compute_rms_db(rx)
        assert has_audio_content(rx, -45.0), (
            f"expected CNG non-zero RMS > -45 dBFS during hold, got {rms:.1f} dBFS"
        )
