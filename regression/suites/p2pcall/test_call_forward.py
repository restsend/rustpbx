"""Call forwarding E2E — per-user forwarding config over real SIP.

Covers the three SipUser::forwarding_config modes:
  * always        → 1002's calls land on 1003 immediately (1002 never rings)
  * when_busy     → 1002 busy (in a call) → forwarded to 1003;
                    1002 idle → call lands on 1002 as usual
  * no_answer     → 1002 doesn't answer → after the timeout the call is
                    forwarded to 1003
"""

from __future__ import annotations

import asyncio

import pytest

import helpers as h


async def _wait_call_done(ua, timeout: float = 25) -> None:
    """sipbot `call` mode only prints RTP counters in the final summary, so
    wait for the process to exit before asserting on stats."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if not ua.is_alive:
            return
        await asyncio.sleep(0.3)

pytestmark = [pytest.mark.p2p]


async def _reg(sipbot_pool, pbx, port: int, username: str, **kw):
    ring_secs = kw.pop("ring_secs", 1)
    answer_mode = kw.pop("answer_mode", "echo")
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=ring_secs, answer_mode=answer_mode, **kw,
    )
    await h.wait_registered(ua)
    return ua


async def _dial(pbx, sipbot_pool, hangup=8, **kw):
    return sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=hangup, **kw,
    )


@pytest.mark.asyncio
async def test_forward_always(pbx, sipbot_pool):
    """always: the call to 1002 must land on 1003; 1002 must NOT ring."""
    pbx.config_builder.set_user_forwarding("1002", "always", "1003")
    h.boot_pbx(pbx)

    fwd_target = await _reg(sipbot_pool, pbx, h.ua_port(15506), "1003")
    # 1002 exists but must never receive the INVITE.
    b2 = await _reg(sipbot_pool, pbx, h.ua_port(15507), "1002")

    caller = await _dial(pbx, sipbot_pool)
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), (
        caller.output
    )
    # The forwarding target answered and media flows caller↔1003.
    await h.wait_rtp_rx(fwd_target, "forwarded-to 1003", timeout=15)
    await _wait_call_done(caller)
    assert caller.get_rtp_stats().is_bidirectional, caller.get_rtp_stats()
    # 1002 (never answering echo UA) must not have been rung: its sipbot
    # reports no incoming call.
    await asyncio.sleep(2)
    assert "Handling INVITE" not in b2.output and "Call task started" not in b2.output, (
        f"1002 was rung despite always-forward: {b2.output[-800:]}"
    )


@pytest.mark.asyncio
@pytest.mark.xfail(
    reason="when_busy forwarding is parsed into dialplan.call_forwarding but no "
           "consumer exists in SipSession — only 'always' mode is implemented "
           "(src/proxy/call.rs:605). Documents the product gap.",
    strict=True,
)
async def test_forward_when_busy(pbx, sipbot_pool):
    """when_busy: 1002 rejects with 486 → the call is forwarded to 1003."""
    pbx.config_builder.set_user_forwarding("1002", "when_busy", "1003")
    h.boot_pbx(pbx)

    # 1002 is "busy": rejects every incoming call with 486.
    b = await _reg(sipbot_pool, pbx, h.ua_port(15508), "1002", reject_code=486)
    c = await _reg(sipbot_pool, pbx, h.ua_port(15509), "1003")

    caller = await _dial(pbx, sipbot_pool, hangup=10)
    # The forward must rescue the call: 1003 answers and media flows.
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), (
        caller.output
    )
    await h.wait_rtp_rx(c, "1003 (busy-forwarded)", timeout=15)
    await _wait_call_done(caller)
    assert caller.get_rtp_stats().is_bidirectional, caller.get_rtp_stats()
    # 1002 really was rung (and returned busy) before the forward kicked in.
    assert await b.wait_output_async(r"Handling INVITE|Call task started", timeout=10), (
        b.output
    )


@pytest.mark.asyncio
@pytest.mark.xfail(
    reason="no_answer forwarding is parsed into dialplan.call_forwarding but no "
           "consumer exists in SipSession — only 'always' mode is implemented "
           "(src/proxy/call.rs:605). Documents the product gap.",
    strict=True,
)
async def test_forward_no_answer_timeout(pbx, sipbot_pool, pbx_config):
    """no_answer: 1002 never answers → after the per-user timeout the call is
    forwarded to 1003 which answers."""
    pbx_config.set_user_forwarding("1002", "no_answer", "1003", timeout_secs=5)
    h.boot_pbx(pbx)

    # 1002 rings but never answers.
    b = await _reg(sipbot_pool, pbx, h.ua_port(15510), "1002", ring_secs=60,
                   answer_mode="none")
    c = await _reg(sipbot_pool, pbx, h.ua_port(15511), "1003")

    caller = await _dial(pbx, sipbot_pool, hangup=25)
    # 1002 must be rung first ...
    assert await b.wait_output_async(r"Handling INVITE|Call task started", timeout=15), (
        b.output
    )
    # ... then, after the ~5s no-answer timeout, 1003 answers the forward.
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=30), (
        caller.output
    )
    await h.wait_rtp_rx(c, "1003 (no-answer-forwarded)", timeout=15)
    await _wait_call_done(caller, timeout=35)
    assert caller.get_rtp_stats().is_bidirectional, caller.get_rtp_stats()
