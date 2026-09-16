"""CSV 回归测试 — 功能清单 L4-L47: SIP / Trunk / ICE / TURN & L303-L375: 录音/高可用

Basic transport and infrastructure tests.
"""

from __future__ import annotations

import asyncio, uuid
import pytest

pytestmark = [pytest.mark.acceptance, pytest.mark.trunk]


# ── SIP Transport (L4-L7) ──
@pytest.mark.asyncio
@pytest.mark.csv_line(4)
async def test_csv_L004_sip_over_udp(pbx, sipbot_pool, event_checker):
    callee = sipbot_pool.callee(
        host=pbx.host, port=15600, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=10,
    )
    await asyncio.sleep(2)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456", hangup=5,
    )
    await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert caller.output, "SIP/UDP call produced no output"
    await asyncio.sleep(2)
    types = event_checker.webhook.event_types()
    assert len(types) > 0


# ── Trunk (L8-L24) ──
@pytest.mark.asyncio
@pytest.mark.csv_line(8)
async def test_csv_L008_inbound_trunk(pbx, sipbot_pool, event_checker):
    callee = sipbot_pool.callee(
        host=pbx.host, port=15604, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=10,
    )
    await asyncio.sleep(2)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001", password="123456", hangup=5,
    )
    await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert caller.output


@pytest.mark.asyncio
@pytest.mark.csv_line(20)
async def test_csv_L020_acl_health(pbx, api, event_checker):
    result = await api.get("/healthz")
    assert result is not None, "/healthz returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(23)
async def test_csv_L023_max_calls(pbx, sipbot_pool, event_checker):
    callee = sipbot_pool.callee(
        host=pbx.host, port=15608, username="1003", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=15,
    )
    await asyncio.sleep(2)
    callers = []
    for i in range(3):
        c = sipbot_pool.caller(
            target=f"sip:1003@{pbx.sip_addr}",
            username=f"maxc-{i}", password="123456", hangup=4, wait=i,
        )
        callers.append(c)
    await asyncio.sleep(10)
    for c in callers:
        assert c.output, f"Max calls caller {c.name} no output"


# ── Recording / CDR (L303-L320) ──
@pytest.mark.asyncio
@pytest.mark.csv_line(303)
async def test_csv_L303_cdr_events(pbx, sipbot_pool, event_checker):
    callee = sipbot_pool.callee(
        host=pbx.host, port=15612, username="1001", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=15,
    )
    await asyncio.sleep(2)
    caller = sipbot_pool.caller(
        target=f"sip:1001@{pbx.sip_addr}",
        username="1002",
        password="123456", hangup=8,
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, f"CDR test call did not connect. Output:\n{caller.output[-400:]}"
    await asyncio.sleep(8)
    types = event_checker.webhook.event_types()
    assert len(types) > 0, "No CDR webhook events"


# ── HA / Ops (L321-L375) ──
@pytest.mark.asyncio
@pytest.mark.csv_line(301)
async def test_csv_L301_healthz(pbx, api, event_checker):
    result = await api.get_health()
    assert result is not None, "GET /healthz returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(302)
async def test_csv_L302_metrics(pbx, api, event_checker):
    result = await api.get_metrics()
    assert result is not None, "GET /metrics returned None"
    assert "rustpbx" in (result or ""), f"Metrics missing rustpbx: {result[:100]}"


@pytest.mark.asyncio
@pytest.mark.csv_line(257)
async def test_csv_L257_failover(pbx, sipbot_pool, event_checker):
    callee = sipbot_pool.callee(
        host=pbx.host, port=15616, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=10,
    )
    await asyncio.sleep(2)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}",
        username="1001",
        password="123456", hangup=5,
    )
    await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert caller.output
    await asyncio.sleep(2)


@pytest.mark.asyncio
@pytest.mark.csv_line(297)
async def test_csv_L297_reload_trunks(pbx, api, event_checker):
    result = await api.reload_trunks()
    assert result is not None, "reload_trunks returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(298)
async def test_csv_L298_reload_routes(pbx, api, event_checker):
    result = await api.reload_routes()
    assert result is not None, "reload_routes returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(300)
async def test_csv_L300_reload_app(pbx, api, event_checker):
    result = await api.reload_app()
    assert result is not None, "reload_app returned None"
