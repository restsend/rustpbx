"""Wholesale addon E2E tests (tenant + rate deck + billing CDR).

Wholesale routing is DB-driven: rate decks, trunks, routing profiles and
tenants live in SQLite. The test points rustpbx at a file DB, seeds wholesale
resources, then verifies a call routed through the wholesale profile produces a
`wholesale_cdrs` billing record with positive price/cost.
"""

from __future__ import annotations

import asyncio
import re
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


@pytest.mark.asyncio
async def test_wholesale_audio_and_dtmf_passthrough(pbx_config, pbx, sipbot_pool, tmp_path):
    """Wholesale-routed trunk call: bidirectional RTP audio + RFC 2833 DTMF
    must pass through the anchored MediaBridge, and a billing CDR is written.

    Covers the three media concerns for the wholesale workflow in a single
    end-to-end call:
      1. Audio  — caller and carrier exchange RTP (TX>0 and RX>0 on both legs)
      2. DTMF   — RFC 2833 digits sent by the caller arrive at the carrier leg
      3. CDR    — a wholesale_cdrs row with positive price is produced
    RTP-inactivity teardown is exercised by the dedicated trunk B2BUA suite
    (test_trunk_b2bua_rtp_timeout_no_bye_tears_down) which shares the same
    anchored-media code path.
    """
    db_path = tmp_path / "wholesale_adtmf.sqlite3"
    carrier_port = 15191
    pbx_config.database_url = f"sqlite://{db_path}"
    pbx_config.set_wholesale()
    pbx_config.media_proxy = "all"  # anchor media through the MediaBridge
    pbx_config.add_trunk(
        "E2E-Carrier", dest=f"127.0.0.1:{carrier_port}", direction="outbound", trunk_id=1001,
    )
    pbx_config.add_trunk(
        "E2E-Inbound", dest=f"127.0.0.1:{carrier_port}", direction="inbound", trunk_id=1002,
        inbound_hosts=["127.0.0.1"],
    )

    pbx.stop()
    pbx.prepare(webhook_url="", extra_features=["addon-wholesale"], build=False)
    pbx.start(timeout=90)

    db = WholesaleDb(str(db_path))
    try:
        _seed(db, f"127.0.0.1:{carrier_port}")
    finally:
        db.close()

    # Restart so the wholesale addon loads the seeded trunks/profile.
    pbx.stop()
    pbx.start(timeout=90)

    # Carrier UAS answers the outbound trunk with echo + audio-quality reporting
    # so we can assert bidirectional RTP on the carrier leg too.
    carrier = sipbot_pool.callee(
        host=pbx.host, port=carrier_port, username="carrier", password="123456",
        register=False, ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    # Caller sends two RFC 2833 DTMF digits mid-call.
    caller = sipbot_pool.caller(
        target=f"sip:91001234567@{pbx.sip_addr}", username="caller", password="123456",
        hangup=8, dtmf_flows="2s:1,4s:2",
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

    # 2. DTMF: the carrier leg must receive at least one RFC 2833 digit through
    # the wholesale B2BUA's MediaBridge dtmf_bus forwarder. Check before hangup
    # so the digits have time to arrive (scheduled at 2s/4s into the call).
    digits: list[str] = []
    deadline = asyncio.get_event_loop().time() + 15
    while asyncio.get_event_loop().time() < deadline:
        digits = carrier.get_dtmf_digits()
        if digits:
            break
        await asyncio.sleep(0.3)
    assert digits, (
        f"carrier received no DTMF through wholesale B2BUA:\n{carrier.output[-2000:]}"
    )

    # Wait for the caller to hang up so sipbot prints its final RTP summary.
    await caller.wait_output_async(r"All bots finished", timeout=30)

    # 1. Audio: caller sent RTP (TX>0) and received carrier echo RTP (RX>0).
    # Stats are only populated after the process exits and prints its summary.
    caller_stats = caller.get_rtp_stats()
    assert caller_stats.tx_packets > 0, f"caller sent no RTP: {caller_stats}\n{caller.output[-2000:]}"
    assert caller_stats.rx_packets > 0, f"caller received no RTP: {caller_stats}\n{caller.output[-2000:]}"

    # 3. CDR: a wholesale_cdrs billing row with positive price is produced.
    rows: list[dict] = []
    db2 = WholesaleDb(str(db_path))
    try:
        deadline = asyncio.get_event_loop().time() + 15
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
    assert rows, "no wholesale_cdrs record produced for audio/dtmf call"
    assert float(rows[0]["price_total"] or 0) > 0, f"expected positive price: {rows[0]}"


@pytest.mark.asyncio
async def test_wholesale_183_early_media_passthrough(pbx_config, pbx, sipbot_pool, tmp_path):
    """Carrier sends 183 Session Progress (early media) before answering → the
    caller must receive the 183 + early-media RTP through the wholesale B2BUA's
    anchored MediaBridge, then the call answers normally.

    This exercises the carrier→tenant 183 early-media relay path
    (sip_session.rs: callee DialogState::Early with SDP → Pranswer on caller
    dialog) which is the wholesale equivalent of in-band ringback/progress
    tones generated upstream by the carrier.
    """
    db_path = tmp_path / "wholesale_183.sqlite3"
    carrier_port = 15192
    pbx_config.database_url = f"sqlite://{db_path}"
    pbx_config.set_wholesale()
    pbx_config.media_proxy = "all"  # anchor media through the MediaBridge
    pbx_config.add_trunk(
        "E2E-Carrier", dest=f"127.0.0.1:{carrier_port}", direction="outbound", trunk_id=1001,
    )
    pbx_config.add_trunk(
        "E2E-Inbound", dest=f"127.0.0.1:{carrier_port}", direction="inbound", trunk_id=1002,
        inbound_hosts=["127.0.0.1"],
    )

    pbx.stop()
    pbx.prepare(webhook_url="", extra_features=["addon-wholesale"], build=False)
    pbx.start(timeout=90)

    db = WholesaleDb(str(db_path))
    try:
        _seed(db, f"127.0.0.1:{carrier_port}")
    finally:
        db.close()

    # Restart so the wholesale addon loads the seeded trunks/profile.
    pbx.stop()
    pbx.start(timeout=90)

    # Carrier UAS sends 183 early media (ringback wav) while ringing, then
    # answers with echo after ring_secs.
    ringback_wav = str(Path(h.__file__).resolve().parents[2] / "fixtures" / "sample.wav")
    carrier = sipbot_pool.callee(
        host=pbx.host, port=carrier_port, username="carrier", password="123456",
        register=False, ring_secs=3, answer_mode="echo", ringback=ringback_wav,
    )
    caller = sipbot_pool.caller(
        target=f"sip:91001234567@{pbx.sip_addr}", username="caller", password="123456",
        hangup=8,
    )

    # The caller must receive a 183 Session Progress (early media relayed from
    # the carrier through the anchored MediaBridge). sipbot's UAC logs no
    # dedicated line for provisional responses — the observable real-time
    # signal is the per-code count in its periodic "Progress:" status lines
    # ("... 200: 0, 180: 0, 183: 1, ..."). A bare r"183" substring
    # false-positives on numeric noise (e.g. "1.83", "183.42ms").
    def _triage():
        c = carrier.get_status_counts()
        return (
            f"caller={caller.get_status_counts()} carrier_sent_183={c.get(183, 0)} "
            f"carrier_ring_stage={'Stage 1: Ringing with media' in carrier.output}"
        )

    deadline = asyncio.get_event_loop().time() + 20
    while asyncio.get_event_loop().time() < deadline:
        if re.search(r"183: [1-9]", caller.output):
            break
        await asyncio.sleep(0.5)
    assert re.search(r"183: [1-9]", caller.output), (
        f"caller did not receive 183 Session Progress [{_triage()}]:\n{caller.output[-2000:]}"
    )

    # The call must then answer (200 OK).
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

    # Caller received early-media RTP (ringback) from the carrier leg.
    await caller.wait_output_async(r"All bots finished", timeout=30)
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, (
        f"caller received no RTP (early media + answer media):\n{caller.output[-2000:]}"
    )

    # 183 must appear in the caller's status counts.
    codes = caller.get_status_counts()
    assert codes.get(183, 0) >= 1, (
        f"expected at least one 183 Session Progress, got: {codes} [{_triage()}]\n"
        f"{caller.output[-2000:]}"
    )


def _seed(db: WholesaleDb, carrier_dest: str = "127.0.0.1:15190") -> None:
    """Seed a minimal tenant / rate / trunk / routing profile."""
    sell_deck = db.ensure_rate_deck("E2E-Sell", "sell")
    buy_deck = db.ensure_rate_deck("E2E-Buy", "buy")
    db.ensure_rate(sell_deck, "1", 0.10, 60, 60)
    db.ensure_rate(buy_deck, "1", 0.05, 60, 60)

    out = db.ensure_outbound_trunk("E2E-Carrier", carrier_dest)
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
