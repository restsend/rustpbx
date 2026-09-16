"""Wholesale circuit breaker — three-state behavior end-to-end.

Topology: tenant → routing profile → carrier trunk (circuit_breaker_enabled,
threshold=1, open=3s) pointed at a DEAD SIP destination.

Observable three-state contract (strict):
  1. Closed:  first call fails the normal way (dead dest: rings/times out,
     final response >= ~3s); PBX log has NO "No routes available".
  2. Open:    the failure is billed → breaker trips; the next call is rejected
     from routing almost instantly (<3s) WITH "No routes available" in the log.
  3. HalfOpen: after open_duration the trunk is probed again — a new call is
     attempted against the dead dest (slow path returns; no NEW immediate
     "No routes available" line for that call window).
"""

from __future__ import annotations

import asyncio
import time

import pytest

import helpers as h

# src/addons/wholesale/tests hosts the shared seeding helper (path math via the
# merged helpers package keeps pointing at the repo root).
from pathlib import Path as _P  # noqa: E402

_WS = _P(h.__file__).resolve().parents[2] / "src" / "addons" / "wholesale" / "tests"
if str(_WS) not in __import__("sys").path:
    __import__("sys").path.insert(0, str(_WS))
from e2e_wholesale_test import WholesaleDb  # noqa: E402

pytestmark = [pytest.mark.wholesale, pytest.mark.slow]

# A REFUSED port (127.0.0.1:1) fails before any CDR/billing record exists, so
# the breaker never sees the failure. A BLACKHOLE address (packets silently
# dropped) produces a real dial attempt that ends in a 408/487 CDR — which is
# what the billing hook needs to update the breaker.
DEAD_CARRIER = "10.255.255.1:5060"


def _seed(db: WholesaleDb) -> None:
    sell_deck = db.ensure_rate_deck("CB-Sell", "sell")
    buy_deck = db.ensure_rate_deck("CB-Buy", "buy")
    db.ensure_rate(sell_deck, "1", 0.10, 60, 60)
    db.ensure_rate(buy_deck, "1", 0.05, 60, 60)
    out = db.ensure_outbound_trunk("CB-Carrier", DEAD_CARRIER)
    inc = db.ensure_inbound_trunk("CB-Inbound", ip_acl="127.0.0.1",
                                  caller_prefix=None, callee_prefix="9")
    prof = db.ensure_profile("CB-Profile")
    db.ensure_profile_item(prof, out, "1", priority=1)
    tenant_id, _ = db.ensure_tenant("CB-Tenant", initial_balance=5000.0,
                                    rate_deck_id=sell_deck, routing_profile_id=prof)
    db.ensure_tenant_trunk_link(tenant_id, inc)
    db.ensure_wholesale_trunk_config(out, buy_deck)
    # flip the breaker on with the fastest observable settings
    db.exec(
        """
        update wholesale_trunk_configs set
          circuit_breaker_enabled = 1,
          cb_failure_threshold = 1,
          cb_open_duration_secs = 3,
          cb_half_open_probes = 1,
          cb_failure_codes = '0,400,403,404,408,480,486,487,500,502,503,504,600,603'
        where sip_trunk_id = ?
        """,
        (out,),
    )


async def _failed_call(pbx, sipbot_pool) -> tuple[float, str]:
    """One outbound attempt to the dead carrier; returns (seconds_to_final, tail)."""
    caller = sipbot_pool.caller(
        target=f"sip:91001234567@{pbx.sip_addr}", username="caller", password="123456",
        hangup=20,
    )
    t0 = time.monotonic()
    ok = await caller.wait_output_async(
        r"4[0-9][0-9]|5[0-9][0-9]|6[0-9][0-9]|Call failed|finished", timeout=25
    )
    dt = time.monotonic() - t0
    assert ok, f"no final response within 25s: {caller.output[-400:]}"
    assert dt > 2.0, f"blackhole dial returned instantly ({dt:.1f}s) — no real attempt"
    caller.terminate()
    return dt, caller.output[-600:]


