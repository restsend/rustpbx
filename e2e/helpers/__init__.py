"""E2E helpers package.

The generic PBX/sipbot/RWI helpers live in the shared CC e2e-regression helpers
package (single source of truth, uses relative imports). This package re-exports
them by extending the package search path, and adds PBX-agnostic helpers such as
`audio_verifier` (WAV generation + content analysis ported from the removed Rust
`tests/helpers/audio_verifier.rs`).
"""

from __future__ import annotations

from pathlib import Path

# Extend this package's module search path to the shared CC helpers dir so that
# `from .sipbot import ...` / `from .config_builder import ...` (and their
# internal relative imports) resolve against the shared implementation.
_CC_HELPERS = (
    Path(__file__).resolve().parents[2]
    / "src"
    / "addons"
    / "cc"
    / "e2e-regression"
    / "helpers"
)
__path__.append(str(_CC_HELPERS))  # type: ignore[name-defined]

from .sipbot import SipBotPool, SipBotProcess, RtpStats  # noqa: F401,E402
from .pbx_server import PbxServer, PbxApiClient, pick_free_port  # noqa: F401,E402
from .config_builder import ConfigBuilder  # noqa: F401,E402
from .rwi_client import RwiClient  # noqa: F401,E402
from .event_checker import EventChecker  # noqa: F401,E402
from .webhook_receiver import WebhookServer, WebhookReceiver  # noqa: F401,E402
from .ws_bridge_echo import WsBridgeEchoServer, WsBridgeCapture  # noqa: F401,E402

from .audio_verifier import (  # noqa: F401,E402
    generate_sine_wav,
    read_wav_stereo,
    read_wav_mono,
    find_signal_start,
    extract_audio_region,
    compute_rms_db,
    has_audio_content,
    find_dominant_frequency,
    goertzel_magnitude_normalized,
)


def boot_pbx(pbx, webhook_url: str = ""):
    """Build config from the (already-mutated) builder and start rustpbx.

    Must be called from the test body after customizing `pbx.config_builder`
    so that routes/queues/IVR/addons take effect before boot.
    """
    import os

    features = os.environ.get(
        "RUSTPBX_E2E_FEATURES",
        "addon-cc,addon-sbc,addon-voicemail,addon-wholesale",
    ).split(",")
    pbx.prepare(webhook_url=webhook_url, extra_features=features, build=False)
    pbx.start(timeout=90)
    return pbx


async def connect_rwi(rwi):
    """Connect + subscribe an (unconnected) RwiClient after the PBX is up."""
    await rwi.connect()
    await rwi.subscribe(["*"])
    return rwi


async def wait_rtp(ua, label: str = "UA", timeout: float = 20):
    """Poll until the sipbot UA reports any RTP (call-mode UAs report reliably)."""
    import asyncio

    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if ua.get_rtp_stats().has_rx or ua.get_rtp_stats().has_tx:
            return
        await asyncio.sleep(0.3)
    raise AssertionError(f"{label}: no RTP after {timeout}s — {ua.get_rtp_stats()}")


async def wait_rtp_rx(ua, label: str = "UA", timeout: float = 20):
    """Poll until the sipbot UA reports received RTP packets.

    Unlike :func:`wait_rtp`, this proves a *peer* is actually sending media
    toward the UA (RX). Used to verify a media bridge is active between two
    parties — a broken/unactivated bridge leaves RX at 0.
    """
    import asyncio

    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if ua.get_rtp_stats().has_rx:
            return
        await asyncio.sleep(0.3)
    raise AssertionError(f"{label}: no RTP RX after {timeout}s — {ua.get_rtp_stats()}")


async def wait_audio(ua, label: str = "UA", timeout: float = 20):
    """Wait for sipbot's AudioQuality has_audio=true (reliable only for RTP media)."""
    ok = await ua.wait_output_async(r"has_audio=true", timeout=timeout)
    if not ok:
        raise AssertionError(f"{label}: no has_audio=true — {ua.output[-800:]}")
