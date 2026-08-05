"""SIP INFO flow E2E test — verifies sipbot --info-flows delivers in-dialog INFO.

Tests that sipbot's new --info-flows CLI flag successfully sends SIP INFO
requests within an established dialog, and that rustpbx processes them.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.media]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


@pytest.mark.asyncio
async def test_sipbot_info_flows_delivered(pbx, sipbot_pool, rwi):
    """sipbot caller sends SIP INFO via --info-flows; rustpbx receives and logs it."""
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)

    callee = sipbot_pool.callee(
        host=pbx.host, port=15200, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await asyncio.sleep(2)

    info_payload = '{"action":"ivr.exec","params":{"route_point":"test-ivr"}}'
    info_flows_str = f'2s:application/vnd.rustpbx+json:{info_payload}'

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=8, info_flows=info_flows_str,
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=20)
    assert answered, f"call not answered:\n{caller.output[-1000:]}"

    await asyncio.sleep(4)

    output = caller.output
    assert "SIP INFO" in output or "INFO flow" in output, (
        f"no SIP INFO log in caller output:\n{output[-1500:]}"
    )

    if pbx.log_file_path and pbx.log_file_path.exists():
        log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace")
        assert "SIP INFO" in log or "INFO" in log, (
            "rustpbx did not log receiving SIP INFO"
        )
        assert "ivr.exec" in log, (
            "rustpbx did not parse ivr.exec action from SIP INFO body"
        )
