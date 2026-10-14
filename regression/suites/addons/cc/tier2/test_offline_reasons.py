"""Tier 2 — user offline reasons (offline / expired / unregistered) on RWI events.

Full e2e chain: a SIP endpoint disappears → the locator / presence layer
emits the CAUSE → the CC registrar bridge stamps `reason_code` on BOTH
`agent_unregistered` and the `agent_state_changed` (→ offline) event → the
RWI webhook (and RWI WS) fan-out receives them.

Scenarios (restsend-cli = the real production client, not a mock):
  1. WS transport drop  — process killed, WebSocket closes:
        agent_unregistered.reason_code == "offline"
  2. REGISTER TTL lapses with NO goodbye (UDP client killed, browser crash
     equivalent) — expiry sweeper reclaims the binding:
        reason_code == "expired"
  3. Explicit REGISTER expires=0 (sip_unregister):
        reason_code == "unregistered"   (historical behaviour, guarded)
  4. PUBLISH presence TTL lapses while the REGISTER stays alive (agent went
     home without logging out) — presence sweeper flips the agent offline:
        agent_state_changed.reason_code == "expired"
"""

from __future__ import annotations

import asyncio
import os

import pytest
import pytest_asyncio

from helpers.assertions.event_schema import assert_reason_code
from helpers.restsend_agent import CLI as RESTSEND_CLI
from helpers.restsend_agent import RestsendAgent

pytestmark = [pytest.mark.tier2, pytest.mark.presence]

AGENT = "1001"

# Binding expiry = client expires + LOCATOR_EXPIRE_GRACE_SECS (30s), observed
# by the 15s sweeper tick.
EXPIRY_DEADLINE = 90.0
# Presence TTL sweep runs on the 60s cleanup cadence.
PUBLISH_TTL_DEADLINE = 100.0


def _restsend_available() -> bool:
    return os.path.exists(RESTSEND_CLI)


async def _wait_agent_status(api, agent_id: str, wanted: str, timeout: float) -> str:
    """Poll GET /api/cc/agents/{id} until status == wanted; return last seen."""
    deadline = asyncio.get_event_loop().time() + timeout
    status = ""
    while asyncio.get_event_loop().time() < deadline:
        try:
            info = await api.get_agent(agent_id)
            status = ((info.get("data") or info).get("status") or "").lower()
        except Exception:  # noqa: BLE001 — transient 5xx during restarts
            status = ""
        if status == wanted:
            return status
        await asyncio.sleep(1)
    return status


async def _bring_online(agent: RestsendAgent, api, *, transport=None,
                        expires: int = 120) -> None:
    """Register (+wait until the bridge lists the agent idle)."""
    assert await agent.register(expires=expires, transport=transport), (
        f"{AGENT} REGISTER failed (transport={transport})"
    )
    status = await _wait_agent_status(api, AGENT, "idle", 20.0)
    assert status == "idle", (
        f"agent must be idle while registered, got {status!r}"
    )
    # Outlive UNREGISTER_GRACE_SECS (5s): a transport-close arriving inside
    # the grace window is deliberately ignored by the locator (rapid
    # reconnect / NAT port reuse protection), so an immediate kill would
    # leave the binding alive and never produce an offline event.
    await asyncio.sleep(6)


async def _assert_offline_events(checker, reason: str, timeout: float) -> None:
    """Both RWI events must arrive carrying reason_code == reason.

    Asserted on the webhook channel; `checker.rwi` (RWI WebSocket) is
    additionally asserted for the event types when available — the two
    transports are fanned out from the same gateway dispatch.
    """
    webhook = checker.webhook
    unreg = await webhook.wait_for_event(
        "agent_unregistered",
        match={"payload.agent_id": AGENT, "payload.reason_code": reason},
        timeout=timeout,
    )
    assert unreg is not None, (
        f"agent_unregistered(reason_code={reason!r}) never arrived; "
        f"got {webhook.event_types()}"
    )
    state = await webhook.wait_for_event(
        "agent_state_changed",
        match={
            "payload.agent_id": AGENT,
            "payload.to_status": "offline",
            "payload.reason_code": reason,
        },
        timeout=10,
    )
    assert state is not None, (
        f"agent_state_changed(to=offline, reason_code={reason!r}) never "
        f"arrived; got {webhook.event_types()}"
    )
    # Strict schema: reason_code must be present and canonical.
    assert_reason_code(unreg, expected=reason)
    assert_reason_code(state, expected=reason)
    # RWI WebSocket must mirror the same reason (real-time consumers).
    # Scan the received buffer for the OFFLINE transition — earlier
    # idle-boundary transitions (offline→idle on REGISTER) share the type.
    if checker.rwi is not None:
        deadline = asyncio.get_event_loop().time() + 10

        def _offline_ws_event():
            for e in reversed(checker.rwi.events):
                p = e.get("payload") or e
                if (
                    (e.get("event_type") or e.get("type")) == "agent_state_changed"
                    and p.get("agent_id") == AGENT
                    and p.get("to_status") == "offline"
                ):
                    return p
            return None

        payload = _offline_ws_event()
        while payload is None and asyncio.get_event_loop().time() < deadline:
            await asyncio.sleep(0.3)
            payload = _offline_ws_event()
        assert payload is not None, (
            "RWI WS never delivered agent_state_changed(to=offline); "
            f"got {[e.get('event_type') for e in checker.rwi.events]}"
        )
        assert payload.get("reason_code") == reason, (
            f"RWI WS agent_state_changed.reason_code must be {reason!r}, "
            f"got {payload.get('reason_code')!r}"
        )


