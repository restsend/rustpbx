"""CSAT via global [csat] (cc.toml) + post_call_ivr — the REAL call-path wiring.

Regression coverage for two bugs that the skill-group-metadata CSAT tests
(``test_csat_survey_score_persisted`` in the cc e2e-regression suite and the
flow1 suite) cannot see:

1. proxy_server_hook wiring: the call-path ``CcCallSessionHook`` used to be
   built WITHOUT the cc addon config, so a global ``[csat]`` section in
   ``config/cc/cc.toml`` was invisible to calls (skill-group metadata and the
   console override still worked, masking the bug). This test enables CSAT
   ONLY via the global ``[csat]`` section — no skill-group metadata — so the
   survey only runs if the hook actually received the cc.toml config.

2. builtin ``post_call_csat`` chaining: with ``post_call_ivr`` set, the hook
   starts the builtin trampoline IVR which must chain into ``csat_survey`` on
   entry (``timeout_ms = None`` semantics). The old 1 ms-timer +
   ``max_retries = 0`` shape hung up on the first timeout instead of ever
   chaining.

This test lives in the generic suite (not the cc e2e-regression suite)
because it needs its OWN function-scoped rustpbx with a custom cc.toml —
the cc suite's session-scoped PBX boots once with a fixed cc.toml shared
by tests that must not get CSAT prompts.

Cases:
- ``test_csat_global_cc_toml_with_post_call_ivr`` — plain queue call; agent
  hangup fires the survey; score persisted.
- ``test_ivr_exec_mid_call_then_csat`` — after the agent answers, an
  ``ivr.exec`` (SIP INFO) injects a mid-call collect IVR on the caller leg
  (agent held); the collected result is POSTed verbatim to the dedicated
  ``ivr_exec_completed`` webhook; the agent then hangs up and the CSAT
  survey still runs (``after_transfer = true``) with the score persisted.
"""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.ivr, pytest.mark.queue]

AGENT = "1002"

# Global CSAT config — appended to config/cc/cc.toml AFTER prepare() wrote the
# base file and BEFORE start(). No skill-group metadata anywhere.
CSAT_CC_TOML = """\

[csat]
enabled = true
post_call_ivr = "post_call_csat"
post_call_menu = "csat"
after_transfer = true

[csat.config]
mode = "score"
score_min = 1
score_max = 5
language = "zh"
max_retries = 1
timeout_secs = 20
"""


def _flow_ivr(greeting: Path) -> str:
    """Entry IVR: short greeting → auto-timeout → queue "support"."""
    return f"""\
[ivr]
name = "csat-flow"
ivr_mode = "tree"

[ivr.root]
greeting = "{greeting}"
greeting_text = "Connecting you to support."
timeout_ms = 1500
max_retries = 0
timeout_action = {{ type = "queue", target = "support" }}
max_retries_action = {{ type = "queue", target = "support" }}
entries = []
"""


