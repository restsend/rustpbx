"""Wholesale addon E2E tests (tenant + rate deck + billing CDR).

Wholesale routing is DB-driven: rate decks, trunks, routing profiles and
tenants live in SQLite. The test points rustpbx at a file DB, seeds wholesale
resources, then verifies a call routed through the wholesale profile produces a
`wholesale_cdrs` billing record with positive price/cost.
"""

from __future__ import annotations

import asyncio
import sys
import uuid
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.wholesale, pytest.mark.slow]

_WS = Path(h.__file__).resolve().parents[2] / "src" / "addons" / "wholesale" / "tests"
if str(_WS) not in sys.path:
    sys.path.insert(0, str(_WS))
from e2e_wholesale_test import WholesaleDb  # noqa: E402


@pytest.mark.asyncio
async def test_wholesale_call_billing(pbx_config, pbx, sipbot_pool, tmp_path):
    """Seeded tenant routes an outbound call through a carrier; billing CDR written."""
    db_path = tmp_path / "wholesale.sqlite3"

    # Point the PBX at a file DB (rustpbx migrations create the schema on start),
    # and enable the wholesale addon with the carrier + inbound trunks.
    pbx_config.database_url = f"sqlite://{db_path}"
    pbx_config.set_wholesale()
    pbx_config.add_trunk(
        "E2E-Carrier", dest="127.0.0.1:15190", direction="outbound", trunk_id=1001,
    )
    pbx_config.add_trunk(
        "E2E-Inbound", dest="127.0.0.1:15190", direction="inbound", trunk_id=1002,
        inbound_hosts=["127.0.0.1"],
    )

    # First start runs migrations against the file DB.
    # (pbx fixture already started with sqlite::memory: — restart against the file.)
    pbx.stop()
    pbx.prepare(webhook_url="", extra_features=["addon-wholesale"], build=False)
    pbx.start(timeout=90)

    # Seed wholesale resources now that the schema exists.
    db = WholesaleDb(str(db_path))
    try:
        _seed(db)
    finally:
        db.close()

    # Restart so the wholesale addon loads the seeded trunks/profile.
    pbx.stop()
    pbx.start(timeout=90)

    # Carrier UAS answers the outbound trunk; caller dials a wholesale-routed number.
    carrier = sipbot_pool.callee(
        host=pbx.host, port=15190, username="carrier", password="123456",
        register=False, ring_secs=1, answer_mode="echo",
    )
    caller = sipbot_pool.caller(
        target=f"sip:91001234567@{pbx.sip_addr}", username="caller", password="123456",
        hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established|4[0-9][0-9]", timeout=25), caller.output

    deadline = asyncio.get_event_loop().time() + 15
    rows: list[dict] = []
    db2 = WholesaleDb(str(db_path))
    try:
        while asyncio.get_event_loop().time() < deadline:
            rows = db2.all(
                "select call_id, tenant_id, price_total, cost_total, status "
                "from wholesale_cdrs order by id desc limit 5"
            )
            if rows:
                break
            await asyncio.sleep(0.5)
    finally:
        db2.close()

    assert rows, "no wholesale_cdrs record produced"
    rec = rows[0]
    assert rec["tenant_id"] is not None, rec
    assert float(rec["price_total"] or 0) > 0, f"expected positive price: {rec}"


def _seed(db: WholesaleDb) -> None:
    """Seed a minimal tenant / rate / trunk / routing profile."""
    sell_deck = db.ensure_rate_deck("E2E-Sell", "sell")
    buy_deck = db.ensure_rate_deck("E2E-Buy", "buy")
    db.ensure_rate(sell_deck, "1", 0.10, 60, 60)
    db.ensure_rate(buy_deck, "1", 0.05, 60, 60)

    out = db.ensure_outbound_trunk("E2E-Carrier", "127.0.0.1:15190")
    inc = db.ensure_inbound_trunk(
        "E2E-Inbound", ip_acl="127.0.0.1", caller_prefix=None, callee_prefix="9"
    )
    prof = db.ensure_profile("E2E-Profile")
    db.ensure_profile_item(prof, out, "1", priority=1)

    tenant_id, _ = db.ensure_tenant(
        "E2E-Tenant", initial_balance=5000.0, rate_deck_id=sell_deck,
        routing_profile_id=prof,
    )
    db.ensure_tenant_trunk_link(tenant_id, inc)
    db.ensure_wholesale_trunk_config(out, buy_deck)
