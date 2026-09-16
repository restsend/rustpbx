"""Registration edge-case E2E.

  * bad-password REGISTER → the PBX answers 401 + WWW-Authenticate (forever;
    there is no loop counter) and the UA must never report success.
  * the PBX keeps serving calls afterwards (the failed auth did not poison
    the registrar).
"""

from __future__ import annotations

import pytest

import helpers as h

pytestmark = [pytest.mark.p2p]


@pytest.mark.asyncio
async def test_register_wrong_password_challenged_no_success(pbx, sipbot_pool):
    h.boot_pbx(pbx)

    bad = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15514), username="1002", password="wrong-pass",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo",
    )
    # sipbot retries the REGISTER with the (wrong) credentials and keeps
    # getting 401s. Give it a few seconds of loop, then check both sides.
    await bad.wait_output_async(r"401", timeout=10)
    assert not await bad.wait_output_async(r"Registered successfully", timeout=5), (
        f"REGISTER with wrong password must not succeed:\n{bad.output[-1200:]}"
    )

    # The registrar is still healthy: a correct registration succeeds.
    good = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15515), username="1003", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo",
    )
    await h.wait_registered(good)

    # And a call completes normally.
    caller = sipbot_pool.caller(
        target=f"sip:1003@{pbx.sip_addr}", username="1001", password="123456",
        hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), (
        caller.output
    )
