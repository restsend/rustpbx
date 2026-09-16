"""Root conftest.py — session-scoped fixtures for the E2E regression suite.

Fixtures provided:
  - pbx:            PbxServer (starts/stops rustpbx binary)
  - webhook_server: WebhookServer (captures RWI webhook POSTs)
  - api:            PbxApiClient (async REST client)
  - rwi:            RwiClient (async WebSocket client)
  - event_checker:  EventChecker (assertion helpers)
  - sipbot_pool:    SipBotPool (manages sipbot subprocesses per test)
  - browser:        Playwright browser instance
  - report:         ReportGenerator (collects results)
"""

from __future__ import annotations

import asyncio
import logging
import os
import sys
from pathlib import Path
from typing import AsyncGenerator, Optional

import pytest
import pytest_asyncio

# Ensure helpers are importable
SCRIPT_DIR = Path(__file__).parent
sys.path.insert(0, str(SCRIPT_DIR))

from helpers.pbx_server import PbxServer, find_project_root
from helpers.webhook_receiver import WebhookServer
from helpers.rwi_client import RwiClient
from helpers.event_checker import EventChecker
from helpers.sipbot import SipBotPool
from helpers.report import ReportGenerator, TestRecord

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Configuration constants
# ---------------------------------------------------------------------------

PROJECT_ROOT = find_project_root(SCRIPT_DIR)
# Isolated work dir — the suite generates config/queue|routes|ivr/cc TOMLs and
# boots rustpbx with CWD=work_dir. Running from PROJECT_ROOT polluted the
# developer's own config/ directory (e2e_*.toml queues + a rewritten
# config/cc/cc.toml wiping the global [csat] section). rustpbx still resolves
# repo assets (sounds/locales/templates) through symlinks created by
# PbxServer.start().
WORK_DIR = PROJECT_ROOT / "target" / "e2e-cc-regression"
REPORT_DIR = Path(os.environ.get("RUSTPBX_E2E_REPORT_DIR", SCRIPT_DIR / "report"))
SCREENSHOT_DIR = REPORT_DIR / "screenshots"

SIP_HOST = os.environ.get("RUSTPBX_SIP_HOST", "127.0.0.1")
# Lane-aware ports: shift by RUSTPBX_E2E_PORT_BASE so parallel lanes
# (see regression/run_full_e2e.sh) never collide with each other or a dev PBX.
_BASE_SHIFT = int(os.environ.get("RUSTPBX_E2E_PORT_BASE", "0"))
SIP_PORT = int(os.environ.get("RUSTPBX_SIP_PORT", "15070")) + _BASE_SHIFT
HTTP_PORT = int(os.environ.get("RUSTPBX_HTTP_PORT", "18080")) + _BASE_SHIFT
WEBHOOK_HOST = os.environ.get("RUSTPBX_WEBHOOK_HOST", "127.0.0.1")
RWI_TOKEN = os.environ.get("RUSTPBX_RWI_TOKEN", "test-api-key-e2e")


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

def _get_tier(request) -> str:
    markers = request.node.get_closest_marker("tier1")
    if markers:
        return "tier1"
    if request.node.get_closest_marker("tier2"):
        return "tier2"
    if request.node.get_closest_marker("tier3"):
        return "tier3"
    return "tier1"


def _get_module(request) -> str:
    for mod in ["trunk", "routing", "ivr", "queue", "acd", "cc_phone", "presence", "webhook",
                 "transfer", "conference", "supervisor"]:
        if request.node.get_closest_marker(mod):
            return mod
    parts = request.node.nodeid.split("/")
    if len(parts) >= 2:
        fname = parts[-1].replace("test_", "").replace(".py", "")
        return fname
    return "unknown"


# ---------------------------------------------------------------------------
# Session-scoped fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session")
def event_loop():
    loop = asyncio.new_event_loop()
    yield loop
    loop.close()


@pytest.fixture(scope="session")
def report_generator() -> ReportGenerator:
    REPORT_DIR.mkdir(parents=True, exist_ok=True)
    SCREENSHOT_DIR.mkdir(parents=True, exist_ok=True)
    return ReportGenerator(REPORT_DIR)


@pytest.fixture(scope="session")
def webhook_server() -> WebhookServer:
    server = WebhookServer(host=WEBHOOK_HOST, port=0)
    server.start()
    yield server
    server.stop()


