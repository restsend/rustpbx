"""Shared session-scoped PBX for the transfer e2e suites.

One rustpbx instance boots for the WHOLE session with a superset config
(boot + migrations dominate suite wall time, not the SIP flows). Isolation
between cases is provided by:
  * per-case webhook receiver clear + active-call drain asserts,
  * REST status nudges (agents return to Idle before each dispatch),
  * `_retire_target` before re-registering a shared bot username,
  * distinct caller port bases per case.
The config unions both transfer suites' needs (R core flows + agent
availability flows); rtp_timeout=10 makes the hold-regression (R12)
meaningful — a held leg must survive 1.5× the watchdog window.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest

import importlib.util
from helpers import generate_sine_wav
from helpers.pbx_server import PbxServer
from helpers.pbx_server import find_project_root

# Load the ROOT conftest by path: pytest imports every conftest.py under the
# basename `conftest`, so a plain `import conftest` here would resolve to
# THIS file (or whichever shadowed sys.modules first) instead of the root.
_root_conftest_path = Path(__file__).resolve().parents[2] / "conftest.py"
_spec = importlib.util.spec_from_file_location(
    "transfer_root_conftest", _root_conftest_path)
root_conftest = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(root_conftest)

CUSTOMER = "1001"
AGENT = "1002"
TARGET = "1003"
BUSY_PEER = "1004"
ECHO = "1014"
TONE730 = "1015"
BREAK_AGENT = "1016"
ROT_A = "1017"
ROT_B = "1018"
ROT_OK = "1019"
AGENT2 = "1012"
IVR_NUM = "8880"
QUEUE_NUM = "9200"
QUEUE_NUM_R11 = "9201"
AV_NUM = "9202"
ROT_NUM = "9203"
ROUTED_AGENT = "8870"   # R17: registrable memory user on an app route
CC_AGENT_NUM = "1004"   # R18: CC agent extension (no SIP user) on an app route
QUEUE_NAME = "support"
SKILL_GROUP = "support"
SKILL_AV = "av"
SKILL_ROT = "rot"


def _transfer_ivr_toml(greeting) -> str:
    return f"""\
[ivr]
name = "transfer_ivr"
ivr_mode = "tree"

[ivr.root]
greeting = "{greeting}"
timeout_ms = 60000
max_retries = 100
timeout_action = {{ type = "repeat" }}

[[ivr.root.entries]]
key = "1"
action = {{ type = "play", prompt = "{greeting}" }}

