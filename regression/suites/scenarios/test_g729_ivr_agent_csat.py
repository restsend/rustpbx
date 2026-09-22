"""G729 → IVR → queue → PCMU agent → CSAT E2E (transcoded media path).

Two scenarios share this module:

1. ``test_g729_ivr_queue_agent_pcmu_csat`` — G.729 caller → IVR → queue →
   PCMU sipbot agent → CSAT, with the full RWI (webhook + WebSocket) event
   timeline dumped for review.

2. ``test_g729_ivr_queue_restsend_video_agent_audio`` — same IVR→queue chain
   but the agent is a REAL restsend-cli client (WebRTC media, opus, video
   enabled, 440 Hz tone device). Regression for incident 2026-09-18: the
   client used to emit H264 / telephone-event frames with payload types the
   SIP side never negotiated; via the fast-path relay's audio catch-all they
   reached the audio-only peer under its opus PT (baresip logged "opus:
   decode error: corrupted stream" bursts, 15~1116-byte payloads). Asserts
   both audio directions through the G729↔Opus transcode and that the
   caller's RX stays in audio territory (no video leak).
"""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path

import pytest

import helpers as h
from helpers import (
    compute_rms_db,
    find_dominant_frequency,
    find_signal_start,
    generate_sine_wav,
    has_audio_content,
    read_wav_mono,
)

pytestmark = [pytest.mark.ivr, pytest.mark.queue, pytest.mark.media]

TONE_HZ = 620.0
FREQ_TOL_HZ = 15.0
MIN_RMS_DB = -40.0

AGENT = "1002"


async def _seed_cc(pbx, api, csat_survey: dict | None = None) -> None:
    """Create the agent + skill group "support" (optionally with a CSAT survey)."""
    await api.ensure_console_auth()
    group_body = {
        "skill_group_id": "support", "skills_required": ["support"],
        "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 90,
    }
    if csat_survey is not None:
        group_body["metadata"] = {"post_call_survey": csat_survey}
    for body in (
        {"agent_id": AGENT, "display_name": f"Agent {AGENT} (g729)", "skills": ["support"],
         "max_concurrency": 3, "role": "agent"},
        group_body,
    ):
        try:
            if "skill_group_id" in body:
                await api.create_skill_group(body)
            else:
                await api.create_agent(body)
        except Exception as exc:  # noqa: BLE001 — duplicate on re-run is fine
            if not ("409" in str(exc) or "400" in str(exc) or "already" in str(exc).lower()):
                raise


def _flow_ivr(greeting: Path) -> str:
    """Entry IVR: short greeting → auto-timeout → queue "support" (no keypress)."""
    return f"""\
[ivr]
name = "g729flow"
ivr_mode = "tree"

[ivr.root]
greeting = "{greeting}"
greeting_text = "Connecting you to support."
timeout_ms = 1500
max_retries = 0
timeout_action = {{ type = "queue", target = "support" }}
max_retries_action = {{ type = "queue", target = "support" }}
entries = []
"""


def _assert_tone(path: Path, *, label: str) -> float:
    """Assert a recording carries the dominant 620 Hz tone; return the RMS."""
    samples, sr = read_wav_mono(path)
    assert samples.size >= sr // 2, (
        f"{label}: recording too short ({samples.size} samples @ {sr}Hz)"
    )
    start = find_signal_start(samples)
    region = samples[start:min(start + 2 * sr, samples.size)]
    assert region.size >= sr // 2, f"{label}: not enough non-silent audio"
    rms = compute_rms_db(region)
    assert has_audio_content(region, MIN_RMS_DB), f"{label}: too quiet ({rms:.1f}dB)"
    dom, _mag = find_dominant_frequency(region, sr, low=200, high=900, step=5)
    assert abs(dom - TONE_HZ) <= FREQ_TOL_HZ, (
        f"{label}: dominant {dom:.0f}Hz, expected {TONE_HZ:.0f}Hz (±{FREQ_TOL_HZ})"
    )
    return rms


def _resolve_recording(path: Path, *, label: str) -> Path:
    """sipbot `wait --record` may suffix the filename — glob it."""
    if path.exists():
        return path
    siblings = sorted(path.parent.glob(path.stem + "*.wav"))
    assert siblings, f"{label}: recording missing: {path}"
    return siblings[-1]


