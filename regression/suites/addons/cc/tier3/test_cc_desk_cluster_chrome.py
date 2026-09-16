"""Tier 3 — cc-desk Chrome E2E (cluster consult / 3-way / WS resilience / forced release).

Runs the Playwright suite in cc-desk-e2e against a dedicated PBX instance
booted by run-pbx.sh (same as cc-desk regression). Keeps cc addon regression
coverage for desk call-control flows that require real Chrome + SIP.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

CC_DESK_E2E = Path(__file__).resolve().parents[3] / "cc-desk-e2e"
SPEC = "tests/cluster-resilience.spec.js"

pytestmark = [pytest.mark.tier3, pytest.mark.cc_desk, pytest.mark.slow, pytest.mark.supervisor]


@pytest.mark.asyncio
async def test_cc_desk_cluster_resilience_chrome():
    """Owner-anchored consult merge, mid-call WS recovery, supervisor takeover."""
    if not (CC_DESK_E2E / "run-pbx.sh").is_file():
        pytest.skip("cc-desk-e2e harness missing")

    env = os.environ.copy()
    proc = subprocess.run(
        ["bash", "./run-pbx.sh", SPEC, "--workers=1"],
        cwd=str(CC_DESK_E2E),
        env=env,
        capture_output=True,
        text=True,
        timeout=600,
    )
    if proc.returncode != 0:
        tail = (proc.stdout or "") + (proc.stderr or "")
        pytest.fail(f"cc-desk cluster chrome e2e failed (exit {proc.returncode}):\n{tail[-4000:]}")
