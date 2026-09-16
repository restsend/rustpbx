"""Unified regression conftest — fixtures for all domain suites.

Ported from e2e/conftest.py (function-scoped pbx, port windows, webhook/RWI/
sipbot plumbing) with the new artifact layout and evidence subsystem:

    artifacts/<RUSTPBX_RUN_ID>/lane-<lane>/worker<N>/   PBX configs/CDR/logs
    artifacts/<RUSTPBX_RUN_ID>/lane-<lane>/tests/<w>-<test>/   evidence dirs

The repo checkout is never used as a scratch dir (fixes the historical
worker-0-writes-into-repo behavior). Read-only asset dirs are symlinked into
each worker dir so the PBX can resolve static assets.
"""

from __future__ import annotations

import asyncio
import logging
import os
import sys
import time
from pathlib import Path

import pytest
import pytest_asyncio

SCRIPT_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = SCRIPT_DIR.parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))  # make `helpers` importable

import helpers  # noqa: F401,E402  (re-exports ConfigBuilder/PbxServer/...)

from helpers.pbx_server import PbxServer, PbxApiClient, find_project_root  # noqa: E402
from helpers.webhook_receiver import WebhookServer  # noqa: E402
from helpers.rwi_client import RwiClient  # noqa: E402
from helpers.event_checker import EventChecker  # noqa: E402
from helpers.sipbot import SipBotPool  # noqa: E402
from helpers.config_builder import ConfigBuilder  # noqa: E402
from helpers.evidence import EvidenceStore  # noqa: E402

logger = logging.getLogger(__name__)

PROJECT_ROOT = find_project_root(SCRIPT_DIR)
SIP_HOST = os.environ.get("RUSTPBX_SIP_HOST", "127.0.0.1")
WEBHOOK_HOST = os.environ.get("RUSTPBX_WEBHOOK_HOST", "127.0.0.1")
RWI_TOKEN = os.environ.get("RUSTPBX_RWI_TOKEN", "test-api-key-e2e")

LANE = os.environ.get("RUSTPBX_LANE", "lane0")
RUN_ID = os.environ.get("RUSTPBX_RUN_ID") or time.strftime("%Y%m%d_%H%M%S")
os.environ.setdefault("RUSTPBX_RUN_ID", RUN_ID)
ARTIFACTS_ROOT = Path(os.environ.get("RUSTPBX_ARTIFACTS_ROOT", SCRIPT_DIR / "artifacts"))


def _worker_index() -> int:
    wid = os.environ.get("PYTEST_XDIST_WORKER", "")
    if wid.startswith("gw"):
        try:
            return int(wid[2:])
        except ValueError:
            pass
    return 0


def _port_free(port: int) -> bool:
    import socket

    for socktype in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
        with socket.socket(socket.AF_INET, socktype) as s:
            try:
                s.bind(("127.0.0.1", port))
            except OSError:
                return False
    return True


PORT_STRIDE = int(os.environ.get("RUSTPBX_E2E_PORT_STRIDE", "4000"))

