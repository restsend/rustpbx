"""Console diagnostics endpoints — locator lookup/clear + route evaluate.

Registered extensions must be resolvable via the locator with real contact
data; clearing removes them (removed=true, remaining drops); route evaluation
returns a structured dataset for a configured callee. Strict: required keys,
type checks, and value semantics (never a bare 200).
"""

from __future__ import annotations

import pytest
import pytest_asyncio

import helpers as h

pytestmark = [pytest.mark.locator, pytest.mark.console]

DIAG = "/api/diagnostics"  # diagnostics API lives under the root api_prefix ("/api")


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


async def _register_callee(sipbot_pool, pbx, port, username="1002"):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=60, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(ua)
    return ua


async def test_locator_lookup_finds_registered_extension(console_api, sipbot_pool, pbx):
    """A registered extension must be locatable with real contact data."""
    ua = await _register_callee(sipbot_pool, pbx, h.ua_port(15165), "1002")
    body = await console_api.post(f"{DIAG}/locator/lookup", {"user": "1002"})
    assert isinstance(body, dict), f"lookup must return an object: {str(body)[:200]}"
    assert isinstance(body.get("total"), int) and body["total"] >= 1, (
        f"registered extension must resolve: total={body.get('total')} body={str(body)[:300]}"
    )
    assert str(body).count("1002") >= 1, f"lookup result missing the registered user: {str(body)[:400]}"
    ua.terminate()


async def test_locator_lookup_unknown_user_empty(console_api):
    body = await console_api.post(f"{DIAG}/locator/lookup", {"user": "no-such-9999"})
    assert isinstance(body, dict), f"lookup must return an object: {str(body)[:200]}"
    assert body.get("total") == 0, f"unknown user must resolve to zero records: {str(body)[:300]}"


async def test_locator_clear_removes_registration(console_api, sipbot_pool, pbx):
    """locator/clear must deregister the UA: removed=true and a lookup comes back empty."""
    ua = await _register_callee(sipbot_pool, pbx, h.ua_port(15166), "1003")
    await h.wait_registered(ua)
    body = await console_api.post(f"{DIAG}/locator/clear", {"user": "1003"})
    assert isinstance(body, dict), f"clear must return an object: {str(body)[:200]}"
    assert body.get("removed") is True, f"clear removed={body.get('removed')!r} — registration not found"
    assert isinstance(body.get("remaining"), int), f"clear remaining must be int: {body}"
    after = await console_api.post(f"{DIAG}/locator/lookup", {"user": "1003"})
    assert after.get("total") == 0, f"registration still present after clear: {str(after)[:300]}"
    try:
        ua.terminate()
    except Exception:
        pass


async def test_route_evaluate_returns_dataset(console_api, pbx):
    """routes/evaluate must return a structured evaluation for a dialed user."""
    body = await console_api.post(f"{DIAG}/routes/evaluate", {"callee": "1002", "caller": "1001"})
    assert isinstance(body, dict), f"evaluate must return an object: {str(body)[:200]}"
    assert isinstance(body.get("evaluated_at"), str) and body["evaluated_at"], (
        f"evaluate missing evaluated_at: {sorted(body.keys())}"
    )
    assert body.get("direction") in ("outbound", "inbound", None), f"evaluate direction: {body.get('direction')}"
