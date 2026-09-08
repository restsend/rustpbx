"""CSAT via global [csat] cc.toml + post_call_ivr — the REAL call-path wiring.

Regression coverage for two bugs that the flow1 CSAT test cannot see:

1. proxy_server_hook wiring: the call-path CcCallSessionHook used to be built
   WITHOUT the cc addon config, so a global `[csat]` section in
   config/cc/cc.toml was invisible to calls (skill-group metadata and console
   overrides still worked, masking the bug). This test enables CSAT ONLY via
   the global [csat] section — no skill-group metadata — so the survey only
   runs if the hook actually received the cc.toml config.

2. builtin post_call_csat chaining: with `post_call_ivr` set, the hook starts
   the builtin trampoline IVR which must chain into csat_survey on entry
   (timeout_ms = None semantics). The old 1 ms-timer + max_retries = 0 shape
   hung up on the first timeout instead of ever chaining.

Flow: caller → IVR (auto-timeout) → queue "support" → agent 1002 answers →
agent hangs up → post_call_csat IVR → csat_survey → caller presses 5 →
CSAT: score collected score=5 → CDR csat_score == 5.
"""

from __future__ import annotations

import asyncio
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
    pbx, sipbot_pool, api, event_checker, webhook_server, tmp_path,
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

    ringing = await event_checker.webhook.wait_for_event("cc_ringing", timeout=20)
    assert ringing is not None, (
        f"no cc_ringing — queue did not dispatch. events: "
        f"{event_checker.webhook.event_types()}"
    )
    answered_ev = await event_checker.webhook.wait_for_event("cc_answered", timeout=20)
    assert answered_ev is not None, "agent never answered (no cc_answered)"

    # Agent hangs up (hangup_after=8) → global-CSAT survey via post_call_ivr.
    await h.wait_log(pbx, r"Post-call survey started", 25, "survey started (global [csat] wiring)")
    await h.wait_log(
        pbx,
        r"skips DTMF wait.*executing timeout_action immediately|chaining to sub-app ivr=post_call_csat",
        15,
        "builtin trampoline chained into csat_survey",
    )
    await h.wait_log(pbx, r"CSAT: playing score prompt", 20, "score prompt")

    # Score 5 via stdin DTMF (two presses hedge prompt barge-in, same as flow1).
    assert caller.send_stdin_dtmf("5"), "caller stdin DTMF failed"
    await asyncio.sleep(2)
    caller.send_stdin_dtmf("5")

    await h.wait_log(pbx, r"CSAT: score collected score=5", 20, "score collected")
    await h.wait_log(pbx, r"CSAT: survey complete", 15, "survey complete")

    # The call record is queryable only after the call has ended.
    hangup_ev = await event_checker.webhook.wait_for_event("cc_hangup", timeout=30)
    assert hangup_ev is not None, (
        f"call never hung up after survey. events: {event_checker.webhook.event_types()}"
    )

    # CDR must carry the surveyed score.
    call_id = (answered_ev.call_id if hasattr(answered_ev, "call_id") else None) or (
        ringing.call_id
    )
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
