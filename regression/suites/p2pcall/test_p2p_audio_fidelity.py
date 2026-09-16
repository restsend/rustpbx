"""P2P audio-fidelity E2E tests — frequency-level verification of audio paths.

Existing p2p/reinvite/webrtc tests assert *packet flow* (RX/TX counts) or RMS
energy. This file upgrades the strongest guarantees to **signal fidelity**:

  * `test_transcode_fidelity_*`  — PCMU caller → callee answering ONLY
    {opus,pcma,g722} forces a transcoding bridge; the callee's recorded WAV
    must carry the same 440 Hz dominant tone (±15 Hz, ~±3%) and non-silent
    RMS, and the caller's echo recording proves the reverse transcode path.
  * `test_hold_resume_tone_fidelity` — caller plays a continuous 440 Hz tone
    and performs a UA-side HOLD/RESUME re-INVITE cycle; the callee recording
    must show the same tone before hold and after resume (renegotiation must
    not corrupt the transcoder output), and the caller keeps receiving RTP
    through the whole cycle (bridge continuity).
  * `test_webrtc_transcode_tone_fidelity` — WebRTC (opus) caller → RTP (pcmu)
    callee: transcoded leg carries the 440 Hz tone (callee WAV), and the
    echo back to the WebRTC side has real audio content.

sipbot records decoded RX audio to WAV (`--record`), so a dominant-frequency
assertion on the recording is a direct end-to-end check of codec conversion
correctness — not just "some bytes flowed".
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest

import helpers as h
from helpers import (
    extract_audio_region,
    find_dominant_frequency,
    find_signal_start,
    generate_sine_wav,
    has_audio_content,
    compute_rms_db,
    read_wav_mono,
)

pytestmark = [pytest.mark.media, pytest.mark.p2p]

TONE_HZ = 440.0
# Dominant-frequency tolerance: find_dominant_frequency scans in 5 Hz steps;
# codec artifacts (G.722 sub-band, opus frames) can shift the peak slightly.
FREQ_TOL_HZ = 15.0
MIN_RMS_DB = -40.0


def _assert_tone_in_wav(path: Path, *, label: str, freq: float = TONE_HZ,
                        region: tuple[float, float] | None = None) -> None:
    """Assert a WAV contains a dominant sine tone (default 440 Hz).

    `region` optionally restricts the analysis to a (start_s, end_s) window of
    the recording (used to compare before-hold vs after-resume segments).
    """
    samples, sr = read_wav_mono(path)
    if region is not None:
        lo, hi = int(region[0] * sr), int(region[1] * sr)
        samples = samples[lo:hi]
    assert samples.size >= sr // 2, (
        f"{label}: recording too short for frequency analysis "
        f"({samples.size} samples @ {sr}Hz): rms={compute_rms_db(samples):.1f}dB"
    )
    start = find_signal_start(samples)
    region_samples = extract_audio_region(
        samples, sr, start, min(2 * sr, samples.size - start)
    )
    assert region_samples.size >= sr // 2, (
        f"{label}: not enough non-silent audio ({region_samples.size} samples)"
    )
    assert has_audio_content(region_samples, MIN_RMS_DB), (
        f"{label}: audio too quiet (rms={compute_rms_db(region_samples):.1f}dB)"
    )
    dom, _mag = find_dominant_frequency(region_samples, sr, low=200, high=900, step=5)
    assert abs(dom - freq) <= FREQ_TOL_HZ, (
        f"{label}: dominant frequency {dom:.0f}Hz, expected {freq:.0f}Hz "
        f"(±{FREQ_TOL_HZ}Hz) — transcoded audio corrupted"
    )


def _resolve_record(path: Path) -> Path:
    """Resolve the actual recording file.

    sipbot `wait --record` suffixes the filename with a timestamp and call-id
    (e.g. `callee_<ts>_<callid>.wav`), so the callee recording must be globbed.
    Caller mode (`call --record`) keeps the exact filename.
    """
    if path.exists():
        return path
    siblings = sorted(path.parent.glob(path.stem + "*.wav"))
    if siblings:
        return siblings[-1]
    raise FileNotFoundError(
        f"no recording matching {path} or {path.stem}*.wav in {path.parent}"
    )


async def _wait_call_done(ua, timeout: float = 20) -> None:
    """Wait until the sipbot UA process has exited (call finished)."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if not ua.is_alive:
            return
        await asyncio.sleep(0.3)


async def _registered_callee(sipbot_pool, pbx, port: int, username: str = "1002",
                             **kwargs):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", **kwargs,
    )
    await h.wait_registered(ua)
    return ua


def _negotiated_codec(ua) -> str:
    import re
    matches = re.findall(r"(?:codec|Codec):\s*([A-Za-z0-9/]+)", ua.output)
    return matches[-1] if matches else ""


# ---------------------------------------------------------------------------
# 1. Transcoding fidelity matrix (PCMU → {OPUS, PCMA, G722})
# ---------------------------------------------------------------------------

