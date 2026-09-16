"""Tier 3 — Desk Call-Info / User-to-User injection + B-leg CTI resolution.

Validates the frozen Desk↔CC contract (restsend-call `docs/desk_rustpbx.md`):

  §3.3  CTI `{call_id}` is the agent leg's B-leg SIP Call-ID — the server must
        resolve it (registry dialog mapping) for hold/unhold/transfer/acw.
  §3.2  The queue→agent INVITE carries a `User-to-User` CC short-code payload
        (q=queue;t=direction;encoding=ascii;purpose=isdn-uui) injected by the
        CC queue-location enricher before dialing.
  §5    `GET /cc/calls/{call_id}/context` returns machine-readable call
        context and accepts the B-leg Call-ID.

A real caller is routed (`sip:8888@…`) straight into the `support` queue app so
the ACD dispatches to a registered agent — the queue-location enricher runs and
the agent's INVITE carries the contract headers. The agent is a `sipbot` UA that
logs the incoming INVITE's `Call-ID` and its `Call-Info` / `User-to-User`
headers (added to sipbot's `handle_invite` for this suite), so tests assert the
headers actually reached the wire.

Call-Info render/info/card content is configuration-driven and exercised by the
Rust unit tests (`queue_location_enricher_tests`); here the always-on
`User-to-User` short code proves the enricher→INVITE plumbing end-to-end.
"""

from __future__ import annotations

import asyncio
import re

import pytest

pytestmark = [pytest.mark.tier3, pytest.mark.desk_call_info_cti]

QUEUE_NUMBER = "8888"  # conftest `queue-dispatch-route` → queue `support`
AGENT_ID = "1001"
CALLER_USER = "1002"


async def _wait_for_invite(bot, username: str, timeout: float = 25) -> str:
    """Wait until `bot` receives an INVITE and return its SIP Call-ID."""
    ok = await bot.wait_output_async(r"Handling INVITE", timeout=timeout)
    assert ok, f"agent {username} never received an INVITE.\n{bot.output[-2000:]}"
    m = re.search(r"\(Call-ID: ([^)]+)\)", bot.output)
    assert m, f"no Call-ID parsed from agent output.\n{bot.output[-2000:]}"
    return m.group(1)


def _make_agent(sipbot_pool, pbx, port: int, hangup: int = 40):
    return sipbot_pool.callee(
        host=pbx.host,
        port=port,
        username=AGENT_ID,
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
        hangup_after=hangup,
    )


async def _reset_agent(api, agent_id: str = AGENT_ID) -> None:
    """Force the agent back to Idle so ACD dispatches to it (a previous call
    leaves it in wrapup). Best-effort; registration alone usually suffices."""
    for path in (f"/api/cc/agents/{agent_id}/wrapup/end",):
        try:
            await api.raw_request("POST", path, {"status": "idle"})
        except Exception:
            pass
        await asyncio.sleep(0.3)
    try:
        await api.raw_request(
            "POST", f"/api/cc/agents/{agent_id}/status", {"status": "idle"})
    except Exception:
        pass


async def _start_queue_call(sipbot_pool, pbx, hangup: int = 8):
    """Dial a caller into the queue; returns the caller bot."""
    return sipbot_pool.caller(
        target=f"sip:{QUEUE_NUMBER}@{pbx.sip_addr}",
        username=CALLER_USER,
        password="123456",
        hangup=hangup,
    )


async def _finish_queue_call(caller, timeout: float = 20) -> None:
    """Let the caller hang up and the call tear down cleanly so the agent
    transitions busy→wrapup (avoids leaving the next test a stuck-Busy agent)."""
    try:
        await caller.wait(timeout=timeout)
    except Exception:
        pass
    await asyncio.sleep(2)


@pytest.mark.asyncio
async def test_bleg_call_id_cti_resolution(pbx, sipbot_pool, api, event_checker):
    """B-leg Call-ID → CTI hold/unhold resolve; garbage → 404."""
    agent = _make_agent(sipbot_pool, pbx, port=15260)
    await asyncio.sleep(2)
    await _reset_agent(api)

    caller = await _start_queue_call(sipbot_pool, pbx)
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, f"caller did not connect to the queue.\n{caller.output[-500:]}"

    agent_call_id = await _wait_for_invite(agent, AGENT_ID)
    assert agent_call_id != QUEUE_NUMBER, "agent leg Call-ID must not be the queue number"

    await agent.wait_output_async(r"Call established|answered|200 OK", timeout=15)
    await asyncio.sleep(1)

    # Hold / unhold with the agent leg's B-leg Call-ID.
    status, body = await api.raw_request(
        "POST", f"/api/cc/calls/{agent_call_id}/hold", {})
    assert status == 200, f"hold via B-leg Call-ID failed: {status} {body!r:.200}"

    status, _ = await api.raw_request(
        "POST", f"/api/cc/calls/{agent_call_id}/unhold", {})
    assert status == 200, f"unhold via B-leg Call-ID failed: {status}"

    # Blind transfer uses the same resolver (get_handle + dialog fallback).
    status, _ = await api.raw_request(
        "POST", f"/api/cc/calls/{agent_call_id}/transfer", {"target": "1001"})
    assert status == 200, f"transfer via B-leg Call-ID failed: {status}"

    # Garbage id → 404.
    status, _ = await api.raw_request(
        "POST", "/api/cc/calls/does-not-exist-xyz/hold", {})
    assert status == 404, f"unknown call_id must 404, got {status}"

    await _finish_queue_call(caller)


