"""Scriptable step-mode IVR provider mock (session-scoped, ephemeral port).

Endpoints (rustpbx step protocol):
    POST /ivr/step        -> next ActionNode JSON (scripted or default)
    POST /ivr/step/start  -> session start (200 {})
    POST /ivr/step/end    -> session end   (200 {})
    POST /ivr/step/fail   -> failure hook (200 {} or scripted)
    GET  /audio/<name>.wav -> serve WAV from assets dir (dynamic prompts)

Scripting (mutate `provider.script` between calls, or pass at construction):
    script = [
        {"type": "prompt", "prompt": "<url-or-path>", "interruptible": False},
        {"type": "dtmf_menu", "prompt": ..., "options": [{"key": "1", "action": {...}}]},
        {"type": "hangup"},
    ]
    provider.responses = {"start": 200, "step": 503, "fail": {"type": "hangup"}}
    provider.fail_after = 2   # inject 503 on /ivr/step after N successful steps

Every request body is recorded in `provider.hits` for event-flow assertions.
"""

from __future__ import annotations

import threading
from pathlib import Path
from typing import Callable, Optional

from aiohttp import web


class StepProviderMock:
    def __init__(self, assets_dir=None, *, script: Optional[list] = None, handler: Optional[Callable] = None):
        self.script = list(script or [])
        self.handler = handler  # full override: fn(body) -> dict
        self.responses: dict = {}  # {"step": 503, "start": {...}} — status code or body
        self.fail_after: Optional[int] = None
        self.hits: list[dict] = []
        self.assets_dir = Path(assets_dir) if assets_dir else None
        self._lock = threading.Lock()
        self._step_count = 0
        self._runner: Optional[web.AppRunner] = None
        self.url: str = ""
        self.base_url: str = ""

    # -- request bookkeeping ------------------------------------------------

    def _record(self, kind: str, body: dict):
        with self._lock:
            self.hits.append({"kind": kind, "body": body})

    def _status_or_body(self, kind: str, default_body):
        cfg = self.responses.get(kind)
        if cfg is None:
            return 200, default_body
        if isinstance(cfg, int):
            return cfg, default_body
        return 200, cfg

    # -- handlers -----------------------------------------------------------

    async def _handle_step(self, request: web.Request):
        body = await request.json()
        self._record("step", body)
        with self._lock:
            self._step_count += 1
            over_limit = self.fail_after is not None and self._step_count > self.fail_after
        if over_limit:
            return web.json_response({"error": "injected failure"}, status=503)
        if self.handler is not None:
            return web.json_response(self.handler(body))
        status, resp = self._status_or_body("step", None)
        if status != 200:
            return web.json_response({"error": "injected"}, status=status)
        if resp is not None:
            return web.json_response(resp)
        with self._lock:
            idx = self._step_count - 1  # _step_count already incremented above (step-only counter)
            script = list(self.script)
        if script:
            node = script[idx] if 0 <= idx < len(script) else {"type": "hangup"}
        else:
            node = {"type": "hangup"}
        return web.json_response(node)

    async def _handle_lifecycle(self, kind: str, request: web.Request):
        body = await request.json()
        self._record(kind, body)
        status, resp = self._status_or_body(kind, {})
        if status != 200:
            return web.json_response({"error": "injected"}, status=status)
        return web.json_response(resp or {})

    async def _handle_audio(self, request: web.Request):
        name = request.match_info["name"]
        if self.assets_dir:
            candidate = self.assets_dir / name
            if candidate.suffix != ".wav":
                candidate = candidate.with_suffix(".wav")
            if candidate.exists():
                return web.FileResponse(candidate)
        return web.json_response({"error": f"no such prompt: {name}"}, status=404)

    # -- lifecycle ----------------------------------------------------------

    async def start(self) -> str:
        app = web.Application()
        app.router.add_post("/ivr/step", self._handle_step)
        app.router.add_post("/ivr/step/fail", lambda r: self._handle_lifecycle("fail", r))
        app.router.add_post("/ivr/step/start", lambda r: self._handle_lifecycle("start", r))
        app.router.add_post("/ivr/step/end", lambda r: self._handle_lifecycle("end", r))
        app.router.add_get("/audio/{name}", self._handle_audio)
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]
        self.base_url = f"http://127.0.0.1:{port}"
        self.url = f"{self.base_url}/ivr/step"
        return self.url

    async def stop(self):
        if self._runner:
            await self._runner.cleanup()
            self._runner = None

    def steps_of_kind(self, kind: str) -> list[dict]:
        return [h["body"] for h in self.hits if h["kind"] == kind]
