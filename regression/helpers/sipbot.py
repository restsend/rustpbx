"""sipbot subprocess manager with RTP stats parsing.

Provides async-friendly wrappers around the `sipbot` CLI for:
  - wait/callee mode (registered or unregistered listener)
  - call/caller mode (outbound call)
  - options mode (SIP OPTIONS health probe)
"""

from __future__ import annotations

import asyncio
import logging
import re
import shutil
import subprocess
import threading
from dataclasses import dataclass, field
from typing import Optional

logger = logging.getLogger(__name__)


@dataclass
class RtpStats:
    rx_packets: int = 0
    rx_bytes: int = 0
    tx_packets: int = 0
    tx_bytes: int = 0

    @property
    def has_rx(self) -> bool:
        return self.rx_packets > 0

    @property
    def has_tx(self) -> bool:
        return self.tx_packets > 0

    @property
    def is_bidirectional(self) -> bool:
        return self.has_rx and self.has_tx

    def __str__(self) -> str:
        return f"RTP RX:{self.rx_packets}p/{self.rx_bytes}b TX:{self.tx_packets}p/{self.tx_bytes}b"


def find_sipbot_binary() -> str:
    path = shutil.which("sipbot")
    if path:
        return path
    raise FileNotFoundError(
        "sipbot binary not found in PATH. Install with: cargo install sipbot"
    )


