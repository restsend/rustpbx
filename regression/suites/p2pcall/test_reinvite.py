"""Re-INVITE (mid-call SDP renegotiation) E2E tests.

Replaces the Rust in-process `test_reinvite.rs` and the re-INVITE paths of
`test_call_e2e.rs` / `test_trunk_b2bua_e2e.rs`:

  * `test_reinvite_audio_hold_unhold`  → caller sends HOLD re-INVITE (sendonly),
    the PBX must forward it and answer; RESUME re-INVITE (sendrecv) must restore
    bidirectional media.
  * `test_reinvite_hold_resume`        → same lifecycle driven through the PBX
    media bridge with real RTP, and the call must still tear down cleanly (CDR).

sipbot's `--reinvite-flows "3s:hold,6s:resume"` drives a real re-INVITE from the
caller UA after the call is answered. The PBX B2BUA forwards/re-answers it, so
`Re-INVITE HOLD/RESUME completed successfully` in sipbot output proves the full
path (caller → PBX → callee → PBX → caller) survived renegotiation. RTP packet
stats prove media flows bidirectionally after resume, and the CDR file proves
the call was torn down cleanly.

The codec-change re-INVITE scenario (`test_reinvite_codec_change`,
`test_trunk_b2bua_mid_call_reinvite`) is NOT covered here: sipbot's re-INVITE
flow only supports `hold`/`resume`, so that coverage stays in the Rust suite.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.reinvite]

# sipbot prints these after a re-INVITE completes successfully.
_REINVITE_HOLD_OK = r"Re-INVITE HOLD completed successfully"
_REINVITE_RESUME_OK = r"Re-INVITE RESUME completed successfully"


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _registered_callee(sipbot_pool, pbx, port, username="1002"):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(ua)
    return ua


async def _wait_rtp(ua, label: str, timeout: float = 20) -> None:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if ua.get_rtp_stats().has_rx or ua.get_rtp_stats().has_tx:
            return
        await asyncio.sleep(0.3)
    raise AssertionError(
        f"{label}: no RTP after {timeout}s — {ua.get_rtp_stats()}\n{ua.output[-1500:]}"
    )


async def _cdr_files(cdr_dir, timeout: float = 10) -> list:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        files = list(cdr_dir.rglob("*.json"))
        if files:
            return files
        await asyncio.sleep(0.5)
    return []


# ---------------------------------------------------------------------------
# P2P: caller sends HOLD then RESUME re-INVITE mid-call
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_reinvite_hold_then_resume_keeps_call_and_media(pbx, sipbot_pool, cdr_dir, tmp_path):
    """Caller sends HOLD re-INVITE at 3s, RESUME at 6s.

    The PBX B2BUA must forward the re-INVITE and answer it so both UAs keep
    their dialog; after RESUME, real bidirectional RTP flows again; the call
    completes and writes a CDR.
    """
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)

    await _registered_callee(sipbot_pool, pbx, h.ua_port(15400), "1002")

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=10, reinvite_flows="3s:hold,6s:resume",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output

    # HOLD re-INVITE (sendonly) must complete through the PBX.
    assert await caller.wait_output_async(_REINVITE_HOLD_OK, timeout=15), (
        f"HOLD re-INVITE not completed:\n{caller.output[-2000:]}"
    )
    # RESUME re-INVITE (sendrecv) must complete and re-open the audio path.
    assert await caller.wait_output_async(_REINVITE_RESUME_OK, timeout=15), (
        f"RESUME re-INVITE not completed:\n{caller.output[-2000:]}"
    )

    await _wait_rtp(caller, "caller after resume")
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"caller RTP not bidirectional after resume: {stats}"

    # Wait for the call to finish and a CDR to be written.
    cdr_files = await _cdr_files(cdr_dir, timeout=15)
    assert cdr_files, "no CDR JSON files produced under config/cdr/"


# ---------------------------------------------------------------------------
# P2P: caller plays a tone and survives a hold/resume cycle
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_reinvite_with_audio_after_resume(pbx, sipbot_pool, tmp_path):
    """Media continues to flow after a HOLD→RESUME cycle with the caller TX
    carrying a 440 Hz tone (the echo callee returns it on RX)."""
    from helpers import generate_sine_wav

    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)

    sine = tmp_path / "sine.wav"
    generate_sine_wav(sine, 440.0, 0.5, 8000, 0.5)

    await _registered_callee(sipbot_pool, pbx, h.ua_port(15401), "1002")

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=12, play_file=str(sine), reinvite_flows="3s:hold,6s:resume",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    assert await caller.wait_output_async(_REINVITE_HOLD_OK, timeout=15), caller.output
    assert await caller.wait_output_async(_REINVITE_RESUME_OK, timeout=15), caller.output

    # The looped tone should produce many TX packets and RX echo.
    deadline = asyncio.get_event_loop().time() + 20
    while asyncio.get_event_loop().time() < deadline:
        stats = caller.get_rtp_stats()
        if stats.is_bidirectional and stats.tx_packets > 30:
            break
        await asyncio.sleep(0.3)
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"caller RTP not bidirectional: {stats}"
    assert stats.tx_packets > 30, f"expected caller TX tone packets after resume: {stats}"


# ---------------------------------------------------------------------------
# Trunk B2BUA: re-INVITE through the inbound-trunk B2BUA path
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_reinvite_trunk_b2bua_mid_call(pbx_config, pbx, sipbot_pool, cdr_dir):
    """Inbound trunk call through the B2BUA survives a HOLD→RESUME re-INVITE.

    The external (trunk) caller renegotiates mid-call; the PBX B2BUA must keep
    both dialogs alive, media must flow after resume, and a CDR must be written.
    """
    callee_port = h.ua_port(15402)
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "mid-reinvite-trunk", dest=f"127.0.0.1:{callee_port}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
    )
    pbx_config.media_proxy = "all"
    h.boot_pbx(pbx)

    await _registered_callee(sipbot_pool, pbx, callee_port, "1002")

    # External trunk caller (From domain differs from the PBX realm → inbound).
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="external", password="123456",
        from_uri="sip:external@trunk.example.com", hangup=10,
        reinvite_flows="3s:hold,6s:resume",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    assert await caller.wait_output_async(_REINVITE_HOLD_OK, timeout=15), caller.output
    assert await caller.wait_output_async(_REINVITE_RESUME_OK, timeout=15), caller.output

    await _wait_rtp(caller, "trunk caller after resume")
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"trunk caller RTP not bidirectional: {stats}"

    cdr_files = await _cdr_files(cdr_dir, timeout=15)
    assert cdr_files, "no CDR JSON files produced for trunk B2BUA re-INVITE call"
