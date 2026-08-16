"""Concurrent call E2E tests.

Ports the Rust concurrency assertions so the in-process Rust suite can be
slimmed:

  * `test_p2p_two_concurrent_calls_rtp_cdr`   → two simultaneous P2P calls, each
    with bidirectional RTP and a completed CDR.
  * `test_trunk_b2bua_two_concurrent_calls`   → two simultaneous inbound-trunk
    B2BUA calls, each with its own CDR.

Each test boots a single PBX and runs two independent caller→callee pairs at the
same time, then verifies media flows and that a CDR is produced per call.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.p2p]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def _read_cdrs(cdr_dir: Path) -> list[dict]:
    out: list[dict] = []
    for p in sorted(cdr_dir.rglob("*.json"), key=lambda f: f.stat().st_mtime, reverse=True):
        try:
            out.append(json.loads(p.read_text(encoding="utf-8")))
        except Exception:
            continue
    return out


async def _reg_callee(sipbot_pool, pbx, port, username):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(ua)
    return ua


async def _wait_rtp(ua, label: str, timeout: float = 20) -> None:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if ua.get_rtp_stats().is_bidirectional:
            return
        await asyncio.sleep(0.3)
    raise AssertionError(
        f"{label}: no bidirectional RTP after {timeout}s — {ua.get_rtp_stats()}\n{ua.output[-1200:]}"
    )


async def _wait_cdr_count(cdr_dir: Path, count: int, timeout: float = 20) -> list[dict]:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        records = _read_cdrs(cdr_dir)
        if len(records) >= count:
            return records
        await asyncio.sleep(0.5)
    return _read_cdrs(cdr_dir)


@pytest.mark.asyncio
async def test_two_concurrent_p2p_calls(pbx, sipbot_pool, cdr_dir):
    """Two simultaneous P2P calls: both carry bidirectional RTP and each writes a CDR."""
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, h.ua_port(15440), "1002")
    await _reg_callee(sipbot_pool, pbx, 15441, "1003")

    c1 = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    c2 = sipbot_pool.caller(
        target=f"sip:1003@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    for c in (c1, c2):
        assert await c.wait_output_async(r"200 OK|Call established", timeout=20), c.output

    await asyncio.gather(_wait_rtp(c1, "call1"), _wait_rtp(c2, "call2"))

    records = await _wait_cdr_count(cdr_dir, 2, timeout=25)
    assert len(records) >= 2, f"expected >=2 CDRs for concurrent calls, got {len(records)}"


@pytest.mark.asyncio
async def test_two_concurrent_trunk_calls(pbx_config, pbx, sipbot_pool, cdr_dir):
    """Two simultaneous inbound-trunk B2BUA calls, each with its own CDR."""
    callee_port = h.ua_port(15442)
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "conc-trunk", dest=f"127.0.0.1:{callee_port}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
    )
    pbx_config.media_proxy = "all"
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, callee_port, "1002")
    await _reg_callee(sipbot_pool, pbx, callee_port + 1, "1003")

    t1 = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="ext1", password="123456",
        from_uri="sip:ext1@trunk.example.com", hangup=6,
    )
    t2 = sipbot_pool.caller(
        target=f"sip:1003@{pbx.sip_addr}", username="ext2", password="123456",
        from_uri="sip:ext2@trunk.example.com", hangup=6,
    )
    for c in (t1, t2):
        assert await c.wait_output_async(r"200 OK|Call established", timeout=20), c.output

    await asyncio.gather(_wait_rtp(t1, "trunk1"), _wait_rtp(t2, "trunk2"))

    records = await _wait_cdr_count(cdr_dir, 2, timeout=25)
    assert len(records) >= 2, f"expected >=2 CDRs for concurrent trunk calls, got {len(records)}"
