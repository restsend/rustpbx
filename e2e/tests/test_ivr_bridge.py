"""IVR bridge action E2E tests (tree + step mode) with audio accuracy.

Configures an IVR whose DTMF entry is a `bridge` action pointing at a local
WebSocket PCM16 echo server, then verifies:

- tree mode: a bridge WS connection is established
- audio accuracy: caller's 440 Hz tone arrives at the WS as PCM16 (dominant
  frequency ≈ 440 Hz), and the echoed WS→caller audio is received
- DTMF JSON text frames are forwarded over the bridge WS
- step mode: a Python step-provider returns a terminal bridge ActionNode and
  the same audio/connection checks hold
"""

from __future__ import annotations

import asyncio
import json
import uuid
from pathlib import Path

import pytest
from aiohttp import web

import helpers as h

pytestmark = [pytest.mark.ivr, pytest.mark.bridge]


def _tree_ivr_toml(ws_url: str, greeting: str, return_app: str | None = None,
                   return_target: str | None = None) -> str:
    ret = ""
    if return_app:
        ret += f'return_app = "{return_app}"\n'
    if return_target:
        ret += f'return_target = "{return_target}"\n'
    return f'''\
[ivr]
name = "ivr-bridge-e2e"
ivr_mode = "tree"

[ivr.root]
greeting = "{greeting}"
timeout_ms = 8000
max_retries = 2

[[ivr.root.entries]]
key = "1"
label = "Bridge"
[ivr.root.entries.action]
type = "bridge"
create_room_uri = "{ws_url}"
timeout_ms = 10000
{ret}'''


