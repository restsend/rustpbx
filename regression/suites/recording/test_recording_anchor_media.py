"""Recording-anchored media regressions (0.5.0-rc.1 field report).

With default `[recording] enabled=true`, media is forced through MediaBridge.
The regenerated plain-RTP SDP must be Legacy-SIP compatible:

1. Audio-only: call answers and bidirectional RTP flows (no silent media).
2. Audio+video: callee offer must not use BUNDLE / rtcp-mux / shared ports
   (strict softphones otherwise reply 488 Not Acceptable Here).

sipbot is used for audio media assertions. Video SDP shape is covered by the
rustpbx-media unit test (sipbot has no video; baresip automation is brittle).
"""

from __future__ import annotations

import asyncio
import os
import re
import subprocess
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.media, pytest.mark.record]


async def _wait_rtp(ua, label: str, timeout: float = 20) -> None:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if ua.get_rtp_stats().is_bidirectional:
            return
        await asyncio.sleep(0.3)
    raise AssertionError(
        f"{label}: RTP not bidirectional after {timeout}s — {ua.get_rtp_stats()}\n"
        f"{ua.output[-1500:]}"
    )


def _offers_from_pbx_log(log_text: str) -> list[str]:
    """Extract callee-leg SDP offers emitted by rustpbx-media debug logs."""
    offers = []
    for m in re.finditer(
        r'leg SDP offer created leg=\S+-callee sdp="((?:\\.|[^"\\])*)"',
        log_text,
    ):
        offers.append(m.group(1).encode("utf-8").decode("unicode_escape"))
    return offers


def _assert_plain_rtp_offer(sdp: str, *, expect_video: bool) -> None:
    assert "a=group:BUNDLE" not in sdp, f"BUNDLE in plain-RTP offer (488 risk):\n{sdp}"
    assert "a=rtcp-mux" not in sdp, f"rtcp-mux in plain-RTP offer (silence/488 risk):\n{sdp}"
    assert "m=application" not in sdp, f"m=application in plain-RTP offer (488 risk):\n{sdp}"
    if expect_video:
        assert "m=video" in sdp, f"expected video m-line:\n{sdp}"
        audio_port = next(
            (l.split()[1] for l in sdp.splitlines() if l.startswith("m=audio ")),
            None,
        )
        video_port = next(
            (l.split()[1] for l in sdp.splitlines() if l.startswith("m=video ")),
            None,
        )
        assert (
            audio_port and video_port and audio_port != video_port
        ), f"audio/video must use distinct ports:\n{sdp}"


@pytest.mark.asyncio
async def test_recording_anchor_audio_bidirectional(pbx, sipbot_pool):
    """recording=true forces MediaBridge; plain-RTP audio must still flow both ways."""
    pbx.config_builder.set_recording_force_file()
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.log_level = "debug"
    h.boot_pbx(pbx)

    callee = sipbot_pool.callee(
        host=pbx.host,
        port=h.ua_port(15510),
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
        audio_quality=True,
    )
    await h.wait_registered(callee)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=8,
        audio_quality=True,
        codecs="pcmu",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), (
        f"call failed:\n{caller.output[-1500:]}"
    )

    await _wait_rtp(caller, "caller")
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"caller RTP not bidirectional under recording: {stats}"

    caller_aq = caller.get_audio_quality()
    callee_aq = callee.get_audio_quality()
    if caller_aq and callee_aq:
        assert (
            caller_aq.get("total_frames", 0) > 0 or stats.has_rx
        ), f"caller got no frames: aq={caller_aq} rtp={stats}"
        assert (
            callee_aq.get("total_frames", 0) > 0 or callee.get_rtp_stats().has_rx
        ), f"callee got no frames: aq={callee_aq}"

    log_text = Path(pbx.log_file_path).read_text(encoding="utf-8", errors="replace")
    offers = _offers_from_pbx_log(log_text)
    if offers:
        _assert_plain_rtp_offer(offers[-1], expect_video=False)


@pytest.mark.asyncio
async def test_recording_anchor_video_offer_is_legacy_sip():
    """Durable A/V SDP-shape check for the recording-anchored MediaBridge path.

    sipbot has no video; baresip automation is host-dependent. The unit test
    builds a plain-RTP A/V LegInner offer and rejects BUNDLE/rtcp-mux/shared
    ports — the exact SDP that caused field 488.
    """
    project = Path(__file__).resolve().parents[2]
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(project / "target")
    result = subprocess.run(
        [
            "cargo",
            "test",
            "-p",
            "rustpbx-media",
            "plain_rtp_av_offer_must_be_legacy_sip_compatible",
            "--",
            "--nocapture",
        ],
        cwd=str(project),
        capture_output=True,
        text=True,
        timeout=180,
        env=env,
    )
    assert result.returncode == 0, (
        f"Legacy-SIP A/V offer regression failed:\n"
        f"stdout:\n{result.stdout[-2000:]}\nstderr:\n{result.stderr[-2000:]}"
    )
    assert "plain RTP A/V offer:" in result.stdout
    offer_section = result.stdout.split("plain RTP A/V offer:", 1)[1]
    offer = offer_section.split("test leg::", 1)[0]
    _assert_plain_rtp_offer(offer, expect_video=True)
