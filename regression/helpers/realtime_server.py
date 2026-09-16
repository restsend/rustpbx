"""Scripted OpenAI-Realtime mock server for realtime app E2E tests.

Accepts the WS upgrade on `/v1/realtime`, captures the `Authorization`
header and `model` query (to assert the credential rides the upgrade
headers, never the URL), records client `input_audio_buffer.append` uplink
frames and `response.cancel` barge-in reactions, and replays a scripted
event sequence (audio delta → transcripts → function_call → barge-in →
close) so the PBX side can be asserted end-to-end without a real API key.

Mirrors the capture style of `ws_bridge_echo.WsBridgeCapture`.
"""

from __future__ import annotations

import asyncio
import base64
import json
import logging
import math
import struct
import threading

from aiohttp import WSMsgType, web

logger = logging.getLogger(__name__)


def _sine_b64(ms: int = 100, rate: int = 8000, freq: int = 440, amp: int = 3000) -> str:
    """One mono PCM16 sine chunk, base64 — what `response.audio.delta` carries."""
    n = int(rate * ms / 1000)
    samples = [int(amp * math.sin(2 * math.pi * freq * i / rate)) for i in range(n)]
    return base64.b64encode(struct.pack(f"<{n}h", *samples)).decode()


class RealtimeCapture:
    """Thread-safe accumulator for mock server observations."""

    def __init__(self):
        self._lock = threading.Lock()
        self.connections = 0
        self.auth_header: str | None = None
        self.query: str = ""
        self.append_frames: list[str] = []
        self.cancel_seen = False
        self.errors: list[str] = []

    def note_connection(self, auth, query):
        with self._lock:
            self.connections += 1
            self.auth_header = auth
            self.query = query

    def note_append(self, audio_b64: str):
        with self._lock:
            self.append_frames.append(audio_b64)

    def note_cancel(self):
        with self._lock:
            self.cancel_seen = True

    def note_error(self, msg: str):
        with self._lock:
            self.errors.append(msg)

    # Snapshot helpers (pytest asserts run on another thread/task).
    def snapshot(self) -> dict:
        with self._lock:
            return {
                "connections": self.connections,
                "auth_header": self.auth_header,
                "query": self.query,
                "append_count": len(self.append_frames),
                "cancel_seen": self.cancel_seen,
                "errors": list(self.errors),
            }


async def _wait(predicate, timeout: float, what: str, poll: float = 0.2):
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if predicate():
            return True
        await asyncio.sleep(poll)
    raise AssertionError(f"timeout waiting for {what}")


class RealtimeMockServer:
    """Async context manager: `async with RealtimeMockServer() as srv:`."""

    def __init__(self, path: str = "/v1/realtime", events: list[dict] | None = None):
        self.path = path
        # Default scripted downlink: ready → audio → transcripts → tool → barge-in.
        self.events = events or [
            {"type": "session.created"},
            {"type": "response.audio.delta", "delta": _sine_b64(100)},
            {"type": "response.audio.delta", "delta": _sine_b64(100)},
            {"type": "response.audio_transcript.delta", "delta": "Hello"},
            {
                "type": "conversation.item.input_audio_transcription.completed",
                "transcript": "hi there",
            },
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "name": "lookup_order",
                    "arguments": '{"id": 42}',
                    "call_id": "call_e2e_1",
                },
            },
            {"type": "input_audio_buffer.speech_started"},
            {"type": "response.audio.delta", "delta": _sine_b64(100)},
            {"type": "input_audio_buffer.speech_stopped"},
        ]
        self.capture = RealtimeCapture()
        self._runner = None
        self._site = None
        self.port: int = 0

    @property
    def ws_url(self) -> str:
        return f"ws://127.0.0.1:{self.port}{self.path}"

    async def __aenter__(self):
        app = web.Application()
        app.router.add_get(self.path, self._handler)
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        self._site = web.TCPSite(self._runner, "127.0.0.1", 0)
        await self._site.start()
        self.port = self._site._server.sockets[0].getsockname()[1]
        return self

    async def __aexit__(self, *exc):
        if self._site:
            await self._site.stop()
        if self._runner:
            await self._runner.cleanup()
        return False

    async def _handler(self, request):
        auth = request.headers.get("Authorization")
        self.capture.note_connection(auth, request.query_string)
        ws = web.WebSocketResponse()
        await ws.prepare(request)

        async def reader():
            async for msg in ws:
                if msg.type == WSMsgType.TEXT:
                    try:
                        v = json.loads(msg.data)
                    except json.JSONDecodeError:
                        self.capture.note_error(f"bad json: {msg.data[:80]}")
                        continue
                    t = v.get("type")
                    if t == "input_audio_buffer.append":
                        self.capture.note_append(v.get("audio", ""))
                    elif t == "response.cancel":
                        self.capture.note_cancel()
                elif msg.type == WSMsgType.ERROR:
                    self.capture.note_error(str(msg.data))

        reader_task = asyncio.create_task(reader())
        try:
            for event in self.events:
                await ws.send_str(json.dumps(event))
                await asyncio.sleep(0.25)
            await asyncio.sleep(0.8)  # let the client drain before close
            await ws.close()
        finally:
            reader_task.cancel()
        return ws

    # ── assertion helpers ─────────────────────────────────────────────────

    async def wait_connection(self, timeout: float = 15):
        await _wait(
            lambda: self.capture.snapshot()["connections"] >= 1,
            timeout,
            "realtime WS connection",
        )

    async def wait_appends(self, min_frames: int = 3, timeout: float = 15):
        await _wait(
            lambda: self.capture.snapshot()["append_count"] >= min_frames,
            timeout,
            f">= {min_frames} uplink append frames "
            f"(got {self.capture.snapshot()['append_count']})",
        )

    async def wait_cancel(self, timeout: float = 10):
        await _wait(
            lambda: self.capture.snapshot()["cancel_seen"],
            timeout,
            "response.cancel (barge-in reaction)",
        )
