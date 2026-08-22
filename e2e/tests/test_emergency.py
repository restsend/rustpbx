"""Emergency routing E2E — 110/911 must be rewritten to the emergency trunk.

[proxy.emergency] rewrites the dialplan of an INVITE whose To-user contains
an emergency number to a single sequential target: the emergency_trunk URI.
The emergency callee (a sipbot on the trunk port) must receive the call.
"""

from __future__ import annotations

import pytest

import helpers as h

pytestmark = [pytest.mark.p2p]


@pytest.mark.asyncio
@pytest.mark.parametrize("number", ["110", "911"])
async def test_emergency_number_routed_to_trunk(pbx, pbx_config, sipbot_pool, number):
    sos_port = h.ua_port(15516)
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.set_emergency(
        f"sip:sos@127.0.0.1:{sos_port}", numbers=["110", "911"]
    )
    h.boot_pbx(pbx)

    sos = sipbot_pool.callee(
        host=pbx.host, port=sos_port, username="sos", password="123456",
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    caller = sipbot_pool.caller(
        target=f"sip:{number}@{pbx.sip_addr}", username="1001", password="123456",
        hangup=8,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), (
        caller.output
    )
    # The SOS callee saw the INVITE and media flows back to the caller.
    assert await sos.wait_output_async(r"Handling INVITE|Call task started", timeout=10), (
        f"emergency trunk callee never rung:\n{sos.output[-800:]}"
    )
    await h.wait_rtp(caller, "caller to emergency trunk", timeout=15)
    assert caller.get_rtp_stats().is_bidirectional, caller.get_rtp_stats()