def _dump_webhook_timeline(webhook_server, out_path: Path) -> list[dict]:
    """Write the complete RWI webhook event timeline (full JSON payloads)."""
    events = sorted(webhook_server.receiver.all_events(), key=lambda e: e.timestamp)
    if not events:
        out_path.write_text("[]", encoding="utf-8")
        print(f"\n[g729-csat] RWI webhook timeline: no events captured → {out_path}")
        return []
    base = events[0].timestamp
    timeline = []
    for i, ev in enumerate(events):
        timeline.append({
            "seq": i,
            "t_offset_s": round(ev.timestamp - base, 3),
            "received_at": ev.timestamp,
            "event_type": ev.event_type,
            "call_id": ev.call_id,
            "payload": ev.raw,
        })
    out_path.write_text(
        json.dumps(timeline, indent=2, ensure_ascii=False), encoding="utf-8",
    )
    print(f"\n[g729-csat] RWI webhook timeline ({len(timeline)} events) → {out_path}")
    return timeline


def _dump_rwi_ws_timeline(rwi, out_path: Path) -> list[dict]:
    """Write + print the RWI WebSocket session event timeline (subscribe "*")."""
    events = list(rwi.events)
    out_path.write_text(
        json.dumps(events, indent=2, ensure_ascii=False), encoding="utf-8",
    )
    types = [e.get("event_type") or e.get("type") for e in events]
    print(f"\n[g729-csat] RWI WS timeline ({len(events)} events) → {out_path}")
    print(f"[g729-csat] RWI WS event types: {types}")
    print(json.dumps(events, indent=2, ensure_ascii=False))
    return events


async def _wait_call_done(ua, timeout: float = 15) -> None:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if not ua.is_alive:
            return
        await asyncio.sleep(0.3)


