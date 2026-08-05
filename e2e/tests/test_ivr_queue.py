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

    await h.wait_rtp(caller, "caller", 25)

    stats = caller.get_rtp_stats()
    assert stats.has_rx or stats.has_tx, f"no RTP after IVR→queue→agent: {stats}"


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
