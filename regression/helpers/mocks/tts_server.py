"""TTS HTTP mock: synthesizes text to an 8kHz sine WAV (deterministic tone per
request hash), with injectable failures for acceptance exception scenarios.

Protocol (matches rustpbx [tts] http driver expectations used in e2e):
    POST /tts  {"text": "...", "voice": "..."}  -> audio/wav bytes
    GET  /tts/health                            -> 200
Failure injection: `mock.fail_requests = N` (next N requests -> 503),
`mock.slow_ms = 3000` (delay before answering).
"""

from __future__ import annotations

import hashlib
import threading
import time
from pathlib import Path

from aiohttp import web

from ..audio_verifier import generate_sine_wav

# Map arbitrary text to a stable test tone (avoid mains hum / DC).
_FREQ_BANDS = (300.0, 440.0, 530.0, 620.0, 700.0, 850.0, 1000.0)


class TtsServerMock:
    def __init__(self, cache_dir=None, *, duration_s: float = 1.2, sample_rate: int = 8000):
        self.cache_dir = Path(cache_dir) if cache_dir else Path("/tmp/rustpbx-tts-mock")
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.duration_s = duration_s
        self.sample_rate = sample_rate
        self.requests: list[dict] = []
        self.fail_requests = 0
        self.slow_ms = 0
        self._lock = threading.Lock()
        self._runner: Optional[web.AppRunner] = None
        self.base_url = ""

    def _tone_for(self, text: str) -> float:
        digest = hashlib.sha1(text.encode("utf-8")).digest()
        return _FREQ_BANDS[digest[0] % len(_FREQ_BANDS)]

    async def _handle_tts(self, request: web.Request):
        try:
            body = await request.json()
        except Exception:
            body = {"text": await request.text()}
        with self._lock:
            self.requests.append(dict(body))
            should_fail = self.fail_requests > 0
            if should_fail:
                self.fail_requests -= 1
        if should_fail:
            return web.json_response({"error": "tts injected failure"}, status=503)
        if self.slow_ms:
            import asyncio

            await asyncio.sleep(self.slow_ms / 1000.0)
        text = str(body.get("text") or body.get("text_content") or "")
        if not text.strip():
            return web.json_response({"error": "empty text"}, status=422)
        tone = self._tone_for(text)
        out = self.cache_dir / f"tts_{hashlib.sha1((text + str(tone)).encode()).hexdigest()[:12]}.wav"
        if not out.exists():
            generate_sine_wav(out, freq_hz=tone, duration_s=self.duration_s, sample_rate=self.sample_rate, amplitude=0.45)
        return web.FileResponse(out, headers={"Content-Type": "audio/wav"})

    async def start(self) -> str:
        app = web.Application()
        app.router.add_post("/tts", self._handle_tts)
        app.router.add_get("/tts/health", lambda r: web.json_response({"ok": True}))
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]
        self.base_url = f"http://127.0.0.1:{port}"
        return self.base_url

    async def stop(self):
        if self._runner:
            await self._runner.cleanup()
            self._runner = None