@pytest_asyncio.fixture
async def restsend_agent(pbx):
    if not _restsend_available():
        pytest.skip(f"restsend-cli not built: {RESTSEND_CLI}")
    agent = RestsendAgent(pbx, AGENT)
    await agent.start()
    yield agent
    agent.kill()


@pytest.mark.asyncio
async def test_ws_drop_offline_reason(pbx, api, event_checker, restsend_agent):
    """WS transport drop (browser killed) → reason_code = "offline"."""
    await _bring_online(restsend_agent, api, transport="ws")
    event_checker.webhook.clear()

    # Kill -9: no unregister, no close frame from the client side — the PBX
    # still sees the WS connection die (read error / close), which is the
    # transport-close path.
    restsend_agent.kill()

    await _assert_offline_events(event_checker, "offline", 30.0)
    status = await _wait_agent_status(api, AGENT, "offline", 15.0)
    assert status == "offline", f"panel must show offline, got {status!r}"


@pytest.mark.asyncio
async def test_register_expiry_expired_reason(pbx, api, event_checker,
                                              restsend_agent):
    """REGISTER TTL lapses with no goodbye (UDP) → sweeper → reason "expired"."""
    # Short client expiry: binding dies at expires+30s grace, sweeper (15s
    # tick) picks it up. UDP has no transport-close — only the TTL remains.
    await _bring_online(restsend_agent, api, transport="udp", expires=10)
    event_checker.webhook.clear()

    restsend_agent.kill()

    await _assert_offline_events(event_checker, "expired", EXPIRY_DEADLINE)
    status = await _wait_agent_status(api, AGENT, "offline", 15.0)
    assert status == "offline", f"panel must show offline, got {status!r}"


@pytest.mark.asyncio
async def test_explicit_unregister_reason(pbx, api, event_checker,
                                          restsend_agent):
    """REGISTER expires=0 → reason_code = "unregistered" (historical)."""
    await _bring_online(restsend_agent, api, transport="udp")
    event_checker.webhook.clear()

    assert await restsend_agent.unregister(), "sip_unregister did not complete"

    await _assert_offline_events(event_checker, "unregistered", 30.0)
    status = await _wait_agent_status(api, AGENT, "offline", 15.0)
    assert status == "offline", f"panel must show offline, got {status!r}"


@pytest.mark.asyncio
async def test_publish_ttl_expired_reason(pbx, api, event_checker,
                                          restsend_agent):
    """PUBLISH TTL lapses while REGISTER stays alive → agent_state_changed
    carries reason_code = "expired" (presence sweeper path)."""
    await _bring_online(restsend_agent, api, transport="udp")
    # Publish with a short TTL; the REGISTER refresher keeps the binding
    # alive but nothing re-publishes — exactly the "went home without
    # logging out" production case.
    await restsend_agent.publish_idle(expires=10)
    event_checker.webhook.clear()

    webhook = event_checker.webhook
    ev = await webhook.wait_for_event(
        "agent_state_changed",
        match={
            "payload.agent_id": AGENT,
            "payload.to_status": "offline",
            "payload.reason_code": "expired",
        },
        timeout=PUBLISH_TTL_DEADLINE,
    )
    assert ev is not None, (
        "presence-TTL expiry never produced agent_state_changed("
        "to=offline, reason_code=expired); got "
        f"{webhook.event_types()}"
    )
    status = await _wait_agent_status(api, AGENT, "offline", 15.0)
    assert status == "offline", f"panel must show offline, got {status!r}"