@pytest.fixture(scope="session")
def pbx(webhook_server: WebhookServer) -> PbxServer:
    server = PbxServer(
        host=SIP_HOST,
        sip_port=SIP_PORT,
        http_port=HTTP_PORT,
        rwi_token=RWI_TOKEN,
        project_root=PROJECT_ROOT,
        work_dir=WORK_DIR,
    )
    server.prepare(webhook_url=webhook_server.url)
    # Add base IVR configs and routes before starting
    from helpers.config_builder import ivr_greeting_dtmf_toml
    server.config_builder.add_ivr(
        "ivr-test",
        ivr_greeting_dtmf_toml(
            route_point="ivr-test",
            greeting_text="Welcome to the test IVR. Press 1 for sales, 2 for support.",
            transfer_target="1002",
        ),
    )
    server.config_builder.add_route(
        "ivr-route",
        match={"to.user": "ivr-test"},
        priority=50,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-test.toml"},
        auto_answer=True,
    )

    # PlayAndHangup fixture app (CSV L105): on DTMF timeout the menu plays a
    # prompt and hangs up from the server side.
    server.config_builder.add_ivr("ivr-pah", """\
[ivr]
name = "ivr-pah"
ivr_mode = "tree"

[ivr.root]
greeting = "sounds/phone-calling.wav"
greeting_text = "This call will end after the prompt."
timeout_ms = 4000
max_retries = 3
timeout_action = { type = "play_and_hangup", prompt_text = "Goodbye." }
""")
    server.config_builder.add_route(
        "ivr-pah-route",
        match={"to.user": "ivr-pah"},
        priority=50,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-pah.toml"},
        auto_answer=True,
    )

    # Max-retries fixture app (CSV L84): no greeting audio and no invalid
    # prompt so invalid-key cycles are fast; the 2nd invalid key exceeds
    # max_retries=1 and must execute max_retries_action (hangup).
    server.config_builder.add_ivr("ivr-retry", """\
[ivr]
name = "ivr-retry"
ivr_mode = "tree"

[ivr.root]
greeting_text = "Press 1 or 2."
timeout_ms = 2000
max_retries = 1
timeout_action = { type = "repeat" }
max_retries_action = { type = "hangup" }

[[ivr.root.entries]]
key = "1"
label = "One"

[ivr.root.entries.action]
type = "hangup"
""")
    server.config_builder.add_route(
        "ivr-retry-route",
        match={"to.user": "ivr-retry"},
        priority=50,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-retry.toml"},
        auto_answer=True,
    )

    # Voicemail fixture app (CSV L94): key "9" chains into the voicemail app
    # with mailbox target 1002.
    server.config_builder.add_ivr("ivr-vm", """\
[ivr]
name = "ivr-vm"
ivr_mode = "tree"

[ivr.root]
greeting = "sounds/phone-calling.wav"
greeting_text = "Press 9 to leave a voicemail."
timeout_ms = 8000
max_retries = 3
timeout_action = { type = "hangup" }

[[ivr.root.entries]]
key = "9"
label = "Voicemail"

[ivr.root.entries.action]
type = "voicemail"
target = "1002"
""")
    server.config_builder.add_route(
        "ivr-vm-route",
        match={"to.user": "ivr-vm"},
        priority=50,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/ivr-vm.toml"},
        auto_answer=True,
    )
    # Queue configs that map skill-group names to dial targets — without these,
    # resolve_queue_config("support") returns None and IVR/queue dispatch fails
    # with "Queue 'support' not found".
    for sg in ("support", "sales", "vip"):
        server.config_builder.add_queue(
            sg,
            strategy_mode="sequential",
            targets=[f"skill-group:{sg}"],
        )
    # Route an external caller straight into the queue app so tests can drive a
    # real queue→ACD→agent dispatch (RWI originate to `queue:…` is rejected with
    # 407 because the RWI UAC caller is unauthenticated).
    server.config_builder.add_route(
        "queue-dispatch-route",
        match={"to.user": "8888"},
        priority=10,
        action="queue",
        queue="support",
    )
    # ── checklist (cc/checklist.md) fixture queues ──
    # Each maps a dialable number to a dedicated skill group so checklist
    # tests can drive real queue→ACD→agent dispatch without touching the
    # shared support/sales/vip groups.
    checklist_queues = {
        "ovf-prime": "8891",
        "ovf-backup": "8892",
        "fb-grp": "8893",
        "mute-grp": "8894",
        "order-grp": "8895",
        "xfer-sg": "8896",
        "batch-sg": "8897",
        "hot-grp": "8898",
        "base-grp": "8899",
        "ring-grp": "8890",
        "loop-sg": "8889",
    }
    for sg, number in checklist_queues.items():
        server.config_builder.add_queue(
            sg,
            strategy_mode="sequential",
            targets=[f"skill-group:{sg}"],
            hold_audio="sounds/phone-calling.wav",
        )
        server.config_builder.add_route(
            f"queue-{sg}-route",
            match={"to.user": number},
            priority=10,
            action="queue",
            queue=sg,
        )
    # SIP accounts for the checklist agents (2xxx) — the auth backend only
    # accepts REGISTERs from users listed here.
    server.config_builder.add_memory_users(
        [str(n) for n in range(2100, 2114)]
        + [str(n) for n in range(2201, 2206)]
        + [str(n) for n in range(2301, 2321)]
    )
    # Re-generate config with IVR routes
    server.config_path = server.config_builder.build()
    server.start(timeout=60)

    # Seed the default CC agents/skill-groups ONCE at session start. The CC
    # addon does not load agents from files (see pbx_server.py:556), so without
    # this, tests that do not request the `api` fixture (e.g.
    # test_agent_registration_webhook, test_queue_return_to_ivr,
    # test_route_multi_trunk_round_robin) run against an empty agent registry
    # and the registrar_bridge logs "No CC agent found for extension" → no
    # agent_registered webhook event → false failures. Seeding here guarantees
    # agents exist for every test regardless of which fixtures it pulls in.
    import asyncio as _asyncio
    import aiohttp as _aiohttp
    from helpers.pbx_server import PbxApiClient as _Client

    async def _seed():
        sess = _aiohttp.ClientSession()
        try:
            cli = _Client(sess, server.http_url, server.rwi_token)
            await cli.ensure_console_auth()
            await cli.seed_default_agents()
        finally:
            await sess.close()

    try:
        _asyncio.run(_seed())
        server._cc_seeded = True
    except Exception as exc:  # noqa: BLE001
        import logging as _logging
        _logging.getLogger(__name__).warning(
            "session-level agent seed failed (will retry lazily in `api`): %s", exc)

    yield server
    server.stop()


