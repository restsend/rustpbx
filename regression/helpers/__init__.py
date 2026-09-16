"""Unified RustPBX regression helpers package (single source of truth).

Merged from the former e2e/helpers (audio_verifier, realtime_server,
ws_bridge_echo + wait/worker utils) and src/addons/cc/e2e-regression/helpers
(pbx_server, config_builder, sipbot, RWI/webhook/event tooling). Internal
modules use relative imports only, so this flat package works standalone.

Subpackages:
    helpers.assertions  -- strict assertion toolkit (guards/audio/cdr/event/sip)
    helpers.mocks       -- scriptable mock services (step provider / TTS / Vision / SSO IdP)
Evidence & reporting:
    helpers.evidence    -- per-test evidence store (screenshots/recordings/CDR/events)
    helpers.report_html -- self-contained final HTML report generator
"""

from __future__ import annotations

# --- PBX process / config / client plumbing (ex CC e2e-regression) ---
from .config_builder import ConfigBuilder  # noqa: F401
from .pbx_server import PbxServer, PbxApiClient, find_project_root, pick_free_port  # noqa: F401
from .sipbot import SipBotPool, SipBotProcess, RtpStats  # noqa: F401
from .rwi_client import RwiClient  # noqa: F401
from .event_checker import EventChecker  # noqa: F401
from .webhook_receiver import WebhookServer, WebhookReceiver  # noqa: F401
from .config_reload import apply_config  # noqa: F401

# --- PBX-agnostic media / protocol helpers (ex e2e) ---
from .audio_verifier import (  # noqa: F401
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
from .ws_bridge_echo import WsBridgeEchoServer, WsBridgeCapture  # noqa: F401

# --- session utilities (ex e2e/helpers/__init__) ---


def ua_port(base: int) -> int:
    """Map a test's fixed local UA port to a per-worker shifted port.

    xdist workers run concurrently on the same host; each worker is assigned a
    non-overlapping port range, so the same hardcoded port in two workers never
    collides. RUSTPBX_UA_PORT_OFFSET is derived from RUSTPBX_E2E_PORT_BASE by
    the unified conftest.
    """
    import os

    return base + int(os.environ.get("RUSTPBX_UA_PORT_OFFSET", "0"))


def boot_pbx(pbx, webhook_url: str = ""):
    """Build config from the (already-mutated) builder and start rustpbx.

    Must be called from the test body after customizing `pbx.config_builder`
    so that routes/queues/IVR/addons take effect before boot.
    """
    pbx.prepare(webhook_url=webhook_url, build=False)
    pbx.start(timeout=90)
    return pbx


async def wait_registered(ua, label: str = "UA", timeout: float = 8):
    """Poll until a register=True sipbot UA reports REGISTER success."""
    import asyncio

    ok = await ua.wait_output_async(r"Registered successfully", timeout=timeout)
    if not ok:
        raise AssertionError(f"{label}: UA did not register within {timeout}s — {ua.output[-800:]}")


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
    """Poll until the sipbot UA reports received RTP packets (peer media bridge active)."""
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


async def wait_log(pbx, pattern: str, timeout: float = 12.0, label: str = ""):
    """Poll the PBX log file until *pattern* (regex) appears."""
    import asyncio
    import re
    from pathlib import Path

    log_path = Path(pbx.log_file_path) if pbx.log_file_path else None
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if log_path and log_path.exists():
            try:
                text = log_path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                text = ""
            if re.search(pattern, text):
                return text
        await asyncio.sleep(0.2)
    text = log_path.read_text(encoding="utf-8", errors="replace") if log_path and log_path.exists() else ""
    raise AssertionError(
        f"{label or pattern!r}: pattern not in PBX log within {timeout}s — tail:\n{text[-2000:]}"
    )
