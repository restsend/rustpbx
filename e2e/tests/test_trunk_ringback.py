"""Trunk ringback / busy / reject / offline / notfound tone E2E tests.

Verifies the per-trunk `RingbackAudio` feature end-to-end with sipbot:

  * `ring`     — played as 183 early media while the callee rings
  * `busy`     — played as 183 early media before the PBX sends 486
  * `reject`   — played as 183 early media before 603
  * `offline`  — played as 183 early media before 480
  * `notfound` — played as 183 early media before 404

Failure tones play once to natural completion (the file's length, or the
tone:// duration) and then the call is rejected.

Audio tones use sipbot's `tone://frequency,duration_ms` spec, which the PBX
renders into a WAV. Early-media RTP is observed on the caller thanks to the
enhanced sipbot that starts an RX observer on 183 Session Progress.

Scenarios:
  1. Inbound trunk with ringback.ring   → caller hears 183 early media, call answers
  2. Inbound trunk with ringback.busy   → caller hears 183 early media, then 486
  3. Inbound trunk reject/offline/notfound tones → 183 early media + 603/480/404
  4. Control: inbound trunk WITHOUT ringback → no 183, no early-media RTP, just 486
  5. Outbound trunk with ringback.busy  → internal caller hears 183 early media, then 486
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.trunk_ringback]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _reg_callee(sipbot_pool, pbx, port, *, username="1002", reject_code=None):
    """Spawn a callee registered to the PBX. Optionally rejects with a code.

    sipbot's `--reject` alone answers with 200 OK; `--reject-prob 100` is what
    forces the configured rejection code to be sent at INVITE time.
    """
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="none" if reject_code else "echo",
        reject_code=reject_code,
        reject_prob=100 if reject_code else None,
    )
    await h.wait_registered(ua)
    return ua


async def _unreg_callee(sipbot_pool, pbx, port, *, username="carrier", reject_code=None):
    """Spawn an unregistered callee (trunk peer) that rejects with a code."""
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=False, ring_secs=1, answer_mode="none" if reject_code else "echo",
        reject_code=reject_code,
        reject_prob=100 if reject_code else None,
    )
    await asyncio.sleep(1)
    return ua


def _trunk_caller(sipbot_pool, pbx, *, target, hangup=4):
    """Spawn an outbound (trunk-originated) caller.

    The From domain must differ from the PBX realm (127.0.0.1) so the call is
    classified as *Inbound* (external caller → local callee). Only Inbound calls
    build a source trunk, which is what carries the per-trunk ringback config.
    """
    return sipbot_pool.caller(
        target=target, username="external", password="123456",
        from_uri="sip:external@trunk.example.com", hangup=hangup,
    )


def _wait_call_ended(ua, timeout: float = 30) -> None:
    code = ua.wait(timeout=timeout)
    assert code == 0, f"sipbot exited with {code}:\n{ua.output[-3000:]}"


# ---------------------------------------------------------------------------
# 1. Ringback ring tone (183 early media) then call answers
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_trunk_ringback_ring_183_early_media(pbx_config, pbx, sipbot_pool):
    """Inbound trunk with ringback.ring: caller receives 183 + early-media RTP,
    then the call answers (200 OK)."""
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "ring-trunk", dest=f"127.0.0.1:{h.ua_port(15200)}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
        ringback={"ring": "tone://440,3000"},
    )
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, h.ua_port(15200), username="1002")
    caller = _trunk_caller(sipbot_pool, pbx, target=f"sip:1002@{pbx.sip_addr}", hangup=6)
    # Call establishes (200 OK).
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), caller.output
    # Wait for the process to exit so the final summary (with RTP stats) is printed.
    _wait_call_ended(caller)

    codes = caller.get_status_counts()
    assert codes.get(183, 0) == 1, f"expected 183 early media, got: {codes}\n{caller.output[-3000:]}"
    assert codes.get(200, 0) == 1, f"expected 200 OK, got: {codes}\n{caller.output[-3000:]}"

    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, f"expected early-media ringback RTP: {stats}\n{caller.output[-3000:]}"


# ---------------------------------------------------------------------------
# 2. Busy tone (183 early media) then 486
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_trunk_busy_tone_183_early_media(pbx_config, pbx, sipbot_pool):
    """Inbound trunk with ringback.busy: caller receives 183 + early-media RTP
    (the busy tone), then the final 486."""
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "busy-trunk", dest=f"127.0.0.1:{h.ua_port(15201)}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
        ringback={"busy": "tone://480,3000"},
    )
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, h.ua_port(15201), username="1002", reject_code=486)
    caller = _trunk_caller(sipbot_pool, pbx, target=f"sip:1002@{pbx.sip_addr}")
    assert await caller.wait_output_async(r"Call failed|4[0-9][0-9]|Busy", timeout=20), caller.output
    _wait_call_ended(caller)

    codes = caller.get_status_counts()
    assert codes.get(183, 0) == 1, f"expected 183 early media, got: {codes}\n{caller.output[-3000:]}"
    assert codes.get(486, 0) == 1, f"expected 486 busy, got: {codes}\n{caller.output[-3000:]}"

    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, f"expected busy-tone early-media RTP: {stats}\n{caller.output[-3000:]}"


# ---------------------------------------------------------------------------
# 3. Reject / offline / notfound tones
# ---------------------------------------------------------------------------

@pytest.mark.parametrize(
    "tone_field,reject_code",
    [
        ("reject", 603),
        ("offline", 480),
        ("notfound", 404),
    ],
)
@pytest.mark.asyncio
async def test_trunk_failure_tones(pbx_config, pbx, sipbot_pool, tone_field, reject_code):
    """Inbound trunk failure tones: caller receives 183 + early-media RTP, then
    the mapped rejection code (603/480/404)."""
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        f"{tone_field}-trunk", dest=f"127.0.0.1:{h.ua_port(15210)}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
        ringback={tone_field: "tone://500,3000"},
    )
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, h.ua_port(15210), username="1002", reject_code=reject_code)
    caller = _trunk_caller(sipbot_pool, pbx, target=f"sip:1002@{pbx.sip_addr}")
    assert await caller.wait_output_async(r"Call failed|4[0-9][0-9]|6[0-9][0-9]", timeout=20), caller.output
    _wait_call_ended(caller)

    codes = caller.get_status_counts()
    assert codes.get(183, 0) == 1, f"expected 183 early media, got: {codes}\n{caller.output[-3000:]}"
    assert codes.get(reject_code, 0) == 1, (
        f"expected {reject_code}, got: {codes}\n{caller.output[-3000:]}"
    )
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, f"expected failure-tone early-media RTP: {stats}\n{caller.output[-3000:]}"


# ---------------------------------------------------------------------------
# 4. No trunk ringback → the GLOBAL default busy tone still plays
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_trunk_no_ringback_uses_global_default_tone(pbx_config, pbx, sipbot_pool):
    """A trunk WITHOUT its own ringback still gets the global failure-tone
    default (`tone://480,3000` busy), so the caller hears 183 early media
    before the 486. This is the "zero-config" behaviour: every call plays a
    failure cue unless explicitly overridden."""
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "plain-trunk", dest=f"127.0.0.1:{h.ua_port(15202)}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
    )
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, h.ua_port(15202), username="1002", reject_code=486)
    caller = _trunk_caller(sipbot_pool, pbx, target=f"sip:1002@{pbx.sip_addr}")
    assert await caller.wait_output_async(r"Call failed|4[0-9][0-9]|Busy", timeout=20), caller.output
    _wait_call_ended(caller)

    codes = caller.get_status_counts()
    assert codes.get(183, 0) == 1, f"global default busy tone must produce 183, got: {codes}\n{caller.output[-3000:]}"
    assert codes.get(486, 0) == 1, f"expected 486 busy, got: {codes}\n{caller.output[-3000:]}"
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, f"global default busy tone must produce early-media RTP: {stats}\n{caller.output[-3000:]}"


# ---------------------------------------------------------------------------
# 5. Outbound trunk busy tone
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_outbound_trunk_busy_tone(pbx_config, pbx, sipbot_pool):
    """Internal registered caller → outbound trunk with ringback.busy: the
    trunk peer rejects 486, caller hears the outbound trunk's busy tone as 183
    early media, then receives 486."""
    callee_port = h.ua_port(15204)
    pbx_config.add_trunk(
        "carrier-trunk", dest=f"127.0.0.1:{callee_port}", direction="outbound",
        ringback={"busy": "tone://480,3000"},
    )
    pbx_config.add_route(
        "out-to-carrier", match={"to.user": "^9"}, dest="carrier-trunk", priority=90,
    )
    h.boot_pbx(pbx)

    # Trunk peer (unregistered) rejects with 486.
    await _unreg_callee(sipbot_pool, pbx, callee_port, username="carrier", reject_code=486)
    # Internal registered caller dials a 9-prefixed number → outbound trunk.
    caller = sipbot_pool.caller(
        target=f"sip:91234567@{pbx.sip_addr}", username="1001", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}",
        hangup=4,
    )
    assert await caller.wait_output_async(r"Call failed|4[0-9][0-9]|Busy", timeout=25), caller.output
    _wait_call_ended(caller)

    codes = caller.get_status_counts()
    assert codes.get(183, 0) == 1, f"expected outbound-trunk 183 early media, got: {codes}\n{caller.output[-3000:]}"
    assert codes.get(486, 0) == 1, f"expected 486 busy, got: {codes}\n{caller.output[-3000:]}"
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, f"expected outbound busy-tone RTP: {stats}\n{caller.output[-3000:]}"


# ---------------------------------------------------------------------------
# 7. No-answer tone (callee rings out)
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_trunk_noanswer_tone(pbx_config, pbx, sipbot_pool):
    """Inbound trunk with ringback.noanswer: the callee never answers, so the
    PBX plays the no-answer tone as 183 early media before the ring-timeout
    rejection (408)."""
    pbx_config.set_realms(["127.0.0.1"])
    pbx_config.add_trunk(
        "noanswer-trunk", dest=f"127.0.0.1:{h.ua_port(15206)}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
        ringback={"noanswer": "tone://480,1000"},
        max_ring_time=3,  # short ring timeout keeps the test fast
    )
    h.boot_pbx(pbx)

    # Register a callee, then kill it so its contact is stale and the PBX's
    # INVITE gets no response → the call rings out → no-answer rejection.
    callee = await _reg_callee(sipbot_pool, pbx, h.ua_port(15206), username="1002")
    callee.terminate()
    await asyncio.sleep(1)

    caller = _trunk_caller(sipbot_pool, pbx, target=f"sip:1002@{pbx.sip_addr}", hangup=8)
    assert await caller.wait_output_async(r"Call failed|4[0-9][0-9]", timeout=30), caller.output
    _wait_call_ended(caller)

    codes = caller.get_status_counts()
    assert codes.get(183, 0) == 1, f"expected no-answer 183 early media, got: {codes}\n{caller.output[-3000:]}"
    assert codes.get(408, 0) == 1, f"expected 408 no-answer rejection, got: {codes}\n{caller.output[-3000:]}"
    stats = caller.get_rtp_stats()
    assert stats.rx_packets > 0, f"expected no-answer tone RTP: {stats}\n{caller.output[-3000:]}"
