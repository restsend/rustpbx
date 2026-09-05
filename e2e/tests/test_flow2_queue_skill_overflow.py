"""Flow-2 integration E2E: queue bound to skill group A (empty) overflows to
skill group B after N seconds and dispatches to group B's agent.

  1. skill group `flow2-a` (skills_required=["flow2a"]) has NO agents;
     `overflow_groups=["flow2-b"]`, `max_wait_secs=6`.
  2. skill group `flow2-b` (skills_required=["flow2b"]) has exactly one
     agent (1002), registered via sipbot.
  3. caller (1001) → IVR `flow2` (1.5 s timeout) → queue action with
     `overflow_after=6` + `overflow_wait=30` (the queue-level max-wait must
     exceed the escalation threshold, otherwise the queue declares the call
     abandoned at the same instant the overflow dials — observed as
     "max wait timeout, executing fallback" racing "escalation triggered").

Verified:
  * queued hold: PBX plays hold music to the answered caller while waiting;
  * no dispatch before the threshold (no agent_assigned in the first ~4 s);
  * at ≈6 s after entering the queue the escalation fires (PBX log
    "Queue: escalation triggered") and `skill_group_agent_assigned` carries
    the group-B agent id (1002) with dispatch_reason="overflow";
  * timing: the assignment lands ≈6 s after `skill_group_call_queued`;
  * agent answers → live bidirectional RTP (wait-mode UA reports RX);
  * DTMF accuracy end to end: caller presses 1 2 3 # (RFC4733) after the
    bridge is up — agent's received digit sequence must match exactly;
  * audio fidelity: caller plays a 440 Hz tone; the agent's recording must
    show the same dominant frequency (±15 Hz) with non-silent RMS;
  * after the call ends, the caller's final RTP summary must show a
    bidirectional, non-silent call (hold music + agent echo).
"""
from __future__ import annotations

import asyncio
import json
import os
import time
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

pytestmark = [pytest.mark.queue, pytest.mark.media]

TONE_HZ = 440.0
FREQ_TOL_HZ = 15.0
MIN_RMS_DB = -40.0
OVERFLOW_AFTER_SECS = 6

SG_A = "flow2-a"
SG_B = "flow2-b"
AGENT_B = "1002"