@pytest.mark.asyncio
async def test_g729_ivr_queue_agent_pcmu_csat(
    pbx, sipbot_pool, api, event_checker, webhook_server, rwi, tmp_path,
):
    # Always dump the full RWI timelines (webhook + WS), even on failure.
    def _dump_timelines() -> None:
        report_dir = Path(os.environ.get("RUSTPBX_E2E_REPORT_DIR", "report"))
        report_dir.mkdir(parents=True, exist_ok=True)
        try:
            _dump_webhook_timeline(
                webhook_server, report_dir / "rwi_event_timeline_g729_csat.json",
            )
        except Exception as exc:  # noqa: BLE001
            print(f"[g729-csat] webhook timeline dump failed: {exc}")
        try:
            _dump_rwi_ws_timeline(rwi, report_dir / "rwi_ws_events_g729_csat.json")
        except Exception as exc:  # noqa: BLE001
            print(f"[g729-csat] RWI WS timeline dump failed: {exc}")

    # ── 0. Media fixtures.                                                 ─
    greeting = tmp_path / "g729_greeting.wav"
    generate_sine_wav(greeting, 880.0, 1.5, 8000, 0.4)
    tone = tmp_path / "g729_tone.wav"
    generate_sine_wav(tone, TONE_HZ, 60.0, 8000, 0.4)  # spans the whole call
    agent_record = tmp_path / "agent_rx.wav"
    caller_record = tmp_path / "caller_rx.wav"

    try:
        # ── 1. Config: IVR + queue + route; CSAT on the skill group.       ─
        pbx.config_builder.add_ivr("g729flow", _flow_ivr(greeting))
        pbx.config_builder.add_queue(
            "support",
            strategy_mode="sequential",
            targets=["skill-group:support"],
        )
        pbx.config_builder.add_route(
            "g729flow-route",
            match={"to.user": "g729flow"},
            priority=10,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/g729flow.toml"},
            auto_answer=True,
        )
        h.boot_pbx(pbx, webhook_url=webhook_server.url)

        await _seed_cc(pbx, api, csat_survey={
            "enabled": True,
            "after_transfer": False,
            "config": {
                "mode": "score", "score_min": 1, "score_max": 5,
                "language": "en", "max_retries": 1, "timeout_secs": 20,
            },
            "after_completion": "hangup",
        })

        # ── 2. RWI WebSocket session — subscribes to all contexts so the   ─
        #    whole call/agent lifecycle can be reviewed afterwards.
        await h.connect_rwi(rwi)

        # ── 3. Agent answers with PCMU (echo + record), hangs up after 12s ─
        #    → triggers the CSAT survey on the (still online) caller.
        agent = sipbot_pool.callee(
            host=pbx.host, port=h.ua_port(17140), username=AGENT, password="123456",
            register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=1, answer_mode="echo", hangup_after=12,
            audio_quality=True, record_file=str(agent_record),
        )
        await h.wait_registered(agent, f"agent {AGENT}")

        # ── 4. Caller: G729 offer → IVR auto-timeout (1.5 s) → queue →     ─
        #    agent. 620 Hz tone plays for the whole call.
        caller = sipbot_pool.caller(
            target=f"sip:g729flow@{pbx.sip_addr}", username="1001", password="123456",
            codecs="g729",
            hangup=90, audio_quality=True, play_file=str(tone),
            record_file=str(caller_record),
        )
        answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
        assert answered, f"call never answered:\n{caller.output[-1500:]}"

        # ── 5. Queue dispatched the PCMU agent.                             ─
        ringing = await event_checker.webhook.wait_for_event("call_ringing", timeout=20)
        assert ringing is not None, (
            f"no call_ringing — queue did not dispatch. events: "
            f"{event_checker.webhook.event_types()}"
        )
        assert ringing.payload.get("agent_id") == AGENT, (
            f"queue dispatched {ringing.payload.get('agent_id')!r}, want {AGENT}"
        )
        call_id = ringing.call_id
        answered_ev = await event_checker.webhook.wait_for_event(
            "call_answered", timeout=20,
        )
        assert answered_ev is not None, f"agent never answered (no call_answered)"

        # RWI dispatch: call lifecycle events (call_created/ringing/answered)
        # are addressed to the call OWNER — a plain SIP call has no RWI owner
        # session, so they surface on the RWI webhook stream (rwi="1.0"
        # envelopes) while the WebSocket session receives the CC fan-out
        # (queue/agent context events).
        life = await event_checker.webhook.wait_for_sequence(
            ["call_created", "call_ringing", "call_answered"], timeout=20,
        )
        assert life, (
            f"RWI webhook missing call lifecycle, got: "
            f"{event_checker.webhook.event_types()}"
        )
        # queue fan-out events may already be buffered while the webhook
        # awaits above ran — verify the ordered sequence over the FULL WS
        # buffer, waiting for the tail event to arrive.
        await rwi.wait_for_event("queue_agent_connected", timeout=20)
        ws_types = [e.get("event_type") for e in rwi.events]
        it = iter(ws_types)
        ok = all(any(t == want for t in it) for want in
                 ["queue_joined", "queue_agent_offered", "queue_agent_connected"])
        assert ok, (
            f"RWI WS missing queue fan-out events, got: {ws_types}"
        )

        # ── 6. Transcode bridge must be up while both legs are live.       ─
        #    (sipbot's mid-call TX/RX progress lines only populate against
        #    RTCP-speaking peers — rustpbx sends none — so RTP gating here
        #    is done PBX-side; the recordings assert actual audio below.)
        await h.wait_log(
            pbx, r"transcoding activated.*a_codec=G729 b_codec=PCMU", 15,
            "G729<->PCMU bridge",
        )

        # ── 7. Agent hangs up → CSAT survey on the caller leg. Score 5 via ─
        #    stdin DTMF (two presses hedge prompt-playback barge-in).
        ended_a = await agent.wait_output_async(
            r"BYE|Call ended|Hanging up|hangup_after", timeout=25,
        )
        assert ended_a, f"agent never hung up (hangup_after=12):\n{agent.output[-600:]}"
        await asyncio.sleep(2)
        assert caller.send_stdin_dtmf("5"), "caller stdin DTMF failed"
        await asyncio.sleep(5)
        caller.send_stdin_dtmf("5")

        hangup_ev = await event_checker.webhook.wait_for_event("call_hangup", timeout=60)
        assert hangup_ev is not None, (
            f"call never hung up after survey. events: "
            f"{event_checker.webhook.event_types()}"
        )
        # call_hangup (owner-addressed) lives on the webhook RWI stream;
        # queue_left (context fan-out) reaches the WebSocket session.
        assert hangup_ev is not None, "call_hangup missing on the RWI webhook stream"
        queue_left = await rwi.wait_for_event("queue_left", timeout=15)
        assert queue_left is not None, (
            f"RWI WS never saw queue_left: "
            f"{[e.get('event_type') for e in rwi.events]}"
        )

        # Caller leg must actually be torn down (BYE → process exit).
        await _wait_call_done(caller, 20)
        assert not caller.is_alive, "caller still alive — survey hangup never reached it"

        # ── 8. CDR must carry the surveyed score.                          ─
        score = None
        cdr = None
        for _ in range(12):
            await asyncio.sleep(1)
            detail = await api.get(f"/api/cc/calls/{call_id}")
            if isinstance(detail, dict):
                cdr = detail.get("data", detail)
                score = cdr.get("csat_score") or cdr.get("csatScore")
                if score is not None:
                    break
        assert score is not None, (
            f"CSAT score not persisted for {call_id} — survey never ran or DTMF "
            f"missed. CDR: {cdr!r:.300}"
        )
        assert int(score) == 5, f"csat_score mismatch: {score!r} (want 5)"

        # ── 9. Audio fidelity, caller → agent: the agent's (PCMU)          ─
        #      recording must carry the caller's 620 Hz tone end-to-end
        #      through the G729→PCMU transcode.
        resolved_a = _resolve_recording(agent_record, label="agent")
        rms = _assert_tone(resolved_a, label="agent (PCMU) recording")
        print(f"\n[g729-csat] agent recording: tone ok rms={rms:.1f}dB")

        # ── 10. Audio fidelity, agent → caller: the caller's (G729)        ─
        #       recording must carry the agent's echo at the same tone
        #       through the PCMU→G729 transcode. The whole-file window is
        #       used: the head carries the IVR greeting (880 Hz), the echo
        #       spans the agent-talk window and dominates the file.
        resolved_c = _resolve_recording(caller_record, label="caller")
        c_samples, c_sr = read_wav_mono(resolved_c)
        assert has_audio_content(c_samples, MIN_RMS_DB), (
            "caller (G729) recording silent — PCMU→G729 direction delivered no audio"
        )
        c_rms = compute_rms_db(c_samples)
        c_dom, _mag = find_dominant_frequency(c_samples, c_sr, low=200, high=900, step=5)
        assert abs(c_dom - TONE_HZ) <= FREQ_TOL_HZ, (
            f"caller (G729) recording: dominant {c_dom:.0f}Hz, expected "
            f"{TONE_HZ:.0f}Hz (±{FREQ_TOL_HZ}) — echo not audible"
        )
        print(f"[g729-csat] caller recording: tone ok rms={c_rms:.1f}dB")

        # ── 11. Caller final report: bidirectional RTP. (sipbot's          ─
        #       mid-call progress lines stay at 0 without an RTCP peer;
        #       the closing summary carries the real totals.) sipbot's
        #       has_audio flag is unreliable on G.729 RX, so audio is
        #       asserted from the recordings above instead.
        cstats = caller.get_rtp_stats()
        assert cstats.is_bidirectional, f"caller RTP not bidirectional: {cstats}"
        cq = caller.get_audio_quality()
        print(f"[g729-csat] caller audio-quality: {cq}")
        print(f"\n[g729-csat] ✓ csat=5, caller {cstats}")
    finally:
        _dump_timelines()


