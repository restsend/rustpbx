"""ACD cluster e2e conftest — fixtures for the remote prd dual-node cluster.

Overrides the session-scoped `ensure_rustpbx_binary` (these tests never boot
a local PBX — everything runs against the deployed cluster at
RUSTPBX_PRD_HOST, default 192.168.3.9).
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path

import pytest
import pytest_asyncio

SCRIPT_DIR = Path(__file__).resolve().parent.parent.parent  # regression/
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from helpers.prd_cluster import (  # noqa: E402
    PrdCluster, PrdRestsendAgent, SuiteTopology,
)
from helpers.sipbot import SipBotPool  # noqa: E402

# Skip the whole suite unless the prd cluster is explicitly targeted (default
# ON — this directory only exists for that cluster; override with
# RUSTPBX_PRD_E2E=0 to disable in generic CI runs).
PRD_E2E_ENABLED = os.environ.get("RUSTPBX_PRD_E2E", "1") == "1"


@pytest.fixture(scope="session")
def ensure_rustpbx_binary() -> None:
    """No local binary needed — remote cluster only."""


@pytest.fixture(scope="session")
def prd() -> PrdCluster:
    cluster = PrdCluster()
    # fail fast with a readable error if the cluster is unreachable
    agents = cluster.call_node.agents()
    assert agents, "prd call-node /cc/agents returned no agents — cluster reachable?"
    yield cluster


@pytest_asyncio.fixture
async def pool() -> SipBotPool:
    p = SipBotPool()
    yield p
    p.terminate_all()


class AgentFleet:
    """Session-scoped restsend agent pool (one long-lived UA per SIP user).

    Why persistent: each fresh CLI process binds a NEW local port, and the
    server-side locator keeps the previous binding until it expires (~50s).
    Re-spawning UAs per test piles up stale contacts — dispatch INVITEs then
    land on dead ports (ring timeout flakiness). One UA per user for the
    whole suite keeps exactly one live binding per agent.
    """

    _next_port = 26900  # session-wide, above per-test caller ports

    def __init__(self, prd: PrdCluster):
        self.prd = prd
        self._by_user: dict[str, PrdRestsendAgent] = {}
        self._order: list[PrdRestsendAgent] = []

    async def agents(self, users: list[str]) -> list[PrdRestsendAgent]:
        """Return UAs for the users, spawning (registering) as needed."""
        out = []
        for user in users:
            if user not in self._by_user:
                AgentFleet._next_port += 1
                ua = PrdRestsendAgent(user, AgentFleet._next_port)
                await ua.start()
                self._by_user[user] = ua
                self._order.append(ua)
            out.append(self._by_user[user])
        return out

    async def stop_all(self) -> None:
        for agent in self._order:
            try:
                await agent.stop()
            except Exception:  # noqa: BLE001
                pass
        self._order.clear()
        self._by_user.clear()


@pytest_asyncio.fixture(scope="session")
async def fleet(prd: PrdCluster) -> AgentFleet:
    """One persistent UA per agent user for the whole suite run."""
    import subprocess
    subprocess.run(["pkill", "-f", "restsend-cli"], check=False)
    await asyncio.sleep(1.0)  # let stale registrations start expiring
    f = AgentFleet(prd)
    yield f
    await f.stop_all()


@pytest.fixture
def topology_factory(prd: PrdCluster):
    """Deploys an exclusive SuiteTopology; tears down after the test."""
    deployed: list[SuiteTopology] = []

    def _factory(**kwargs) -> SuiteTopology:
        topo = SuiteTopology(prd, **kwargs).deploy()
        deployed.append(topo)
        return topo

    yield _factory
    for topo in reversed(deployed):
        try:
            topo.teardown()
        except Exception as exc:  # noqa: BLE001
            print(f"topology teardown error: {exc}")