[[ivr.root.entries]]
key = "9"
action = {{ type = "hangup" }}
"""


def _seed_cc_sync(pbx) -> None:
    import aiohttp
    from helpers.pbx_server import PbxApiClient

    async def _run():
        session = aiohttp.ClientSession()
        try:
            client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
            assert await client.ensure_console_auth(), "console auth failed"
            for body in (
                {"agent_id": AGENT, "display_name": "Agent 1002 (transfer-e2e)",
                 "skills": [SKILL_GROUP], "max_concurrency": 3, "role": "agent"},
                {"agent_id": TARGET, "display_name": "Agent 1003 (transfer-e2e)",
                 "skills": [SKILL_GROUP], "max_concurrency": 3, "role": "agent"},
                {"agent_id": AGENT2, "display_name": "Agent 1012 (transfer-e2e)",
                 "skills": ["r11"], "max_concurrency": 3, "role": "agent"},
                {"agent_id": BREAK_AGENT, "display_name": "Agent 1016 (av)",
                 "skills": [SKILL_AV], "max_concurrency": 3, "role": "agent"},
                {"agent_id": ROT_A, "display_name": "Agent 1017 (rot)",
                 "skills": [SKILL_ROT], "max_concurrency": 3, "role": "agent"},
                {"agent_id": ROT_B, "display_name": "Agent 1018 (rot)",
                 "skills": [SKILL_ROT], "max_concurrency": 3, "role": "agent"},
                {"agent_id": ROT_OK, "display_name": "Agent 1019 (rot)",
                 "skills": [SKILL_ROT], "max_concurrency": 3, "role": "agent"},
                # R18: 1004 is a CC agent WITHOUT a SIP user — the
                # queue:agent:<ext> hand-off vehicle only needs the row.
                {"agent_id": "1004", "display_name": "Agent 1004 (ccreg)",
                 "skills": ["ccreg"], "max_concurrency": 3, "role": "agent"},
                {"skill_group_id": SKILL_GROUP, "skills_required": [SKILL_GROUP],
                 "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 90,
                 "metadata": {"wrapup_time_secs": 2}},
                {"skill_group_id": "r11", "skills_required": ["r11"],
                 "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 120,
                 "metadata": {"wrapup_time_secs": 2}},
                {"skill_group_id": SKILL_AV, "skills_required": [SKILL_AV],
                 "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 10,
                 "metadata": {"wrapup_time_secs": 2}},
                {"skill_group_id": SKILL_ROT, "skills_required": [SKILL_ROT],
                 "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 120,
                 "metadata": {"wrapup_time_secs": 2}},
            ):
                try:
                    if "skill_group_id" in body:
                        await client.create_skill_group(body)
                    else:
                        await client.create_agent(body)
                except Exception as exc:  # noqa: BLE001 — duplicates fine
                    if not ("409" in str(exc) or "400" in str(exc)
                            or "already" in str(exc).lower()):
                        raise
        finally:
            await session.close()

    asyncio.run(_run())


@pytest.fixture(scope="session")
def transfer_pbx(webhook_server, tmp_path_factory) -> PbxServer:
    greeting_dir = tmp_path_factory.mktemp("transfer_e2e")
    greeting = greeting_dir / "greet.wav"
    generate_sine_wav(greeting, 440.0, 2.0, 8000, 0.4)

    server = PbxServer(
        host=root_conftest.SIP_HOST,
        sip_port=root_conftest.SIP_PORT,
        http_port=root_conftest.HTTP_PORT,
        rwi_token=root_conftest.RWI_TOKEN,
        project_root=root_conftest.PROJECT_ROOT,
        work_dir=root_conftest.ARTIFACT_ROOT,
    )
    cb = server.config_builder
    cb.add_memory_users([BUSY_PEER, AGENT2, ECHO, TONE730, BREAK_AGENT,
                         ROT_A, ROT_B, ROT_OK,
                         "1051", "1052", "1053", "1054",
                         ROUTED_AGENT, "1020"])
    cb.add_ivr("transfer_ivr", _transfer_ivr_toml(greeting))
    cb.add_route(
        "transfer-ivr-route", match={"to.user": IVR_NUM}, priority=10,
        action="application", app="ivr",
        app_params={"file": "config/ivr/transfer_ivr.toml"}, auto_answer=True,
    )
    cb.add_queue(
        QUEUE_NAME, strategy_mode="sequential",
        targets=[f"skill-group:{SKILL_GROUP}"],
        ring_timeout_secs=6, wait_timeout_secs=30,
    )
    cb.add_route(
        "queue-entry-route", match={"to.user": QUEUE_NUM}, priority=10,
        action="queue", queue=QUEUE_NAME, auto_answer=True,
    )
    cb.add_queue(
        "support_r11", strategy_mode="sequential",
        targets=["skill-group:r11"],
        ring_timeout_secs=6, wait_timeout_secs=30, fallback_failure_code=486,
    )
    cb.add_route(
        "queue-entry-route-r11", match={"to.user": QUEUE_NUM_R11}, priority=10,
        action="queue", queue="support_r11", auto_answer=True,
    )
    cb.add_queue(
        "av", strategy_mode="sequential",
        targets=[f"skill-group:{SKILL_AV}"],
        ring_timeout_secs=6, wait_timeout_secs=10, fallback_failure_code=486,
    )
    cb.add_route(
        "av-entry-route", match={"to.user": AV_NUM}, priority=10,
        action="queue", queue="av", auto_answer=True,
    )
    cb.add_queue(
        "rot", strategy_mode="sequential",
        targets=[f"skill-group:{SKILL_ROT}"],
        ring_timeout_secs=4, wait_timeout_secs=60,
    )
    cb.add_route(
        "rot-entry-route", match={"to.user": ROT_NUM}, priority=10,
        action="queue", queue="rot", auto_answer=True,
    )
    # R17's number carries BOTH an application route AND a registrable
    # memory user (production 39300: route-first hijacked agent→agent
    # transfers) — locator-first resolution must dial the endpoint.
    cb.add_route(
        "transfer-routed-agent-route", match={"to.user": ROUTED_AGENT},
        priority=10, action="application", app="ivr",
        app_params={"file": "config/ivr/transfer_ivr.toml"}, auto_answer=True,
    )
    # R18's number: a CC agent extension (no SIP user) that ALSO matches an
    # application route — queue:agent:<ext> hand-off first, IVR fallback.
    cb.add_route(
        "transfer-routed-cc-agent-route", match={"to.user": CC_AGENT_NUM},
        priority=10, action="application", app="ivr",
        app_params={"file": "config/ivr/transfer_ivr.toml"}, auto_answer=True,
    )
    cb.set_proxy_extra(
        conference_factory_uri=f"sip:conf-factory@{root_conftest.SIP_HOST}:{root_conftest.SIP_PORT}",
        # 10 s RTP inactivity window: the R12 hold (15 s) must survive 1.5×
        # the watchdog — this turns the old vacuous assertion into a real
        # regression for the hold/rtpTimeout fix.
        rtp_timeout=10,
    )
    server.prepare(webhook_url=webhook_server.url, build=False)
    server.start(timeout=90)
    _seed_cc_sync(server)
    yield server
    server.stop()