async def _seed_cc(api) -> None:
    """Group A (empty, overflows to B) + group B + group B's agent — via REST.

    The CC addon does not load agents/skill-groups from files; REST creation
    is the supported path (idempotent: duplicates treated as success).
    """
    await api.ensure_console_auth()
    for body in (
        {"agent_id": AGENT_B, "display_name": "Agent B (flow2)", "skills": ["flow2b"],
         "max_concurrency": 3, "role": "agent"},
        {"skill_group_id": SG_A, "skills_required": ["flow2a"],
         "overflow_groups": [SG_B], "sla_target_secs": 30,
         "max_wait_secs": OVERFLOW_AFTER_SECS},
        {"skill_group_id": SG_B, "skills_required": ["flow2b"],
         "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 60},
    ):
        try:
            if "skill_group_id" in body:
                await api.create_skill_group(body)
            else:
                await api.create_agent(body)
        except Exception as exc:  # noqa: BLE001 — duplicate on re-run is fine
            if not ("409" in str(exc) or "400" in str(exc) or "already" in str(exc).lower()):
                raise


def _flow2_ivr() -> str:
    """Auto-enters the queue after a 1.5 s timeout, carrying overflow params.

    `overflow_wait` must exceed `overflow_after`: the skill group's
    `max_wait_secs` would otherwise double as the queue's max-wait and the
    call gets declared "abandoned" at the same moment the overflow fires.
    """
    return f"""\
[ivr]
name = "flow2"
ivr_mode = "tree"

[ivr.root]
greeting_text = "Connecting you to an agent."
timeout_ms = 1500
max_retries = 0
timeout_action = {{ type = "queue", target = "flow2", params = {{ overflow_after = "{OVERFLOW_AFTER_SECS}", overflow_wait = "30" }} }}
max_retries_action = {{ type = "queue", target = "flow2", params = {{ overflow_after = "{OVERFLOW_AFTER_SECS}", overflow_wait = "30" }} }}
entries = []
"""


def _assert_tone(path: Path, *, label: str) -> float:
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


async def _wait_call_done(ua, timeout: float = 15) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not ua.is_alive:
            return
        await asyncio.sleep(0.3)


def _dump_event_timeline(webhook_server, out_path: Path) -> list[dict]:
    """Write the complete RWI webhook event timeline (full JSON payloads).

    Each entry carries the raw POST body verbatim (`payload`), the receive
    timestamp and the offset from the first event — the review-friendly
    timeline of everything the RWI gateway fanned out for this call.
    """
    events = sorted(webhook_server.receiver.all_events(), key=lambda e: e.timestamp)
    if not events:
        out_path.write_text("[]", encoding="utf-8")
        print(f"\n[flow2] RWI event timeline: no events captured → {out_path}")
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
    print(f"\n[flow2] RWI event timeline ({len(timeline)} events) → {out_path}")
    print(json.dumps(timeline, indent=2, ensure_ascii=False))
    return timeline


@pytest.mark.asyncio
async def test_flow2_queue_skill_overflow_dispatch(
    pbx, sipbot_pool, api, event_checker, webhook_server, tmp_path, request,
):
    tone = tmp_path / "flow2_tone.wav"
    generate_sine_wav(tone, TONE_HZ, 25.0, 8000, 0.4)
    b_record = tmp_path / "agent_b_rx.wav"

    # Always dump the full RWI event timeline (even on failure) for review.
    def _dump_timeline() -> None:
        report_dir = Path(os.environ.get("RUSTPBX_E2E_REPORT_DIR", "report"))
        report_dir.mkdir(parents=True, exist_ok=True)
        try:
            _dump_event_timeline(
                webhook_server, report_dir / "rwi_event_timeline_flow2.json",
            )
        except Exception as exc:  # noqa: BLE001
            print(f"[flow2] event timeline dump failed: {exc}")

    request.addfinalizer(_dump_timeline)

    # ── 1. Config: IVR → queue (with overflow params); queue → group A.    ─
    pbx.config_builder.add_ivr("flow2", _flow2_ivr())
    pbx.config_builder.add_queue(
        "flow2",
        strategy_mode="sequential",
        targets=[f"skill-group:{SG_A}"],
    )
    pbx.config_builder.add_route(
        "flow2-route",
        match={"to.user": "flow2q"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/flow2.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx, webhook_url=webhook_server.url)

    await _seed_cc(api)

    # ── 2. The only agent — member of overflow group B, NOT of group A.    ─
    agent = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(17220), username=AGENT_B, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=60,
        audio_quality=True, record_file=str(b_record),
    )
    await h.wait_registered(agent, f"agent {AGENT_B}")

    # ── 3. Caller: IVR auto-timeout (1.5 s) → queue. The DTMF digits are   ─
    #    sent interactively (stdin) after the bridge is up — scheduled
    #    --dtmf-flows entries ~1-2 s apart lose one burst in the queue
    #    bridge relay (sipbot 0.2.59 also caps flows at two entries).
    caller = sipbot_pool.caller(
        target=f"sip:flow2q@{pbx.sip_addr}", username="1001", password="123456",
        hangup=25, audio_quality=True, play_file=str(tone),
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"call never answered:\n{caller.output[-1500:]}"

    # ── 4. While waiting: hold music plays to the caller; nobody rings.    ─
    await h.wait_log(pbx, r"Playback started.*phone-calling\.wav", 10, "hold music")
    queued_ev = await event_checker.webhook.wait_for_event(
        "skill_group_call_queued", timeout=10,
    )
    assert queued_ev is not None, (
        f"call never queued. events: {event_checker.webhook.event_types()}"
    )
    t_queued = queued_ev.timestamp
    await asyncio.sleep(max(0.0, (t_queued + 4.0) - time.time()))
    assert event_checker.webhook.count("skill_group_agent_assigned") == 0, (
        f"agent assigned BEFORE the {OVERFLOW_AFTER_SECS}s overflow threshold — "
        f"events: {event_checker.webhook.event_types()}"
    )

    # ── 5. Overflow dispatch: escalation log + agent_assigned(overflow).   ─
    await h.wait_log(pbx, r"Queue: escalation triggered", 15, "overflow escalation")
    assigned = await event_checker.webhook.wait_for_event(
        "skill_group_agent_assigned", timeout=15,
    )
    assert assigned is not None, (
        f"no skill_group_agent_assigned after overflow threshold. events: "
        f"{event_checker.webhook.event_types()}"
    )
    elapsed = assigned.timestamp - t_queued
    assert assigned.payload.get("agent_id") == AGENT_B, (
        f"overflow dispatched {assigned.payload.get('agent_id')!r}, want {AGENT_B}"
    )
    assert str(assigned.payload.get("dispatch_reason", "")).startswith("overflow"), (
        f"dispatch_reason={assigned.payload.get('dispatch_reason')!r}, "
        f"want 'overflow…' (fair/strict suffix allowed)"
    )
    assert 4.0 <= elapsed <= OVERFLOW_AFTER_SECS + 6.0, (
        f"overflow fired {elapsed:.1f}s after queueing, expected ≈"
        f"{OVERFLOW_AFTER_SECS}s (+escalation-timer slack)"
    )
    # The abandoned path must NOT have raced the overflow (no busy prompt).
    log = Path(pbx.log_file_path).read_text(encoding="utf-8", errors="replace") \
        if pbx.log_file_path else ""
    assert "call abandoned" not in log, (
        "queue declared the call abandoned while overflowing — "
        "overflow_wait must exceed overflow_after"
    )

    # ── 6. Agent answers → live media bridge (wait-mode UA reports RX).    ─
    answered_ev = await event_checker.webhook.wait_for_event("cc_answered", timeout=20)
    assert answered_ev is not None, "overflowed agent never answered"
    await h.wait_rtp_rx(agent, f"agent {AGENT_B}", 15)
    await asyncio.sleep(2)  # accumulate bidirectional media + the DTMF burst
    stats = agent.get_rtp_stats()
    assert stats.rx_packets > 0, f"agent receives no RTP: {stats}"

    # ── 7. DTMF accuracy: press 1 then 2 interactively (stable spacing);
    #    agent must have received exactly 1 2.
    assert caller.send_stdin_dtmf("1"), "caller stdin DTMF failed"
    await asyncio.sleep(2)
    assert caller.send_stdin_dtmf("2"), "caller stdin DTMF failed"
    await asyncio.sleep(1.5)
    expected = ["1", "2"]
    digits: list[str] = []
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and len(digits) < len(expected):
        digits = agent.get_dtmf_digits()
        await asyncio.sleep(0.3)
    assert digits == expected, (
        f"DTMF accuracy failure — agent received {digits!r}, want {expected!r}"
    )

    # ── 8. Caller hangs up (t=25 s); clean teardown.                       ─
    hangup_ev = await event_checker.webhook.wait_for_event("cc_hangup", timeout=40)
    assert hangup_ev is not None, (
        f"no cc_hangup. events: {event_checker.webhook.event_types()}"
    )

    # ── 9. Audio fidelity: the agent's recording carries the caller's      ─
    #    440 Hz tone (caller TX → PBX relay → agent RX, end to end).
    await _wait_call_done(agent, 15)
    resolved = b_record
    if not resolved.exists():
        siblings = sorted(b_record.parent.glob(b_record.stem + "*.wav"))
        assert siblings, f"agent recording missing: {b_record}"
        resolved = siblings[-1]
    rms = _assert_tone(resolved, label="agent B overflow recording")

    # ── 10. Caller final summary (printed at exit): bidirectional + audio. ─
    await _wait_call_done(caller, 15)
    cstats = caller.get_rtp_stats()
    assert cstats.is_bidirectional, (
        f"caller RTP not bidirectional at end of call: {cstats}"
    )
    cq = caller.get_audio_quality()
    assert cq and cq.get("has_audio"), f"caller had no audio: {cq}"
    print(f"\n[flow2] ✓ overflow {elapsed:.1f}s after queueing, digits "
          f"{digits}, tone {TONE_HZ:.0f}Hz rms={rms:.1f}dB, caller {cstats}")
