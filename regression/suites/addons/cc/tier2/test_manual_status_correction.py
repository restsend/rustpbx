"""Tier 2 — manual status switching corrects state and leaves a reason trail.

Production driver (2026-10-14): agents were flipped offline by presence-side
events with no visible cause and no self-service recovery path. Contracts
locked here:

  1. Manual switch (console REST POST /status) works from Offline → Idle and
     leaves `reason_code = "manual"` on BOTH transitions in
     `cc_agent_status_history` (read back via
     GET /cc/agents/{id}/status_history).
  2. The manual correction STICKS while the client stays registered — a
     connected agent must not be yanked back offline by any sweeper
     (offline watchdog's 30s ACD tick included).
  3. The switch matrix round-trips (idle ↔ dnd) with a manual stamp on each
     hop, filterable via the status_history endpoint.

(Manual OFFLINE while the agent is Ringing/Busy defers to
`auto_offline_after_wrapup` — covered by the registrar-bridge unit tests,
not duplicated here.)
"""

from __future__ import annotations

import asyncio
import os

import pytest
import pytest_asyncio

from helpers.restsend_agent import CLI as RESTSEND_CLI
from helpers.restsend_agent import RestsendAgent

pytestmark = [pytest.mark.tier2, pytest.mark.presence]

AGENT = "1001"
# Outlive at least one full offline-watchdog tick (e2e workdir config: 10s
# ACD cadence): the "sticks" assertion is only meaningful past a reconcile
# opportunity.
STICKS_WINDOW = 15.0


def _restsend_available() -> bool:
    return os.path.exists(RESTSEND_CLI)


async def _wait_agent_status(api, agent_id: str, wanted: str, timeout: float) -> str:
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


async def _status_history(api, agent_id: str, **params) -> list[dict]:
    qs = "&".join(f"{k}={v}" for k, v in params.items())
    resp = await api.get(
        f"/api/cc/agents/{agent_id}/status_history" + (f"?{qs}" if qs else "")
    )
    return (resp.get("data") or [])


def _ok(resp) -> bool:
    return isinstance(resp, dict) and resp.get("success") is not False


@pytest_asyncio.fixture
async def restsend_agent(pbx):
    if not _restsend_available():
        pytest.skip(f"restsend-cli not built: {RESTSEND_CLI}")
    agent = RestsendAgent(pbx, AGENT)
    await agent.start()
    yield agent
    agent.kill()


@pytest.mark.asyncio
async def test_manual_switch_offline_to_idle_sticks_and_records_reason(
    pbx, api, restsend_agent
):
    """Manual Offline→Idle corrects the state, sticks while registered,
    and every hop lands in the history log with reason_code="manual"."""
    assert await restsend_agent.register(transport="udp"), "REGISTER failed"
    status = await _wait_agent_status(api, AGENT, "idle", 20.0)
    assert status == "idle", f"agent must start idle, got {status!r}"

    # Manual logout (the 下线 button) → offline.
    assert _ok(await api.update_agent_status(AGENT, "offline"))
    status = await _wait_agent_status(api, AGENT, "offline", 10.0)
    assert status == "offline", f"manual offline must apply, got {status!r}"

    # THE CORRECTION: manual switch back to Idle.
    assert _ok(await api.update_agent_status(AGENT, "idle"))
    status = await _wait_agent_status(api, AGENT, "idle", 10.0)
    assert status == "idle", f"manual correction must apply, got {status!r}"

    # The correction must STICK: the UA is still registered, so no sweeper
    # (offline watchdog included) may yank the agent back offline.
    await asyncio.sleep(STICKS_WINDOW)
    status = await _wait_agent_status(api, AGENT, "idle", 1.0)
    assert status == "idle", (
        f"manual idle must stick while the registration is alive, got {status!r}"
    )

    # The reason trail: both hops carry reason_code="manual".
    rows = await _status_history(api, AGENT, per_page=20)
    manual_hops = [
        (r["from_status"], r["to_status"])
        for r in rows
        if r.get("reason_code") == "manual"
    ]
    assert ("idle", "offline") in manual_hops, f"missing manual offline row: {rows}"
    assert ("offline", "idle") in manual_hops, (
        f"missing manual correction row: {rows}"
    )


@pytest.mark.asyncio
async def test_manual_switch_matrix_round_trip(pbx, api, restsend_agent):
    """idle ↔ dnd round trip with a manual stamp on each hop."""
    assert await restsend_agent.register(transport="udp"), "REGISTER failed"
    status = await _wait_agent_status(api, AGENT, "idle", 20.0)
    assert status == "idle"

    for wanted in ("dnd", "idle", "dnd", "idle"):
        assert _ok(await api.update_agent_status(AGENT, wanted))
        status = await _wait_agent_status(api, AGENT, wanted, 10.0)
        assert status == wanted, f"manual switch to {wanted} failed, got {status!r}"

    rows = await _status_history(api, AGENT, to_status="dnd", reason="manual")
    assert len(rows) >= 2, f"each manual dnd hop must be recorded: {rows}"


@pytest.mark.asyncio
async def test_status_history_filters_and_pagination(pbx, api, restsend_agent):
    """The status_history endpoint honours to_status/reason filters and
    pagination (the console drawer contract)."""
    assert await restsend_agent.register(transport="udp"), "REGISTER failed"
    status = await _wait_agent_status(api, AGENT, "idle", 20.0)
    assert status == "idle"

    for wanted in ("dnd", "idle"):
        assert _ok(await api.update_agent_status(AGENT, wanted))
        assert await _wait_agent_status(api, AGENT, wanted, 10.0) == wanted

    filtered = await _status_history(
        api, AGENT, to_status="dnd", reason="manual", per_page=1
    )
    assert len(filtered) == 1, f"per_page=1 must cap the page: {filtered}"
    assert filtered[0]["to_status"] == "dnd"
    assert filtered[0]["reason_code"] == "manual"
    assert filtered[0].get("created_at"), "row must carry a timestamp"

    total = await _status_history(api, AGENT, per_page=200)
    assert len(total) >= 3, f"expected ≥3 transitions, got {len(total)}"
