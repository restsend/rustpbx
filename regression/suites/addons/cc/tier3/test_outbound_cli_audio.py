"""CLI outbound call — REAL restsend-cli agent dials OUT, audio content asserted.

Closes the coverage gap: outbound (`sip_call`) tests previously stopped at
`connected` and never verified a single audio sample. This test dials OUT
from the production client and asserts audio CONTENT in both directions.
The callee runs in echo mode, so the CLI's 440 Hz tone is the ONLY content
flowing — every hop carries it:

  CLI (--device tone, 440 Hz) ──INVITE──▶ PCMU callee (records the RX mix)
        ▲                                        │
        └──────────── echo (440 Hz) ◀────────────┘

  - CLI→callee: the callee's recording must be dominated by the CLI's
    440 Hz (dominant-frequency + RMS assertions, g729-style).
  - callee→CLI (echo): the CLI's receive-RMS debug windows (`--device
    tone` logs them at debug level) must show the echoed audio actually
    arriving (max rms > 1000 i16), not silence.
"""
from __future__ import annotations

import asyncio
import re

import pytest

import helpers as h
from helpers import (
    compute_rms_db,
    find_dominant_frequency,
    find_signal_start,
    has_audio_content,
    read_wav_mono,
)
from helpers.restsend_agent import RestsendAgent

pytestmark = [pytest.mark.tier3, pytest.mark.media]

CLI_TONE_HZ = 440.0
FREQ_TOL_HZ = 15.0
MIN_RMS_DB = -40.0
SOAK_SECONDS = 16  # ≥3 CLI 5 s RMS windows

AGENT = "1002"
CALLEE = "1001"


