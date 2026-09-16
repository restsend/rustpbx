"""Tier 2 — declarative CC workflow scenarios.

Every scenario in ``scenarios/*.toml`` runs end-to-end against the
session-scoped pbx via the :class:`Orchestrator`. The complete RWI event-hook
JSON flow (webhook + RWI WebSocket payloads) of each scenario is dumped to
``report/flows/`` by the conftest report hook and linked from the HTML report.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from helpers.orchestrator import Orchestrator
from helpers.scenario import Scenario

pytestmark = [pytest.mark.tier2, pytest.mark.scenario]

SCENARIOS_DIR = Path(__file__).resolve().parents[2] / "scenarios"


def _scenarios() -> list[Path]:
    return sorted(SCENARIOS_DIR.glob("*.toml"))


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "scenario_path",
    [str(p) for p in _scenarios()],
    ids=[p.stem for p in _scenarios()],
)
async def test_scenario_workflow(
    scenario_path, request, pbx, sipbot_pool, api, event_checker
):
    scenario = Scenario.load(Path(scenario_path))
    orch = Orchestrator(pbx, sipbot_pool, api, event_checker)
    result = await orch.run(scenario)

    # Expose this scenario's run trace + RWI events so the conftest report
    # hook can attach the full JSON event-hook flow to the test record.
    request.node._scenario_trace = result
    request.node._rwi_events = list(event_checker.rwi.events)

    if not result["ok"]:
        pytest.fail(result["error"])
