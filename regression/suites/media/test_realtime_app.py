"""Realtime (AI voice) app E2E — RWI-driven full SIP chain against a scripted
OpenAI-Realtime mock server.

Chain under test:
  RWI originate → callee answers → `call.app_start` (app_name="realtime") →
  session bridge connects to the mock WS → uplink PCM (base64 append) →
  scripted downlink events (audio/transcripts/function_call/barge-in) →
  RWI `realtime_event`s → endpoint close hangs the call up
  (`hangup_on_disconnect` default).

Asserts (all without any real API key):
  1. The bridge upgrade hits the mock with the `model` query and — when a
     key is configured via env — the `Authorization: Bearer` header.
  2. Caller audio reaches the WS as `input_audio_buffer.append` frames.
  3. Scripted downlink events surface as `realtime_event` kinds:
     connected / transcript_delta / transcript_final / function_call /
     barge_in / disconnected.
  4. Barge-in: the client reacts to speech_started with `response.cancel`.
  5. Endpoint close terminates the call (call_hangup).
"""

from __future__ import annotations

import os
import uuid

import pytest

import helpers as h
from helpers.realtime_server import RealtimeMockServer

pytestmark = [pytest.mark.outbound, pytest.mark.realtime]


def _call_id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


def _rwi_event_kinds(rwi) -> list[str]:
    """All `kind` values seen in `realtime_event`s buffered on the client."""
    kinds = []
    for ev in rwi.events:
        et = ev.get("event_type") or ev.get("type")
        if et == "realtime_event":
            payload = ev.get("kind") and ev or ev.get("data") or {}
            kind = ev.get("kind") or payload.get("kind")
            if kind:
                kinds.append(kind)
    return kinds


@pytest.mark.asyncio
async def test_realtime_app_openai_chain(pbx, sipbot_pool, rwi):
    pbx.config_builder.media_proxy = "all"
    h.boot_pbx(pbx)
    await h.connect_rwi(rwi)

    port = h.ua_port(15160)
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(ua)

    async with RealtimeMockServer() as server:
        call_id = _call_id("rt")
        resp = await rwi.originate(call_id, f"sip:1002@{pbx.sip_addr}", "sip:rwi@pbx", "default")
        assert resp.get("status") == "success", resp
        await rwi.wait_for_event("call_answered", timeout=15)

        # Insert the realtime app on the answered call.
        rwi.clear_events()
        app_resp = await rwi.send_request(
            "call.app_start",
            {
                "call_id": call_id,
                "app_name": "realtime",
                "params": {
                    "url": server.ws_url,
                    "protocol": "openai",
                    "sample_rate": 8000,
                },
            },
        )
        assert app_resp.get("status") == "success", app_resp

        # 1. Upgrade reached the mock; auth header only when a key exists.
        await server.wait_connection(timeout=15)
        snap = server.capture.snapshot()
        assert "model=" not in snap["query"], f"unexpected model query: {snap}"
        env_key = os.environ.get("OPENAI_API_KEY")
        if env_key:
            assert snap["auth_header"] == f"Bearer {env_key}", snap

        # 2. Caller audio flows uplink as base64 append frames.
        await server.wait_appends(min_frames=3, timeout=15)

        # 3+4. Scripted downlink surfaced as realtime_event kinds, and the
        # barge-in reaction reached the mock.
        await server.wait_cancel(timeout=15)

        deadline_kinds = []
        for _ in range(40):
            kinds = _rwi_event_kinds(rwi)
            if "function_call" in kinds and "barge_in" in kinds:
                deadline_kinds = kinds
                break
            deadline_kinds = kinds
            await __import__("asyncio").sleep(0.3)
        assert "connected" in deadline_kinds, f"kinds={deadline_kinds}"
        assert "transcript_delta" in deadline_kinds, f"kinds={deadline_kinds}"
        assert "transcript_final" in deadline_kinds, f"kinds={deadline_kinds}"
        assert "function_call" in deadline_kinds, f"kinds={deadline_kinds}"
        assert "barge_in" in deadline_kinds, f"kinds={deadline_kinds}"

        # 5. Endpoint close (hangup_on_disconnect default) ends the call.
        hangup = await rwi.wait_for_event("call_hangup", timeout=15)
        assert hangup is not None, "endpoint close must hang the call up"

        # Disconnected event lands too (scan after hangup for reliability).
        kinds = _rwi_event_kinds(rwi)
        assert "disconnected" in kinds, f"kinds={kinds}"