class SipBotProcess:
    """sipbot subprocess manager with thread-safe output capture and RTP parsing."""

    def __init__(self, name: str = "sipbot"):
        self.name = name
        self.process: Optional[subprocess.Popen] = None
        self._output: list[str] = []
        self._lock = threading.Lock()
        self._reader: Optional[threading.Thread] = None
        self.binary = find_sipbot_binary()

    # ---- callee (wait) mode ----

    def start_callee(
        self,
        *,
        host: str = "127.0.0.1",
        port: int = 5070,
        username: str = "callee",
        password: str = "123456",
        proxy: Optional[str] = None,
        register: bool = False,
        domain: Optional[str] = None,
        codecs: str = "pcmu",
        ring_secs: int = 2,
        answer_mode: str = "echo",
        reject_code: Optional[int] = None,
        reject_prob: Optional[int] = None,
        hangup_after: Optional[int] = None,
        record_file: Optional[str] = None,
        refer_reject: Optional[int] = None,
        dtmf_flows: Optional[str] = None,
        reinvite_flows: Optional[str] = None,
        info_flows: Optional[str] = None,
        headers: Optional[list[str]] = None,
        audio_quality: bool = False,
        webrtc: bool = False,
        ws_url: Optional[str] = None,
        srtp: bool = False,
        nack: bool = False,
        jitter: bool = False,
        ringback: Optional[str] = None,
    ) -> None:
        cmd = [
            self.binary, "wait",
            "-a", f"{host}:{port}",
            "--username", username,
            "--password", password,
            "--codecs", codecs,
            "--ring-duration", str(ring_secs),
            "-v",
        ]
        if register:
            reg = proxy or f"{host}:5060"
            cmd.extend(["--register", reg])
        if domain:
            cmd.extend(["--domain", domain])
        if reject_code is not None:
            cmd.extend(["--reject", str(reject_code)])
            # sipbot 0.2.x ignores --reject unless a rejection probability is
            # also given; default to always-reject so reject_code behaves as
            # documented.
            if reject_prob is None:
                cmd.extend(["--reject-prob", "100"])
        if reject_prob is not None:
            cmd.extend(["--reject-prob", str(reject_prob)])
        elif answer_mode == "none":
            # Ring without answering: enter the ring stage (built-in ringing
            # tone) and cap it with --ring-duration (ring_secs, e.g. 90) so
            # the bot sends 180 Ringing but never 200 in the test window.
            if ringback is None:
                cmd.extend(["--ringback"])
        elif answer_mode == "echo":
            cmd.append("--echo")
        elif answer_mode and answer_mode != "none":
            cmd.extend(["--answer", answer_mode])
        if hangup_after is not None:
            cmd.extend(["--hangup", str(hangup_after)])
        if record_file:
            cmd.extend(["--record", record_file])
        if refer_reject is not None:
            cmd.extend(["--refer-reject", str(refer_reject)])
        if dtmf_flows:
            cmd.extend(["--dtmf-flows", dtmf_flows])
        if reinvite_flows:
            cmd.extend(["--reinvite-flows", reinvite_flows])
        if info_flows:
            cmd.extend(["--info-flows", info_flows])
        if headers:
            for hdr in headers:
                cmd.extend(["-H", hdr])
        if audio_quality:
            cmd.append("--audio-quality")
        if webrtc:
            cmd.append("--webrtc")
        if ws_url:
            cmd.extend(["--ws-url", ws_url])
        if srtp:
            cmd.append("--srtp")
        if nack:
            cmd.append("--nack")
        if jitter:
            cmd.append("--jitter")
        if ringback:
            cmd.extend(["--ringback", ringback])
        self._launch(cmd)

    # ---- caller (call) mode ----

    def start_caller(
        self,
        *,
        target: str,
        username: str = "caller",
        password: Optional[str] = "123456",
        proxy: Optional[str] = None,
        register: bool = False,
        domain: Optional[str] = None,
        from_uri: Optional[str] = None,
        codecs: str = "pcmu",
        hangup: int = 10,
        wait: Optional[int] = None,
        play_file: Optional[str] = None,
        record_file: Optional[str] = None,
        dtmf_flows: Optional[str] = None,
        reinvite_flows: Optional[str] = None,
        info_flows: Optional[str] = None,
        headers: Optional[list[str]] = None,
        audio_quality: bool = False,
        webrtc: bool = False,
        ws_url: Optional[str] = None,
        srtp: bool = False,
        nack: bool = False,
        jitter: bool = False,
        cancel_prob: int = 0,
        total: int = 1,
        cps: int = 1,
        addr: Optional[str] = None,
    ) -> None:
        cmd = [
            self.binary, "call",
            "-t", target,
            "--username", username,
            "--codecs", codecs,
            "--hangup", str(hangup),
            "-v",
        ]
        if password is not None:
            cmd.extend(["--password", password])
        if addr:
            # Unique local bind port — required when many caller processes run
            # concurrently (the default bind would collide).
            cmd.extend(["-a", addr])
        if register:
            reg = proxy or "127.0.0.1:5060"
            cmd.extend(["--register", reg])
        if domain:
            cmd.extend(["--domain", domain])
        if from_uri:
            cmd.extend(["--from", from_uri])
        if wait is not None:
            cmd.extend(["--wait", str(wait)])
        if play_file:
            cmd.extend(["--play", play_file])
        if record_file:
            cmd.extend(["--record", record_file])
        if dtmf_flows:
            cmd.extend(["--dtmf-flows", dtmf_flows])
        if reinvite_flows:
            cmd.extend(["--reinvite-flows", reinvite_flows])
        if info_flows:
            cmd.extend(["--info-flows", info_flows])
        if headers:
            for hdr in headers:
                cmd.extend(["-H", hdr])
        if audio_quality:
            cmd.append("--audio-quality")
        if webrtc:
            cmd.append("--webrtc")
        if ws_url:
            cmd.extend(["--ws-url", ws_url])
        if srtp:
            cmd.append("--srtp")
        if nack:
            cmd.append("--nack")
        if jitter:
            cmd.append("--jitter")
        if cancel_prob > 0:
            cmd.extend(["--cancel-prob", str(cancel_prob)])
        if total != 1:
            cmd.extend(["--total", str(total)])
        if cps != 1:
            cmd.extend(["--cps", str(cps)])
        self._launch(cmd)

    # ---- options probe ----

    def start_options(self, target: str) -> None:
        cmd = [self.binary, "options", target]
        self._launch(cmd)

    # ---- process management ----

    def _launch(self, cmd: list[str]) -> None:
        logger.info("[%s] %s", self.name, " ".join(cmd))
        self.process = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            stdin=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def send_stdin_dtmf(self, digits: str) -> bool:
        """Send DTMF digits via the UA's interactive stdin channel.

        sipbot call mode reads lines from stdin ("Single call mode: type
        digits (0-9,*,#,A-D) to send DTMF") — unlike --dtmf-flows (capped at
        2 scheduled entries in 0.2.59), stdin digits are unlimited and can be
        timed from the test (e.g. a mid-call survey score).
        """
        if self.process is None or self.process.poll() is not None:
            return False
        try:
            if self.process.stdin is None:
                return False
            self.process.stdin.write(digits + "\n")
            self.process.stdin.flush()
            logger.info("[%s] stdin DTMF: %s", self.name, digits)
            return True
        except Exception as exc:  # noqa: BLE001
            logger.warning("[%s] stdin DTMF failed: %s", self.name, exc)
            return False

    def _read_loop(self) -> None:
        assert self.process and self.process.stdout
        try:
            for line in self.process.stdout:
                line = line.rstrip()
                if not line:
                    continue
                with self._lock:
                    self._output.append(line)
                logger.debug("[%s] %s", self.name, line)
        except Exception as exc:
            logger.warning("[%s] reader error: %s", self.name, exc)

    @property
    def output(self) -> str:
        with self._lock:
            return "\n".join(self._output)

    @property
    def is_alive(self) -> bool:
        return self.process is not None and self.process.poll() is None

    def get_rtp_stats(self) -> RtpStats:
        out = self.output
        stats = RtpStats()
        # Use the LAST progress/summary line: sipbot emits periodic progress
        # lines (starting with an early 0p/0b one before any RTP flows), so
        # `re.search`'s first match would always report zero. The trailing line
        # carries the current cumulative RX/TX totals.
        rx = re.findall(r"RX:\s*(\d+)p/(\d+)b", out)
        tx = re.findall(r"TX:\s*(\d+)p/(\d+)b", out)
        if rx:
            stats.rx_packets = int(rx[-1][0])
            stats.rx_bytes = int(rx[-1][1])
        if tx:
            stats.tx_packets = int(tx[-1][0])
            stats.tx_bytes = int(tx[-1][1])
        return stats

    def get_status_counts(self) -> dict:
        """Parse the per-code SIP response counts from sipbot's final summary.

        sipbot prints `Status: [183:1, 486:1, ...]` in its closing summary, so
        tests can assert exactly which provisional/final codes the UA received.
        """
        m = re.search(r"Status:\s*\[([^\]]*)\]", self.output)
        counts: dict = {}
        if not m:
            return counts
        for part in m.group(1).split(","):
            part = part.strip()
            if not part:
                continue
            code, _, cnt = part.partition(":")
            try:
                counts[int(code)] = int(cnt)
            except ValueError:
                continue
        return counts

    def has_status(self, code: int) -> bool:
        """True if the UA received the given SIP response code at least once."""
        return self.get_status_counts().get(code, 0) > 0

    async def wait_rtp_rx(self, label: str = "", timeout: float = 20) -> RtpStats:
        """Wait until the UA has received RTP packets (covers 183 early media
        when the enhanced sipbot is used), then return the stats."""
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            stats = self.get_rtp_stats()
            if stats.rx_packets > 0:
                return stats
            await asyncio.sleep(0.3)
        raise AssertionError(
            f"{label}: no RTP RX after {timeout}s — {self.get_rtp_stats()}\n{self.output[-2000:]}"
        )

    def get_audio_quality(self) -> Optional[dict]:
        """Parse the AudioQuality progress line. Supports both the legacy
        (`silence=../.., has_audio=..`) and current
        (`frames=.., silence_frames=.., avg_rms=..`) sipbot formats."""
        out = self.output
        # Current format: frames=200, ..., silence_frames=0, ..., avg_rms=123.5
        m = re.search(
            r"AudioQuality: frames=(\d+), .*silence_frames=(\d+), avg_rms=([\d.]+)",
            out,
        )
        if m:
            frames = int(m.group(1))
            silence = int(m.group(2))
            avg_rms = float(m.group(3))
            return {
                "silence_frames": silence,
                "total_frames": frames,
                "avg_rms": avg_rms,
                "has_audio": frames > 0 and silence < frames,
                "silence_ratio": (silence / frames) if frames else 1.0,
            }
        # Legacy format: silence=../.., clipping=../.., ..., has_audio=..
        m = re.search(
            r"AudioQuality: silence=(\d+)/(\d+), clipping=(\d+)/(\d+), "
            r"shrill=(\d+), muffled=(\d+), has_audio=(\w+)",
            out,
        )
        if not m:
            return None
        total = int(m.group(2))
        silence = int(m.group(1))
        return {
            "silence_frames": silence,
            "total_frames": total,
            "clipping_frames": int(m.group(3)),
            "shrill_count": int(m.group(5)),
            "muffled_count": int(m.group(6)),
            "has_audio": m.group(7) == "true",
            "silence_ratio": (silence / total) if total else 1.0,
        }

    def get_dtmf_digits(self) -> list[str]:
        """Received RFC4733 DTMF digits (sipbot `RX_DTMF_DIGIT:` lines)."""
        return re.findall(r"RX_DTMF_DIGIT:\s*([0-9*#A-D])", self.output)

    def get_dtmf_counts(self) -> dict:
        m = re.search(r"RX_DTMF: (\d+), TX_DTMF: (\d+)", self.output)
        if not m:
            return {"rx": 0, "tx": 0}
        return {"rx": int(m.group(1)), "tx": int(m.group(2))}

    def wait_output(self, pattern: str, timeout: float = 30) -> bool:
        """Block until *pattern* appears in output or timeout."""
        import time
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if re.search(pattern, self.output):
                return True
            if not self.is_alive:
                return re.search(pattern, self.output) is not None
            time.sleep(0.2)
        return re.search(pattern, self.output) is not None

    async def wait_output_async(self, pattern: str, timeout: float = 30) -> bool:
        compiled = re.compile(pattern)
        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            if compiled.search(self.output):
                return True
            if not self.is_alive:
                return compiled.search(self.output) is not None
            await asyncio.sleep(0.2)
        return compiled.search(self.output) is not None

    def wait(self, timeout: int = 60) -> int:
        if self.process:
            try:
                return self.process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.terminate()
                return -1
        return 0

    def terminate(self) -> None:
        if self.process and self.process.poll() is None:
            logger.info("[%s] terminating", self.name)
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                logger.warning("[%s] force kill", self.name)
                self.process.kill()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.terminate()


