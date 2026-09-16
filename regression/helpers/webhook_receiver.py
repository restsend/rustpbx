"""HTTP webhook receiver to capture RWI webhook POSTs.

The receiver stores every incoming POST in an in-memory list, timestamped,
so tests can assert on the full event stream.  It also exposes a simple
health endpoint for startup probes.
"""

from __future__ import annotations

import asyncio
import json
import logging
import threading
import time
from typing import Optional

from aiohttp import web

logger = logging.getLogger(__name__)


def _dig(obj, path: str):
    cur = obj
    for part in path.split("."):
        if isinstance(cur, dict):
            cur = cur.get(part)
        elif isinstance(cur, list):
            try:
                cur = cur[int(part)]
            except (ValueError, IndexError):
                return None
        elif cur is not None and not isinstance(cur, (str, int, float, bool)):
            cur = getattr(cur, part, None)
        else:
            return None
        if cur is None:
            return None
    return cur


class WebhookEvent:
    __slots__ = ("timestamp", "sequence", "call_id", "event_type", "payload", "raw")

    def __init__(self, raw: dict, received_at: float):
        self.raw = raw
        self.timestamp = received_at
        self.sequence = raw.get("sequence", 0)
        self.call_id = raw.get("call_id")
        self.event_type = raw.get("event_type")
        self.payload = raw.get("event", raw)

    @property
    def is_broadcast(self) -> bool:
        return not self.call_id

    def __repr__(self) -> str:
        return f"WebhookEvent(type={self.event_type}, call_id={self.call_id}, seq={self.sequence})"


class WebhookReceiver:
    """Capture RWI webhook events with thread-safe access."""

    def __init__(self):
        self.events: list[WebhookEvent] = []
        self._lock = threading.Lock()
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._event_ready = asyncio.Event()

    def clear(self) -> None:
        with self._lock:
            self.events.clear()

    def all_events(self) -> list[WebhookEvent]:
        with self._lock:
            return list(self.events)

    def events_for_call(self, call_id: str) -> list[WebhookEvent]:
        with self._lock:
            return [e for e in self.events if e.call_id == call_id]

    def event_types(self) -> list[str]:
        with self._lock:
            return [e.event_type or "" for e in self.events]

    def event_types_for_call(self, call_id: str) -> list[str]:
        with self._lock:
            return [e.event_type or "" for e in self.events if e.call_id == call_id]

    def find(self, event_type: str) -> Optional[WebhookEvent]:
        with self._lock:
            for e in self.events:
                if e.event_type == event_type:
                    return e
        return None

    def count(self, event_type: Optional[str] = None) -> int:
        with self._lock:
            if event_type is None:
                return len(self.events)
            return sum(1 for e in self.events if e.event_type == event_type)

    def has_sequence(self, expected: list[str], call_id: Optional[str] = None) -> bool:
        types = self.event_types_for_call(call_id) if call_id else self.event_types()
        idx = 0
        for t in types:
            if idx < len(expected) and t == expected[idx]:
                idx += 1
        return idx == len(expected)

    async def wait_for_event(
        self,
        event_type: str,
        timeout: float = 15.0,
        call_id: Optional[str] = None,
        occurrence: int = 1,
        match: Optional[dict] = None,
    ) -> Optional[WebhookEvent]:
        """Wait until the `occurrence`-th matching event has arrived.

        `match` is an optional dict of dotted-path → expected value applied
        against each candidate event (e.g. {"payload.agent_id": "1003"}).
        """

        def _matches(e: WebhookEvent) -> bool:
            if e.event_type != event_type:
                return False
            if call_id is not None and e.call_id != call_id:
                return False
            if match:
                for path, expected in match.items():
                    if _dig(e, path) != expected:
                        return False
            return True

        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            with self._lock:
                hits = [e for e in self.events if _matches(e)]
            if len(hits) >= occurrence:
                return hits[occurrence - 1]
            await asyncio.sleep(0.15)
        return None

    async def wait_for_sequence(
        self,
        expected: list[str],
        timeout: float = 30.0,
        call_id: Optional[str] = None,
    ) -> bool:
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            if self.has_sequence(expected, call_id=call_id):
                return True
            await asyncio.sleep(0.2)
        return self.has_sequence(expected, call_id=call_id)

    async def wait_for_min_events(self, count: int, timeout: float = 15.0) -> bool:
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            if self.count() >= count:
                return True
            await asyncio.sleep(0.2)
        return self.count() >= count


# ---------------------------------------------------------------------------
# aiohttp server
# ---------------------------------------------------------------------------


async def _handle_webhook(request: web.Request) -> web.Response:
    receiver: WebhookReceiver = request.app["receiver"]
    try:
        body = await request.json()
    except Exception:
        raw = await request.text()
        logger.warning("non-JSON webhook body: %s", raw[:500])
        return web.Response(status=400)
    ev = WebhookEvent(body, time.time())
    with receiver._lock:
        receiver.events.append(ev)
    logger.debug("webhook: %s", ev)
    return web.json_response({"ok": True})


async def _handle_health(request: web.Request) -> web.Response:
    receiver: WebhookReceiver = request.app["receiver"]
    return web.json_response({"status": "ok", "events": receiver.count()})


def _create_app(receiver: WebhookReceiver) -> web.Application:
    app = web.Application()
    app["receiver"] = receiver
    app.router.add_post("/webhook", _handle_webhook)
    app.router.add_get("/health", _handle_health)
    app.router.add_post("/", _handle_webhook)
    return app


class WebhookServer:
    """Lifecycle manager for the aiohttp webhook receiver server."""

    def __init__(self, host: str = "127.0.0.1", port: int = 0):
        self.host = host
        self.port = port
        self.receiver = WebhookReceiver()
        self._runner: Optional[web.AppRunner] = None
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._thread: Optional[threading.Thread] = None
        self._actual_port: Optional[int] = None
        self._started = threading.Event()

    @property
    def url(self) -> str:
        p = self._actual_port or self.port
        return f"http://{self.host}:{p}/webhook"

    def start(self) -> None:
        """Start server in a background thread with its own event loop."""
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()
        self._started.wait(timeout=10)

    def _run(self) -> None:
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        self._loop = loop
        app = _create_app(self.receiver)
        self._runner = web.AppRunner(app)
        loop.run_until_complete(self._runner.setup())
        site = web.TCPSite(self._runner, self.host, self.port)
        loop.run_until_complete(site.start())
        self._actual_port = site._server.sockets[0].getsockname()[1]
        logger.info("WebhookServer listening on %s:%s", self.host, self._actual_port)
        self._started.set()
        loop.run_forever()

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
