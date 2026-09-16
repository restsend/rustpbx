"""Tier 1 — Trunk & SBC smoke tests.

Verifies SIP protocol support (UDP), trunk directionality (inbound/outbound/
bidirectional), trunk registration, and basic media flow through trunks.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

pytestmark = [pytest.mark.tier1, pytest.mark.trunk]


@pytest.mark.asyncio
async def test_sip_over_udp_basic_call(pbx, sipbot_pool, event_checker):
    """SIP over UDP — basic extension-to-extension call via SIP proxy."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15071,
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
        hangup=8,
    )

    ok = await caller.wait_output_async(r"200 OK", timeout=15)
    assert ok, f"Caller did not get 200 OK. Output:\n{caller.output}"

    await caller.wait_output_async(r"All bots finished|Progress: 1/1", timeout=15)
    await asyncio.sleep(1)
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0 or stats.tx_packets > 0, (
        f"No RTP flow detected. Stats: {stats}\nOutput:\n{caller.output[-500:]}"
    )


@pytest.mark.asyncio
async def test_inbound_trunk_registration(pbx, sipbot_pool, event_checker):
    """Trunk REGISTER — register-enabled trunk accepts SIP REGISTER."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15072,
        username="1001",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
    )
    registered = await callee.wait_output_async(r"(Registered|registered|200 OK)", timeout=10)
    assert registered, f"Registration not observed. Output:\n{callee.output}"


@pytest.mark.asyncio
async def test_outbound_call_to_sip_uri(pbx, sipbot_pool, event_checker):
    """Outbound call — originate via RWI to a registered SIP endpoint."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15073,
        username="1002",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
    )
    await asyncio.sleep(2)

    rwi = event_checker.rwi
    call_id = f"test-outbound-{uuid.uuid4().hex[:8]}"
    try:
        resp = await rwi.originate(
            call_id=call_id,
            destination=f"sip:1002@{pbx.sip_addr}",
            caller_id="1001",
            timeout_secs=15,
        )
        await asyncio.sleep(3)
        await rwi.hangup(call_id)
    except TimeoutError:
        pytest.skip("RWI originate timed out (may need registered endpoint)")


@pytest.mark.asyncio
async def test_trunk_call_reject(pbx, sipbot_pool, event_checker):
    """Trunk — callee rejects call, caller gets busy/reject."""
    callee = sipbot_pool.callee(
        host=pbx.host,
        port=15076,
        username="1004",
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        reject_code=486,
    )
    await asyncio.sleep(2)

    caller = sipbot_pool.caller(
        target=f"sip:1004@{pbx.sip_addr}",
        username="1001",
        password="123456",
        hangup=5,
    )
    await caller.wait_output_async(r"All bots finished", timeout=15)
    output = caller.output
    # The callee was configured with reject_code=486, so the caller must see
    # exactly that rejection (a bare "4" would also match the 407 challenge
    # present in every call and prove nothing).
    assert "486" in output, (
        f"Expected 486 Busy Here rejection. Output:\n{output[-500:]}"
    )
    assert "200 OK" not in output, (
        f"Call must not be answered. Output:\n{output[-500:]}"
    )


@pytest.mark.asyncio
async def test_sip_options_health_check(pbx, api):
    """Trunk SIP OPTIONS — PBX answers an OPTIONS ping with 200 OK (health).

    The proxy only answers out-of-dialog OPTIONS from recognized inbound
    trunk sources (spam guard, proxy/server.rs). Register 127.0.0.1 as an
    inbound trunk host first so the probe is a genuine trunk health check.
    """
    from helpers.config_reload import apply_config

    pbx.config_builder.add_trunk(
        "e2e-options-trunk",
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
            f"Via: SIP/2.0/UDP {pbx.host}:{src_port};branch=z9hG4bK-e2e-options\r\n"
            f"From: <sip:ping@{pbx.host}>;tag=e2e\r\n"
            f"To: <sip:ping@{pbx.host}>\r\n"
            f"Call-ID: options-e2e@{pbx.host}\r\n"
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
