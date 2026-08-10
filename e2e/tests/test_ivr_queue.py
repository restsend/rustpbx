"""IVR → Queue → Agent + ivr.exec E2E tests.

C1: IVR DTMF → queue transfer → agent answers → audio
C2: Mid-call ivr.exec via SIP INFO (--info-flows)
C3: Queue failure paths (all busy, no answer)
"""

from __future__ import annotations

import asyncio
import json
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.queue, pytest.mark.ivr]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _reg_callee(sipbot_pool, pbx, port, username="1002"):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await asyncio.sleep(2)
    return ua


# ---------------------------------------------------------------------------
# C1: IVR → Queue → Agent answers
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_ivr_to_queue_to_agent(pbx, sipbot_pool, tmp_path):
    """Caller → IVR (greeting + DTMF 1→queue) → queue → agent answers → RTP.

    Full flow: IVR greeting plays → caller sends DTMF 1 → IVR transfers to
    queue "support" → queue holds caller + dials agent → agent answers →
    caller connected to agent with bidirectional RTP.
    """
    from helpers import generate_sine_wav

    greeting = tmp_path / "ivr_greeting.wav"
    generate_sine_wav(greeting, 440.0, 1.5, 8000, 0.4)

    pbx.config_builder.add_ivr("ivr-queue", f'''\
[ivr]
name = "ivr-queue"
ivr_mode = "tree"

[ivr.root]
greeting = "{greeting}"
greeting_text = "Press 1 for support."
timeout_ms = 8000
max_retries = 3
timeout_action = {{ type = "repeat" }}
max_retries_action = {{ type = "hangup" }}

[[ivr.root.entries]]
key = "1"
[ivr.root.entries.action]
type = "queue"
target = "support"
''')
    pbx.config_builder.add_queue(
        "support",
        strategy_mode="sequential",
        targets=[f"sip:1002@127.0.0.1:15410"],
        accept_immediately=True,
        wait_timeout_secs=15,
    )
    pbx.config_builder.add_route(
        "to-ivr-queue",
        match={"to.user": "ivr-queue"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-queue.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    agent = await _reg_callee(sipbot_pool, pbx, 15410, "1002")

    caller = sipbot_pool.caller(
        target=f"sip:ivr-queue@{pbx.sip_addr}", username="1001", password="123456",
        hangup=15, dtmf_flows="3s:1",
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"call not answered:\n{caller.output[-1500:]}"

    # The caller-side `has_rx or has_tx` check is weak: the caller already
    # receives IVR/hold-music RTP and sends DTMF, so it passes even when the
    # caller↔agent media bridge was never activated. Assert the *agent*
    # receives RTP from the caller — that only happens if the bridge is live.
    await h.wait_rtp_rx(agent, "agent", 25)


@pytest.mark.asyncio
async def test_ivr_to_queue_hold_music_during_wait(pbx, sipbot_pool, tmp_path):
    """IVR → queue: caller receives hold music while waiting for agent."""
    from helpers import generate_sine_wav

    greeting = tmp_path / "g.wav"
    generate_sine_wav(greeting, 440.0, 1.0, 8000, 0.4)
    hold = tmp_path / "hold.wav"
    generate_sine_wav(hold, 300.0, 2.0, 8000, 0.5)

    pbx.config_builder.add_ivr("ivr-q2", f'''\
[ivr]
name = "ivr-q2"
ivr_mode = "tree"
[ivr.root]
greeting = "{greeting}"
timeout_ms = 8000
max_retries = 1
max_retries_action = {{ type = "hangup" }}
[[ivr.root.entries]]
key = "1"
[ivr.root.entries.action]
type = "queue"
target = "slowq"
''')
    pbx.config_builder.add_queue(
        "slowq",
        strategy_mode="sequential",
        targets=[f"sip:nobody@127.0.0.1:15420"],
        accept_immediately=True,
        hold_audio=str(hold),
        loop_playback=True,
        wait_timeout_secs=20,
    )
    pbx.config_builder.add_route(
        "to-ivr-q2",
        match={"to.user": "ivr-q2"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-q2.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:ivr-q2@{pbx.sip_addr}", username="1001", password="123456",
        hangup=10, dtmf_flows="2s:1",
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"call not answered:\n{caller.output[-1000:]}"

    await h.wait_rtp(caller, "caller", 20)
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, f"caller RX=0 during queue hold: {stats}"


# ---------------------------------------------------------------------------
# C2: ivr.exec mid-call via SIP INFO (--info-flows)
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_ivr_exec_mid_call(pbx, sipbot_pool):
    """Mid-call ivr.exec via SIP INFO: PBX starts IVR app on active call.

    Caller establishes a call with echo callee, then sends ivr.exec via
    --info-flows. The PBX should accept the ivr.exec and attempt to start
    the referenced IVR route point.
    """
    pbx.config_builder.add_ivr("exec-target", '''\
[ivr]
name = "exec-target"
ivr_mode = "tree"
[ivr.root]
greeting_text = "You are being surveyed."
timeout_ms = 3000
max_retries = 1
max_retries_action = { type = "hangup" }
''')
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)

    callee = await _reg_callee(sipbot_pool, pbx, 15430, "1002")

    ivr_exec_body = json.dumps({
        "action": "ivr.exec",
        "params": {
            "route_point": "exec-target",
            "request_id": "test-req-001",
        },
    })
    info_flows_str = f'2s:application/vnd.rustpbx+json:{ivr_exec_body}'

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=10, info_flows=info_flows_str,
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=20)
    assert answered, f"call not answered:\n{caller.output[-1000:]}"

    await asyncio.sleep(5)

    caller_output = caller.output
    assert "SIP INFO" in caller_output or "INFO flow" in caller_output, (
        f"no SIP INFO log in caller output"
    )

    log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace") if pbx.log_file_path else ""
    assert "ivr.exec" in log, (
        f"rustpbx did not process ivr.exec"
    )
    assert "SIP INFO rustpbx command accepted" in log, (
        f"rustpbx did not accept ivr.exec command"
    )


# ---------------------------------------------------------------------------
# C3: Queue failure paths
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_queue_no_agent_plays_failure_audio(pbx, sipbot_pool):
    """Queue with no reachable agent: caller stays queued, call doesn't crash."""
    pbx.config_builder.add_queue(
        "empty-q",
        strategy_mode="sequential",
        targets=[f"sip:nobody@127.0.0.1:15440"],
        accept_immediately=True,
        wait_timeout_secs=5,
    )
    pbx.config_builder.add_route(
        "to-empty-q",
        match={"to.user": "empty-q"},
        priority=10,
        action="queue",
        queue="empty-q",
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:empty-q@{pbx.sip_addr}", username="1001", password="123456",
        hangup=8,
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"call not answered:\n{caller.output[-1000:]}"

    await h.wait_rtp(caller, "caller", 15)
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, (
        f"caller RX=0 during queue wait (hold music expected): {stats}"
    )


@pytest.mark.asyncio
async def test_ivr_to_queue_no_agent_plays_busy_prompt_before_hangup(pbx, sipbot_pool, tmp_path):
    """IVR → queue transfer (app path) with zero agents: the configured
    busy_prompt must play in full before the call hangs up.

    Regression for the bug where `handle_play` ignored `await_completion`, so
    the busy prompt was cut off the instant it started and the caller heard
    nothing before the 480 hangup. We assert via PBX log timestamps that the
    gap between "Playback started <busy>" and the hangup is on the order of
    the prompt duration (not ~0ms).
    """
    from helpers import generate_sine_wav

    greeting = tmp_path / "g.wav"
    generate_sine_wav(greeting, 440.0, 1.0, 8000, 0.4)
    # ~2.0s busy prompt — long enough that a sub-second gap proves the bug.
    busy = tmp_path / "busy.wav"
    generate_sine_wav(busy, 330.0, 2.0, 8000, 0.5)

    pbx.config_builder.add_ivr("ivr-noagent", f'''\
[ivr]
name = "ivr-noagent"
ivr_mode = "tree"
[ivr.root]
greeting = "{greeting}"
timeout_ms = 8000
max_retries = 1
max_retries_action = {{ type = "hangup" }}
[[ivr.root.entries]]
key = "1"
[ivr.root.entries.action]
type = "queue"
target = "noagent"
''')
    # skill-group:nonexistent resolves to zero agents → app path plays
    # busy_prompt then executes fallback (hangup).
    pbx.config_builder.add_queue(
        "noagent",
        strategy_mode="sequential",
        targets=["skill-group:nonexistent"],
        voice_prompts={"busy_prompt": str(busy)},
    )
    pbx.config_builder.add_route(
        "to-ivr-noagent",
        match={"to.user": "ivr-noagent"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-noagent.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:ivr-noagent@{pbx.sip_addr}", username="1001", password="123456",
        hangup=20, dtmf_flows="2s:1",
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"call not answered:\n{caller.output[-1500:]}"

    # Caller should receive the busy prompt audio before being hung up.
    await h.wait_rtp(caller, "caller", 15)
    # Give the PBX time to play the ~2s prompt and hang up.
    await asyncio.sleep(6)

    log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace") if pbx.log_file_path else ""
    assert "playing busy prompt before fallback" in log, (
        f"expected app-path busy prompt log in PBX log:\n{log[-2000:]}"
    )

    # Parse timestamps for "Playback started ... busy.wav" and the subsequent
    # hangup fallback. With the fix the gap ≈ prompt duration (~2s);
    # without the fix it was ~0ms (prompt cut off by instant hangup).
    import re
    from datetime import datetime

    def _ts(line: str):
        m = re.match(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)\+", line)
        return datetime.fromisoformat(m.group(1)) if m else None

    play_started = next(
        (_ts(l) for l in log.splitlines() if "Playback started" in l and "busy.wav" in l),
        None,
    )
    hangup_log = next(
        (_ts(l) for l in log.splitlines()
         if "hangup fallback" in l.lower() or "play then hangup fallback" in l.lower()),
        None,
    )
    assert play_started and hangup_log, (
        f"could not find playback/hangup-fallback log lines:\n{log[-2000:]}"
    )
    gap = (hangup_log - play_started).total_seconds()
    assert gap >= 1.5, (
        f"busy prompt was cut off before hangup (gap={gap:.3f}s, expected ~2s). "
        f"This means await_completion is not being honored.\n{log[-2000:]}"
    )


@pytest.mark.asyncio
async def test_ivr_to_queue_no_agent_returns_to_ivr(pbx, sipbot_pool, tmp_path):
    """IVR → queue (return_app=ivr) with zero reachable agents: the busy
    prompt plays, then the caller returns to the IVR instead of dead air.

    Regression for the `AlreadyRunning("queue")` bug: when the IVR handed
    control to the queue via AppAction::Transfer, the queue app failed to start
    because the IVR was still registered as the running app on the runtime.
    The caller heard nothing (no busy prompt), and the queue fallback
    (return_to_ivr) never ran. With the fix the queue app starts, plays the
    busy prompt, then transfers back to the IVR so the greeting replays.
    """
    from helpers import generate_sine_wav
    import re
    from datetime import datetime

    greeting = tmp_path / "g.wav"
    generate_sine_wav(greeting, 440.0, 1.5, 8000, 0.4)
    busy = tmp_path / "busy.wav"
    generate_sine_wav(busy, 330.0, 1.5, 8000, 0.5)

    pbx.config_builder.add_ivr("ivr-ret-noagent", f'''\
[ivr]
name = "ivr-ret-noagent"
ivr_mode = "tree"
[ivr.root]
greeting = "{greeting}"
timeout_ms = 8000
max_retries = 1
max_retries_action = {{ type = "hangup" }}
[[ivr.root.entries]]
key = "1"
[ivr.root.entries.action]
type = "queue"
target = "noagent-r"
return_app = "ivr"
return_target = "ivr-ret-noagent"
''')
    # skill-group:nonexistent resolves to zero agents → queue app path plays
    # busy_prompt then executes the return_to_ivr fallback.
    pbx.config_builder.add_queue(
        "noagent-r",
        strategy_mode="sequential",
        targets=["skill-group:nonexistent"],
        accept_immediately=False,
        voice_prompts={"busy_prompt": str(busy)},
    )
    pbx.config_builder.add_route(
        "to-ivr-ret-noagent",
        match={"to.user": "ivr-ret-noagent"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-ret-noagent.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:ivr-ret-noagent@{pbx.sip_addr}", username="1001", password="123456",
        hangup=14, dtmf_flows="2s:1",
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"call not answered:\n{caller.output[-1500:]}"

    # Give the queue app time to start, play the ~1.5s busy prompt, and return
    # the caller to the IVR (which replays the greeting).
    await asyncio.sleep(9)

    log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace") if pbx.log_file_path else ""

    # 1. The queue app must have started (guards the AlreadyRunning dead-air bug).
    assert "playing busy prompt before fallback" in log, (
        f"expected queue app busy prompt; the call may have hit AlreadyRunning dead-air:\n{log[-3000:]}"
    )

    # 2. The queue transfer must be configured to return to the IVR on fallback.
    assert "will return to IVR on fallback" in log, (
        f"expected return-to-IVR override log:\n{log[-3000:]}"
    )

    # 3. The IVR must be restarted AFTER the busy prompt (ordering proves the
    #    queue fallback actually re-entered the IVR rather than dead air).
    def _ts(line: str):
        m = re.match(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)\+", line)
        return datetime.fromisoformat(m.group(1)) if m else None

    busy_ts = next(
        (_ts(l) for l in log.splitlines() if "playing busy prompt before fallback" in l),
        None,
    )
    ivr_restart = next(
        (_ts(l) for l in log.splitlines()
         if "Starting IVR application" in l
         and _ts(l) is not None and busy_ts is not None and _ts(l) > busy_ts),
        None,
    )
    assert busy_ts is not None, (
        f"could not find busy-prompt log line:\n{log[-3000:]}"
    )
    assert ivr_restart is not None, (
        f"IVR must restart after the busy prompt "
        f"(busy={busy_ts}); the call may have sat in dead air.\n{log[-3000:]}"
    )

    # 4. The call must still be alive at this point (not dropped by the queue
    #    fallback) — sipbot hangs up itself at `hangup` seconds.
    assert caller.is_alive, (
        f"caller exited before its own hangup; the call may have been dropped "
        f"after the queue fallback:\n{caller.output[-1500:]}"
    )

    # 5. When the caller eventually hangs up, it must have received audio
    #    (greeting + busy prompt + replayed greeting) → RX > 0.
    caller.wait(timeout=40)
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, (
        f"caller RX=0 (no prompt/greeting audio reached the caller): {stats}"
    )


# ---------------------------------------------------------------------------
# C4: Queue return_to_ivr — agent hangs up → caller returns to IVR
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_ivr_to_queue_agent_hangup_returns_to_ivr(pbx, sipbot_pool, tmp_path):
    """IVR → queue(return_to_ivr) → agent answers → agent hangs up → caller
    returns to the IVR app.

    Verifies the B5 fix: when a connected dynamic-leg (queue agent) hangs up
    and meta.transfer_return_to_ivr is set, the session restarts the IVR app
    instead of hanging up the caller.
    """
    from helpers import generate_sine_wav

    greeting = tmp_path / "g.wav"
    generate_sine_wav(greeting, 440.0, 1.5, 8000, 0.4)

    pbx.config_builder.add_ivr("ivr-return", f'''\
[ivr]
name = "ivr-return"
ivr_mode = "tree"
[ivr.root]
greeting = "{greeting}"
timeout_ms = 8000
max_retries = 1
max_retries_action = {{ type = "hangup" }}
[[ivr.root.entries]]
key = "1"
[ivr.root.entries.action]
type = "queue"
target = "returnq"
return_app = "ivr"
return_target = "ivr-return"
''')
    pbx.config_builder.add_queue(
        "returnq",
        strategy_mode="sequential",
        targets=[f"sip:1002@127.0.0.1:15450"],
        accept_immediately=True,
        wait_timeout_secs=20,
    )
    pbx.config_builder.add_route(
        "to-ivr-return",
        match={"to.user": "ivr-return"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-return.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    # Agent answers briefly then hangs up
    agent = sipbot_pool.callee(
        host=pbx.host, port=15450, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=3,
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:ivr-return@{pbx.sip_addr}", username="1001", password="123456",
        hangup=15, dtmf_flows="2s:1",
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"call not answered:\n{caller.output[-1500:]}"

    # Wait for agent to hang up and IVR to restart
    await asyncio.sleep(8)

    log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace") if pbx.log_file_path else ""
    assert "starting return app" in log or "returning caller to IVR" in log or "B‑leg hung up; returning caller to IVR" in log or "Connected dynamic leg ended" in log, (
        f"expected return-to-app log after agent hangup:\n{log[-3000:]}"
    )


# ---------------------------------------------------------------------------
# C5: Queue in 183 ringback phase — fallback transfer completes from early state
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_queue_early_media_fallback_redirect_completes(pbx, sipbot_pool, tmp_path):
    """Route → queue (accept_immediately=false, trunk ringback) → caller in 183
    → agents unreachable → fallback redirect completes the transfer from the
    183 early-media state.

    Asserts: caller receives 183 before 200 OK, and after fallback the redirect
    target answers (200 OK + RTP).
    """
    from helpers import generate_sine_wav

    pbx.config_builder.set_realms(["127.0.0.1"])
    # Inbound trunk with ringback tone → proactive 183 early media
    pbx.config_builder.add_trunk(
        "ring-trunk", dest="127.0.0.1:15460", direction="inbound",
        inbound_hosts=["127.0.0.1"],
        ringback={"ring": "tone://440,3000"},
    )
    # Queue with unreachable agent + fallback redirect to a reachable echo callee
    pbx.config_builder.add_queue(
        "early-q",
        strategy_mode="sequential",
        targets=[f"sip:nobody@127.0.0.1:19999"],  # nothing listening
        accept_immediately=False,
        wait_timeout_secs=3,
        fallback_redirect=f"sip:1003@127.0.0.1:15470",
    )
    pbx.config_builder.add_route(
        "to-early-q",
        match={"to.user": "early-q"},
        priority=10,
        action="queue",
        queue="early-q",
    )
    h.boot_pbx(pbx)

    # Register the fallback redirect target
    fallback = sipbot_pool.callee(
        host=pbx.host, port=15470, username="1003", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo",
    )
    await asyncio.sleep(2)

    # Trunk-originated caller (From domain differs → classified as Inbound)
    caller = sipbot_pool.caller(
        target=f"sip:early-q@{pbx.sip_addr}", username="external", password="123456",
        from_uri="sip:external@trunk.example.com", hangup=12,
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"call not answered (fallback redirect should complete):\n{caller.output[-2000:]}"

    await h.wait_rtp(caller, "caller", 15)

    # Verify 183 was received during the ringback phase
    _wait_call_ended(caller)
    codes = caller.get_status_counts()
    assert codes.get(183, 0) >= 1, (
        f"expected 183 early media (ringback) during queue wait, got: {codes}\n{caller.output[-2000:]}"
    )
    assert codes.get(200, 0) >= 1, (
        f"expected 200 OK after fallback redirect, got: {codes}\n{caller.output[-2000:]}"
    )


def _wait_call_ended(ua, timeout: float = 30) -> None:
    code = ua.wait(timeout=timeout)
    assert code == 0, f"sipbot exited with {code}:\n{ua.output[-3000:]}"
