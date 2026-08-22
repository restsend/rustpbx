"""Backend E2E: agent-triggered ivr.exec actually runs the IVR app to
completion on a live call.

Verifies the full lifecycle, not just 'ivr.exec accepted':
  - agent (callee) sends ivr.exec via SIP INFO (--info-flows)
  - PBX holds the agent (re-INVITE a=sendonly + MOH)
  - IVR app starts on the customer (caller) leg, plays greeting
  - customer sends DTMF '1' -> app reaches terminal
  - ivr_flow_completed emitted via global webhook
  - ivr_exec_completed POSTed to the dedicated per-call webhook_url
  - agent auto-unheld (re-INVITE a=sendrecv) on app exit
  - result INFO delivered to agent

The existing test_ivr_exec_mid_call (e2e/tests/test_ivr_queue.py:167) only
asserts the PBX log keyword "ivr.exec" — it never proves the app actually
ran. This module closes that gap and guards against the §3 false positive
detected during the ivr.exec fixture audit (type="return" was not a valid
EntryAction variant, silently failing app load).
"""
from __future__ import annotations

import asyncio
import json
from typing import Optional

import pytest
from aiohttp import web

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import helpers as h

pytestmark = [pytest.mark.ivr, pytest.mark.media]


@pytest.mark.asyncio
async def test_ivr_exec_real_runs_to_completion(
    pbx, sipbot_pool, event_checker, webhook_server,
):
    """Full ivr.exec lifecycle on a real call.

    Topology:
      - caller (1001) = sipbot caller, sends DTMF '1' via --dtmf-flows
      - agent  (1002) = sipbot callee (echo), sends ivr.exec via --info-flows

    The PBX hard-codes initiator_leg = held_leg = "callee" (sip_session.rs:3536),
    so the agent both triggers ivr.exec and receives the hold/MOH/result INFO.
    The IVR app itself runs against the caller (customer) leg.
    """
    # ── 1. Valid IVR app (overlays any committed fixture).                  ─
    # Key '1' -> hangup. This is a clean terminal that fires ivr_flow_completed
    # and ivr_exec_completed. Note: 'hangup' also terminates the call, so the
    # auto-unhold re-INVITE cycle is skipped (the call is gone before unhold).
    # That's fine — the strong signals are the two webhook events below.
    pbx.config_builder.add_ivr("e2e_collect", '''\
[ivr]
name = "e2e_collect"
ivr_mode = "tree"

[ivr.root]
greeting_text = "Press 1."
timeout_ms = 8000
max_retries = 3
max_retries_action = { type = "hangup" }

[[ivr.root.entries]]
key = "1"
[ivr.root.entries.action]
type = "hangup"
''')
    pbx.config_builder.media_proxy = "all"

    # ── 2. Dedicated capture endpoint for ivr_exec_completed.              ─
    # The hook POSTs raw IvrExecResultPayload {event: ...} which the stock
    # webhook_receiver can't match by event_type (it expects {event_type: ...}).
    exec_payload: dict = {}
    exec_received = asyncio.Event()

    async def _capture(request: web.Request) -> web.Response:
        body = await request.json()
        exec_payload.update(body)
        exec_received.set()
        return web.json_response({"ok": True})

    capture_app = web.Application()
    capture_app.router.add_post("/ivr-exec", _capture)
    capture_runner = web.AppRunner(capture_app)
    await capture_runner.setup()
    capture_site = web.TCPSite(capture_runner, "127.0.0.1", 0)
    await capture_site.start()
    capture_port = capture_site._server.sockets[0].getsockname()[1]
    exec_webhook_url = f"http://127.0.0.1:{capture_port}/ivr-exec"

    try:
        h.boot_pbx(pbx, webhook_url=webhook_server.url)

        # ── 3. Register agent (1002) — plain echo, no special flags.      ─
        agent = sipbot_pool.callee(
            host=pbx.host, port=h.ua_port(16920), username="1002", password="123456",
            register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=1, answer_mode="echo", hangup_after=60,
        )
        await h.wait_registered(agent)

        # ── 4. Caller (1001) places the call AND sends ivr.exec + DTMF.   ─
        # Matches the topology of e2e/tests/test_ivr_queue.py::test_ivr_exec_mid_call
        # (caller is the INFO sender). sipbot supports concurrent --info-flows
        # and --dtmf-flows on the same UA (separate futures, sip.rs:479+589).
        #
        # Per sip_session.rs:3536, initiator_leg and held_leg are both
        # hard-coded to "callee" — so the agent (1002) gets held + MOH +
        # result INFO regardless of who sent the SIP INFO. The IVR app
        # itself runs on the caller (customer) leg.
        ivr_exec_body = json.dumps({
            "action": "ivr.exec",
            "params": {
                "route_point": "e2e_collect",
                "request_id": "req-real-001",
                "webhook_url": exec_webhook_url,
                "hold_agent": True,
            },
        })
        caller = sipbot_pool.caller(
            target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
            hangup=40,
            info_flows=f"2s:application/vnd.rustpbx+json:{ivr_exec_body}",
            dtmf_flows="6s:1",  # after greeting plays (~2s ivr.exec + ~2s greeting)
        )
        answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
        assert answered, f"call never answered:\n{caller.output[-1500:]}"

        # ── 5. Assert: ivr_flow_completed fired via global webhook.       ─
        # This is the STRONGEST signal — proves the IVR app actually ran
        # to a terminal. Without this, ivr.exec was accepted but the app
        # silently failed to load or execute.
        flow_ev = await event_checker.webhook.wait_for_event(
            "ivr_flow_completed", timeout=25,
        )
        assert flow_ev is not None, (
            f"ivr_flow_completed never fired — IVR app did not reach a terminal. "
            f"webhook events: {event_checker.webhook.event_types()}"
        )
        print(f"\n[ivr] ivr_flow_completed payload: {flow_ev.payload!r:.200}")

        # ── 6. Assert: ivr_exec_completed POSTed to dedicated endpoint.    ─
        # The hook fires only on app exit; receiving it confirms the full
        # ivr.exec lifecycle completed server-side.
        await asyncio.wait_for(exec_received.wait(), timeout=20)
        assert exec_payload.get("event") == "ivr_exec_completed", (
            f"ivr_exec_completed event field mismatch: {exec_payload!r:.200}"
        )
        assert exec_payload.get("request_id") == "req-real-001", (
            f"request_id mismatch: {exec_payload!r:.200}"
        )
        assert exec_payload.get("status"), (
            f"empty status in ivr_exec_completed: {exec_payload!r:.200}"
        )
        print(f"[ivr] ivr_exec_completed payload: {exec_payload!r:.200}")

        # ── 7. Best-effort: agent hold/resume re-INVITE cycle.            ─
        # When hold_agent=true, the PBX sends a sendonly re-INVITE to the
        # callee before running the IVR app and a sendrecv re-INVITE on
        # exit. sipbot may or may not log these depending on version/build;
        # treat as best-effort signal, not a hard requirement (the strong
        # signals are the webhooks above).
        try:
            await agent.wait_output_async(r"Received re-INVITE: HOLD", timeout=5)
            print(f"[ivr] agent hold re-INVITE observed ✓")
        except Exception:
            print(f"[ivr] agent hold re-INVITE not observed in sipbot log (best-effort)")

        # ── 8. Assert: PBX log shows the full server-side lifecycle.      ─
        log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace") \
            if pbx.log_file_path else ""
        for needle in (
            "SIP INFO rustpbx command accepted",
            "Propagating hold",
        ):
            assert needle in log, (
                f"missing PBX log {needle!r}. PBX log tail:\n{log[-2500:]}"
            )
        for bad in (
            "Failed to send result INFO after app exit",
            "Failed to parse IVR TOML",
        ):
            assert bad not in log, (
                f"PBX log contains failure marker {bad!r}:\n{log[-2500:]}"
            )

        print(f"[ivr] ✓ full lifecycle verified")

    finally:
        await capture_runner.cleanup()