@pytest.mark.asyncio
async def test_call_context_endpoint(pbx, sipbot_pool, api, event_checker):
    """GET /cc/calls/{call_id}/context — shape + B-leg Call-ID + 404."""
    agent = _make_agent(sipbot_pool, pbx, port=15261)
    await asyncio.sleep(2)
    await _reset_agent(api)

    caller = await _start_queue_call(sipbot_pool, pbx)
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, f"caller did not connect to the queue.\n{caller.output[-500:]}"

    agent_call_id = await _wait_for_invite(agent, AGENT_ID)
    await agent.wait_output_async(r"Call established|answered|200 OK", timeout=15)
    await asyncio.sleep(1)

    # Via the B-leg Call-ID (client fallback path, contract §3.3).
    status, body = await api.raw_request(
        "GET", f"/api/cc/calls/{agent_call_id}/context")
    assert status == 200, f"context via B-leg Call-ID failed: {status} {body!r:.200}"
    assert isinstance(body, dict), f"context not a JSON object: {body!r:.200}"
    for key in ("call_id", "caller", "callee", "direction", "state",
                "caller_name", "started_at", "duration_secs"):
        assert key in body, f"context missing key {key}: {body}"
    queue_id = body.get("queue_id") or body.get("queueId")
    assert queue_id == "support", f"context queue_id expected support, got {body}"

    # Via the proxy session id (legacy alias) — the context's call_id IS the
    # session id.
    session_id = body.get("call_id")
    assert session_id, f"context did not expose call_id/session id: {body}"
    status, _ = await api.raw_request(
        "GET", f"/api/cc/calls/{session_id}/context")
    assert status == 200, f"context via session id failed: {status}"

    # Garbage id → 404.
    status, _ = await api.raw_request(
        "GET", "/api/cc/calls/does-not-exist-xyz/context")
    assert status == 404, f"unknown context must 404, got {status}"

    await _finish_queue_call(caller)


@pytest.mark.asyncio
async def test_user_to_user_shortcode_injected(pbx, sipbot_pool, api, event_checker):
    """The queue→agent INVITE carries the CC User-to-User short code (q/t)."""
    agent = _make_agent(sipbot_pool, pbx, port=15262)
    await asyncio.sleep(2)
    await _reset_agent(api)

    caller = await _start_queue_call(sipbot_pool, pbx)
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, f"caller did not connect to the queue.\n{caller.output[-500:]}"

    await _wait_for_invite(agent, AGENT_ID)
    ok = await agent.wait_output_async(r"CC headers: .*User-to-User", timeout=10)
    if not ok:
        pytest.fail(
            f"agent INVITE did not log CC headers; enricher injection missing.\n"
            f"{agent.output[-2000:]}"
        )
    assert "q=support" in agent.output, f"UUI missing q=support.\n{agent.output[-2000:]}"
    assert re.search(r"t=(inbound|outbound)", agent.output), (
        f"UUI missing t= direction.\n{agent.output[-2000:]}"
    )
    assert "encoding=ascii" in agent.output, (
        f"UUI missing encoding=ascii.\n{agent.output[-2000:]}"
    )
    assert "purpose=isdn-uui" in agent.output, (
        f"UUI missing purpose=isdn-uui.\n{agent.output[-2000:]}"
    )

    await _finish_queue_call(caller)


@pytest.mark.asyncio
async def test_agent_current_calls_zero_after_queue_call_wrapup(pbx, sipbot_pool, api):
    """Regression: cc_agent_presence must not retain current_calls=1 after call end."""
    agent = _make_agent(sipbot_pool, pbx, port=15263, hangup=60)
    await asyncio.sleep(2)
    await _reset_agent(api)

    caller = await _start_queue_call(sipbot_pool, pbx, hangup=6)
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=15)
    assert ok, f"caller did not connect to the queue.\n{caller.output[-500:]}"

    await _wait_for_invite(agent, AGENT_ID)
    await agent.wait_output_async(r"Call established|answered|200 OK", timeout=15)
    await _finish_queue_call(caller, timeout=12)

    # Default wrapup timer is 30s — wait for agent to return idle/offline.
    await asyncio.sleep(35)

    info = await api.get_agent(AGENT_ID)
    if info is None:
        pytest.skip("CC REST API requires PhoneAuth JWT authentication")
    current_calls = info.get("current_calls")
    if current_calls is None and isinstance(info.get("data"), dict):
        current_calls = info["data"].get("current_calls")
    assert current_calls == 0, (
        f"Agent {AGENT_ID} must release capacity after wrapup, got current_calls={current_calls!r}"
    )