async def _seed_cc(pbx, api) -> None:
    """Create agent 1002 + skill group "support" WITHOUT CSAT metadata.

    CSAT is enabled globally via cc.toml [csat]; the DB skill-group row must
    exist (resolve_survey_config declines unknown groups) but carries no
    metadata override. Idempotent.
    """
    await api.ensure_console_auth()
    for body in (
        {"agent_id": AGENT, "display_name": "Agent 1002 (global-csat)",
         "skills": ["support"], "max_concurrency": 1, "role": "agent"},
        {"skill_group_id": "support", "skills_required": ["support"],
         "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 90},
    ):
        try:
            if "skill_group_id" in body:
                await api.create_skill_group(body)
            else:
                await api.create_agent(body)
        except Exception as exc:  # noqa: BLE001 — duplicate on re-run is fine
            if not ("409" in str(exc) or "400" in str(exc) or "already" in str(exc).lower()):
                raise


@pytest.mark.asyncio
async def test_csat_global_cc_toml_with_post_call_ivr(
    pbx, sipbot_pool, api, event_checker, webhook_server, tmp_path
):
    greeting = tmp_path / "csat_flow_greeting.wav"
    h.generate_sine_wav(greeting, 880.0, 1.5, 8000, 0.4)

    pbx.config_builder.add_ivr("csat-flow", _flow_ivr(greeting))
    pbx.config_builder.add_queue(
        "support",
        strategy_mode="sequential",
        targets=["skill-group:support"],
    )
    pbx.config_builder.add_route(
        "csat-flow-route",
        match={"to.user": "csat-flow"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/csat-flow.toml"},
        auto_answer=True,
    )

    # Boot with the [csat] section injected between config generation and
    # process start — config_builder.build() already ran inside prepare().
    pbx.prepare(webhook_url=webhook_server.url, build=False)
    cc_toml = pbx.work_dir / "config" / "cc" / "cc.toml"
    base = cc_toml.read_text(encoding="utf-8")
    assert "[csat]" not in base, "test precondition: base cc.toml must not enable csat"
    cc_toml.write_text(base + CSAT_CC_TOML, encoding="utf-8")
    pbx.start(timeout=90)

    await _seed_cc(pbx, api)

    # Agent: answers ~2 s in, hangs up 8 s later → CSAT fires on the caller.
    agent = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(17220), username=AGENT, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=8,
    )
    await h.wait_registered(agent, f"agent {AGENT}")

    caller = sipbot_pool.caller(
        target=f"sip:csat-flow@{pbx.sip_addr}", username="1001", password="123456",
        hangup=35,
    )
    answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert answered, f"call never answered:\n{caller.output[-1500:]}"

    # Queue dispatched the agent.
    ringing = await event_checker.webhook.wait_for_event("call_ringing", timeout=20)
    assert ringing is not None, (
        f"no call_ringing — queue did not dispatch. events: "
        f"{event_checker.webhook.event_types()}"
    )
    answered_ev = await event_checker.webhook.wait_for_event("call_answered", timeout=20)
    assert answered_ev is not None, "agent never answered (no call_answered)"

    # Agent hangs up (hangup_after=8) → global-CSAT survey via post_call_ivr.
    await h.wait_log(pbx, r"Post-call survey started", 25, "survey started (global [csat] wiring)")
    await h.wait_log(
        pbx,
        r"skips DTMF wait.*executing timeout_action immediately"
        r"|chaining to sub-app ivr=post_call_csat",
        15,
        "builtin trampoline chained into csat_survey",
    )
    await h.wait_log(pbx, r"CSAT: playing score prompt", 30, "score prompt")

    # Score 5 via stdin DTMF (two presses hedge prompt barge-in).
    assert caller.send_stdin_dtmf("5"), "caller stdin DTMF failed"
    await asyncio.sleep(2)
    caller.send_stdin_dtmf("5")

    await h.wait_log(pbx, r"CSAT: score collected score=5", 20, "score collected")
    await h.wait_log(pbx, r"CSAT: survey complete", 15, "survey complete")

    # NOTE: with post_call_ivr the hook forces after_completion="return_ivr";
    # with no IVR to return to the caller leg stays open until its own BYE —
    # documented behavior, the safety-net hangup=35 ends the call.
    hangup_ev = await event_checker.webhook.wait_for_event("call_hangup", timeout=30)
    assert hangup_ev is not None, (
        f"call never hung up after survey. events: {event_checker.webhook.event_types()}"
    )
    call_id = hangup_ev.call_id

    # CDR must carry the surveyed score (queryable only after the call ends).
    score = None
    cdr = None
    for _ in range(12):
        await asyncio.sleep(1)
        detail = await api.get(f"/api/cc/calls/{call_id}")
        if isinstance(detail, dict):
            cdr = detail.get("data", detail)
            score = cdr.get("csat_score") or cdr.get("csatScore")
            if score is not None:
                break
    assert score is not None, (
        f"CSAT score not persisted for {call_id} — global [csat] survey never "
        f"ran. CDR: {cdr!r:.300}"
    )
    assert int(score) == 5, f"csat_score mismatch: {score!r} (want 5)"
    print(f"\n[global-csat] ✓ csat=5 via cc.toml [csat] + post_call_ivr (call {call_id})")


def _exec_collect_ivr() -> str:
    """Mid-call ivr.exec target: unknown key seeds a 2-digit `order` collect.

    "4" is unmapped → unknown_key_action starts collecting `order` seeded
    with "4"; "2" completes it. Root timeout then exits the IVR (call stays
    up, the ivr_exec hook unholds the agent) and the result is POSTed
    verbatim to the dedicated webhook as `ivr_exec_completed`
    (`collected.order == "42"`).
    """
    return """\
[ivr]
name = "csat-exec"
ivr_mode = "tree"

[ivr.root]
greeting_text = "Please enter your order number."
timeout_ms = 4000
max_retries = 2
timeout_action = { type = "exit" }
max_retries_action = { type = "exit" }
unknown_key_action = { type = "collect", variable = "order", min_digits = 2, max_digits = 2, inter_digit_timeout_ms = 3000 }
entries = []
"""


@pytest.mark.asyncio
async def test_ivr_exec_mid_call_then_csat(
    pbx, sipbot_pool, api, event_checker, webhook_server, tmp_path
):
    """Agent answers → mid-call ivr.exec (SIP INFO) runs a collect IVR on the
    caller leg with the agent held → result POSTed to the dedicated webhook →
    agent hangs up → the global-[csat] survey still fires (after_transfer)
    and the score is persisted."""
    from aiohttp import web

    greeting = tmp_path / "csat_exec_greeting.wav"
    h.generate_sine_wav(greeting, 880.0, 1.5, 8000, 0.4)

    pbx.config_builder.add_ivr("csat-flow", _flow_ivr(greeting))
    pbx.config_builder.add_ivr("csat-exec", _exec_collect_ivr())
    pbx.config_builder.add_queue(
        "support",
        strategy_mode="sequential",
        targets=["skill-group:support"],
    )
    pbx.config_builder.add_route(
        "csat-flow-route",
        match={"to.user": "csat-flow"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/csat-flow.toml"},
        auto_answer=True,
    )

    pbx.prepare(webhook_url=webhook_server.url, build=False)
    cc_toml = pbx.work_dir / "config" / "cc" / "cc.toml"
    cc_toml.write_text(cc_toml.read_text(encoding="utf-8") + CSAT_CC_TOML, encoding="utf-8")
    pbx.start(timeout=90)

    await _seed_cc(pbx, api)

    # Dedicated capture endpoint for the ivr_exec_completed result POST (the
    # global webhook cannot match its {event: ...} envelope).
    exec_payload: dict = {}
    exec_received = asyncio.Event()

    async def _capture(request):
        exec_payload.update(await request.json())
        exec_received.set()
        return web.json_response({"ok": True})

    capture_app = web.Application()
    capture_app.router.add_post("/ivr-exec", _capture)
    runner = web.AppRunner(capture_app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 0)
    await site.start()
    exec_webhook_url = f"http://127.0.0.1:{site._server.sockets[0].getsockname()[1]}/ivr-exec"

    try:
        # Agent answers ~t2.5, hangs up at t17 (hangup_after=15).
        agent = sipbot_pool.callee(
            host=pbx.host, port=h.ua_port(17230), username=AGENT, password="123456",
            register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=1, answer_mode="echo", hangup_after=15,
        )
        await h.wait_registered(agent, f"agent {AGENT}")

        # Caller: queue → agent; INFO fires at 6 s (after the answer).
        ivr_exec_body = json.dumps({
            "action": "ivr.exec",
            "params": {
                "route_point": "csat-exec",
                "request_id": "csat-exec-001",
                "webhook_url": exec_webhook_url,
                "hold_agent": True,
            },
        })
        caller = sipbot_pool.caller(
            target=f"sip:csat-flow@{pbx.sip_addr}", username="1001", password="123456",
            hangup=45,
            info_flows=f"6s:application/vnd.rustpbx+json:{ivr_exec_body}",
        )
        answered = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
        assert answered, f"call never answered:\n{caller.output[-1500:]}"

        await event_checker.expect_webhook_event("call_answered", timeout=20)

        # ── ivr.exec: collect IVR runs on the caller leg, agent held. ──
        await h.wait_log(pbx, r"SIP INFO rustpbx command accepted", 20, "ivr.exec INFO")
        await h.wait_log(
            pbx, r"ivr=csat-exec menu=.root. retry_count=0", 20, "collect IVR ready",
        )
        # "4" unmapped → unknown_key_action seeds `order` with 4; "2" completes.
        assert caller.send_stdin_dtmf("4"), "caller stdin DTMF failed"
        await h.wait_log(pbx, r"unknown_key_action.*digit=4", 10, "collect seeded with '4'")
        await asyncio.sleep(0.4)
        assert caller.send_stdin_dtmf("2"), "caller stdin DTMF failed"

        await asyncio.wait_for(exec_received.wait(), timeout=30)
        assert exec_payload.get("event") == "ivr_exec_completed", (
            f"ivr_exec envelope mismatch: {exec_payload!r:.300}"
        )
        assert exec_payload.get("status") not in (None, "", "error"), (
            f"collect IVR did not complete cleanly: {exec_payload!r:.300}"
        )
        collected = exec_payload.get("collected") or {}
        assert collected.get("order") == "42", (
            f"DTMF accuracy failure — collected={collected!r}, want order=='42'. "
            f"payload: {exec_payload!r:.400}"
        )

        # ── Agent hangs up (hangup_after=15) → global-[csat] survey. ──
        await h.wait_log(pbx, r"Post-call survey started", 30, "survey after ivr.exec call")
        await h.wait_log(pbx, r"CSAT: playing score prompt", 30, "score prompt")
        assert caller.send_stdin_dtmf("5"), "caller stdin DTMF failed"
        await asyncio.sleep(2)
        caller.send_stdin_dtmf("5")

        await h.wait_log(pbx, r"CSAT: score collected score=5", 20, "score collected")
        await h.wait_log(pbx, r"CSAT: survey complete", 15, "survey complete")

        hangup_ev = await event_checker.webhook.wait_for_event("call_hangup", timeout=30)
        assert hangup_ev is not None, "call never hung up after survey"
        call_id = hangup_ev.call_id

        score = None
        cdr = None
        for _ in range(12):
            await asyncio.sleep(1)
            detail = await api.get(f"/api/cc/calls/{call_id}")
            if isinstance(detail, dict):
                cdr = detail.get("data", detail)
                score = cdr.get("csat_score") or cdr.get("csatScore")
                if score is not None:
                    break
        assert score is not None, (
            f"CSAT score not persisted for {call_id} after ivr.exec mid-call. "
            f"CDR: {cdr!r:.300}"
        )
        assert int(score) == 5, f"csat_score mismatch: {score!r} (want 5)"
        print(
            f"\n[global-csat] ✓ ivr.exec collected order='42', csat=5 "
            f"after agent hangup (call {call_id})"
        )
    finally:
        await runner.cleanup()