class SipBotPool:
    """Manages multiple SipBotProcess instances for a single test.

    Use as an async context manager — on exit, all spawned UAs are terminated.
    """

    def __init__(self):
        self._procs: list[SipBotProcess] = []

    def callee(self, **kwargs) -> SipBotProcess:
        p = SipBotProcess(name=kwargs.get("username", f"callee-{len(self._procs)}"))
        p.start_callee(**kwargs)
        self._procs.append(p)
        return p

    def caller(self, **kwargs) -> SipBotProcess:
        p = SipBotProcess(name=kwargs.get("username", f"caller-{len(self._procs)}"))
        p.start_caller(**kwargs)
        self._procs.append(p)
        return p

    def options(self, target: str) -> SipBotProcess:
        p = SipBotProcess(name="options")
        p.start_options(target)
        self._procs.append(p)
        return p

    def terminate_all(self) -> None:
        for p in reversed(self._procs):
            p.terminate()
        self._procs.clear()

    def terminate_user(self, username: str) -> int:
        """Terminate every still-running bot registered as `username`.

        Bots live for the whole session (terminate_all runs at session end),
        so a later test re-registering the same user would race stale bots
        for inbound calls — the PBX may INVITE (and hang up) the OLD bot.
        Returns the number of bots terminated.
        """
        victims = [
            p
            for p in self._procs
            if p.name == username and p.process and p.process.poll() is None
        ]
        for p in victims:
            p.terminate()
        self._procs = [p for p in self._procs if p not in victims]
        if victims:
            logger.info(
                "terminated %d stale bot(s) for %s", len(victims), username
            )
        return len(victims)

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        self.terminate_all()
