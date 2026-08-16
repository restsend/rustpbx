"""Trunk B2BUA DTMF passthrough E2E test.

Ports the Rust `test_trunk_b2bua_e2e.rs::test_trunk_b2bua_dtmf_info_passthrough`
(which used SIP INFO DTMF) to a Python sipbot test using RFC 2833 (telephone-
event) DTMF — sipbot's `--dtmf-flows` sends real RTP RFC 2833 digits. The callee
UA must receive at least one of the digits through the inbound-trunk B2BUA, and
the call must complete with a CDR.

The SIP INFO DTMF variant remains covered by the Rust `test_sip_info_dtmf_e2e.rs`
suite and the Python `test_sip_info.py`/`test_ivr_queue.py` info-flow tests.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.trunk_ringback, pytest.mark.media]


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


@pytest.mark.asyncio
async def test_trunk_b2bua_dtmf_rfc2833_passthrough(pbx_config, pbx, sipbot_pool, cdr_dir):
    """Trunk caller sends RFC 2833 DTMF mid-call → the callee must receive it
    through the B2BUA, and the call writes a completed CDR."""
    callee_port = 15430
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "dtmf-trunk", dest=f"127.0.0.1:{callee_port}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
    )
    pbx_config.media_proxy = "all"
    h.boot_pbx(pbx)

    callee = await _reg_callee(sipbot_pool, pbx, callee_port, "1002")

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="external", password="123456",
        from_uri="sip:external@trunk.example.com", hangup=10,
        dtmf_flows="2s:1,4s:2",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output

    # Wait until the callee reports received RFC 2833 DTMF digits.
    deadline = asyncio.get_event_loop().time() + 20
    digits: list[str] = []
    while asyncio.get_event_loop().time() < deadline:
        digits = callee.get_dtmf_digits()
        if digits:
            break
        await asyncio.sleep(0.3)
    assert digits, (
        f"callee received no DTMF through trunk B2BUA:\n{callee.output[-2000:]}"
    )

    # The call must complete cleanly.
    await caller.wait_output_async(r"All bots finished", timeout=30)
    deadline = asyncio.get_event_loop().time() + 15
    records: list[dict] = []
    while asyncio.get_event_loop().time() < deadline:
        records = []
        for p in sorted(
            cdr_dir.rglob("*.json"), key=lambda f: f.stat().st_mtime, reverse=True
        ):
            try:
                text = p.read_text(encoding="utf-8")
                if text.strip():
                    records.append(json.loads(text))
            except (json.JSONDecodeError, OSError):
                # The writer may still be mid-flush (file created but empty or
                # partially written) — skip and let the retry loop pick it up.
                continue
        if records:
            break
        await asyncio.sleep(0.5)
    assert records, "no CDR written for trunk DTMF call"