@pytest.mark.asyncio
async def test_cli_outbound_audio_bidirectional(
    pbx, webhook_server, sipbot_pool, event_checker, tmp_path
):
    callee_record = tmp_path / "out_callee_rx.wav"

    agent = RestsendAgent(
        pbx, AGENT, local_port=h.ua_port(25240), device="tone", log_level="debug"
    )
    await agent.start()
    callee = None
    try:
        # ── 1. Register the CLI agent (outbound needs no presence publish) ─
        assert await agent.register(expires=300), "CLI agent REGISTER failed"

        # ── 2. The callee: PCMU sipbot, echo mode, records the RX mix ─────
        callee = sipbot_pool.callee(
            host=pbx.host,
            port=h.ua_port(17240),
            username=CALLEE,
            password="123456",
            register=True,
            proxy=f"{pbx.host}:{pbx.sip_port}",
            domain=pbx.host,
            ring_secs=1,
            answer_mode="echo",
            # The mixdown recording is flushed when the bot PROCESS exits,
            # so give it a self-exit timer just past the call (~22 s) and
            # poll for the file below — the g729 scenario's proven pattern.
            hangup_after=45,
            audio_quality=True,
            record_file=str(callee_record),
        )
        await h.wait_registered(callee, f"callee {CALLEE}")

        # ── 3. CLI dials OUT ───────────────────────────────────────────────
        await agent.cmd({"cmd": "sip_call", "remote_uri": f"sip:{CALLEE}@{pbx.sip_addr}"})
        connected = await agent.wait_event(
            "state_changed", predicate=lambda e: e.get("name") == "connected",
            timeout=15,
        )
        assert connected, (
            f"outbound call never connected. CLI stderr tail:\n"
            f"{agent.stderr_text()[-400:]}"
        )
        t_connected = asyncio.get_event_loop().time()

        # ── 4. Media path: anchored transcode bridge must be up ───────────
        await h.wait_log(
            pbx, r"transcoding activated.*a_codec=Opus b_codec=PCMU", 15,
            "Opus<->PCMU bridge (CLI webrtc -> PCMU callee)",
        )

        # ── 5. Soak: ≥3 CLI RMS windows with audio flowing ────────────────
        await asyncio.sleep(SOAK_SECONDS)

        # ── 6. callee→CLI content: the echo of the 440 Hz must ARRIVE ─────
        cli_log = agent.stderr_text()
        plain = re.sub(r"\x1b\[[0-9;]*m", "", cli_log)
        rms_values = [
            int(m) for m in re.findall(r"rx audio rms.*?rms=(\d+)", plain, re.DOTALL)
        ]
        assert rms_values, (
            "CLI logged no rx-audio RMS windows — tone device or debug "
            f"logging missing. log tail: {cli_log[-400:]!r}"
        )
        assert max(rms_values) > 1000, (
            f"CLI received silence (max rx rms={max(rms_values)}) — "
            "callee→CLI echo audio broken on the outbound leg"
        )
        print(f"\n[cli-out] callee→CLI echo rms windows: {rms_values}")

        # ── 7. Webhook chain (A2A: served agent = callee 1001) ─────────────
        await event_checker.webhook.wait_for_event(
            "call_ringing", timeout=20, match={"payload.agent_id": CALLEE},
        )
        answered = await event_checker.webhook.wait_for_event(
            "call_answered", timeout=20, match={"payload.agent_id": CALLEE},
        )
        call_id = answered.call_id
        assert call_id, "call_answered webhook missing call_id"

        # ── 8. Clean teardown: CLI hangs up; wait-mode callee stays alive ─
        # (its --hangup timer keeps the process running — only the CALL ends)
        await agent.hangup()
        hangup = await event_checker.webhook.wait_for_event(
            "call_hangup", timeout=30, call_id=call_id,
        )
        assert hangup is not None, "call_hangup webhook never arrived"

        # ── 9. CLI→callee content: wait for the bot to self-exit (flushing ─
        #      its mixdown recording), then assert the recording IS the ────
        #      CLI's 440 Hz tone ─────────────────────────────────────────────
        rec_deadline = asyncio.get_event_loop().time() + 40
        while (
            not callee_record.parent.glob(callee_record.stem + "*.wav")
            and asyncio.get_event_loop().time() < rec_deadline
        ):
            await asyncio.sleep(0.5)
        siblings = sorted(callee_record.parent.glob(callee_record.stem + "*.wav"))
        assert siblings, (
            f"callee recording missing after bot exit: {callee_record} — "
            "media never flowed"
        )
        callee_record = siblings[-1]  # sipbot suffixes the filename (g729 pattern)
        samples, sr = read_wav_mono(callee_record)
        assert has_audio_content(samples, MIN_RMS_DB), (
            "callee recording silent — CLI→callee audio broken"
        )
        start = find_signal_start(samples)
        region = samples[start:min(start + 5 * sr, samples.size)]
        assert region.size >= sr // 2, "not enough non-silent audio to analyse"
        rms = compute_rms_db(region)
        assert rms >= MIN_RMS_DB, f"callee recording too quiet ({rms:.1f}dB)"
        dom, _mag = find_dominant_frequency(
            region, sr, low=200, high=900, step=5,
        )
        assert abs(dom - CLI_TONE_HZ) <= FREQ_TOL_HZ, (
            f"callee recording dominant {dom:.0f}Hz, expected the CLI's "
            f"{CLI_TONE_HZ:.0f}Hz (±{FREQ_TOL_HZ}) — CLI→callee audio "
            "broken or corrupted"
        )
        print(f"[cli-out] callee recording: 440Hz ok rms={rms:.1f}dB")

        # ── 10. Bidirectional RTP (closing summary carries real totals) ───
        stats = callee.get_rtp_stats()
        assert stats.is_bidirectional, f"callee RTP not bidirectional: {stats}"
        elapsed = asyncio.get_event_loop().time() - t_connected
        print(f"[cli-out] ✓ audio bidirectional clean, connected {elapsed:.0f}s")
    finally:
        await agent.stop()
        if callee is not None and callee.is_alive:
            sipbot_pool.terminate_user(CALLEE)
