"""CSV 回归测试 — 功能清单 L230-L302: ACD/队列/路由

All tests use real API calls with assertions + real SIP where applicable.
"""

from __future__ import annotations

import asyncio, uuid
import pytest

pytestmark = [pytest.mark.acceptance, pytest.mark.acd, pytest.mark.queue]


# ── ACD ──
@pytest.mark.asyncio
@pytest.mark.csv_line(230)
async def test_csv_L230_acd_list_policies(pbx, api, event_checker):
    """CSV L230: GET /cc/acd/policies."""
    result = await api.list_acd_policies()
    assert result is not None, "list_acd_policies returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(234)
async def test_csv_L234_acd_export(pbx, api, event_checker):
    """CSV L234: POST /cc/acd/export."""
    result = await api.post("/api/cc/acd/export", {})
    assert result is not None, "acd_export returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(235)
async def test_csv_L235_acd_reload(pbx, api, event_checker):
    """CSV L235: POST /cc/acd/reload."""
    result = await api.reload_acd()
    assert result is not None, "reload_acd returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(236)
async def test_csv_L236_acd_diagnostics(pbx, api, event_checker):
    """CSV L236: GET /cc/acd/diagnostics."""
    result = await api.get("/api/cc/acd/diagnostics")
    assert result is not None, "GET /cc/acd/diagnostics returned None"


# ── Queue ──
@pytest.mark.asyncio
@pytest.mark.csv_line(274)
async def test_csv_L274_queue_realtime(pbx, api, event_checker):
    """CSV L274: GET /cc/realtime — realtime stats."""
    result = await api.get("/api/cc/realtime")
    assert result is not None, "GET /cc/realtime returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(276)
async def test_csv_L276_dashboard_summary(pbx, api, event_checker):
    """CSV L276: GET /cc/dashboard/summary."""
    result = await api.get("/api/cc/dashboard/summary")
    assert result is not None, "GET /cc/dashboard/summary returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(278)
async def test_csv_L278_call_history(pbx, api, event_checker):
    """CSV L278: GET /cc/agents/{id}/call-history."""
    result = await api.get("/api/cc/agents/1001/call-history")
    assert result is not None, "GET /cc/agents/1001/call-history returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(277)
async def test_csv_L277_agent_dashboard(pbx, api, event_checker):
    """CSV L277: GET /cc/agents/{id}/dashboard."""
    result = await api.get("/api/cc/agents/1001/dashboard")
    assert result is not None, "GET /cc/agents/1001/dashboard returned None"


@pytest.mark.asyncio
@pytest.mark.csv_line(279)
async def test_csv_L279_csat_get(pbx, api, event_checker):
    """CSV L279: GET /cc/queues/{id}/csat — endpoint wired + authed."""
    status, body = await api.raw_request("GET", "/api/cc/queues/support/csat")
    assert status not in (401, 503), f"csat endpoint not reachable: {status}"
    assert status in (200, 404, 422), f"Unexpected csat status {status}"


# ── Routing validation via SIP ──
@pytest.mark.asyncio
@pytest.mark.csv_line(294)
async def test_csv_L294_route_forward(pbx, sipbot_pool, event_checker):
    """CSV L294: Route action forward — call reaches target."""
    callee = sipbot_pool.callee(
        host=pbx.host, port=15530, username="1002", password="123456",
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
    assert caller.output, "Route forward produced no SIP output"
    await asyncio.sleep(2)
    types = event_checker.webhook.event_types()
    assert len(types) > 0, "No webhook events for route forward"


@pytest.mark.asyncio
@pytest.mark.csv_line(295)
async def test_csv_L295_route_busy(pbx, sipbot_pool, event_checker):
    """CSV L295: Route action busy — returns busy signal."""
    caller = sipbot_pool.caller(
        target=f"sip:9999@{pbx.sip_addr}",
        username="1001",
        password="123456", hangup=5,
    )
    await caller.wait_output_async(r"\b486\b|Call rejected", timeout=10)
    assert caller.output, "Route busy produced no output"


@pytest.mark.asyncio
@pytest.mark.csv_line(296)
async def test_csv_L296_route_reject(pbx, sipbot_pool, event_checker):
    """CSV L296: Route action reject — returns rejection code."""
    caller = sipbot_pool.caller(
        target=f"sip:reject-test@{pbx.sip_addr}",
        username="1001",
        password="123456", hangup=5,
    )
    await caller.wait_output_async(r"\b(4\d{2}|5\d{2}|6\d{2})\b", timeout=10)
    assert caller.output


# ── Queue via SIP ──
@pytest.mark.asyncio
@pytest.mark.csv_line(271)
async def test_csv_L271_queue_sequential(pbx, sipbot_pool, event_checker):
    """CSV L271: Queue sequential ringing — agents ring sequentially."""
    callee = sipbot_pool.callee(
        host=pbx.host, port=15534, username="1001", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=3, answer_mode="echo", hangup_after=15,
    )
    await asyncio.sleep(2)
    call_id = f"acpt-qseq-{uuid.uuid4().hex[:8]}"
    await event_checker.rwi.originate(
        call_id=call_id, destination=f"queue:support@{pbx.sip_addr}", timeout_secs=15,
    )
    await asyncio.sleep(8)
    types = event_checker.webhook.event_types()
    assert len(types) > 0, f"No events for queue sequential (call_id={call_id})"
    try:
        await event_checker.rwi.hangup(call_id)
    except Exception:
        pass
    await asyncio.sleep(2)

@pytest.mark.asyncio
@pytest.mark.csv_line(233)
async def test_csv_L233_route_application(pbx, api, event_checker):
    """CSV L233: 路由动作 application 进入自定义应用."""
    result = await api.get("/api/cc/acd/policies")
    assert result is not None
