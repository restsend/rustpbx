"""WebSocket bridge echo/capture server for IVR bridge E2E tests.

The rustpbx `bridge:` transfer connects to a WebSocket endpoint and streams
raw PCM16 (i16 LE) binary frames plus DTMF JSON text frames. This helper:

- accepts any number of WS connections
- buffers received binary PCM16 frames (caller audio) into a numpy int16 array
- buffers received text DTMF JSON frames
- (optionally) echoes binary frames back so a WS→caller direction can be tested

Mirrors the Rust in-process `WsEchoServer` from
`src/proxy/tests/test_bridge_e2e.rs`.
"""

from __future__ import annotations

import asyncio
import logging
import threading
from typing import Optional

import numpy as np
from aiohttp import web

logger = logging.getLogger(__name__)


class WsBridgeCapture:
    """Thread-safe accumulator for bridge WS connections/audio/DTMF."""

    def __init__(self):
        self._lock = threading.Lock()
        self._connections = 0
        self._pcm_chunks: list[bytes] = []
        self._dtmf: list[str] = []

    def connection_opened(self) -> None:
        with self._lock:
            self._connections += 1

    def add_pcm(self, data: bytes) -> None:
        if len(data) < 2:
            return
        with self._lock:
            self._pcm_chunks.append(bytes(data))

    def add_dtmf(self, text: str) -> None:
        with self._lock:
            self._dtmf.append(text)

    def connection_count(self) -> int:
        with self._lock:
            return self._connections

    def pcm_bytes(self) -> bytes:
        with self._lock:
            return b"".join(self._pcm_chunks)

    def pcm_samples(self) -> np.ndarray:
        """Concatenated PCM16 (little-endian) decoded as an int16 numpy array."""
        data = self.pcm_bytes()
        if not data:
            return np.zeros(0, dtype=np.int16)
        count = len(data) // 2
        return np.frombuffer(data[: count * 2], dtype="<i2").astype(np.int16)

    def dtmf_frames(self) -> list[str]:
        with self._lock:
            return list(self._dtmf)

    def clear(self) -> None:
        with self._lock:
            self._connections = 0
            self._pcm_chunks.clear()
            self._dtmf.clear()


async def _ws_handler(request: web.Request) -> web.WebSocketResponse:
    ws = web.WebSocketResponse()
    await ws.prepare(request)
    capture: WsBridgeCapture = request.app["capture"]
    echo: bool = request.app.get("echo", True)
    capture.connection_opened()
    logger.info("bridge ws connected (total=%d)", capture.connection_count())
    try:
        async for msg in ws:
            if msg.type == web.WSMsgType.BINARY:
                capture.add_pcm(msg.data)
                if echo:
                    await ws.send_bytes(msg.data)
            elif msg.type == web.WSMsgType.TEXT:
                capture.add_dtmf(msg.data)
                if echo:
                    await ws.send_str(msg.data)
            elif msg.type == web.WSMsgType.ERROR:
                break
    finally:
        pass
    return ws


def _create_app(capture: WsBridgeCapture, echo: bool) -> web.Application:
    app = web.Application()
    app["capture"] = capture
    app["echo"] = echo
    app.router.add_get("/ws", _ws_handler)
    app.router.add_get("/health", lambda r: web.json_response({"ok": True}))
    return app


class WsBridgeEchoServer:
    """Lifecycle manager for the aiohttp WS bridge echo/capture server."""

    def __init__(self, host: str = "127.0.0.1", port: int = 0, echo: bool = True):
        self.host = host
        self.port = port
        self.echo = echo
        self.capture = WsBridgeCapture()
        self._runner: Optional[web.AppRunner] = None
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._thread: Optional[threading.Thread] = None
        self._actual_port: Optional[int] = None
        self._started = threading.Event()
        self._stop = threading.Event()

    @property
    def ws_url(self) -> str:
        p = self._actual_port or self.port
        return f"ws://{self.host}:{p}/ws"

    @property
    def url(self) -> str:
        return self.ws_url

    def start(self) -> None:
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        self._started.wait(timeout=10)

    def _run(self) -> None:
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        self._loop = loop
        app = _create_app(self.capture, self.echo)
        self._runner = web.AppRunner(app)
        loop.run_until_complete(self._runner.setup())
        site = web.TCPSite(self._runner, self.host, self.port)
        loop.run_until_complete(site.start())
        self._actual_port = site._server.sockets[0].getsockname()[1]
        logger.info("WsBridgeEchoServer listening on %s:%s", self.host, self._actual_port)
        self._started.set()
        try:
            loop.run_forever()
        finally:
            pending = asyncio.all_tasks(loop)
            for t in pending:
                t.cancel()
            loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
            loop.close()

    def stop(self) -> None:
        if self._runner and self._loop:
            if self._loop.is_running():
                try:
                    asyncio.run_coroutine_threadsafe(
                        self._runner.cleanup(), self._loop
                    ).result(timeout=5)
                except Exception:
                    pass
                self._loop.call_soon_threadsafe(self._loop.stop)
        if self._thread:
            self._thread.join(timeout=5)
