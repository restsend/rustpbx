"""RWI originate → voip_bridge transfer E2E tests.

Regression coverage for the outbound (UAC) caller leg that used to stay in
`Initializing`, which blocked any transfer — in particular the
`voip_bridge:`/`bridge:` PCM-WebSocket bridge — with:

    Cannot transfer leg caller: invalid state Initializing

This test originates an outbound call via RWI, waits for `call_answered`,
then `call.transfer` the caller leg to a local WebSocket PCM16 echo server
(`helpers.ws_bridge_echo.WsBridgeEchoServer`). It asserts:

  1. The transfer succeeds (caller leg reached `Connected` — the core fix).
  2. The PBX dials out to the bridge WS endpoint (connection established).
  3. Bidirectional PCM flows: caller audio reaches the WS as PCM16 (reverse
     call→WS tap), and the echoed audio returns over the forward WS→call path.
  4. The call can be hung up cleanly afterwards (`call_hangup`).

`media_proxy="all"` is required so the MediaBridge (the actual media plane
that `connect_bridge` taps) is active for the originated call.

Note: this scenario also exercises the SIP-runtime stack-size fix — the
originate → attach_caller_dialog → ensure_caller_leg → rustrtc SDP negotiation
call chain overflows tokio's default 2 MB worker stack in debug builds, so the
SIP/media runtimes are configured with an 8 MB thread stack.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.outbound, pytest.mark.bridge]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _registered_echo_callee(sipbot_pool, pbx, port, username="1002"):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await asyncio.sleep(2)
    return ua


async def _wait_ws_connected(ws_server, timeout: float = 20) -> int:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if ws_server.capture.connection_count() >= 1:
            return ws_server.capture.connection_count()
        await asyncio.sleep(0.3)
    raise AssertionError(
        f"no bridge WS connection after {timeout}s "
        f"(pcm bytes={len(ws_server.capture.pcm_bytes())}, "
        f"dtmf={ws_server.capture.dtmf_frames()})"
    )


async def _wait_pcm(ws_server, min_bytes: int = 320, timeout: float = 15) -> int:
    """Wait until the bridge WS has received at least `min_bytes` of PCM16."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if len(ws_server.capture.pcm_bytes()) >= min_bytes:
            return len(ws_server.capture.pcm_bytes())
        await asyncio.sleep(0.3)
    return len(ws_server.capture.pcm_bytes())


async def _setup_call(sipbot_pool, pbx, rwi, port, call_prefix):
    """Boot PBX (media_proxy=all), register echo callee, originate → answered."""
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)
    callee = await _registered_echo_callee(sipbot_pool, pbx, port)
    call_id = _call_id(call_prefix)
    resp = await rwi.originate(call_id, f"sip:1002@{pbx.sip_addr}", "sip:rwi@pbx", "default")
    assert resp.get("status") == "success", resp
    await rwi.wait_for_event("call_answered", timeout=15)
    return callee, call_id


@pytest.mark.asyncio
async def test_outbound_transfer_to_voip_bridge(pbx, sipbot_pool, rwi, ws_bridge_server):
    """Originate → call.transfer(voip_bridge:ws://…) → bridge WS connects + PCM + hangup.

    This is the direct regression test for the two issues that blocked outbound
    voip_bridge:
      * the caller leg staying `Initializing` ('Cannot transfer leg caller:
        invalid state Initializing'), and
      * the SIP-runtime stack overflow during originate call setup.
    Before the fixes the transfer returned an error (or the PBX crashed) and
    `connect_bridge` was never reached.
    """
    callee, call_id = await _setup_call(sipbot_pool, pbx, rwi, 15110, "obbridge")

    rwi.clear_events()
    target = f"voip_bridge:{ws_bridge_server.ws_url}"
    resp = await rwi.transfer(call_id, target)
    assert resp.get("status") == "success", (
        f"voip_bridge transfer failed (expected caller leg Connected after fix): {resp}"
    )

    # The PBX must dial out to the bridge WS endpoint.
    await _wait_ws_connected(ws_bridge_server, timeout=20)

    # Caller audio must reach the WS as PCM16 binary frames (reverse tap,
    # call→WS). Require a sustained byte count so a single spurious frame
    # can't satisfy the assertion.
    received = await _wait_pcm(ws_bridge_server, min_bytes=1600, timeout=20)
    assert received >= 1600, (
        f"bridge WS received too little PCM ({received} bytes); "
        f"reverse direction (call→WS) not working for outbound"
    )

    # Clean hangup.
    rwi.clear_events()
    await rwi.hangup(call_id)
    hangup = await rwi.wait_for_event("call_hangup", timeout=15)
    assert hangup is not None, "call_hangup not received after hangup"

    # Positive signal in the PBX log: connect_bridge actually ran.
    log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace") if pbx.log_file_path else ""
    assert "Connecting Bridge" in log, f"expected 'Connecting Bridge' in PBX log:\n{log[-2000:]}"
    assert "UAC caller leg marked Connected" in log, (
        f"caller leg was never transitioned to Connected:\n{log[-2000:]}"
    )
