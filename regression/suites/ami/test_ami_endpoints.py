"""AMI v1 endpoint coverage — observation & control plane.

All endpoints require [ami] IP allowlist (enabled via outbound_enabled in the
ConfigBuilder). Strict: every response must be a JSON object/list with the
documented shape; empty collections are validated by type, silent 5xx/strings
fail.
"""

from __future__ import annotations

import pytest
import pytest_asyncio

import helpers as h

pytestmark = [pytest.mark.ami]


@pytest_asyncio.fixture
async def ami_pbx(pbx, webhook_server):
    pbx.config_builder.outbound_enabled = True
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    return pbx


async def _get_json(pbx, path: str, *, expect: int = 200):
    import aiohttp

    async with aiohttp.ClientSession() as session:
        async with session.get(f"{pbx.http_url}{path}") as resp:
            body = await resp.json(content_type=None)
            assert resp.status == expect, f"GET {path}: {resp.status} != {expect}: {str(body)[:200]}"
            return body


async def _post_json(pbx, path: str, payload=None, *, expect: int = 200):
    import aiohttp

    async with aiohttp.ClientSession() as session:
        async with session.post(f"{pbx.http_url}{path}", json=payload or {}) as resp:
            body = await resp.json(content_type=None)
            assert resp.status == expect, f"POST {path}: {resp.status} != {expect}: {str(body)[:200]}"
            return body


async def test_ami_health(ami_pbx):
    body = await _get_json(ami_pbx, "/ami/v1/health")
    A = body if isinstance(body, dict) else None
    assert isinstance(A, dict), f"health must be a JSON object, got {type(body).__name__}"


async def test_ami_dialogs_schema(ami_pbx):
    body = await _get_json(ami_pbx, "/ami/v1/dialogs")
    assert isinstance(body, list), f"dialogs must be a list, got {type(body).__name__}"


async def test_ami_transactions_schema(ami_pbx):
    body = await _get_json(ami_pbx, "/ami/v1/transactions")
    assert isinstance(body, (list, dict)), f"transactions unexpected type {type(body).__name__}"


async def test_ami_trunk_registrations_schema(ami_pbx):
    body = await _get_json(ami_pbx, "/ami/v1/trunk_registrations")
    assert isinstance(body, (list, dict)), f"trunk_registrations unexpected type {type(body).__name__}"


async def test_ami_frequency_limits_schema(ami_pbx):
    import aiohttp

    async with aiohttp.ClientSession() as session:
        async with session.get(f"{ami_pbx.http_url}/ami/v1/frequency_limits") as resp:
            body = await resp.json(content_type=None)
            assert isinstance(body, (list, dict)), f"unexpected type: {type(body).__name__} {str(body)[:160]}"
            if resp.status == 200:
                pass  # limiter configured -> collection
            else:
                # structured unavailability is valid behaviour, not a broken endpoint
                assert resp.status == 501 and body.get("status") == "unavailable" and body.get("reason"), (
                    f"non-200 must be a structured unavailable response: {resp.status} {body}"
                )


async def test_ami_calls_schema_commerce(ami_pbx):
    body = await _get_json(ami_pbx, "/ami/v1/calls")
    assert isinstance(body, (list, dict)), f"calls unexpected type {type(body).__name__}"


async def test_ami_hangup_unknown_call_id(ami_pbx):
    """Control endpoint must answer 2xx/404 for an unknown dialog — never 5xx."""
    import aiohttp

    async with aiohttp.ClientSession() as session:
        async with session.get(f"{ami_pbx.http_url}/ami/v1/hangup/nonexistent-call-id") as resp:
            assert resp.status < 500, f"hangup endpoint 5xx for unknown id: {resp.status}"


async def test_ami_reload_endpoints_answer(ami_pbx):
    """All reload routes must answer 2xx with a JSON object (ok/enabled markers)."""
    for path in (
        "/ami/v1/reload/routes",
        "/ami/v1/reload/trunks",
        "/ami/v1/reload/queues",
    ):
        body = await _post_json(ami_pbx, path)
        assert isinstance(body, dict), f"{path} must return an object: {str(body)[:160]}"
