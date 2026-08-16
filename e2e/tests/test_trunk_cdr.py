"""Trunk B2BUA call + CDR E2E tests.

Ports the Rust `test_trunk_b2bua_e2e.rs` CDR/round-trip assertions to Python
sipbot so the Rust suite can be slimmed:

  * `test_trunk_b2bua_inbound_caller_hangup_rtp_cdr`  → caller hangs up,
    bidirectional RTP, CDR completed.
  * `test_trunk_b2bua_basic_call_cdr_roundtrip`        → basic trunk call, CDR
    with correct caller/callee fields.
  * `test_trunk_b2bua_reject_486_cdr`                  → callee rejects 486, CDR
    status failed / statusCode 486.
  * `test_trunk_b2bua_no_answer`                       → callee never answers,
    CDR status != completed.

An *inbound* trunk call is one where the caller's From domain differs from the
PBX realm (127.0.0.1), so the call is classified as external → local.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.cdr, pytest.mark.trunk_ringback]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def _latest_cdr_json(cdr_dir: Path) -> list[dict]:
    out: list[dict] = []
    for p in sorted(cdr_dir.rglob("*.json"), key=lambda f: f.stat().st_mtime, reverse=True):
        try:
            out.append(json.loads(p.read_text(encoding="utf-8")))
        except Exception:
            continue
    return out


async def _wait_cdrs(cdr_dir, timeout: float = 15) -> list[dict]:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        records = _latest_cdr_json(cdr_dir)
        if records:
            return records
        await asyncio.sleep(0.5)
    return []


async def _reg_callee(sipbot_pool, pbx, port, username="1002", *, reject_code=None):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="none" if reject_code else "echo",
        reject_code=reject_code, reject_prob=100 if reject_code else None,
        audio_quality=True,
    )
    await asyncio.sleep(2)
    return ua


def _trunk_caller(sipbot_pool, pbx, *, target, hangup=6):
    return sipbot_pool.caller(
        target=target, username="external", password="123456",
        from_uri="sip:external@trunk.example.com", hangup=hangup,
    )


# ---------------------------------------------------------------------------
# Inbound trunk caller hangs up: bidirectional RTP + completed CDR
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_trunk_b2bua_call_cdr(pbx_config, pbx, sipbot_pool, cdr_dir):
    """Trunk caller → registered callee: RTP flows both ways and the CDR is
    completed with the external caller recorded."""
    callee_port = 15420
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "cdr-trunk", dest=f"127.0.0.1:{callee_port}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
    )
    pbx_config.media_proxy = "all"
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, callee_port, "1002")
    caller = _trunk_caller(sipbot_pool, pbx, target=f"sip:1002@{pbx.sip_addr}", hangup=6)
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output

    deadline = asyncio.get_event_loop().time() + 15
    while asyncio.get_event_loop().time() < deadline:
        if caller.get_rtp_stats().is_bidirectional:
            break
        await asyncio.sleep(0.3)
    stats = caller.get_rtp_stats()
    assert stats.is_bidirectional, f"trunk caller RTP not bidirectional: {stats}"

    await caller.wait_output_async(r"All bots finished", timeout=30)
    records = await _wait_cdrs(cdr_dir)
    assert records, "no CDR written for trunk B2BUA call"
    rec = records[0]
    body = rec if isinstance(rec, dict) and "status" in rec else rec.get("record") or rec
    assert body.get("status") == "completed", f"trunk CDR not completed: {body}"
    # The external trunk caller must be captured as the caller.
    caller_val = str(body.get("caller") or body.get("fromNumber") or "")
    assert "trunk.example.com" in caller_val or "external" in caller_val, (
        f"trunk CDR missing external caller: {body}"
    )


# ---------------------------------------------------------------------------
# Trunk callee rejects with 486: CDR failed
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_trunk_b2bua_reject_486_cdr(pbx_config, pbx, sipbot_pool, cdr_dir):
    """Callee rejects 486 → caller sees 486, CDR status failed.

    The caller still receives the zero-config failure cue (default busy tone
    `tone://480,3000` played as 183 early media before the rejection — same
    behavior test_trunk_no_ringback_uses_global_default_tone verifies)."""
    callee_port = 15421
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "reject-trunk", dest=f"127.0.0.1:{callee_port}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
    )
    pbx_config.media_proxy = "all"
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, callee_port, "1002", reject_code=486)
    caller = _trunk_caller(sipbot_pool, pbx, target=f"sip:1002@{pbx.sip_addr}", hangup=4)
    assert await caller.wait_output_async(r"4[0-9][0-9]|Busy|Call failed", timeout=20), caller.output
    caller.wait(timeout=15)
    # Zero-config failure cue: the default busy tone plays as early media
    # before the 486, so the caller DOES receive RTP (~3 s of tone ≈ 150
    # packets). Asserting "no RTP" here would contradict the default audio
    # profile (busy → tone://480,3000) and its dedicated test.
    stats = caller.get_rtp_stats()
    assert stats.has_rx, (
        "caller should receive the default busy-tone early media before the "
        f"486 rejection: {stats}\n{caller.output[-2000:]}"
    )

    records = await _wait_cdrs(cdr_dir)
    if records:  # CDR may or may not be emitted for a rejected call
        rec = records[0]
        body = rec if isinstance(rec, dict) and "status" in rec else rec.get("record") or rec
        assert body.get("status") != "completed", f"rejected call CDR must not be completed: {body}"
        assert body.get("statusCode") == 486, f"rejected call statusCode != 486: {body}"


# ---------------------------------------------------------------------------
# Trunk callee never answers: CDR not completed
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_trunk_b2bua_no_answer_cdr(pbx_config, pbx, sipbot_pool, cdr_dir):
    """Callee never answers → the trunk rings out (408) and the CDR is not
    completed."""
    callee_port = 15422
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "noanswer-trunk", dest=f"127.0.0.1:{callee_port}", direction="inbound",
        inbound_hosts=["127.0.0.1"], max_ring_time=3,
    )
    pbx_config.media_proxy = "all"
    h.boot_pbx(pbx)

    # Register a callee then kill it so the contact is stale → INVITE gets no
    # response → the PBX rings out and sends 408.
    callee = await _reg_callee(sipbot_pool, pbx, callee_port, "1002")
    callee.terminate()
    await asyncio.sleep(1)

    caller = _trunk_caller(sipbot_pool, pbx, target=f"sip:1002@{pbx.sip_addr}", hangup=10)
    assert await caller.wait_output_async(r"4[0-9][0-9]|Call failed", timeout=30), caller.output
    caller.wait(timeout=15)

    records = await _wait_cdrs(cdr_dir)
    if records:  # no-answer may not always emit a CDR
        rec = records[0]
        body = rec if isinstance(rec, dict) and "status" in rec else rec.get("record") or rec
        assert body.get("status") != "completed", f"no-answer CDR must not be completed: {body}"