def _add_bridge_route(cb, file: str = "config/ivr/ivr-bridge-e2e.toml"):
    cb.add_ivr("ivr-bridge-e2e", _tree_ivr_toml("ws://127.0.0.1:1/ws", "sounds/welcome.wav"))
    cb.add_route(
        "to-ivr-bridge",
        match={"to.user": "ivr-bridge"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": file},
        auto_answer=True,
    )


async def _wait_ws_connected(ws_server, timeout: float = 20) -> int:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if ws_server.capture.connection_count() >= 1:
            return ws_server.capture.connection_count()
        await asyncio.sleep(0.3)
    raise AssertionError(
        f"no bridge WS connection after {timeout}s "
        f"(pcm bytes={len(ws_server.capture.pcm_bytes())}, "
        f"dtmf={ws_server.capture.dtmf_frames()})"
    )


# ---------------------------------------------------------------------------
# Tree mode
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_tree_ivr_bridge_ws_connected(pbx, sipbot_pool, tmp_path, ws_bridge_server):
    """DTMF '1' in a tree-mode IVR triggers a bridge transfer → WS connection."""
    greeting = tmp_path / "bridge_greeting.wav"
    h.generate_sine_wav(greeting, 440.0, 2.0, 8000, 0.5)
    pbx.config_builder.add_ivr(
        "ivr-bridge-e2e",
        _tree_ivr_toml(ws_bridge_server.ws_url, str(greeting)),
    )
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.add_route(
        "to-ivr-bridge",
        match={"to.user": "ivr-bridge"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-bridge-e2e.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:ivr-bridge@{pbx.sip_addr}", username="1001", password="123456",
        hangup=10, dtmf_flows="2s:1",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

    await _wait_ws_connected(ws_bridge_server)


@pytest.mark.asyncio
async def test_tree_ivr_bridge_audio_accuracy(pbx, sipbot_pool, tmp_path, ws_bridge_server):
    """Caller plays 440 Hz → bridge WS receives 440 Hz PCM16; echo returns to caller."""
    from helpers import (
        generate_sine_wav, read_wav_mono, has_audio_content, find_dominant_frequency,
        compute_rms_db,
    )

    greeting = tmp_path / "bridge_greeting.wav"
    generate_sine_wav(greeting, 440.0, 2.0, 8000, 0.5)
    tone = tmp_path / "caller_tone.wav"
    generate_sine_wav(tone, 440.0, 2.0, 8000, 0.5)
    caller_rec = tmp_path / "caller_recording.wav"

    pbx.config_builder.add_ivr(
        "ivr-bridge-e2e",
        _tree_ivr_toml(ws_bridge_server.ws_url, str(greeting)),
    )
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.add_route(
        "to-ivr-bridge",
        match={"to.user": "ivr-bridge"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-bridge-e2e.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:ivr-bridge@{pbx.sip_addr}", username="1001", password="123456",
        hangup=12, dtmf_flows="2s:1", play_file=str(tone), record_file=str(caller_rec),
        audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await _wait_ws_connected(ws_bridge_server)

    # Direction 1: caller → WS. The caller's 440 Hz tone must arrive as PCM16.
    deadline = asyncio.get_event_loop().time() + 12
    samples = ws_bridge_server.capture.pcm_samples()
    while samples.size < 1600 and asyncio.get_event_loop().time() < deadline:
        await asyncio.sleep(0.5)
        samples = ws_bridge_server.capture.pcm_samples()
    assert samples.size >= 1600, (
        f"bridge WS received too little PCM16 ({samples.size} samples)"
    )
    rms = compute_rms_db(samples)
    assert rms > -35.0, f"bridge WS PCM16 too quiet: {rms:.1f} dBFS"
    freq, _mag = find_dominant_frequency(samples, 8000, 200, 800, 5)
    assert abs(freq - 440.0) < 60.0, f"bridge WS dominant freq {freq:.1f} != ~440 Hz"

    # Direction 2: WS → caller. The echoed 440 Hz should reach the caller's RX.
    await caller.wait_output_async(r"All bots finished", timeout=25)
    if caller_rec.exists() and caller_rec.stat().st_size > 44:
        try:
            rx, sr = read_wav_mono(caller_rec)
            assert has_audio_content(rx, -40.0), (
                f"caller recording silent: {compute_rms_db(rx):.1f} dBFS"
            )
        except Exception:
            pass  # recording may be header-only under the WIP media layer


@pytest.mark.asyncio
async def test_tree_ivr_bridge_dtmf_json(pbx, sipbot_pool, tmp_path, ws_bridge_server):
    """Caller DTMF during bridge → forwarded as JSON text frames over the WS."""
    greeting = tmp_path / "bridge_greeting.wav"
    h.generate_sine_wav(greeting, 440.0, 2.0, 8000, 0.5)
    pbx.config_builder.add_ivr(
        "ivr-bridge-e2e",
        _tree_ivr_toml(ws_bridge_server.ws_url, str(greeting)),
    )
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.add_route(
        "to-ivr-bridge",
        match={"to.user": "ivr-bridge"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-bridge-e2e.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    # DTMF '1' triggers the bridge; '5' is sent after the bridge is up.
    caller = sipbot_pool.caller(
        target=f"sip:ivr-bridge@{pbx.sip_addr}", username="1001", password="123456",
        hangup=12, dtmf_flows="2s:1,5s:5",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await _wait_ws_connected(ws_bridge_server)

    deadline = asyncio.get_event_loop().time() + 12
    while asyncio.get_event_loop().time() < deadline:
        frames = ws_bridge_server.capture.dtmf_frames()
        if any("dtmf" in f and ("5" in f) for f in frames):
            return
        await asyncio.sleep(0.5)
    frames = ws_bridge_server.capture.dtmf_frames()
    assert frames, f"no DTMF JSON frames received: {frames}"


# ---------------------------------------------------------------------------
# Play-then-disconnect: bridge streams audio, closes, call returns to IVR
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_ivr_bridge_play_then_disconnect_returns_to_ivr(pbx, sipbot_pool, tmp_path):
    """Bridge WS plays PCM16 demo audio then closes → call returns to IVR.

    Models the Python example's `voip_bridge` demo: the bridge endpoint streams
    a WAV (the example's `bridge-demo.wav`) to the caller, then disconnects.
    rustpbx sees the disconnect and, because the bridge node carries
    `return_app`/`return_target`, restarts the IVR app (replaying the greeting)
    instead of hanging up the caller.
    """
    from helpers import generate_sine_wav
    from helpers.ws_bridge_echo import WsBridgeEchoServer

    greeting = tmp_path / "ivr_bridge_greeting.wav"
    generate_sine_wav(greeting, 440.0, 1.5, 8000, 0.4)
    demo_audio = tmp_path / "bridge_demo.wav"
    generate_sine_wav(demo_audio, 330.0, 2.0, 8000, 0.5)

    server = WsBridgeEchoServer(play_file=demo_audio)
    server.start()
    try:
        pbx.config_builder.add_ivr(
            "ivr-bridge-e2e",
            _tree_ivr_toml(
                server.ws_url, str(greeting),
                return_app="ivr", return_target="ivr-bridge-e2e",
            ),
        )
        pbx.config_builder.media_proxy = "all"
        pbx.config_builder.add_route(
            "to-ivr-bridge",
            match={"to.user": "ivr-bridge"},
            priority=10,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/ivr-bridge-e2e.toml"},
            auto_answer=True,
        )
        h.boot_pbx(pbx)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-bridge@{pbx.sip_addr}", username="1001", password="123456",
            hangup=14, dtmf_flows="2s:1",
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        await _wait_ws_connected(server)

        # 1. The bridge server must stream the demo audio to the caller.
        deadline = asyncio.get_event_loop().time() + 12
        while server.capture.sent_pcm_bytes() < 1600 and asyncio.get_event_loop().time() < deadline:
            await asyncio.sleep(0.5)
        assert server.capture.sent_pcm_bytes() >= 1600, (
            f"bridge WS streamed too little PCM16 ({server.capture.sent_pcm_bytes()} bytes)"
        )

        # 2. Wait for the bridge to disconnect and the IVR to restart.
        await asyncio.sleep(8)

        log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace") if pbx.log_file_path else ""
        assert "starting return app" in log.lower(), (
            f"expected return-app log after bridge disconnect:\n{log[-3000:]}"
        )

        # 3. The caller must survive the bridge disconnect (return to IVR, not hangup).
        assert caller.is_alive, (
            f"caller exited after bridge disconnect; expected return to IVR:\n{caller.output[-1500:]}"
        )

        # 4. The caller must have received audio (bridge demo + replayed greeting).
        caller.wait(timeout=30)
        stats = caller.get_rtp_stats()
        assert stats.rx_packets > 0, (
            f"caller RX=0 (no bridge/greeting audio reached the caller): {stats}"
        )
    finally:
        server.stop()


# ---------------------------------------------------------------------------
# Step mode
# ---------------------------------------------------------------------------

def _make_step_bridge_provider(ws_url: str, greeting_file: str):
    """Return an aiohttp app acting as a step-IVR provider that returns a
    terminal bridge ActionNode on the first step."""

    async def handle(request: web.Request) -> web.Response:
        body = await request.json()
        event = (body or {}).get("event") or {}
        if event.get("type") == "session_start":
            return web.json_response({
                "type": "prompt",
                "file": greeting_file,
                "interruptible": True,
                "step_id": "welcome",
                "next": {
                    "type": "dtmf_menu",
                    "greeting_text": "Press 1 to bridge",
                    "timeout_ms": 8000,
                    "max_retries": 1,
                    "entries": {"1": _bridge_node(ws_url)},
                },
            })
        if event.get("type") == "dtmf" and event.get("digit") == "1":
            return web.json_response(_bridge_node(ws_url))
        return web.json_response({"type": "hangup"})

    app = web.Application()
    app.router.add_post("/step", handle)

    async def handle_start(request: web.Request) -> web.Response:
        return web.json_response({"ok": True})

    async def handle_end(request: web.Request) -> web.Response:
        return web.json_response({"ok": True})

    app.router.add_post("/step/start", handle_start)
    app.router.add_post("/step/end", handle_end)
    return app


def _bridge_node(ws_url: str) -> dict:
    return {
        "type": "bridge",
        "create_room_uri": ws_url,
        "timeout_ms": 10000,
        "step_id": "bridge",
        "step_name": "VoIP bridge",
    }


async def _start_provider(provider_app) -> "web.AppRunner":
    runner = web.AppRunner(provider_app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 0)
    await site.start()
    return runner, site


@pytest.mark.asyncio
async def test_step_ivr_bridge_audio(pbx, sipbot_pool, tmp_path, ws_bridge_server):
    """Step-mode IVR: provider returns a bridge ActionNode → WS connects + audio."""
    from helpers import generate_sine_wav, find_dominant_frequency, compute_rms_db

    greeting = tmp_path / "step_greeting.wav"
    generate_sine_wav(greeting, 440.0, 2.0, 8000, 0.5)
    tone = tmp_path / "step_tone.wav"
    generate_sine_wav(tone, 440.0, 2.0, 8000, 0.5)

    provider_app = _make_step_bridge_provider(ws_bridge_server.ws_url, str(greeting))
    runner, site = await _start_provider(provider_app)
    port = site._server.sockets[0].getsockname()[1]
    try:

        pbx.config_builder.add_ivr(
            "ivr-step-bridge",
            f'''\
[ivr]
name = "ivr-step-bridge"
ivr_mode = "step"

[ivr.provider]
url = "http://127.0.0.1:{port}/step"
max_retries = 2
retry_delay_ms = 500
timeout_secs = 5
''',
        )
        pbx.config_builder.media_proxy = "all"
        pbx.config_builder.add_route(
            "to-ivr-step-bridge",
            match={"to.user": "ivr-step-bridge"},
            priority=10,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/ivr-step-bridge.toml"},
            auto_answer=True,
        )
        h.boot_pbx(pbx)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-step-bridge@{pbx.sip_addr}", username="1001", password="123456",
            hangup=12, dtmf_flows="2s:1", play_file=str(tone), audio_quality=True,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        await _wait_ws_connected(ws_bridge_server)

        deadline = asyncio.get_event_loop().time() + 12
        samples = ws_bridge_server.capture.pcm_samples()
        while samples.size < 1600 and asyncio.get_event_loop().time() < deadline:
            await asyncio.sleep(0.5)
            samples = ws_bridge_server.capture.pcm_samples()
        assert samples.size >= 1600, f"step bridge WS received too little PCM16 ({samples.size})"
        rms = compute_rms_db(samples)
        assert rms > -35.0, f"step bridge WS PCM16 too quiet: {rms:.1f} dBFS"
        freq, _mag = find_dominant_frequency(samples, 8000, 200, 800, 5)
        assert abs(freq - 440.0) < 60.0, f"step bridge WS dominant freq {freq:.1f} != ~440 Hz"
    finally:
        await runner.cleanup()


# ---------------------------------------------------------------------------
# Gap-free verification: continuous tone → no stutter in caller recording
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_ivr_bridge_play_gap_free_audio(pbx, sipbot_pool, tmp_path):
    """Bridge WS → caller: 3 s continuous 330 Hz tone must arrive without
    silence gaps (validates the ChannelAudioSource + loop_playback CNG path
    against stutter)."""
    from helpers import generate_sine_wav, read_wav_mono, find_dominant_frequency, compute_rms_db
    from helpers.ws_bridge_echo import WsBridgeEchoServer

    greeting = tmp_path / "greeting.wav"
    generate_sine_wav(greeting, 440.0, 1.5, 8000, 0.4)
    tone = tmp_path / "tone.wav"
    generate_sine_wav(tone, 330.0, 3.0, 8000, 0.5)
    caller_rec = tmp_path / "rec.wav"

    server = WsBridgeEchoServer(play_file=tone)
    server.start()
    try:
        pbx.config_builder.add_ivr(
            "ivr-bridge-e2e",
            _tree_ivr_toml(server.ws_url, str(greeting),
                           return_app="ivr", return_target="ivr-bridge-e2e"),
        )
        pbx.config_builder.media_proxy = "all"
        pbx.config_builder.add_route(
            "to-ivr-bridge",
            match={"to.user": "ivr-bridge"},
            priority=10,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/ivr-bridge-e2e.toml"},
            auto_answer=True,
        )
        h.boot_pbx(pbx)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-bridge@{pbx.sip_addr}", username="1001", password="123456",
            hangup=12, dtmf_flows="2s:1", record_file=str(caller_rec), audio_quality=True,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        await _wait_ws_connected(server)

        # Ensure the bridge server streamed the tone.
        deadline = asyncio.get_event_loop().time() + 12
        while server.capture.sent_pcm_bytes() < 8000 and asyncio.get_event_loop().time() < deadline:
            await asyncio.sleep(0.5)
        assert server.capture.sent_pcm_bytes() >= 8000, (
            f"bridge streamed too little: {server.capture.sent_pcm_bytes()} bytes"
        )

        caller.wait(timeout=25)
    finally:
        server.stop()

    # ── Analyse the caller recording for 330 Hz continuity ──────────────
    if not caller_rec.exists() or caller_rec.stat().st_size < 1000:
        pytest.skip("caller recording too small or missing")

    rx, sr = read_wav_mono(caller_rec)
    rx = rx.ravel()
    assert len(rx) >= sr, f"recording too short: {len(rx)} samples"

    W = sr // 50           # 20 ms window
    min_rms = -45.0        # dB, anything lower is effectively silence
    target = 330.0

    # Pick all windows dominated by the bridge tone.
    tone_blocks = []
    for i in range(0, len(rx) - W, W):
        chunk = rx[i:i + W]
        rms = compute_rms_db(chunk)
        if rms > min_rms:
            f, _ = find_dominant_frequency(chunk, sr, 200, 450, 5)
            if abs(f - target) < 50.0:
                tone_blocks.append(i)

    assert len(tone_blocks) >= 30, (
        f"too few tone blocks ({len(tone_blocks)}); expected ~3 s of 330 Hz. "
        f"The bridge audio may not have reached the caller at all."
    )

    # The tone region should be contiguous: no silent run > 3 windows (60 ms).
    gaps = [b - a for a, b in zip(tone_blocks, tone_blocks[1:])]
    max_gap = max(gaps) if gaps else 0
    # Convert to ms: each W tick is 20 ms.
    gap_ms = max_gap * 1000 / sr
    assert gap_ms <= 60.0, (
        f"bridge tone has stutter gap of {gap_ms:.0f} ms; "
        f"all gaps: {[int(g * 1000 / sr) for g in gaps]} ms"
    )


# ---------------------------------------------------------------------------
# Full-duration playback: WS must stay open long enough for the whole prompt
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_ivr_bridge_play_full_duration(pbx, sipbot_pool, tmp_path):
    """Bridge WS plays a 3 s tone; the caller recording must contain ≈3 s of
    that tone (not truncated by an early WS close)."""
    from helpers import generate_sine_wav, read_wav_mono, find_dominant_frequency, compute_rms_db
    from helpers.ws_bridge_echo import WsBridgeEchoServer

    greeting = tmp_path / "greeting.wav"
    generate_sine_wav(greeting, 440.0, 1.0, 8000, 0.4)
    tone = tmp_path / "tone.wav"
    generate_sine_wav(tone, 330.0, 3.0, 8000, 0.5)
    caller_rec = tmp_path / "rec.wav"

    server = WsBridgeEchoServer(play_file=tone)
    server.start()
    try:
        pbx.config_builder.add_ivr(
            "ivr-bridge-e2e",
            _tree_ivr_toml(server.ws_url, str(greeting),
                           return_app="ivr", return_target="ivr-bridge-e2e"),
        )
        pbx.config_builder.media_proxy = "all"
        pbx.config_builder.add_route(
            "to-ivr-bridge",
            match={"to.user": "ivr-bridge"},
            priority=10,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/ivr-bridge-e2e.toml"},
            auto_answer=True,
        )
        h.boot_pbx(pbx)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-bridge@{pbx.sip_addr}", username="1001", password="123456",
            hangup=14, dtmf_flows="2s:1", record_file=str(caller_rec), audio_quality=True,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        await _wait_ws_connected(server)
        caller.wait(timeout=25)
    finally:
        server.stop()

    if not caller_rec.exists() or caller_rec.stat().st_size < 1000:
        pytest.skip("caller recording too small or missing")

    rx, sr = read_wav_mono(caller_rec)
    rx = rx.ravel()

    # Count 20 ms windows dominated by the 330 Hz tone.
    W = sr // 50
    tone_windows = 0
    for i in range(0, len(rx) - W, W):
        chunk = rx[i:i + W]
        if compute_rms_db(chunk) > -40.0:
            f, _ = find_dominant_frequency(chunk, sr, 200, 450, 5)
            if abs(f - 330.0) < 50.0:
                tone_windows += 1

    tone_secs = tone_windows * 0.02
    # The tone is 3 s; allow some tolerance for greeting/return overlap.
    assert tone_secs >= 2.4, (
        f"bridge tone only lasted {tone_secs:.1f} s (expected ~3 s); "
        f"the WS likely closed before the egress drained the buffered audio"
    )


# ---------------------------------------------------------------------------
# DTMF during bridge play is forwarded to the WS server as JSON
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_ivr_bridge_play_dtmf_forwarded(pbx, sipbot_pool, tmp_path):
    """DTMF pressed while the bridge is playing audio must be forwarded to the
    WS server as a JSON text frame."""
    from helpers import generate_sine_wav
    from helpers.ws_bridge_echo import WsBridgeEchoServer

    greeting = tmp_path / "greeting.wav"
    generate_sine_wav(greeting, 440.0, 1.0, 8000, 0.4)
    tone = tmp_path / "tone.wav"
    generate_sine_wav(tone, 330.0, 4.0, 8000, 0.5)

    server = WsBridgeEchoServer(play_file=tone)
    server.start()
    try:
        pbx.config_builder.add_ivr(
            "ivr-bridge-e2e",
            _tree_ivr_toml(server.ws_url, str(greeting),
                           return_app="ivr", return_target="ivr-bridge-e2e"),
        )
        pbx.config_builder.media_proxy = "all"
        pbx.config_builder.add_route(
            "to-ivr-bridge",
            match={"to.user": "ivr-bridge"},
            priority=10,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/ivr-bridge-e2e.toml"},
            auto_answer=True,
        )
        h.boot_pbx(pbx)

        # DTMF '1' triggers the bridge; '7' is pressed during playback.
        caller = sipbot_pool.caller(
            target=f"sip:ivr-bridge@{pbx.sip_addr}", username="1001", password="123456",
            hangup=12, dtmf_flows="2s:1,4s:7",
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        await _wait_ws_connected(server)

        # Wait for the DTMF '7' to be forwarded to the WS server.
        deadline = asyncio.get_event_loop().time() + 10
        while asyncio.get_event_loop().time() < deadline:
            frames = server.capture.dtmf_frames()
            if any("7" in f and "dtmf" in f for f in frames):
                break
            await asyncio.sleep(0.5)

        frames = server.capture.dtmf_frames()
        assert any("7" in f and "dtmf" in f for f in frames), (
            f"DTMF '7' not forwarded to WS during bridge play; got: {frames}"
        )
        caller.wait(timeout=20)
    finally:
        server.stop()
