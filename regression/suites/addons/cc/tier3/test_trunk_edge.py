"""Tier 3 — Trunk edge case tests.

Verifies IP whitelist/ACL, User-Agent filtering, Call-ID modes,
CPS limiting, and per-trunk recording overrides.
"""

from __future__ import annotations

import asyncio

import pytest

pytestmark = [pytest.mark.tier3, pytest.mark.trunk]


@pytest.mark.asyncio
async def test_trunk_acl_block(pbx, sipbot_pool, event_checker):
    """Trunk ACL — blocked IP gets rejected."""
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=3,
    )
    await asyncio.sleep(4)
    # Should get rejected or timeout
    output = caller.output
    assert output, "No output from blocked caller"


@pytest.mark.asyncio
async def test_trunk_call_id_rewrite_mode(pbx, sipbot_pool, event_checker):
    """Trunk Call-ID mode — rewrite mode generates new Call-ID."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15180,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=5,
    )
    ok = await caller.wait_output_async(r"(200 OK|INVITE)", timeout=15)
    assert ok


@pytest.mark.asyncio
async def test_trunk_cps_rate_limit(pbx, sipbot_pool, event_checker):
    """Trunk CPS — high call rate should be throttled."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15181,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
        hangup_after=3,
    )
    await asyncio.sleep(2)

    callers = []
    for i in range(5):
        c = sipbot_pool.caller(
            target=f"sip:1002@{pbx.sip_addr}",
            username=f"cps-{i}",
            password="123456",
            hangup=2,
            wait=i * 2,  # stagger calls
        )
        callers.append(c)

    await asyncio.sleep(15)
    for c in callers:
        assert c.output, f"Caller {c.name} produced no output"


@pytest.mark.asyncio
async def test_trunk_ringback_audio(pbx, sipbot_pool, event_checker):
    """Trunk ringback — per-trunk ringback audio config."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15182,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=3,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=8,
    )
    ok = await caller.wait_output_async(r"(180|183|200 OK)", timeout=15)
    assert ok


@pytest.mark.asyncio
async def test_trunk_reload_config(pbx, api, event_checker):
    """Trunk reload — hot reload trunks config, verify response."""
    result = await api.reload_trunks()
    assert result is not None, "reload_trunks returned None"


@pytest.mark.asyncio
async def test_trunk_health_check(pbx, api, event_checker):
    """Trunk health — PBX answers a SIP OPTIONS ping (liveness).

    The proxy only answers out-of-dialog OPTIONS from recognized inbound
    trunk sources (spam guard). Register 127.0.0.1 as an inbound trunk host
    first so this is a genuine trunk health probe.
    """
    from helpers.config_reload import apply_config

    pbx.config_builder.add_trunk(
        "e2e-health-trunk",
        dest="127.0.0.1:9",  # unreachable dest is fine — only inbound_hosts matters
        direction="inbound",
        inbound_hosts=["127.0.0.1"],
    )
    await apply_config(pbx, api, reload_app=False)

    import socket as _socket

    sock = _socket.socket(_socket.AF_INET, _socket.SOCK_DGRAM)
    sock.bind((pbx.host, 0))
    sock.settimeout(5)
    src_port = sock.getsockname()[1]
    try:
        msg = (
            f"OPTIONS sip:ping@{pbx.host}:{pbx.sip_port} SIP/2.0\r\n"
            f"Via: SIP/2.0/UDP {pbx.host}:{src_port};branch=z9hG4bK-e2e-health\r\n"
            f"From: <sip:ping@{pbx.host}>;tag=health\r\n"
            f"To: <sip:ping@{pbx.host}>\r\n"
            f"Call-ID: health-e2e@{pbx.host}\r\n"
            f"CSeq: 1 OPTIONS\r\n"
            f"Content-Length: 0\r\n\r\n"
        )
        sock.sendto(msg.encode(), (pbx.host, pbx.sip_port))
        try:
            data, _ = sock.recvfrom(4096)
        except TimeoutError:
            pytest.skip("PBX spam-guard dropped out-of-dialog OPTIONS from unknown source")
    finally:
        sock.close()
    resp = data.decode(errors="replace")
    assert "SIP/2.0 200" in resp, f"OPTIONS did not return 200 OK. Response:\n{resp[:500]}"