@pytest_asyncio.fixture
async def api(pbx: PbxServer):
    """Function-scoped REST client — each test gets its own aiohttp session.

    Auto-establishes console auth (register first superuser + login) so that
    /api/cc/* console + phone-control routes are authenticated instead of
    401/303. Seeds the default CC agents/skill-groups once per pbx session
    (the CC addon does not load agents from files).
    """
    import aiohttp
    session = aiohttp.ClientSession()
    from helpers.pbx_server import PbxApiClient
    client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
    try:
        # A prior test may have called /ami/v1/reload/app (or reload/routes),
        # which restarts the app and briefly drops HTTP. Wait for readiness so
        # this test doesn't hit ConnectionRefused at setup.
        import asyncio as _asyncio
        for _ in range(20):
            try:
                async with session.get(f"{pbx.http_url}/console/cc", timeout=2) as resp:
                    if resp.status < 500:
                        break
            except Exception:
                pass
            await _asyncio.sleep(0.5)
        await client.ensure_console_auth()
        # Always (re)seed default agents/skill-groups. Seeding is idempotent
        # (409/duplicate treated as success), and the session DB can be reset
        # mid-run (e.g. by console superuser re-creation), which wipes the
        # CC agent registry. Re-seeding every test guarantees agents exist
        # regardless of prior state — without this, registrar_bridge logs
        # "No CC agent found" and registration/queue/hold tests see no events.
        await client.seed_default_agents()
        pbx._cc_seeded = True
    except Exception as exc:  # noqa: BLE001 — don't let auth/seed setup mask test errors
        import logging
        logging.getLogger(__name__).warning("api setup (auth/seed) failed: %s", exc)
    yield client
    await session.close()


