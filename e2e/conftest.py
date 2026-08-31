"""Root conftest.py — fixtures for the unified Python+sipbot E2E suite.

The `pbx` fixture is function-scoped and configurable: tests customize the
generated config (routes / queues / IVR / addons) via the `pbx_config`
fixture, then the rustpbx binary is built once (cached) and (re)started with
that config for each test.
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

import helpers  # noqa: F401  (bootstraps sys.path + re-exports)

from helpers.pbx_server import PbxServer, find_project_root, pick_free_port
from helpers.webhook_receiver import WebhookServer
from helpers.rwi_client import RwiClient
from helpers.event_checker import EventChecker
from helpers.sipbot import SipBotPool
from helpers.config_builder import ConfigBuilder

logger = logging.getLogger(__name__)

SCRIPT_DIR = Path(__file__).parent
PROJECT_ROOT = find_project_root(SCRIPT_DIR)

REPORT_DIR = Path(os.environ.get("RUSTPBX_E2E_REPORT_DIR", SCRIPT_DIR / "report"))

SIP_HOST = os.environ.get("RUSTPBX_SIP_HOST", "127.0.0.1")


def _worker_index() -> int:
    """xdist worker index (gw0/gw1/...); 0 when not running under xdist."""
    wid = os.environ.get("PYTEST_XDIST_WORKER", "")
    if wid.startswith("gw"):
        try:
            return int(wid[2:])
        except ValueError:
            pass
    return 0


# Under pytest-xdist each worker needs a non-overlapping port range so its PBX
# and sipbot UAs never collide with another worker's (or with unrelated
# services on the host). The UA port space spans ~15070..16920, so each worker
# probes for a free window of that size (plus its HTTP port) starting at
# `worker * PORT_STRIDE` and advancing until the whole window is free.
PORT_STRIDE = int(os.environ.get("RUSTPBX_E2E_PORT_STRIDE", "4000"))


def _port_free(port: int) -> bool:
    """True if 127.0.0.1:port is free on both TCP and UDP."""
    import socket

    for socktype in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
        with socket.socket(socket.AF_INET, socktype) as s:
            try:
                s.bind(("127.0.0.1", port))
            except OSError:
                return False
    return True


def _pick_worker_offset(worker: int) -> int:
    """Find a free port window for this worker (0 for worker 0).

    Probes every UA port the suite actually uses (shifted by the candidate
    offset) plus the PBX SIP/HTTP ports, advancing by PORT_STRIDE until the
    whole set is free. This keeps xdist workers disjoint from each other and
    from unrelated host services (e.g. a docker port map on 20080).
    """
    base_shift = int(os.environ.get("RUSTPBX_E2E_PORT_BASE", "0"))
    if worker == 0:
        return base_shift
    base_sip = int(os.environ.get("RUSTPBX_SIP_PORT", "15070"))
    base_http = int(os.environ.get("RUSTPBX_HTTP_PORT", "18080"))
    # Every fixed local UA/trunk port used across the test suite.
    ua_ports = [
        15080, 15081, 15082, 15083, 15084, 15085, 15086,
        15100, 15101, 15102, 15103, 15104,
        15110, 15111, 15112, 15113, 15114,
        15120, 15121, 15122, 15130, 15131, 15132, 15133,
        15140, 15141, 15142, 15143, 15144, 15145, 15146,
        15150, 15151, 15152, 15160, 15161,
        15170, 15171, 15172,
        15190, 15191, 15192,
        15200, 15201, 15202, 15203, 15204, 15206, 15210,
        15220, 15221, 15222, 15223, 15224,
        15300, 15301,
        15400, 15401,
        15402, 15410, 15420, 15421, 15422, 15430, 15440, 15441, 15442, 15450,
        15460, 15470,
        15480, 15481, 15482, 15483, 15484,
        15500, 15501, 15502, 15503, 15504, 15505, 15506, 15507,
        15508, 15509, 15510, 15511, 15512, 15513, 15514, 15515, 15516, 15517,
        15518, 15519,
        15600, 15601, 15602,
        16700, 16920,
    ]
    off = base_shift + worker * PORT_STRIDE
    while True:
        ok = all(_port_free(p + off) for p in ua_ports) and _port_free(
            base_sip + off
        ) and _port_free(base_http + off)
        if ok:
            return off
        off += PORT_STRIDE


WORKER = _worker_index()
_UA_OFFSET = _pick_worker_offset(WORKER)

SIP_PORT = int(os.environ.get("RUSTPBX_SIP_PORT", "15070")) + _UA_OFFSET
HTTP_PORT = int(os.environ.get("RUSTPBX_HTTP_PORT", "18080")) + _UA_OFFSET
# Make ua_port() in helpers use the same shift as this worker.
os.environ["RUSTPBX_UA_PORT_OFFSET"] = str(_UA_OFFSET)
if WORKER:
    logger.info(
        "xdist worker %s: SIP_PORT=%d HTTP_PORT=%d UA_OFFSET=%d",
        WORKER, SIP_PORT, HTTP_PORT, _UA_OFFSET,
    )

WEBHOOK_HOST = os.environ.get("RUSTPBX_WEBHOOK_HOST", "127.0.0.1")


def _artifact_root() -> Path:
    """Per-worker scratch dir for PBX configs/CDR/sipflow/voicemail/logs.

    xdist workers share the checkout, so each worker gets its own work dir to
    avoid racing on rustpbx_regression.toml / config/{routes,trunks,queue,ivr}
    / config/cdr / config/sipflow. Worker 0 of the DEFAULT session keeps
    PROJECT_ROOT (historical layout) so single-worker runs are unchanged; a
    session with RUSTPBX_E2E_PORT_BASE set (parallel scenario sessions) gets
    its own dir regardless of worker index.

    The PBX resolves static assets (console pages, dev consoles, locales,
    sounds) relative to its cwd, so read-only asset dirs are symlinked into
    each worker dir to keep them resolvable.
    """
    session_base = int(os.environ.get("RUSTPBX_E2E_PORT_BASE", "0"))
    if not WORKER and not session_base:
        return PROJECT_ROOT
    if session_base and not WORKER:
        d = PROJECT_ROOT / "e2e-artifacts" / f"session{session_base}"
    else:
        d = PROJECT_ROOT / "e2e-artifacts" / f"worker{WORKER}"
    d.mkdir(parents=True, exist_ok=True)
    (d / "config").mkdir(parents=True, exist_ok=True)
    for rel in ("src", "static", "locales", "templates", "config/sounds"):
        target = PROJECT_ROOT / rel
        link = d / rel
        if target.exists() and not link.exists() and not link.is_symlink():
            try:
                link.symlink_to(target, target_is_directory=True)
            except OSError:
                pass
    return d


ARTIFACT_ROOT = _artifact_root()
RWI_TOKEN = os.environ.get("RUSTPBX_RWI_TOKEN", "test-api-key-e2e")


def _parse_addon_list(raw: str) -> list[str]:
    return [a.strip() for a in raw.split(",") if a.strip()]


# Default runtime addons for the PBX fixture. Keep this to CC-core routing
# (file-based IVR/queue/p2p). Do NOT put wholesale here: WholesaleRouteInvite
# replaces DefaultRouteInvite and returns NotHandled for non-wholesale trunks,
# which makes every IVR/queue/app route look like "user offline" (480).
# Wholesale / voicemail tests opt in via ConfigBuilder.set_wholesale(),
# add_voicemail().
_raw_addons = os.environ.get("RUSTPBX_E2E_ADDONS", "cc")
DEFAULT_ADDONS = _parse_addon_list(_raw_addons)
if "wholesale" in DEFAULT_ADDONS:
    logger.warning(
        "RUSTPBX_E2E_ADDONS includes 'wholesale' (%r); stripping it from the "
        "default fixture. Wholesale routing replaces file-based routes and "
        "breaks IVR/queue/p2p. Wholesale tests call set_wholesale() themselves. "
        "Use: ./run.sh wholesale  (or ./run.sh scenarios).",
        _raw_addons,
    )
    DEFAULT_ADDONS = [a for a in DEFAULT_ADDONS if a != "wholesale"]
if not DEFAULT_ADDONS:
    DEFAULT_ADDONS = ["cc"]

# The full-featured binary is built once (see `FULL_E2E_FEATURES` in
# helpers.pbx_server) and reused by every session/worker, so no per-run build
# is needed and no feature plumbing is required beyond that.
DEFAULT_FEATURES = _parse_addon_list(
    os.environ.get(
        "RUSTPBX_E2E_FEATURES",
        "addon-cc,addon-voicemail,addon-wholesale",
    )
)


@pytest.fixture(scope="session", autouse=True)
def ensure_rustpbx_binary() -> None:
    """Ensure the rustpbx binary exists, building it once if needed.

    Reuses the stable, feature-complete `target/debug/rustpbx-cc-e2e` copy when
    present (avoids recompiling a ~1GB binary on every session). Only when it is
    missing does it build with the full feature set and copy it to the stable
    path, so all subsequent sessions reuse the same binary.
    """
    from helpers.pbx_server import find_or_build_binary, FULL_E2E_FEATURES

    find_or_build_binary(PROJECT_ROOT, FULL_E2E_FEATURES)


@pytest.fixture(scope="session")
def event_loop():
    loop = asyncio.new_event_loop()
    yield loop
    loop.close()


@pytest.fixture(scope="session")
def webhook_server() -> WebhookServer:
    server = WebhookServer(host=WEBHOOK_HOST, port=0)
    server.start()
    yield server
    server.stop()


# ---------------------------------------------------------------------------
# PBX lifecycle (function-scoped, configurable)
# ---------------------------------------------------------------------------

@pytest.fixture
def pbx_config(webhook_server: WebhookServer) -> ConfigBuilder:
    """Fresh ConfigBuilder per test. Mutate it (routes/queues/IVR/addons) then
    the `pbx` fixture builds and starts rustpbx with it."""
    cb = ConfigBuilder(
        project_root=PROJECT_ROOT,
        work_dir=ARTIFACT_ROOT,
        sip_port=SIP_PORT,
        http_port=HTTP_PORT,
        rwi_token=RWI_TOKEN,
        webhook_url=webhook_server.url,
        addons=list(DEFAULT_ADDONS),
    )
    return cb


@pytest.fixture
def pbx(pbx_config: ConfigBuilder, webhook_server: WebhookServer) -> PbxServer:
    """Return a *prepared but not started* PbxServer.

    Tests customize the config via `pbx.config_builder` (the injected
    `pbx_config`), then call `pbx.prepare(...)` + `pbx.start(timeout=90)`.
    This lets each test tailor routes/queues/IVR/addons before boot.
    Teardown stops the server if it was started.
    """
    server = PbxServer(
        host=SIP_HOST,
        sip_port=SIP_PORT,
        http_port=HTTP_PORT,
        rwi_token=RWI_TOKEN,
        project_root=PROJECT_ROOT,
        work_dir=ARTIFACT_ROOT,
    )
    server._config_builder = pbx_config
    # Build the default config so tests that don't customize can just start().
    server.prepare(webhook_url=webhook_server.url, build=False)
    yield server
    server.stop()


def boot_pbx(pbx: PbxServer, webhook_url: str = "") -> PbxServer:
    """(Re)build config from the mutated builder and start rustpbx."""
    pbx.prepare(webhook_url=webhook_url, build=False)
    pbx.start(timeout=90)
    return pbx


@pytest_asyncio.fixture
async def api(pbx: PbxServer):
    import aiohttp
    from helpers.pbx_server import PbxApiClient

    session = aiohttp.ClientSession()
    client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
    yield client
    await session.close()


@pytest_asyncio.fixture
async def rwi(pbx: PbxServer) -> RwiClient:
    """Return an *unconnected* RwiClient. Tests must `await helpers.connect_rwi(rwi)`
    after booting the PBX (boot happens in the test body, not at fixture setup)."""
    return RwiClient(pbx.rwi_ws_url, RWI_TOKEN)


@pytest.fixture
def sipbot_pool() -> SipBotPool:
    pool = SipBotPool()
    yield pool
    pool.terminate_all()


@pytest.fixture
def ws_bridge_server():
    """WS bridge echo/capture server for IVR bridge E2E tests (function-scoped)."""
    from helpers.ws_bridge_echo import WsBridgeEchoServer

    server = WsBridgeEchoServer()
    server.start()
    yield server
    server.stop()


@pytest.fixture
def webhook_session(webhook_server: WebhookServer):
    webhook_server.receiver.clear()
    return webhook_server.receiver


@pytest.fixture
def event_checker(webhook_session, rwi) -> EventChecker:
    rwi.clear_events()
    return EventChecker(webhook=webhook_session, rwi=rwi)


# ---------------------------------------------------------------------------
# Artifact dirs
# ---------------------------------------------------------------------------

@pytest.fixture
def cdr_dir(pbx: PbxServer) -> Path:
    d = ARTIFACT_ROOT / "config" / "cdr"
    d.mkdir(parents=True, exist_ok=True)
    return d


@pytest.fixture
def sipflow_dir(pbx: PbxServer) -> Path:
    d = ARTIFACT_ROOT / "config" / "sipflow"
    d.mkdir(parents=True, exist_ok=True)
    return d
