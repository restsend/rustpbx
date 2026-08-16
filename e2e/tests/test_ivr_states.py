"""IVR call-state + DTMF combination E2E tests.

Verifies:
- IVR answers the caller (200) and plays the greeting
- DTMF routes to the transfer target -> target answers with 200
- hangup mid-IVR is handled cleanly (call ends)
"""

from __future__ import annotations

import asyncio

import pytest

import helpers as h

pytestmark = [pytest.mark.ivr, pytest.mark.cdr]


def _ivr_toml(greeting: str, transfer_target: str) -> str:
    return f'''\
[ivr]
name = "ivr-state"
ivr_mode = "tree"

[ivr.root]
greeting_text = "{greeting}"
timeout_ms = 8000
max_retries = 3

[[ivr.root.entries]]
key = "1"
[ivr.root.entries.action]
type = "transfer"
target = "{transfer_target}"
'''


def _add_ivr_route(cb, toml_body: str, file: str = "config/ivr/ivr-state.toml"):
    cb.add_ivr("ivr-state", toml_body)
    cb.add_route(
        "to-ivr-state",
        match={"to.user": "ivr-state"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": file},
        auto_answer=True,
    )


async def _reg_callee(sipbot_pool, pbx, port, username="1002"):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(ua)
    return ua


@pytest.mark.asyncio
async def test_ivr_200_and_dtmf_transfer_sequence(pbx, sipbot_pool):
    """IVR answers (200) -> DTMF '1' -> transfer target answers (200)."""
    _add_ivr_route(pbx.config_builder, _ivr_toml("Press 1 to transfer.", "1002"))
    h.boot_pbx(pbx)
    await _reg_callee(sipbot_pool, pbx, h.ua_port(15160))

    caller = sipbot_pool.caller(
        target=f"sip:ivr-state@{pbx.sip_addr}", username="1001", password="123456",
        hangup=10, dtmf_flows="3s:1",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 20)
    # DTMF routes to the transfer target; the call stays up (target answers 200).
    assert await caller.wait_output_async(r"\b200:\s*[1-9]", timeout=20), caller.output


@pytest.mark.asyncio
async def test_ivr_hangup_mid_flow_clean(pbx, sipbot_pool):
    """Caller hangs up while in the IVR greeting — call ends cleanly, no dead session."""
    _add_ivr_route(pbx.config_builder, _ivr_toml("Press 1 to transfer.", "1002"))
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:ivr-state@{pbx.sip_addr}", username="1001", password="123456",
        hangup=2,  # hang up shortly after IVR answers
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 10)
    # The call terminates cleanly (BYE) after the short hangup.
    assert await caller.wait_output_async(r"BYE|bye|hangup|terminated|Call ended", timeout=20), caller.output
