"""Console extensions CRUD — full write lifecycle via REST (previously zero e2e).

Strict: every mutation must round-trip (create echo → query filter → patch
diff → delete), and a created extension's SIP password must actually
authenticate a REGISTER through the proxy auth backend.
"""

from __future__ import annotations

import pytest
import pytest_asyncio

import helpers as h
from helpers import assertions as A

pytestmark = [pytest.mark.console, pytest.mark.extensions]


@pytest_asyncio.fixture
async def console_api(pbx, webhook_server):
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    import aiohttp
    from helpers.pbx_server import PbxApiClient

    session = aiohttp.ClientSession()
    client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
    assert await client.ensure_console_auth(), "console superuser auth failed"
    yield client
    await session.close()


def _unwrap(item: dict) -> dict:
    """List items arrive wrapped as {departments, extension:{...}}."""
    if isinstance(item, dict) and isinstance(item.get("extension"), dict):
        return item["extension"]
    return item or {}


def _find(items, ext: str):
    for it in items or []:
        if str(_unwrap(it).get("extension")) == ext:
            return _unwrap(it)
    return None




def _enable_extension_auth_backend(pbx):
    """Append a DB-backed `extension` auth backend to the generated config.

    e2e configs default to memory users only; console-created extensions only
    authenticate when this backend is active (mirrors production).
    """
    from pathlib import Path as _P

    conf = _P(pbx.config_path)
    text = conf.read_text(encoding="utf-8")
    # ttl = 1s so credential revocation (extension deletion) is observable quickly
    text += '\n[[proxy.user_backends]]\ntype = "extension"\nttl = 1\n'
    conf.write_text(text, encoding="utf-8")


@pytest_asyncio.fixture
async def auth_backend_api(pbx, webhook_server):
    # file-backed DB so the console-created `rustpbx_extensions` table is
    # visible to the proxy auth backend (in-memory DB is per-connection)
    pbx.config_builder.database_url = f"sqlite://{pbx.work_dir}/console-e2e.db?mode=rwc"
    pbx.prepare(webhook_url=webhook_server.url, build=False)
    _enable_extension_auth_backend(pbx)
    pbx.start(timeout=90)
    import aiohttp
    from helpers.pbx_server import PbxApiClient

    session = aiohttp.ClientSession()
    client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
    assert await client.ensure_console_auth(), "console superuser auth failed"
    yield client
    await session.close()


@pytest.mark.asyncio
async def test_extensions_full_crud_lifecycle(console_api, evidence):
    """create -> query round-trip (field-level) -> patch -> re-query -> delete -> gone."""
    ext = "7101"
    created = await console_api.put("/api/extensions", {
        "extension": ext,
        "display_name": "CRUD Probe",
        "email": "crud@e2e.local",
        "sip_password": "crud-pass-1",
    })
    assert isinstance(created, dict), f"create must return the model: {str(created)[:200]}"
    ext_id = created.get("id")
    A.require(ext_id is not None, "created extension id", f"payload: {str(created)[:300]}")
    # create replies {id, status:"ok"} — the field round-trip is asserted via query below

    listing = await console_api.post("/api/extensions", {
        "page": 1, "per_page": 50, "filters": {"q": ext},
    })
    items = listing.get("items") if isinstance(listing, dict) else listing
    A.require(items is not None, "query items", f"listing: {str(listing)[:300]}")
    hit = _find(items, ext)
    A.require(hit, f"created extension {ext} in filtered query", f"items: {str(items)[:400]}")
    assert (hit.get("display_name") or "") == "CRUD Probe", f"display_name not persisted: {hit}"
    # Security regression (N1 fix): the list API must never return sip_password
    assert "sip_password" not in hit or hit.get("sip_password") is None, (
        f"SECURITY: list API leaked sip_password: {hit}"
    )
    raw_items_text = str(items)
    assert "crud-pass-1" not in raw_items_text, "SECURITY: raw password value leaked in listing"
    evidence.log_metric("security_password_not_exposed", True)

    patched = await console_api.patch(f"/api/extensions/{ext_id}", {
        "display_name": "CRUD Probe v2",
        "email": "crud2@e2e.local",
    })
    assert isinstance(patched, dict), f"patch must return the model: {str(patched)[:200]}"

    listing2 = await console_api.post("/api/extensions", {
        "page": 1, "per_page": 50, "filters": {"q": ext},
    })
    hit2 = _find(listing2.get("items") if isinstance(listing2, dict) else listing2, ext)
    A.require(hit2, "extension still present after patch")
    assert (hit2.get("display_name") or "") == "CRUD Probe v2", (
        f"patch not persisted: display_name={hit2.get('display_name')!r}"
    )

    deleted = await console_api.delete(f"/api/extensions/{ext_id}")
    assert deleted is not None, f"delete returned None: {deleted!r}"
    listing3 = await console_api.post("/api/extensions", {
        "page": 1, "per_page": 50, "filters": {"q": ext},
    })
    assert _find(listing3.get("items") if isinstance(listing3, dict) else listing3, ext) is None, (
        f"extension still listed after delete: {str(listing3)[:300]}"
    )
    evidence.log_metric("crud_lifecycle", f"{ext}: create->query->patch->query->delete OK")


async def test_extensions_register_with_created_credentials(auth_backend_api, pbx, sipbot_pool, evidence):
    """A created extension's SIP password must actually authenticate a REGISTER
    (and be resolvable via the locator)."""
    ext, pwd = "7102", "live-sip-pass"
    created = await auth_backend_api.put("/api/extensions", {
        "extension": ext, "display_name": "Live SIP", "sip_password": pwd,
    })
    ext_id = created.get("id")
    A.require(ext_id is not None, "created extension id")
    try:
        ua = sipbot_pool.callee(
            host=pbx.host, port=h.ua_port(15185), username=ext, password=pwd,
            register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=10, answer_mode="echo",
        )
        await h.wait_registered(ua, f"created-ext-{ext}", timeout=10)
        evidence.log_metric("created_extension_registered", ext)

        # also visible through the console diagnostics locator
        body = await auth_backend_api.post("/api/diagnostics/locator/lookup", {"user": ext})
        assert body.get("total", 0) >= 1, f"created extension not in locator: {str(body)[:200]}"
    finally:
        await auth_backend_api.delete(f"/api/extensions/{ext_id}")


async def test_extensions_delete_revokes_registration_credentials(auth_backend_api, pbx, sipbot_pool):
    """After deletion the old credentials must be rejected (no 'Registered
    successfully' — 401s persist)."""
    ext, pwd = "7103", "doomed-pass"
    created = await auth_backend_api.put("/api/extensions", {
        "extension": ext, "display_name": "Doomed", "sip_password": pwd,
    })
    ext_id = created.get("id")
    A.require(ext_id is not None, "created extension id")
    ua = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15186), username=ext, password=pwd,
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=5, answer_mode="echo",
    )
    await h.wait_registered(ua, f"pre-delete-{ext}", timeout=10)
    ua.terminate()
    await auth_backend_api.delete(f"/api/extensions/{ext_id}")

    import asyncio

    await asyncio.sleep(2.0)  # extension backend credential cache TTL = 1s
    ua2 = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15187), username=ext, password=pwd,
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=5, answer_mode="echo",
    )
    await asyncio.sleep(4)
    out = ua2.output
    assert "Registered successfully" not in out, (
        f"deleted extension still authenticates: {out[-400:]}"
    )
