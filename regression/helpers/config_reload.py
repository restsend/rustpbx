"""Runtime config reload helper for tests that inject IVR/routes after pbx start.

The CC pbx reads its routes file and IVR TOML files at startup; adding entries
to `ConfigBuilder` in-memory after `PbxServer.start()` does nothing until the
files are re-written AND the pbx is told to reload. `apply_config` does both.
"""

from __future__ import annotations

import asyncio
import logging

import aiohttp

logger = logging.getLogger(__name__)


async def apply_config(pbx, api, *, reload_app: bool = False) -> None:
    """Re-write config files (routes/ivr/cc.toml) and trigger a pbx reload.

    Call this AFTER `pbx.config_builder.add_ivr(...)` / `add_route(...)` so the
    newly added routes become live for the current pbx session.

    ⚠ `reload_app=True` performs a FULL in-process restart (all SIP
    registrations, agent presence and ACD state are wiped — sipbot UAs only
    re-REGISTER on expiry, i.e. never mid-test). Tests that need a registered
    agent/UA afterwards MUST (re-)register AFTER calling this, once
    `wait_for_pbx_ready` returns. Prefer `reload_app=False` when a routes
    hot-reload suffices (IVR/route additions do).
    """
    pbx.config_builder.build()
    try:
        await api.reload_routes()
    except Exception as exc:  # noqa: BLE001
        logger.debug("reload_routes failed (may be benign): %s", exc)
    # Queue/IVR definitions live in config/queue/*.toml which is only read on
    # a queues reload — reload_routes alone does NOT pick up newly written
    # queue files (a queue added by a test would resolve as "not found").
    try:
        await api.reload_queues()
    except Exception as exc:  # noqa: BLE001
        logger.debug("reload_queues failed (may be benign): %s", exc)
    # Trunk definitions (config/trunks/*.toml) likewise need their own reload
    # for e.g. inbound_hosts (trunk-source OPTIONS answering) to take effect.
    try:
        await api.reload_trunks()
    except Exception as exc:  # noqa: BLE001
        logger.debug("reload_trunks failed (may be benign): %s", exc)
    if reload_app:
        try:
            await api.reload_app()
        except Exception as exc:  # noqa: BLE001
            logger.debug("reload_app failed (may be benign): %s", exc)
    # Give the proxy a moment to pick up the reloaded routes.
    await asyncio.sleep(1)
    if reload_app:
        await wait_for_pbx_ready(pbx)


async def wait_for_pbx_ready(pbx, timeout: float = 60.0) -> bool:
    """Wait until the pbx HTTP endpoint answers again after an app reload.

    The reload handler cancels the app token and the runtime loop sleeps 3 s
    (socket release) before rebuilding, so requests right after `reload_app`
    may hit the old instance or a torn-down one.
    """
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        try:
            if pbx._http_session is None:
                return True  # no session yet — nothing to probe with
            async with pbx._http_session.get(
                f"{pbx.http_url}/api/notifications/unread-count",
                timeout=aiohttp.ClientTimeout(total=2),
            ) as resp:
                if resp.status in (200, 401):
                    return True
        except Exception:  # noqa: BLE001
            pass
        await asyncio.sleep(0.5)
    logger.warning("pbx HTTP endpoint did not become ready within %ss", timeout)
    return False