@pytest.mark.parametrize("callee_codec,expect_codec", [
    ("opus", "OPUS"),
    ("pcma", "PCMA"),
    ("g722", "G722"),
])
@pytest.mark.asyncio
async def test_transcode_fidelity_matrix(pbx, sipbot_pool, tmp_path,
                                         callee_codec, expect_codec):
    """PCMU caller plays a 440 Hz tone; callee answers codec-only SDP so the
    PBX must transcode. The callee's recording must carry the same tone, and
    the caller's echo recording proves the reverse (decoded→PCMU) path."""
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)

    sine = tmp_path / "sine.wav"
    generate_sine_wav(sine, TONE_HZ, 1.0, 8000, 0.5)

    callee_wav = tmp_path / "callee.wav"
    caller_wav = tmp_path / "caller.wav"
    callee = await _registered_callee(
        sipbot_pool, pbx, h.ua_port(15480), codecs=callee_codec,
        record_file=str(callee_wav),
    )
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        codecs="pcmu", hangup=8, play_file=str(sine), record_file=str(caller_wav),
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), (
        caller.output
    )

    # Both directions must flow while the call is up.
    await h.wait_rtp(caller, "pcmu caller", timeout=15)
    assert caller.get_rtp_stats().is_bidirectional, caller.get_rtp_stats()

    await _wait_call_done(caller)
    await _wait_call_done(callee, timeout=5)

    # The callee really negotiated the forced codec → the bridge transcoded.
    codec = _negotiated_codec(callee)
    assert codec.upper() == expect_codec, (
        f"callee negotiated {codec!r}, expected {expect_codec!r}:\n{callee.output[-1500:]}"
    )

    # Forward path: PCMU → <codec> must preserve the tone.
    _assert_tone_in_wav(_resolve_record(callee_wav), label=f"callee({callee_codec}) rx")
    # Reverse path: <codec> → PCMU echo must preserve the tone too.
    _assert_tone_in_wav(_resolve_record(caller_wav), label=f"caller pcmu rx (echo via {callee_codec})")


# ---------------------------------------------------------------------------
# 2. Hold/resume tone fidelity through the media bridge
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_hold_resume_tone_fidelity(pbx, sipbot_pool, tmp_path):
    """A continuous 440 Hz tone survives a HOLD→RESUME re-INVITE cycle.

    The callee recording must show the SAME dominant tone in the pre-hold
    window [1.0, 2.5]s and the post-resume window [7.5, 9.5]s, proving the
    renegotiated bridge (and any transcoder state) did not corrupt the signal.
    The caller must keep receiving RTP during hold (bridge continuity).
    """
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)

    sine = tmp_path / "sine.wav"
    generate_sine_wav(sine, TONE_HZ, 1.0, 8000, 0.5)

    callee_wav = tmp_path / "callee.wav"
    callee = await _registered_callee(
        sipbot_pool, pbx, h.ua_port(15481), record_file=str(callee_wav),
    )
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=12, play_file=str(sine), reinvite_flows="3s:hold,6s:resume",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), (
        caller.output
    )
    assert await caller.wait_output_async(
        r"Re-INVITE HOLD completed successfully", timeout=15
    ), caller.output
    assert await caller.wait_output_async(
        r"Re-INVITE RESUME completed successfully", timeout=15
    ), caller.output

    # sipbot `call` mode only prints RTP counters in the final summary, so
    # wait for the call to end and then assert media flowed both ways across
    # the whole hold/resume cycle.
    await _wait_call_done(caller)
    await _wait_call_done(callee, timeout=5)
    final = caller.get_rtp_stats()
    assert final.is_bidirectional and final.tx_packets > 100, (
        f"media did not flow across hold/resume cycle: {final}"
    )

    # Same tone before hold and after resume.
    callee_rec = _resolve_record(callee_wav)
    _assert_tone_in_wav(callee_rec, label="pre-hold", region=(1.0, 2.5))
    _assert_tone_in_wav(callee_rec, label="post-resume", region=(7.5, 9.5))


# ---------------------------------------------------------------------------
# 3. WebRTC (opus) → RTP (pcmu) transcode fidelity
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_webrtc_transcode_tone_fidelity(pbx, sipbot_pool, tmp_path):
    """WebRTC caller (opus) → plain-RTP callee (pcmu-only): the transcoded
    leg must deliver the 440 Hz tone intact (callee WAV frequency check), and
    the echoed audio back through pcmu→opus must have real content."""
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.set_webrtc_users(["1001"])
    h.boot_pbx(pbx)

    sine = tmp_path / "sine.wav"
    generate_sine_wav(sine, TONE_HZ, 1.0, 8000, 0.5)

    callee_wav = tmp_path / "callee.wav"
    callee = await _registered_callee(
        sipbot_pool, pbx, h.ua_port(15301), codecs="pcmu", audio_quality=True,
        record_file=str(callee_wav),
    )
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=10, webrtc=True, audio_quality=True, codecs="opus,pcmu",
        play_file=str(sine),
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), (
        caller.output
    )

    # Callee side: pcmu-only answer → opus→pcmu transcode happened.
    codec = _negotiated_codec(callee)
    assert codec.upper() == "PCMU", (
        f"callee negotiated {codec!r}, expected PCMU:\n{callee.output[-1500:]}"
    )

    # Caller (WebRTC) side keeps receiving the transcoded echo.
    aq = None
    deadline = asyncio.get_event_loop().time() + 20
    while asyncio.get_event_loop().time() < deadline:
        aq = caller.get_audio_quality()
        if aq and aq.get("has_audio") and aq.get("total_frames", 0) >= 50:
            break
        await asyncio.sleep(0.5)
    assert aq and aq.get("has_audio"), (
        f"WebRTC caller received no audio content: {aq}\n{caller.output[-1500:]}"
    )

    await _wait_call_done(caller)
    await _wait_call_done(callee, timeout=5)
    _assert_tone_in_wav(_resolve_record(callee_wav), label="webrtc→pcmu callee rx")
