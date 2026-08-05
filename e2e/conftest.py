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
SIP_PORT = int(os.environ.get("RUSTPBX_SIP_PORT", "15070"))
HTTP_PORT = int(os.environ.get("RUSTPBX_HTTP_PORT", "18080"))
WEBHOOK_HOST = os.environ.get("RUSTPBX_WEBHOOK_HOST", "127.0.0.1")
RWI_TOKEN = os.environ.get("RUSTPBX_RWI_TOKEN", "test-api-key-e2e")
DEFAULT_ADDONS = os.environ.get("RUSTPBX_E2E_ADDONS", "cc").split(",")
# Features the rustpbx binary must be compiled with (community addons used by
# wholesale/voicemail/sbc tests). `cargo build --features ...` is incremental:
# it only rebuilds when the requested feature set differs from the current binary.
DEFAULT_FEATURES = os.environ.get(
    "RUSTPBX_E2E_FEATURES",
    "addon-cc,addon-sbc,addon-voicemail,addon-wholesale",
).split(",")


@pytest.fixture(scope="session", autouse=True)
def ensure_rustpbx_binary() -> None:
    """Build rustpbx once with the full addon feature set (incremental)."""
    import subprocess

    features = ",".join(DEFAULT_FEATURES)
    logger.info("Ensuring rustpbx binary with features: %s", features)
    result = subprocess.run(
        ["cargo", "build", "--features", features],
        cwd=str(PROJECT_ROOT),
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"cargo build failed:\n{result.stderr[-2000:]}")


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
        work_dir=PROJECT_ROOT,
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
        work_dir=PROJECT_ROOT,
    )
    server._config_builder = pbx_config
    # Build the default config so tests that don't customize can just start().
    server.prepare(webhook_url=webhook_server.url, extra_features=DEFAULT_FEATURES, build=False)
    yield server
    server.stop()


def boot_pbx(pbx: PbxServer, webhook_url: str = "") -> PbxServer:
    """(Re)build config from the mutated builder and start rustpbx."""
    pbx.prepare(webhook_url=webhook_url, extra_features=DEFAULT_FEATURES, build=False)
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
    d = PROJECT_ROOT / "config" / "cdr"
    d.mkdir(parents=True, exist_ok=True)
    return d


@pytest.fixture
def sipflow_dir(pbx: PbxServer) -> Path:
    d = PROJECT_ROOT / "config" / "sipflow"
    d.mkdir(parents=True, exist_ok=True)
    return d