# Fixed local UA/trunk ports used across the suite (ported from e2e/conftest).
UA_PORTS = [
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


def _pick_worker_offset(worker: int) -> int:
    base_shift = int(os.environ.get("RUSTPBX_E2E_PORT_BASE", "0"))
    env_base_set = "RUSTPBX_E2E_PORT_BASE" in os.environ
    if worker == 0:
        if env_base_set:
            return base_shift
        # Direct (non-runner) invocation: if the default SIP port is busy
        # (e.g. a dev instance on config.toml.dev), auto-shift to a free window
        # instead of failing every test with "Address already in use".
        base_sip = int(os.environ.get("RUSTPBX_SIP_PORT", "15070"))
        base_http = int(os.environ.get("RUSTPBX_HTTP_PORT", "18080"))
        off = 0
        while not (
            all(_port_free(p + off) for p in UA_PORTS)
            and _port_free(base_sip + off)
            and _port_free(base_http + off)
        ):
            off += PORT_STRIDE
            if off > 400000:
                raise RuntimeError("no free port window found (probed up to +400000)")
        return off
    base_sip = int(os.environ.get("RUSTPBX_SIP_PORT", "15070"))
    base_http = int(os.environ.get("RUSTPBX_HTTP_PORT", "18080"))
    off = base_shift + worker * PORT_STRIDE
    while True:
        if (
            all(_port_free(p + off) for p in UA_PORTS)
            and _port_free(base_sip + off)
            and _port_free(base_http + off)
        ):
            return off
        off += PORT_STRIDE


WORKER = _worker_index()
_UA_OFFSET = _pick_worker_offset(WORKER)
SIP_PORT = int(os.environ.get("RUSTPBX_SIP_PORT", "15070")) + _UA_OFFSET
HTTP_PORT = int(os.environ.get("RUSTPBX_HTTP_PORT", "18080")) + _UA_OFFSET
if SIP_PORT > 65535 or HTTP_PORT > 65535:
    # keep the suite runnable: fall back to the highest safe window instead of
    # hard-failing collection (the guard previously aborted every test).
    _SAFE = 40000
    if (_UA_OFFSET - _SAFE) >= 0:
        _UA_OFFSET -= _SAFE
        SIP_PORT -= _SAFE
        HTTP_PORT -= _SAFE
        logger.warning("port overflow avoided: shifted down by %d (SIP=%d HTTP=%d)", _SAFE, SIP_PORT, HTTP_PORT)
    else:
        raise RuntimeError(
            f"port overflow: SIP_PORT={SIP_PORT} HTTP_PORT={HTTP_PORT} "
            f"(RUSTPBX_E2E_PORT_BASE too large; must keep all ports < 65535)"
        )
os.environ["RUSTPBX_UA_PORT_OFFSET"] = str(_UA_OFFSET)


def _artifact_root() -> Path:
    """Per-lane/per-worker scratch dir under regression/artifacts/<run_id>/.

    Never the repo checkout. Read-only asset dirs are symlinked so the PBX
    resolves static assets relative to its cwd.
    """
    worker_dir = f"worker{WORKER}" if WORKER else "worker0"
    d = ARTIFACTS_ROOT / RUN_ID / f"lane-{LANE}" / worker_dir
    d.mkdir(parents=True, exist_ok=True)
    (d / "config").mkdir(parents=True, exist_ok=True)
    (d / "tests" / "logs").mkdir(parents=True, exist_ok=True)
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

if WORKER:
    logger.info(
        "lane=%s worker=%s: SIP_PORT=%d HTTP_PORT=%d UA_OFFSET=%d artifacts=%s",
        LANE, WORKER, SIP_PORT, HTTP_PORT, _UA_OFFSET, ARTIFACT_ROOT,
    )


def _parse_addon_list(raw: str) -> list[str]:
    return [a.strip() for a in raw.split(",") if a.strip()]


# Default runtime addons: CC-core routing only. Wholesale replaces default
# routing and must stay opt-in (tests call set_wholesale() themselves).
_raw_addons = os.environ.get("RUSTPBX_E2E_ADDONS", "cc")
DEFAULT_ADDONS = _parse_addon_list(_raw_addons)
if "wholesale" in DEFAULT_ADDONS:
    DEFAULT_ADDONS = [a for a in DEFAULT_ADDONS if a != "wholesale"]
if not DEFAULT_ADDONS:
    DEFAULT_ADDONS = ["cc"]


# ---------------------------------------------------------------------------
# Evidence subsystem (per-test store + failure capture)
# ---------------------------------------------------------------------------

@pytest.hookimpl(hookwrapper=True, tryfirst=True)
def pytest_runtest_makereport(item, call):
    """Stash each phase's report on the item for fixture-time failure checks."""
    outcome = yield
    rep = outcome.get_result()
    setattr(item, f"rep_{rep.when}", rep)


@pytest.fixture(autouse=True)
def evidence(request, tmp_path) -> EvidenceStore:
    """Autouse: every test gets an evidence store; failures auto-capture
    browser screenshots + PBX log tails across the whole suite."""
    """Per-test evidence store; auto-captures on failure:
    * browser page screenshot (when a `page`/`browser_page` fixture exists)
    * PBX log tail (when a `pbx` fixture was used)
    """
    store = EvidenceStore.create(
        request.node.nodeid,
        ARTIFACTS_ROOT,
        worker=os.environ.get("PYTEST_XDIST_WORKER", "gw0"),
        lane=LANE,
        run_id=RUN_ID,
    )
    yield store
    rep = getattr(request.node, "rep_call", None)
    failed = bool(rep and rep.failed)
    if failed:
        store.set_status("failed", rep.longreprtext if hasattr(rep, "longreprtext") else str(rep.longrepr))
        # 1) auto-screenshot from any live Playwright page
        for fname in ("page", "browser_page", "cc_phone_page"):
            page = request.node.funcargs.get(fname)
            if page is not None:
                try:
                    shot = store.dir / "failure_autoshot.png"
                    page.screenshot(path=str(shot))
                    store.entries.append({"kind": "screenshot", "file": shot.name, "note": "auto on failure"})
                    break
                except Exception:
                    continue
        # 2) PBX log tail snapshot
        pbx = request.node.funcargs.get("pbx")
        if pbx is not None and getattr(pbx, "log_file_path", None):
            try:
                from helpers.evidence import _sanitize  # local import guard

                log = Path(pbx.log_file_path)
                if log.exists():
                    tail = log.read_text(encoding="utf-8", errors="replace")[-20000:]
                    (store.dir / "pbx_log_tail.txt").write_text(tail, encoding="utf-8")
                    store.entries.append({"kind": "text", "file": "pbx_log_tail.txt", "note": "pbx log tail on failure"})
            except Exception:
                pass
    else:
        store.set_status("passed" if rep else "unknown")
    store.flush()


# ---------------------------------------------------------------------------
# PBX lifecycle + plumbing (ported, function-scoped)
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session", autouse=True)
def ensure_rustpbx_binary() -> None:
    """Use RUSTPBX_E2E_BIN when the runner prebuilt it; otherwise reuse/build
    the stable feature-complete binary exactly once."""
    from helpers.pbx_server import find_or_build_binary, FULL_E2E_FEATURES

    prebuilt = os.environ.get("RUSTPBX_E2E_BIN")
    if prebuilt and Path(prebuilt).exists():
        return
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


@pytest.fixture
def pbx_config(webhook_server: WebhookServer) -> ConfigBuilder:
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
    server = PbxServer(
        host=SIP_HOST,
        sip_port=SIP_PORT,
        http_port=HTTP_PORT,
        rwi_token=RWI_TOKEN,
        project_root=PROJECT_ROOT,
        work_dir=ARTIFACT_ROOT,
    )
    server._config_builder = pbx_config
    server.prepare(webhook_url=webhook_server.url, build=False)
    yield server
    server.stop()


@pytest_asyncio.fixture
async def api(pbx: PbxServer):
    import aiohttp

    session = aiohttp.ClientSession()
    client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
    yield client
    await session.close()


@pytest_asyncio.fixture
async def rwi(pbx: PbxServer) -> RwiClient:
    return RwiClient(pbx.rwi_ws_url, RWI_TOKEN)


@pytest.fixture
def sipbot_pool() -> SipBotPool:
    pool = SipBotPool()
    yield pool
    pool.terminate_all()


@pytest.fixture
def ws_bridge_server():
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