@pytest_asyncio.fixture
async def rwi(pbx: PbxServer) -> AsyncGenerator[RwiClient, None]:
    """Function-scoped RWI client.

    MUST be function-scoped (not session): the receive_loop task runs on the
    fixture's event loop, and pytest-asyncio drives session-scoped async
    fixtures on a *different* loop than function-scoped tests. With a session
    scope the receive_loop never advances during a test, so originate/transfer
    replies (and all WS events) never get processed → every RWI request times
    out. Function scope puts the receive_loop on the same loop as the test.
    """
    client = RwiClient(pbx.rwi_ws_url, RWI_TOKEN)
    await client.connect()
    await client.subscribe(["*"])
    yield client
    await client.disconnect()


# ---------------------------------------------------------------------------
# Function-scoped fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def sipbot_pool() -> SipBotPool:
    pool = SipBotPool()
    yield pool
    pool.terminate_all()


@pytest.fixture
def webhook_session(webhook_server: WebhookServer):
    """Clear webhook events before each test."""
    webhook_server.receiver.clear()
    return webhook_server.receiver


@pytest.fixture
def event_checker(webhook_session, rwi) -> EventChecker:
    rwi.clear_events()
    return EventChecker(webhook=webhook_session, rwi=rwi)


# ---------------------------------------------------------------------------
# Playwright fixtures
# ---------------------------------------------------------------------------

@pytest_asyncio.fixture
async def _playwright_instance():
    try:
        from playwright.async_api import async_playwright
    except ImportError:
        pytest.skip("playwright not installed")
    pw = await async_playwright().start()
    yield pw
    await pw.stop()


@pytest_asyncio.fixture
async def browser(_playwright_instance):
    br = await _playwright_instance.chromium.launch(
        headless=True,
        args=[
            "--use-fake-ui-for-media-stream",
            "--use-fake-device-for-media-stream",
            "--allow-http-screen-capture",
            "--no-sandbox",
        ],
    )
    yield br
    await br.close()


@pytest_asyncio.fixture
async def browser_context(browser):
    context = await browser.new_context(
        viewport={"width": 1600, "height": 1000},
        permissions=["microphone"],
    )
    yield context
    await context.close()


@pytest_asyncio.fixture
async def page(browser_context):
    pg = await browser_context.new_page()
    yield pg
    await pg.close()


# ---------------------------------------------------------------------------
# ACD test isolation (session-scoped pbx → per-test agent cleanup)
# ---------------------------------------------------------------------------

@pytest_asyncio.fixture(autouse=True)
async def acd_test_isolation(request, sipbot_pool, api, pbx):
    """Reset shared agents before/after every @pytest.mark.acd test."""
    if request.node.get_closest_marker("acd") is None:
        yield
        return
    from helpers.acd_strategy_e2e import cleanup_acd_agents

    await cleanup_acd_agents(sipbot_pool, api, pbx)
    yield
    await cleanup_acd_agents(sipbot_pool, api, pbx)


# ---------------------------------------------------------------------------
# Report collection hook
# ---------------------------------------------------------------------------

@pytest.fixture(autouse=True)
def _record_test_result(request, report_generator: ReportGenerator, webhook_server):
    """Collect test results into the report after each test."""
    yield
    # Note: webhook events are collected post-test via the hook below
    tier = _get_tier(request)
    module = _get_module(request)
    rep = request.node.rep_call if hasattr(request.node, "rep_call") else None

    status = "passed"
    error = None
    duration = 0.0
    if rep is not None:
        duration = rep.duration
        if rep.failed:
            status = "failed"
            error = str(rep.longrepr) if rep.longrepr else "Test failed"
        elif rep.skipped:
            status = "skipped"

    record = TestRecord(
        name=request.node.name,
        module=module,
        tier=tier,
        status=status,
        duration=duration,
        error=error,
        webhook_events=[
            {
                "timestamp": e.timestamp,
                "event_type": e.event_type,
                "call_id": e.call_id,
            }
            for e in webhook_server.receiver.all_events()
        ],
    )
    report_generator.add_record(record)


@pytest.hookimpl(hookwrapper=True, tryfirst=True)
def pytest_runtest_makereport(item, call):
    """Store test report on the item for the result collection fixture."""
    outcome = yield
    rep = outcome.get_result()
    setattr(item, f"rep_{rep.when}", rep)


# Module-level singleton for report generator
_global_report: Optional[ReportGenerator] = None


@pytest.fixture(scope="session", autouse=True)
def _init_global_report(report_generator: ReportGenerator):
    global _global_report
    _global_report = report_generator
    yield
    if _global_report:
        _global_report.write()
