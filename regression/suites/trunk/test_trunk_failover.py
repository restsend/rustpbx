"""Outbound trunk failover E2E — documents the current routing behavior.

Findings (2026-08-22):
  * A route `dest` LIST is a load-balancing group (``select`` picks ONE
    entry) — it is NOT a sequential failover list: when the picked dest is
    dead the call fails; the next entry is never tried.
  * Trunk health (`health_check_*`) probes and marks trunks UNHEALTHY and
    logs "auto-failover activated", but nothing in the routing/dial path
    consumes the health state — the fallback trunk is never dialed.

So the strict-xfail test below documents the missing reactive failover; the
passing test pins the current contract: a dead trunk fails FAST (no zombie
ringing) and the caller sees a final error.
"""

from __future__ import annotations

import asyncio
import socket

import pytest

import helpers as h

pytestmark = [pytest.mark.p2p]


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.mark.asyncio
async def test_dead_trunk_fails_fast(pbx, pbx_config, sipbot_pool):
    """A route whose (only) trunk dest is dead must reject the caller quickly
    with a final status — no zombie ringing, no hang."""
    dead_port = _free_port()
    pbx_config.add_route(
        "dead-trunk-route",
        match={"to.user": "^77[0-9]+$"},
        dest=[f"sip:777@127.0.0.1:{dead_port}"],
        max_ring_time=6,
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:77123@{pbx.sip_addr}", username="1001", password="123456",
        hangup=15,
    )
    # The caller must see a FINAL failure (4xx/5xx), not just ring forever.
    assert await caller.wait_output_async(
        r"5[0-9][0-9]|4[0-9][0-9]|Unavailable|Terminated|failed", timeout=20
    ), f"dead trunk call was not rejected:\n{caller.output[-1200:]}"
    assert not caller.get_rtp_stats().has_rx, "no media can flow from a dead trunk"


@pytest.mark.asyncio
@pytest.mark.xfail(
    reason="reactive trunk failover is not implemented: route dest lists are "
           "load-balance groups (one pick, no retry of the next entry), and "
           "trunk-health 'auto-failover' is advisory only (log line) — the "
           "health state is not consumed by routing. Documents the product gap.",
    strict=True,
)
async def test_trunk_failover_dead_primary_reaches_backup(pbx, pbx_config, sipbot_pool):
    live_port = h.ua_port(15517)
    dead_port = _free_port()
    pbx_config.add_route(
        "failover-route",
        match={"to.user": "^77[0-9]+$"},
        dest=[
            f"sip:777@127.0.0.1:{dead_port}",  # dead primary
            f"sip:777@127.0.0.1:{live_port}",  # live backup
        ],
        max_ring_time=4,
    )
    h.boot_pbx(pbx)

    backup = sipbot_pool.callee(
        host=pbx.host, port=live_port, username="backup", password="123456",
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    caller = sipbot_pool.caller(
        target=f"sip:77123@{pbx.sip_addr}", username="1001", password="123456",
        hangup=12,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=30), (
        f"failover to backup did not connect the caller:\n{caller.output[-1500:]}"
    )
    assert await backup.wait_output_async(r"Handling INVITE|Call task started", timeout=10), (
        f"backup callee never rung:\n{backup.output[-800:]}"
    )
    await h.wait_rtp(caller, "caller via backup trunk", timeout=15)
    assert caller.get_rtp_stats().is_bidirectional, caller.get_rtp_stats()