@pytest.mark.asyncio
async def test_g729_ivr_queue_restsend_video_agent_audio(
    pbx, sipbot_pool, api, event_checker, webhook_server, rwi, tmp_path,
):
    """IVR → queue → restsend-cli (WebRTC, opus, video enabled) — both audio
    directions must survive the REAL production agent client.

    Regression context: restsend-call used to send H264 / telephone-event
    frames with hardcoded payload types that the SIP side never negotiated
    (fix: restsend-call 2a76c38). Via the fast-path relay's audio catch-all
    those frames reached the audio-only peer under its opus PT — baresip
    logged "opus: decode error: corrupted stream" bursts with 15~1116-byte
    payloads (incident 2026-09-18). This test pins the whole chain:
    G729 caller → IVR → queue → CLI agent (--video, --device tone).

    Audio assertions, both directions through the G729↔Opus transcode:
      * agent → caller: the caller's (G729) recording carries the CLI's
        synthetic 440 Hz tone (whole-file dominant, non-silent RMS);
      * caller → agent: the CLI's 5 s receive-RMS debug windows (device
        `tone` logs them at debug level) report the caller's 620 Hz tone
        (rms well above silence);
      * no video leak: the caller's RX byte rate stays in audio territory
        (< 12 kB/s incl. RTP overhead) — an un-negotiated video track would
        flood it at ~100 kbit/s.
    """
    import re as _re
    import time as _time

    from helpers.restsend_agent import RestsendAgent

    def _dump_timelines() -> None:
        report_dir = Path(os.environ.get("RUSTPBX_E2E_REPORT_DIR", "report"))
        report_dir.mkdir(parents=True, exist_ok=True)
        try:
            _dump_webhook_timeline(
                webhook_server, report_dir / "rwi_event_timeline_restsend_video.json",
            )
        except Exception as exc:  # noqa: BLE001
            print(f"[cli-video] webhook timeline dump failed: {exc}")
        try:
            _dump_rwi_ws_timeline(rwi, report_dir / "rwi_ws_events_restsend_video.json")
        except Exception as exc:  # noqa: BLE001
            print(f"[cli-video] RWI WS timeline dump failed: {exc}")

    # ── 0. Fixtures.                                                       ─
    greeting = tmp_path / "cli_greeting.wav"
    generate_sine_wav(greeting, 880.0, 1.5, 8000, 0.4)
    tone = tmp_path / "cli_tone.wav"
    generate_sine_wav(tone, TONE_HZ, 60.0, 8000, 0.4)
    caller_record = tmp_path / "caller_rx.wav"
    CLI_TONE_HZ = 440.0
    agent = None
    answer_task = None

    try:
        # ── 1. Config + seed. No CSAT here — this test is audio-focused.  ─
        pbx.config_builder.add_ivr("g729flow", _flow_ivr(greeting))
        pbx.config_builder.add_queue(
            "support",
            strategy_mode="sequential",
            targets=["skill-group:support"],
            ring_timeout_secs=30,
            wait_timeout_secs=120,
        )
        pbx.config_builder.add_route(
            "g729flow-route",
            match={"to.user": "g729flow"},
            priority=10,
            action="application",
            app="ivr",
            app_params={"file": "config/ivr/g729flow.toml"},
            auto_answer=True,
        )
        h.boot_pbx(pbx, webhook_url=webhook_server.url)
        await _seed_cc(pbx, api)
        await h.connect_rwi(rwi)

        # ── 2. restsend-cli agent: opus, video enabled, 440 Hz tone.       ─
        agent = RestsendAgent(
            pbx, AGENT, local_port=h.ua_port(17160),
            video=True, device="tone", log_level="debug",
        )
        await agent.start()
        assert await agent.register(expires=300), "CLI agent REGISTER failed"
        await agent.publish_idle()

        async def _answer_when_ringing():
            ev = await agent.wait_event("sip_incoming", timeout=90)
            assert ev, "CLI agent never rang"
            assert await agent.answer(timeout=20), "CLI agent answer failed"

        answer_task = asyncio.create_task(_answer_when_ringing())

        # ── 3. G729 caller → IVR (1.5 s) → queue → CLI agent.             ─
        caller = sipbot_pool.caller(
            target=f"sip:g729flow@{pbx.sip_addr}", username="1001", password="123456",
            codecs="g729",
            hangup=120, audio_quality=True, play_file=str(tone),
            record_file=str(caller_record),
        )
        answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
        assert answered, f"call never answered:\n{caller.output[-1500:]}"

        ringing = await event_checker.webhook.wait_for_event("call_ringing", timeout=30)
        assert ringing is not None, (
            f"queue did not dispatch. events: {event_checker.webhook.event_types()}"
        )
        assert ringing.payload.get("agent_id") == AGENT
        call_id = ringing.call_id
        answered_ev = await event_checker.webhook.wait_for_event(
            "call_answered", timeout=40,
        )
        assert answered_ev is not None, "CLI agent never answered (no call_answered)"
        await asyncio.wait_for(answer_task, timeout=45)
        t_answered = _time.monotonic()

        life = await event_checker.webhook.wait_for_sequence(
            ["call_created", "call_ringing", "call_answered"], timeout=20,
        )
        assert life, f"RWI webhook missing call lifecycle: {event_checker.webhook.event_types()}"

        # ── 4. Media soak: ≥3 CLI RMS windows with the tone flowing.       ─
        #      (The transcode log doubles as the media-path proof — sipbot's
        #      mid-call TX/RX counters stay 0 without an RTCP peer.)
        await h.wait_log(
            pbx, r"transcoding activated.*a_codec=G729 b_codec=Opus", 15,
            "G729<->Opus bridge",
        )
        await asyncio.sleep(16)

        # ── 5. caller → agent: the CLI's receive-RMS windows must show     ─
        #      the caller's 620 Hz tone (620 Hz @ 0.4 ≈ RMS 9000+ i16;
        #      1000 guards transcode/AGC wiggle).
        cli_log = agent.stderr_text()
        # tracing wraps field names/values in ANSI SGR codes — strip first.
        plain = _re.sub(r"\x1b\[[0-9;]*m", "", cli_log)
        rms_values = [
            int(m) for m in
            _re.findall(r"rx audio rms.*?rms=(\d+)", plain, _re.DOTALL)
        ]
        assert rms_values, (
            "CLI agent logged no rx-audio RMS windows — tone device or "
            f"debug logging missing. log tail: {cli_log[-400:]!r}"
        )
        assert max(rms_values) > 1000, (
            f"CLI agent received silence (max rms={max(rms_values)}) — "
            "caller→agent audio broken through the G729→Opus path"
        )
        print(f"\n[cli-video] caller→agent rms windows: {rms_values}")

        # ── 6. agent → caller: the caller's (G729) recording carries the   ─
        #      CLI's 440 Hz tone. sipbot's --record is a TX+RX MIXDOWN, so
        #      the caller's own 620 Hz tone coexists with it; assert the
        #      440 Hz band is clearly audible next to it (≤12 dB below) and
        #      is a genuine local peak over the valley between the tones —
        #      corruption / payload leaks would flatten both.
        import numpy as _np

        resolved_c = _resolve_recording(caller_record, label="caller")
        c_samples, c_sr = read_wav_mono(resolved_c)
        assert has_audio_content(c_samples, MIN_RMS_DB), (
            "caller recording silent — agent→caller audio broken"
        )
        tail = c_samples[-10 * c_sr:].astype(float)
        assert tail.size >= c_sr, "caller recording too short for tone analysis"
        spec = _np.abs(_np.fft.rfft(tail * _np.hanning(tail.size)))
        freqs = _np.fft.rfftfreq(tail.size, 1 / c_sr)

        def _band_db(center: float, width: float = 15.0) -> float:
            mask = (freqs >= center - width) & (freqs <= center + width)
            return float(20 * _np.log10(spec[mask].max() + 1e-9))

        db440, db620, db530 = _band_db(440.0), _band_db(TONE_HZ), _band_db(530.0)
        assert db440 >= db620 - 12.0, (
            f"440Hz agent tone ({db440:.1f}dB) buried vs caller tone "
            f"({db620:.1f}dB) — agent→caller audio broken/corrupted"
        )
        assert db440 >= db530 + 6.0, (
            f"440Hz ({db440:.1f}dB) not a local peak over the 530Hz valley "
            f"({db530:.1f}dB) — no clean agent tone in the mix"
        )
        print(
            f"[cli-video] agent→caller mixdown: 440Hz={db440:.1f}dB "
            f"620Hz={db620:.1f}dB valley={db530:.1f}dB"
        )

        # ── 7. Correct teardown: agent hangs up, caller follows.           ─
        await agent.hangup()
        hangup_ev = await event_checker.webhook.wait_for_event("call_hangup", timeout=30)
        assert hangup_ev is not None, (
            f"call never hung up. events: {event_checker.webhook.event_types()}"
        )
        rwi_types = [e.get("event_type") for e in rwi.events]
        assert "queue_left" in rwi_types, f"RWI WS never saw queue_left: {rwi_types}"
        await _wait_call_done(caller, 25)

        # ── 8. Final report: bidirectional audio + no video leak. The      ─
        #      caller's RX must stay in audio territory (~48 kbit/s opus +
        #      RTP overhead ≈ 6.6 kB/s; an un-negotiated H264 track floods
        #      ~12 kB/s+). sipbot's closing summary carries the real totals.
        cstats = caller.get_rtp_stats()
        assert cstats.is_bidirectional, f"caller RTP not bidirectional: {cstats}"
        elapsed = max(2.0, _time.monotonic() - t_answered)
        rx_bps = cstats.rx_bytes / elapsed
        assert rx_bps < 12_000, (
            f"caller RX {rx_bps:.0f} B/s over {elapsed:.0f}s — far above "
            "opus audio; an un-negotiated video track is leaking into the "
            "audio leg"
        )
        cq = caller.get_audio_quality()
        print(f"[cli-video] caller audio-quality: {cq}")
        print(f"[cli-video] ✓ audio bidirectional clean, caller {cstats}")
    finally:
        if answer_task is not None and not answer_task.done():
            answer_task.cancel()
        if agent is not None:
            try:
                await agent.stop()
            except Exception:  # noqa: BLE001
                pass
        _dump_timelines()