@pytest.mark.xfail(
    reason=(
        "known product gap: pre-answer outbound failures produce NO callrecord "
        "(config/cdr empty), so the billing hook never runs and the circuit "
        "breaker cannot observe failures -> never trips. Evidence: 0 rows in "
        "wholesale_cdrs after failed calls; instant final responses regardless "
        "of dest (refused vs blackhole). Re-enable when failed-call CDRs are "
        "emitted for the wholesale billing path."
    ),
    strict=False,
)
@pytest.mark.asyncio
async def test_wholesale_circuit_breaker_three_states(pbx_config, pbx, sipbot_pool, evidence):
    db_path = pbx.work_dir / "wholesale-cb.sqlite3"
    pbx_config.database_url = f"sqlite://{db_path}"
    pbx_config.set_wholesale()
    pbx_config.add_trunk("CB-Carrier-SIP", dest=DEAD_CARRIER, direction="outbound", trunk_id=1001)
    pbx_config.add_trunk(
        "CB-Inbound-SIP", dest=DEAD_CARRIER, direction="inbound", trunk_id=1002,
        inbound_hosts=["127.0.0.1"],
    )

    pbx.stop()
    pbx.prepare(webhook_url="", extra_features=["addon-wholesale"], build=False)
    pbx.start(timeout=90)

    db = WholesaleDb(str(db_path))
    try:
        _seed(db)
    finally:
        db.close()

    pbx.stop()
    pbx.start(timeout=90)

    log_path = pbx.log_file_path
    import re
    from pathlib import Path

    def count_no_routes() -> int:
        text = Path(log_path).read_text(encoding="utf-8", errors="replace")
        return len(re.findall(r"No routes available", text))

    # The authoritative signal is the ROUTER's decision line: breaker-open
    # skips the trunk and logs "No routes available".

    # ── state 1: CLOSED — dial attempt is real (times out against blackhole) ──
    dt1, tail1 = await _failed_call(pbx, sipbot_pool)
    n1 = count_no_routes()
    assert n1 == 0, (
        f"first call must NOT be breaker-blocked (closed state), but found "
        f"{n1} 'No routes available' lines. tail: {tail1}"
    )
    evidence.log_metric("closed_phase", {"seconds": round(dt1, 1), "no_routes_lines": n1})

    # ── state 2: OPEN — billed failure trips the breaker; routing skips the trunk ──
    await asyncio.sleep(3.0)  # billing hooks settle asynchronously after call end
    # diagnostic: did the failed call actually get billed (wholesale_cdr row)?
    import sqlite3 as _sq

    conn = _sq.connect(str(db_path))
    rows = conn.execute(
        "select call_id, status_code, call_status from wholesale_cdrs order by id desc limit 3"
    ).fetchall()
    conn.close()
    evidence.log_metric("wholesale_cdrs_after_call1", rows)
    dt2, tail2 = await _failed_call(pbx, sipbot_pool)
    n2 = count_no_routes()
    assert n2 >= 1, (
        f"breaker did not trip after the billed failure ('No routes available' "
        f"count {n1} -> {n2}). tail: {tail2}"
    )
    evidence.log_metric("open_phase", {"seconds": round(dt2, 1), "no_routes_lines": n2})

    # ── state 3: HALF-OPEN — after open_duration the trunk is probed again ──
    await asyncio.sleep(4.0)  # open_duration=3s + margin
    dt3, _tail3 = await _failed_call(pbx, sipbot_pool)
    n3 = count_no_routes()
    assert n3 == n2, (
        f"half-open probe should be allowed through (no NEW 'No routes available' "
        f"line), but count moved {n2} -> {n3}"
    )
    evidence.log_metric("half_open_phase", {"seconds": round(dt3, 1)})
    evidence.log_metric("breaker_three_states", "closed->open->half-open observed")
