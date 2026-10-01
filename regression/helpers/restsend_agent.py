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
import socket
import subprocess
import threading
from typing import Any, Optional


def _udp_free_port(preferred: int) -> int:
    """Return `preferred` if it can be bound on 127.0.0.1, else a free
    ephemeral port."""
    for port in (preferred, 0):
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock.bind(("127.0.0.1", port))
            return sock.getsockname()[1]
        except (OSError, OverflowError):
            continue
        finally:
            sock.close()
    return preferred

CLI = os.environ.get(
    "RESTSEND_CLI",
    "/Users/pi/workspace/rs/restsend-call/target/debug/restsend-cli",
)


class RestsendAgent:
    def __init__(self, pbx, user: str, password: str = "123456",
                 local_port: int = 25100,
                 video: bool = False,
                 device: str = "null",
                 log_level: str = "info",
                 enable_video: Optional[bool] = None,
                 tone_hz: Optional[int] = None):
        """video: pass --video so the mock camera TX pump runs (H264 when the
        video policy allows). device: null|tone|cpal|auto — `tone` drives a
        synthetic source while Connected (440 Hz by default; pass tone_hz to
        override via RESTSEND_TONE_HZ — e2e suites pick frequencies that do
        not collide with the PBX MOH spectrum). enable_video:
        explicit `init.enable_video` — the wire default is TRUE, so a plain
        init already enables the client's video policy; pass False only to
        force-disable."""
        self.pbx = pbx
        self.user = user
        self.password = password
        self.local_port = local_port
        self.video = video
        self.device = device
        self.log_level = log_level
        self.enable_video = enable_video
        self.tone_hz = tone_hz
        self.proc: Optional[asyncio.subprocess.Process] = None
        self.events: list[dict] = []
        self.last_consult_uri: str = ""
        self._reader: Optional[asyncio.Task] = None

    # ── lifecycle ────────────────────────────────────────────────────────
    async def start(self) -> None:
        if not os.path.exists(CLI):
            raise FileNotFoundError(f"restsend-cli not built: {CLI}")
        # Suites hardcode 25xxx local bind ports; in parallel `all` runs those
        # fall inside OTHER lanes' UA port windows → instant bind failure and
        # a cryptic "REGISTER failed". Probe the preferred port and fall back
        # to a free ephemeral one when busy.
        self.local_port = _udp_free_port(self.local_port)
        env = dict(os.environ, RUST_LOG=self.log_level)
        if self.tone_hz:
            env["RESTSEND_TONE_HZ"] = str(self.tone_hz)
        # stderr carries the engine's tracing log — keep it for diagnostics.
        self._stderr_path = f"/tmp/restsend-{self.user}-{self.local_port}.log"
        self._stderr_fh = open(self._stderr_path, "w")
        args = [CLI, "--media", "rtc", "--device", self.device]
        if self.video:
            args.append("--video")
        self.proc = await asyncio.create_subprocess_exec(
            *args,
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

    # ── call / transfer controls (docs/cli_protocol.md v1) ───────────────

    async def call(self, uri: str, timeout: float = 15.0,
                   media: Optional[str] = None) -> bool:
        """Outbound call (`sip_call`); waits for `connected`."""
        cmd: dict[str, Any] = {"cmd": "sip_call", "remote_uri": uri}
        if media:
            cmd["media"] = media
        await self.cmd(cmd)
        return await self.wait_state("connected", timeout=timeout) is not None

    async def wait_state(self, name: str, timeout: float = 15.0) -> Optional[dict]:
        ev = await self.wait_event(
            "state_changed", predicate=lambda e: e.get("name") == name,
            timeout=timeout)
        return ev

    async def current_state(self) -> str:
        """Latest `state_changed.name` observed (engine auto-falls back to
        `listening` after each call, so this is authoritative between polls)."""
        for ev in reversed(self.events):
            if ev.get("evt") == "state_changed":
                return ev.get("name", "")
        return ""

    async def consult(self, uri: str, timeout: float = 15.0) -> Optional[dict]:
        """Consult (B–C) leg: `sip_call consult=true` — the MAIN call state is
        untouched; progress rides `sip_call_leg(consult=true)` only. Waits for
        events appended AFTER the command (no stale match)."""
        self.last_consult_uri = uri
        mark = len(self.events)
        await self.cmd({"cmd": "sip_call", "remote_uri": uri, "consult": True})
        return await self.wait_event_after(
            "sip_call_leg", mark,
            predicate=lambda e: e.get("consult") is True
            and e.get("name") == "connected",
            timeout=timeout)

    async def refer_blind(self, refer_to: str, timeout: float = 10.0,
                          wait: bool = True) -> Optional[dict]:
        """Blind REFER (no Replaces). Returns the `sip_refer` completion
        event — which fires at the FINAL NOTIFY, so pass `wait=False` for
        deliberately in-flight REFERs (verify progress on the target).
        Waits only for events appended AFTER the command (no stale match)."""
        mark = len(self.events)
        await self.cmd({"cmd": "sip_refer", "refer_to": refer_to})
        if not wait:
            return {"evt": "sip_refer", "status": None, "attended": False}
        return await self.wait_event_after(
            "sip_refer", mark,
            predicate=lambda e: not e.get("attended"),
            timeout=timeout)

    async def refer_attended(self, refer_to: Optional[str] = None,
                             timeout: float = 10.0) -> Optional[dict]:
        """Attended REFER (Refer-To + Replaces=consult). The wire command
        REQUIRES `refer_to` (attended mode derives the actual target from the
        consult leg's tags; the field is carried but unused). History-safe."""
        mark = len(self.events)
        await self.cmd({"cmd": "sip_refer",
                        "refer_to": refer_to or getattr(self, "last_consult_uri", "") or "sip:attended@localhost",
                        "attended": True})
        return await self.wait_event_after(
            "sip_refer", mark,
            predicate=lambda e: e.get("attended") is True,
            timeout=timeout)

    async def hangup_consult(self) -> None:
        """BYE only the consult leg; the main call stays (`sip_hangup_consult`).
        The engine AUTO-RESUMES the held customer leg as part of the cancel
        (`resume_customer_after_consult`) — do NOT send an explicit `resume`
        afterwards (a duplicate re-INVITE races the follow-up BYE)."""
        await self.cmd({"cmd": "sip_hangup_consult"})

    async def resume_call(self) -> None:
        """Explicit re-INVITE sendrecv on the main call (`resume`). Only use
        when the engine has NOT auto-resumed (e.g. after a FAILED transfer,
        where the consult leg died remotely)."""
        await self.cmd({"cmd": "resume"})

    async def wait_event_after(self, etype: str, since: int,
                               predicate=None, timeout: float = 15.0) -> Optional[dict]:
        """Wait for an event of `etype` among events appended AFTER index
        `since` (no historical matches)."""
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while True:
            for ev in self.events[since:]:
                if ev.get("evt") == etype and (
                        predicate is None or predicate(ev)):
                    return ev
            if loop.time() >= deadline:
                return None
            await asyncio.sleep(0.2)

    async def wait_recovered(self, timeout: float = 15.0) -> bool:
        """After `hangup_consult()`/cancel: the engine auto-resumes the main
        call — wait for a NEW `connected` assertion (post-call snapshot, so a
        historical event cannot satisfy it)."""
        mark = len(self.events)
        ev = await self.wait_event_after(
            "state_changed", mark,
            predicate=lambda e: e.get("name") == "connected",
            timeout=timeout)
        return ev is not None

    async def ensure_recovered(self, timeout: float = 20.0) -> bool:
        """Robust retrieve tail: wait for the auto-resume; if the main call
        is still held after a grace period, compensate with an explicit
        `resume` (the engine tolerates it via reinvite_pending). Returns the
        final connected-ness."""
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        resumed = False
        while loop.time() < deadline:
            if (await self.current_state()) == "connected":
                return True
            if not resumed and loop.time() >= deadline - timeout + 4.0:
                await self.resume_call()
                resumed = True
            await asyncio.sleep(0.8)
        return (await self.current_state()) == "connected"

    async def hold_call(self) -> None:
        """re-INVITE sendonly on the main call (`hold`)."""
        await self.cmd({"cmd": "hold"})

    async def switch_consult(self) -> None:
        """切换: toggle the talking party between the two consult legs
        (`sip_consult_switch`, client-side dual re-INVITE)."""
        await self.cmd({"cmd": "sip_consult_switch"})

    async def conference(self, factory_uri: str) -> None:
        """Network conference: INVITE factory → REFER both legs in (C1 order)."""
        await self.cmd({"cmd": "sip_conference", "factory_uri": factory_uri})

    async def wait_consult_party(self, customer_active: bool,
                                 timeout: float = 12.0) -> Optional[dict]:
        """`sip_consult_party` edge — both re-INVITEs confirmed, active party
        flipped. `customer_active=True` = the customer leg is talking."""
        return await self.wait_event(
            "sip_consult_party",
            predicate=lambda e: bool(e.get("customer_active")) == customer_active,
            timeout=timeout)

    async def reject_incoming(self, code: int = 486) -> None:
        """Reject the ringing incoming call (`sip_reject`)."""
        await self.cmd({"cmd": "sip_reject", "code": code})

    async def send_dtmf(self, digit, method: str = "rfc4733") -> None:
        """`digit` is the wire integer 0–15 (0-9 digits, 10=`*`, 11=`#`,
        12-15=A-D); accepts a char like "5"/"*" and maps it."""
        mapping = {"*": 10, "#": 11, "A": 12, "B": 13, "C": 14, "D": 15}
        if isinstance(digit, str) and digit in mapping:
            value: Any = mapping[digit]
        else:
            value = int(digit)
        await self.cmd({"cmd": "send_dtmf", "digit": value, "method": method})

    # ── media liveness (frame/packet counting level) ─────────────────────

    def stats_index(self) -> int:
        """Event-list snapshot for a later `stats_delta(since)` baseline."""
        return len(self.events)

    def stats_delta(self, since: int) -> dict:
        """packets_received/lost deltas across `stats` events after `since`."""
        samples = [e for e in self.events[since:] if e.get("evt") == "stats"]
        if len(samples) < 2:
            return {"samples": len(samples), "received": 0, "lost": 0}
        first, last = samples[0], samples[-1]
        return {
            "samples": len(samples),
            "received": last.get("packets_received", 0) - first.get("packets_received", 0),
            "lost": last.get("packets_lost", 0) - first.get("packets_lost", 0),
            "last": last,
        }

    async def wait_media_flow(self, since: Optional[int] = None,
                              min_received: int = 10,
                              timeout: float = 10.0,
                              label: str = "") -> dict:
        """Wait until `stats.packets_received` advances by `min_received` after
        the baseline index (media keeps flowing), with no loss spike. The CLI
        does not push `stats` on its own — this polls `poll_stats`."""
        base = len(self.events) if since is None else since
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while True:
            await self.cmd({"cmd": "poll_stats"})
            await asyncio.sleep(0.4)
            delta = self.stats_delta(base)
            if delta["received"] >= min_received:
                return delta
            if loop.time() >= deadline:
                raise AssertionError(
                    f"{label or self.user}: media not flowing "
                    f"(received +{delta['received']} < {min_received}); "
                    f"last stats: {delta.get('last')}\n"
                    f"stderr tail: {self.stderr_text()[-400:]}")

    # ── high-level ops ───────────────────────────────────────────────────
    async def register(self, expires: int = 120) -> bool:
        init_cmd: dict[str, Any] = {"cmd": "init"}
        if self.enable_video is not None:
            init_cmd["enable_video"] = self.enable_video
        await self.cmd(init_cmd)
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

    async def answer(self, timeout: float = 20.0,
                     since: Optional[int] = None) -> bool:
        """Wait for a `sip_incoming` arriving AFTER `since` (an events-list
        index captured BEFORE triggering the dial — the INVITE can land on
        the wire within milliseconds), answer, and wait for `connected`.
        When `since` is omitted the mark is taken at entry."""
        mark = len(self.events) if since is None else since
        if not await self.wait_event_after("sip_incoming", mark, timeout=timeout):
            return False
        await self.cmd({"cmd": "sip_answer"})
        ev = await self.wait_event_after(
            "state_changed", mark,
            predicate=lambda e: e.get("name") == "connected",
            timeout=timeout)
        return ev is not None

    async def hangup(self) -> None:
        """BYE the main call. The engine settles in-flight re-INVITEs before
        writing the BYE (reinvite_pending wait), so no extra pacing needed."""
        await self.cmd({"cmd": "hangup"})

    def stderr_text(self) -> str:
        """The engine's tracing log (tracing writes to stderr)."""
        try:
            with open(self._stderr_path, encoding="utf-8", errors="replace") as fh:
                return fh.read()
        except OSError:
            return ""

    @property
    def rang(self) -> bool:
        """Did the phone ever ring (sip_incoming observed)?"""
        return self.has_event("sip_incoming") is not None
