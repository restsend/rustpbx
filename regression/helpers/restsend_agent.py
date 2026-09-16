"""restsend-call (restsend-cli) agent driver — the REAL production client.

Drives `restsend-cli` in REPL mode (stdin/stdout JSON-lines, see
docs/cli_protocol.md) so scenario tests can use an actual cc-phone class UA
(SIP REGISTER + presence PUBLISH + WebRTC media) instead of sipbot:

    agent = RestsendAgent(pbx, "1001")
    await agent.start()
    await agent.register(expires=120)   # sip_bind → sip_register → reg ok
    await agent.publish_idle()          # PUBLISH ready=true (note=idle)
    ev = await agent.wait_event("sip_incoming", timeout=15)  # did it RING?
    await agent.answer()                # sip_answer → connected

The `sip_incoming` event is the authoritative "the agent's phone rang"
signal for dispatch verification.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
from typing import Any, Optional

CLI = os.environ.get(
    "RESTSEND_CLI",
    "/Users/pi/workspace/rs/restsend-call/target/debug/restsend-cli",
)


class RestsendAgent:
    def __init__(self, pbx, user: str, password: str = "123456",
                 local_port: int = 25100):
        self.pbx = pbx
        self.user = user
        self.password = password
        self.local_port = local_port
        self.proc: Optional[asyncio.subprocess.Process] = None
        self.events: list[dict] = []
        self._reader: Optional[asyncio.Task] = None

    # ── lifecycle ────────────────────────────────────────────────────────
    async def start(self) -> None:
        if not os.path.exists(CLI):
            raise FileNotFoundError(f"restsend-cli not built: {CLI}")
        env = dict(os.environ, RUST_LOG="info")
        # stderr carries the engine's tracing log — keep it for diagnostics.
        self._stderr_path = f"/tmp/restsend-{self.user}-{self.local_port}.log"
        self._stderr_fh = open(self._stderr_path, "w")
        self.proc = await asyncio.create_subprocess_exec(
            CLI, "--media", "rtc", "--device", "null",
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=self._stderr_fh,
            env=env,
        )
        self._reader = asyncio.get_running_loop().create_task(self._read())

    async def _read(self) -> None:
        assert self.proc and self.proc.stdout
        while True:
            line = await self.proc.stdout.readline()
            if not line:
                break
            try:
                self.events.append(json.loads(line))
            except (ValueError, UnicodeDecodeError):
                pass

    def kill(self) -> None:
        """Hard-kill WITHOUT unregister — simulates crash / network loss.

        The registration lease keeps the agent a schedulable idle candidate
        until it lapses, exactly like a real cc-phone being killed.
        """
        if self.proc and self.proc.returncode is None:
            self.proc.kill()
        self._cleanup_reader()

    def _cleanup_reader(self) -> None:
        if self._reader:
            self._reader.cancel()
            self._reader = None

    async def stop(self) -> None:
        if self.proc and self.proc.returncode is None:
            try:
                await self.cmd({"cmd": "quit"})
                await asyncio.wait_for(self.proc.wait(), timeout=5)
            except Exception:  # noqa: BLE001
                self.proc.kill()
        self._cleanup_reader()

    # ── protocol ─────────────────────────────────────────────────────────
    async def cmd(self, obj: dict) -> None:
        assert self.proc and self.proc.stdin
        self.proc.stdin.write((json.dumps(obj) + "\n").encode())
        await self.proc.stdin.drain()

    def has_event(self, etype: str, predicate=None) -> Optional[dict]:
        for ev in self.events:
            if ev.get("evt") == etype and (predicate is None or predicate(ev)):
                return ev
        return None

    async def wait_event(self, etype: str, predicate=None,
                         timeout: float = 15.0) -> Optional[dict]:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while True:
            ev = self.has_event(etype, predicate)
            if ev:
                return ev
            if loop.time() >= deadline:
                return None
            await asyncio.sleep(0.2)

    async def wait_gone(self, etype: str, predicate=None,
                        quiet_secs: float = 5.0) -> bool:
        """True when no matching event arrives within the window."""
        ev = await self.wait_event(etype, predicate, timeout=quiet_secs)
        return ev is None

    # ── high-level ops ───────────────────────────────────────────────────
    async def register(self, expires: int = 120) -> bool:
        await self.cmd({"cmd": "init"})
        await self.cmd({"cmd": "sip_bind",
                        "local_uri": f"sip:{self.user}@127.0.0.1:{self.local_port}"})
        await self.wait_event("sip_bound", timeout=8)
        await self.cmd({"cmd": "sip_register",
                        "registrar": f"{self.pbx.host}:{self.pbx.sip_port}",
                        "user": self.user, "password": self.password,
                        "expires": expires})
        ev = await self.wait_event("sip_reg_state",
                                   predicate=lambda e: e.get("reason") == "ok",
                                   timeout=10)
        return ev is not None

    async def publish_idle(self) -> None:
        await self.cmd({"cmd": "sip_publish", "ready": True})

    async def answer(self, timeout: float = 10.0) -> bool:
        await self.cmd({"cmd": "sip_answer"})
        ev = await self.wait_event(
            "state_changed",
            predicate=lambda e: e.get("name") == "connected",
            timeout=timeout)
        return ev is not None

    async def hangup(self) -> None:
        await self.cmd({"cmd": "hangup"})

    @property
    def rang(self) -> bool:
        """Did the phone ever ring (sip_incoming observed)?"""
        return self.has_event("sip_incoming") is not None
