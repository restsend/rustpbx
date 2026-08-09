#! /usr/bin/env python3
"""Unified IVR example — step provider + PCM16 bridge echo + SIP INFO builders.

Merges the former `step_ivr_provider.py`, `bridge_ivr_provider.py` and
`ivr_exec_demo.py` into one stdlib-only example:

  1. A step-mode IVR HTTP provider (POST /ivr/step, /ivr/step/start,
     /ivr/step/end). Prompts are pre-synthesized **PCMA (a-law) WAV files**
     served over the same HTTP server (GET /audio/<name>.wav), except the
     "current time" branch which uses `tts_text` so rustpbx's edge-cli TTS
     provider path is exercised at call time.
  2. A WebSocket PCM16 echo server for the `voip_bridge` (bridge) action.
  3. SIP INFO body builders (`ivr.exec` / `app.start` / `app.stop` / `hold`)
     — construct the payloads an external CTI would send over SIP INFO.

Prompts live in `examples/prompts/` (PCMA wav, fmt tag 6, 8 kHz mono). They are
committed; regenerate with `--generate-prompts` (needs `edge-cli` + `ffmpeg`).

Menu:
  1 → current time (dynamic `tts_text` — tests the TTS provider)
  2 → transfer to extension 2001
  3 → join queue "sales"
  4 → bridge call audio to the local WebSocket PCM16 echo server
  0 → play goodbye and hang up (SIP 200)
  anything else → invalid prompt, retry up to 3 times then hang up

Run:
    python3 examples/unified_ivr_provider.py [ivr_port] [--ws-port PORT]
                                            [--host HOST] [--self-test]
                                            [--print-sip-info]
                                            [--generate-prompts]

Wire it into rustpbx (route file, e.g. config/routes/unified-ivr.toml):

    [[routes]]
    name = "to-unified-step-ivr"
    priority = 10
    action = "application"
    app = "ivr"
    auto_answer = true
    app_params = { mode = "step", url = "http://127.0.0.1:8080/ivr/step" }

    [routes.match]
    "to.user" = "*88"

Then dial *88. If rustpbx and the example are on different hosts, pass
`--host <reachable-ip>` and use that IP in `url`.

Self-test (no SIP stack needed):
    python3 examples/unified_ivr_provider.py --self-test

Unit tests:
    python3 -m unittest examples/unified_ivr_provider.py
"""

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import threading
import time
import unittest
import uuid
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from socketserver import ThreadingMixIn
from threading import Lock
from urllib.parse import urlsplit

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

# ── Prompts ──────────────────────────────────────────────────────────────────

PROMPTS_DIR = Path(__file__).resolve().parent / "prompts"

# name → spoken copy (English, synthesized with an English edge-cli voice)
PROMPT_COPY = {
    "welcome": (
        "IVR step. Press 1 to get the current time, press 2 to transfer to a "
        "human agent, press 3 to join the queue, press 4 to bridge to an "
        "external service, press 0 to hang up."
    ),
    "menu": "Please press 1, 2, 3, 4 or 0.",
    "invalid": "Sorry, that option is invalid. Please try again.",
    "timeout": "No input received. Please try again.",
    "goodbye": "Goodbye.",
    "bridge-demo": (
        "Bridge demo. Playing example audio through the WebSocket bridge. "
        "The bridge will now disconnect and return to the IVR menu."
    ),
}


def generate_prompts(prompts_dir: Path, voice: str = "en-US-JennyNeural") -> None:
    """Synthesize the PCMA prompt WAVs with edge-cli + ffmpeg."""
    import subprocess

    prompts_dir.mkdir(parents=True, exist_ok=True)
    for name, text in PROMPT_COPY.items():
        mp3 = prompts_dir / f"{name}.mp3"
        wav = prompts_dir / f"{name}.wav"
        subprocess.run(
            ["edge-cli", "speak", "-t", text, "-v", voice, "-o", str(mp3)],
            check=True,
            capture_output=True,
        )
        subprocess.run(
            ["ffmpeg", "-y", "-v", "error", "-i", str(mp3), "-ar", "8000", "-ac", "1",
             "-c:a", "pcm_alaw", str(wav)],
            check=True,
            capture_output=True,
        )
        mp3.unlink(missing_ok=True)
        print(f"generated {wav.name}")


# ── G.711 a-law helpers (no audioop on Python 3.13) ──────────────────────────

def alaw2linear(b: int) -> int:
    """Decode one 8-bit a-law byte to a 16-bit linear PCM sample (ITU G.711).
    Matches the standard FFmpeg / libavcodec implementation."""
    b ^= 0x55
    exponent = (b >> 4) & 0x07
    mantissa = (b & 0x0F) << 4
    if exponent == 0:
        sample = mantissa + 8
    elif exponent == 1:
        sample = mantissa + 0x108
    else:
        sample = (mantissa + 0x108) << (exponent - 1)
    return sample if (b & 0x80) else -sample


def wav_info(path: Path) -> tuple:
    """Return (format_tag, channels, sample_rate, bits, data_bytes)."""
    data = path.read_bytes()
    if len(data) < 44 or data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError(f"{path.name}: not a WAV file")
    tag = int.from_bytes(data[20:22], "little")
    channels = int.from_bytes(data[22:24], "little")
    rate = int.from_bytes(data[24:28], "little")
    bits = int.from_bytes(data[34:36], "little")
    off, data_len = 12, 0
    while off + 8 <= len(data):
        cid = data[off : off + 4]
        size = int.from_bytes(data[off + 4 : off + 8], "little")
        if cid == b"data":
            data_len = size
            break
        off += 8 + size + (size & 1)
    return tag, channels, rate, bits, data_len


def rms_of_alaw_wav(path: Path) -> float:
    """Decode the data chunk of a PCMA wav and return its RMS amplitude."""
    tag, _, rate, bits, _ = wav_info(path)
    if tag != 6 or bits != 8:
        return 0.0
    data = path.read_bytes()
    off, pcm = 12, bytearray()
    while off + 8 <= len(data):
        cid = data[off : off + 4]
        size = int.from_bytes(data[off + 4 : off + 8], "little")
        if cid == b"data":
            pcm = data[off + 8 : off + 8 + size]
            break
        off += 8 + size + (size & 1)
    if not pcm:
        return 0.0
    n = len(pcm)
    total = sum(alaw2linear(b) ** 2 for b in pcm)
    return (total / n) ** 0.5


# ── IVR session state machine ────────────────────────────────────────────────

def _time_text() -> str:
    return "The current time is " + time.strftime("%Y-%m-%d %H:%M:%S")


def action_prompt(file=None, tts_text=None, interruptible=False):
    node = {"type": "prompt", "interruptible": interruptible}
    if file:
        node["file"] = file
    if tts_text:
        node["tts_text"] = tts_text
    return node


def action_bridge(ws_endpoint: str, return_app: str | None = None, return_target: str | None = None):
    node = {
        "type": "voip_bridge",
        "create_room_uri": ws_endpoint,
        "timeout_ms": 10000,
        "step_id": "bridge",
        "step_name": "VoIP bridge",
    }
    if return_app:
        node["return_app"] = return_app
    if return_target:
        node["return_target"] = return_target
    return node


def action_hangup():
    return {"type": "hangup"}


class IvrSession:
    """Per-call state machine. rustpbx calls POST /ivr/step on each event."""

    def __init__(self, caller, callee, base_url, ws_endpoint, ws_play_endpoint=None):
        self.caller = caller
        self.callee = callee
        self.base_url = base_url  # http://host:port — audio URLs are built from it
        self.ws_endpoint = ws_endpoint
        self.ws_play_endpoint = ws_play_endpoint or (ws_endpoint.rstrip("/") + "/play")
        self._state = "start"
        self._retries = 0

    def _audio(self, name: str) -> str:
        return f"{self.base_url}/audio/{name}.wav"

    def _menu_prompt(self):
        return action_prompt(file=self._audio("menu"), interruptible=True)

    def _hangup_with_prompt(self):
        return {
            "type": "play_and_hangup",
            "prompt": self._audio("goodbye"),
            "code": 200,
        }

    def next_action(self, event: dict) -> dict:
        ev_type = event.get("type", "")

        # TTS failure (edge-cli unavailable): never return another tts_text
        # (would loop); fall back to a static-file prompt / terminal action.
        if ev_type == "error":
            self._state = "menu"
            return self._menu_prompt()

        if self._state == "start":
            self._state = "menu"
            return action_prompt(file=self._audio("welcome"), interruptible=True)

        if self._state == "menu":
            if ev_type == "dtmf":
                return self._on_dtmf(event.get("digit", ""))
            if ev_type == "audio_complete":
                # welcome/menu finished with no input → re-offer the menu
                return self._menu_prompt()
            if ev_type == "dtmf_timeout":
                return self._hangup_with_prompt()

        if self._state == "time":
            if ev_type == "audio_complete":
                self._state = "menu"
                return self._menu_prompt()

        return action_hangup()

    def _on_dtmf(self, digit: str) -> dict:
        if digit == "1":
            self._state = "time"
            return action_prompt(tts_text=_time_text())
        if digit == "2":
            return {"type": "transfer", "target": "2001"}
        if digit == "3":
            return {"type": "queue", "target": "sales"}
        if digit == "4":
            return action_bridge(
                self.ws_play_endpoint, return_app="ivr", return_target="main"
            )
        if digit == "0":
            return self._hangup_with_prompt()

        self._retries += 1
        if self._retries >= 3:
            return self._hangup_with_prompt()
        return action_prompt(file=self._audio("invalid"), interruptible=True)


# ── stdlib WebSocket PCM16 echo server ───────────────────────────────────────

class _EchoStats:
    def __init__(self):
        self.lock = Lock()
        self.connections = 0
        self.frames_received = 0
        self.bytes_received = 0
        self.last_samples = None
        self.outbound_dtmf = None

    def snapshot(self):
        with self.lock:
            return {
                "connections": self.connections,
                "frames_received": self.frames_received,
                "bytes_received": self.bytes_received,
                "last_samples": self.last_samples,
                "outbound_dtmf": self.outbound_dtmf,
            }


ECHO_STATS = _EchoStats()


def _ws_handshake(sock) -> str | None:
    """Complete a WS upgrade handshake; return the request path (e.g. '/echo')."""
    f = sock.makefile("rb", 0)
    headers = {}
    path = "/"
    try:
        start_line = f.readline().decode("latin-1").strip()
        if not start_line.startswith("GET "):
            return None
        parts = start_line.split(" ")
        if len(parts) > 1:
            path = parts[1].split("?")[0] or "/"
        while True:
            line = f.readline().decode("latin-1").strip()
            if not line:
                break
            if ":" in line:
                k, v = line.split(":", 1)
                headers[k.strip().lower()] = v.strip()
    finally:
        f.close()
    key = headers.get("sec-websocket-key")
    if not key:
        return None
    accept = base64.b64encode(
        hashlib.sha1((key + WS_GUID).encode("ascii")).digest()
    ).decode("ascii")
    sock.sendall(
        (
            "HTTP/1.1 101 Switching Protocols\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
        ).encode("ascii")
    )
    return path


def _recv_exactly(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def _read_frame(sock):
    hdr = _recv_exactly(sock, 2)
    if hdr is None:
        return None
    b0, b1 = hdr[0], hdr[1]
    opcode = b0 & 0x0F
    masked = (b1 & 0x80) != 0
    length = b1 & 0x7F
    if length == 126:
        ext = _recv_exactly(sock, 2)
        if ext is None:
            return None
        length = struct.unpack(">H", ext)[0]
    elif length == 127:
        ext = _recv_exactly(sock, 8)
        if ext is None:
            return None
        length = struct.unpack(">Q", ext)[0]
    mask = _recv_exactly(sock, 4) if masked else b""
    if masked and mask is None:
        return None
    payload = _recv_exactly(sock, length) if length else b""
    if length and payload is None:
        return None
    if masked:
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    return opcode, payload


def _write_frame(sock, opcode, payload, mask=False):
    out = bytearray()
    out.append(0x80 | (opcode & 0x0F))
    n = len(payload)
    mask_bit = 0x80 if mask else 0x00
    mask_key = os.urandom(4) if mask else b""
    if n < 126:
        out.append(mask_bit | n)
    elif n < 65536:
        out.append(mask_bit | 126)
        out.extend(struct.pack(">H", n))
    else:
        out.append(mask_bit | 127)
        out.extend(struct.pack(">Q", n))
    if mask:
        out.extend(mask_key)
        out.extend(bytes(b ^ mask_key[i % 4] for i, b in enumerate(payload)))
    else:
        out.extend(payload)
    sock.sendall(bytes(out))


def _echo_conn(sock, addr):
    try:
        with ECHO_STATS.lock:
            ECHO_STATS.connections += 1
        print(f"[WS Echo] connection from {addr}")
        sent_outbound = False
        while True:
            frame = _read_frame(sock)
            if frame is None:
                break
            opcode, payload = frame
            if opcode == 0x8:  # close
                _write_frame(sock, 0x8, b"")
                break
            if opcode == 0x9:  # ping
                _write_frame(sock, 0xA, payload)
                continue
            if opcode == 0xA:  # pong
                continue
            if opcode in (0x1, 0x2):  # text / binary
                with ECHO_STATS.lock:
                    ECHO_STATS.frames_received += 1
                    ECHO_STATS.bytes_received += len(payload)
                    if opcode == 0x2 and len(payload) % 2 == 0:
                        ECHO_STATS.last_samples = list(
                            struct.unpack("<%dh" % (len(payload) // 2), payload)
                        )
                if opcode == 0x1:  # text → JSON message (DTMF or other CTI control)
                    try:
                        data = json.loads(payload.decode("utf-8"))
                    except (json.JSONDecodeError, UnicodeDecodeError):
                        data = None
                    if isinstance(data, dict):
                        print(
                            "[WS Echo] JSON message:\n"
                            + json.dumps(data, indent=2, ensure_ascii=False)
                        )
                    else:
                        print(f"[WS Echo] text: {payload.decode('utf-8', 'replace')}")
                    if isinstance(data, dict) and data.get("type") == "dtmf":
                        digit = data.get("digit", "?")
                        leg = data.get("leg_id", "?")
                        print(f"[WS Echo] DTMF from {leg}: '{digit}'")
                        if not sent_outbound:
                            # Demonstrate outbound DTMF injection back to rustpbx.
                            outbound = json.dumps(
                                {"type": "dtmf", "digit": "5"}
                            ).encode("utf-8")
                            _write_frame(sock, 0x1, outbound)
                            with ECHO_STATS.lock:
                                ECHO_STATS.outbound_dtmf = "5"
                            sent_outbound = True
                        continue
                    # Unknown JSON / non-JSON text: echo it back as text.
                    _write_frame(sock, 0x1, payload)
                    continue
                # Binary → echo back as PCM16 round-trip.
                _write_frame(sock, 0x2, payload)
    except (OSError, struct.error):
        pass


def _play_conn(sock, addr, path):
    """Play a committed PCMA prompt as PCM16 binary frames, then close.

    The server streams all audio immediately (burst mode) so the rustpbx
    forward loop fills the internal channel buffer ahead of the egress's
    20 ms cadence, absorbing network jitter.  Incoming text frames (DTMF
    JSON) are drained after the payload is sent.
    """
    import select as _select

    wav = PROMPTS_DIR / "bridge-demo.wav"
    if not wav.is_file():
        print(f"[WS Play] missing {wav.name} — run --generate-prompts")
        _write_frame(sock, 0x8, b"")
        return
    tag, _, rate, bits, _ = wav_info(wav)
    if tag != 6 or bits != 8:
        print(f"[WS Play] {wav.name} is not PCMA 8-bit (tag={tag} bits={bits})")
        _write_frame(sock, 0x8, b"")
        return
    pcm = bytearray()
    data = wav.read_bytes()
    off = 12
    while off + 8 <= len(data):
        cid = data[off : off + 4]
        size = int.from_bytes(data[off + 4 : off + 8], "little")
        if cid == b"data":
            chunk = data[off + 8 : off + 8 + size]
            pcm = b"".join(struct.pack("<h", alaw2linear(b)) for b in chunk)
            break
        off += 8 + size + (size & 1)
    if not pcm:
        print(f"[WS Play] {wav.name} has no data chunk")
        _write_frame(sock, 0x8, b"")
        return
    print(f"[WS Play] streaming {wav.name} ({len(pcm) // 2} PCM16 samples) to {addr}")
    frame_size = rate // 50  # 20 ms per frame

    # Burst-send all audio frames immediately so the forward loop fills its
    # channel buffer and the egress drains at its own pace (filetrack mode).
    for i in range(0, len(pcm), frame_size * 2):
        _write_frame(sock, 0x2, bytes(pcm[i : i + frame_size * 2]))
        # Drain any incoming text/close frames without blocking.
        while _select.select([sock], [], [], 0)[0]:
            try:
                frame = _read_frame(sock)
            except (BlockingIOError, OSError, ValueError):
                break
            if frame is None:
                break
            opcode, payload = frame
            if opcode == 0x1:  # text → JSON (DTMF etc.)
                try:
                    data = json.loads(payload.decode("utf-8"))
                except (json.JSONDecodeError, UnicodeDecodeError):
                    data = None
                if isinstance(data, dict):
                    print(
                        "[WS Play] JSON message:\n"
                        + json.dumps(data, indent=2, ensure_ascii=False)
                    )
                    if data.get("type") == "dtmf":
                        print(
                            f"[WS Play] DTMF from {data.get('leg_id','?')}: "
                            f"'{data.get('digit','?')}'"
                        )
                else:
                    print(f"[WS Play] text: {payload.decode('utf-8', 'replace')}")
            elif opcode == 0x8:  # client closed early
                return

    # The egress drains the burst-buffered audio at real-time pace.  Keep the
    # connection open for the full audio duration so the forward loop keeps
    # feeding the channel — closing early truncates the tail of the prompt.
    audio_secs = len(pcm) / 2 / rate
    deadline = time.time() + audio_secs
    while time.time() < deadline:
        # Drain any incoming text/close frames without blocking.
        while _select.select([sock], [], [], 0)[0]:
            try:
                frame = _read_frame(sock)
            except (BlockingIOError, OSError, ValueError):
                break
            if frame is None:
                break
            opcode, payload = frame
            if opcode == 0x1:  # text → JSON (DTMF etc.)
                try:
                    data = json.loads(payload.decode("utf-8"))
                except (json.JSONDecodeError, UnicodeDecodeError):
                    data = None
                if isinstance(data, dict):
                    print(
                        "[WS Play] JSON message:\n"
                        + json.dumps(data, indent=2, ensure_ascii=False)
                    )
                    if data.get("type") == "dtmf":
                        print(
                            f"[WS Play] DTMF from {data.get('leg_id','?')}: "
                            f"'{data.get('digit','?')}'"
                        )
                else:
                    print(f"[WS Play] text: {payload.decode('utf-8', 'replace')}")
            elif opcode == 0x8:  # client closed early
                print(f"[WS Play] client closed early {addr}")
                return
        time.sleep(0.02)
    print(f"[WS Play] done, closing {addr}")
    _write_frame(sock, 0x8, b"")


def _handle_ws_conn(sock, addr):
    """WS dispatcher: perform the handshake, then route by request path."""
    try:
        sock.settimeout(15.0)
        path = _ws_handshake(sock)
        if path is None:
            return
        if path.rstrip("/").endswith("/play"):
            _play_conn(sock, addr, path)
        else:
            _echo_conn(sock, addr)
    except (OSError, struct.error):
        pass
    finally:
        try:
            sock.close()
        except OSError:
            pass


def _ws_serve(srv):
    print(f"[WS Echo] listening on ws://0.0.0.0:{srv.getsockname()[1]} (raw PCM16 binary echo + /play demo)")
    while True:
        try:
            sock, addr = srv.accept()
        except OSError:
            break
        threading.Thread(target=_handle_ws_conn, args=(sock, addr), daemon=True).start()


def start_ws_echo(port: int) -> tuple:
    """Start the WS echo server; returns (thread, actual_bound_port)."""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(16)
    actual = srv.getsockname()[1]
    t = threading.Thread(target=_ws_serve, args=(srv,), daemon=True)
    t.start()
    return t, actual


# ── SIP INFO body builders (formerly ivr_exec_demo.py) ───────────────────────

RUSTPBX_CT = "application/vnd.rustpbx+json"


def make_ivr_exec(
    request_id: str | None = None,
    app: str = "ivr",
    ivr_params: dict | None = None,
    music: dict | None = None,
    hold_agent: bool = True,
    webhook_url: str | None = None,
    metadata: dict | None = None,
) -> bytes:
    """Build an ivr.exec SIP INFO body (mid-call IVR injection)."""
    params: dict = {"request_id": request_id or str(uuid.uuid4()), "app": app}
    if ivr_params:
        params["ivr_params"] = ivr_params
    if music:
        params["music"] = music
    params["hold_agent"] = hold_agent
    if webhook_url:
        params["webhook_url"] = webhook_url
    if metadata:
        params["metadata"] = metadata
    return json.dumps({"action": "ivr.exec", "params": params}, ensure_ascii=False).encode("utf-8")


def make_app_start(app_name: str = "ivr", app_params: dict | None = None) -> bytes:
    return json.dumps(
        {"action": "app.start", "params": {"app_name": app_name, "app_params": app_params or {}}},
        ensure_ascii=False,
    ).encode("utf-8")


def make_app_stop() -> bytes:
    return json.dumps({"action": "app.stop", "params": {}}).encode("utf-8")


def make_hold(leg_id: str = "caller", music: dict | None = None) -> bytes:
    params: dict = {"leg_id": leg_id}
    if music:
        params["music"] = music
    return json.dumps({"action": "hold", "params": params}, ensure_ascii=False).encode("utf-8")


def print_sip_info_demos() -> None:
    print("=" * 60)
    print("Demo 1: ivr.exec — run the unified step IVR mid-call (holds agent)")
    print("=" * 60)
    body = make_ivr_exec(
        app="ivr",
        ivr_params={"mode": "step", "url": "http://127.0.0.1:8080/ivr/step"},
        hold_agent=True,
    )
    print(f"Content-Type: {RUSTPBX_CT}")
    print(f"Body ({len(body)} bytes): {body.decode()}")
    print()
    print("=" * 60)
    print("Demo 2: ivr.exec with a custom request_id")
    print("=" * 60)
    print(make_ivr_exec(request_id="my-correlator-001").decode())
    print()
    print("=" * 60)
    print("Demo 3: app.start voicemail")
    print("=" * 60)
    print(make_app_start(app_name="voicemail", app_params={"mailbox": "1000"}).decode())
    print()
    print("=" * 60)
    print("Demo 4: hold with custom music")
    print("=" * 60)
    print(make_hold(leg_id="caller", music={"source_type": "file", "uri": "sounds/premium_hold.wav"}).decode())
    print()
    print("Note: sending these requires a live 2-party call and a SIP stack that can")
    print("send in-dialog SIP INFO (e.g. sipbot --info-flows or aiosip).")


# ── HTTP server: step provider + audio ───────────────────────────────────────

def _safe_audio_name(name: str) -> bool:
    return bool(name) and name.replace(".", "").replace("_", "").replace("-", "").isalnum() and name.endswith(".wav")


class _ThreadedHTTPServer(ThreadingMixIn, HTTPServer):
    daemon_threads = True


class UnifiedIvrHandler(BaseHTTPRequestHandler):
    sessions: dict = {}
    sessions_lock = Lock()
    base_url = "http://127.0.0.1:8080"
    ws_endpoint = "ws://127.0.0.1:9090"
    ws_play_endpoint = "ws://127.0.0.1:9090/play"
    prompts_dir = PROMPTS_DIR

    # ---- GET /audio/<name>.wav ------------------------------------------------
    def do_GET(self):
        path = urlsplit(self.path).path
        if not path.startswith("/audio/"):
            self._send_json(404, {"error": "not_found", "path": path})
            return
        name = path[len("/audio/") :].lstrip("/")
        if not _safe_audio_name(name):
            self._send_json(400, {"error": "bad_audio_name"})
            return
        file = self.prompts_dir / name
        if not file.is_file():
            self._send_json(404, {"error": "audio_not_found", "name": name})
            return
        data = file.read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    # ---- POST /ivr/step* ------------------------------------------------------
    def do_POST(self):
        path = self.path.rstrip("/")
        if path == "/ivr/step":
            self._handle_step()
        elif path == "/ivr/step/start":
            self._handle_start()
        elif path == "/ivr/step/end":
            self._handle_end()
        else:
            self._send_json(404, {"error": "not_found", "path": path})

    def _handle_start(self):
        body = self._read_body()
        sid = body.get("session_id", "")
        with self.sessions_lock:
            self.sessions[sid] = IvrSession(
                body.get("caller", ""), body.get("callee", ""),
                self.base_url, self.ws_endpoint, self.ws_play_endpoint,
            )
        self._send_json(200, {"status": "ok"})

    def _handle_end(self):
        body = self._read_body()
        sid = body.get("session_id", "")
        with self.sessions_lock:
            self.sessions.pop(sid, None)
        self._send_json(200, {"status": "ok"})

    def _handle_step(self):
        body = self._read_body()
        sid = body.get("session_id", "")
        with self.sessions_lock:
            session = self.sessions.get(sid)
        if session is None:
            session = IvrSession(
                body.get("caller", ""), body.get("callee", ""),
                self.base_url, self.ws_endpoint, self.ws_play_endpoint,
            )
            with self.sessions_lock:
                self.sessions[sid] = session
        event = body.get("event", {"type": "session_start"})
        node = session.next_action(event)
        self._send_json(200, node)

    def _read_body(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length > 0 else b"{}"
        body = json.loads(raw) if raw else {}
        self._log_json("request", body)
        return body

    def _send_json(self, status, data):
        self._log_json("response", data, status=status)
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode("utf-8"))

    def _log_json(self, kind, data, status=None):
        path = urlsplit(self.path).path or self.path
        prefix = f"[IVR] {kind.upper()} {path}"
        if status is not None:
            prefix += f" status={status}"
        print(f"{prefix} {json.dumps(data, ensure_ascii=False, separators=(',', ':'))}")

    def log_message(self, *args):
        pass


def serve(ivr_port: int, ws_port: int, host: str):
    """Start the HTTP (IVR + audio) and WS servers; block forever."""
    ws_thread, ws_actual = start_ws_echo(ws_port)
    server = _ThreadedHTTPServer(("0.0.0.0", ivr_port), UnifiedIvrHandler)
    ivr_actual = server.server_address[1]
    UnifiedIvrHandler.base_url = f"http://{host}:{ivr_actual}"
    UnifiedIvrHandler.ws_endpoint = f"ws://{host}:{ws_actual}"
    UnifiedIvrHandler.ws_play_endpoint = f"ws://{host}:{ws_actual}/play"
    UnifiedIvrHandler.prompts_dir = PROMPTS_DIR
    print(f"[IVR] step provider on http://0.0.0.0:{ivr_actual}/ivr/step")
    print(f"[IVR] audio on http://{host}:{ivr_actual}/audio/<name>.wav")
    print(f"[IVR] bridge target = ws://{host}:{ws_actual}")
    print(f"[IVR] bridge play demo = ws://{host}:{ws_actual}/play (plays bridge-demo.wav, then closes → return to IVR)")
    print()
    print("[IVR] Add this route (e.g. config/routes/unified-ivr.toml):")
    print("    [[routes]]")
    print("    name = \"to-unified-step-ivr\"")
    print("    priority = 10")
    print("    action = \"application\"")
    print("    app = \"ivr\"")
    print("    auto_answer = true")
    print(f"    app_params = {{ mode = \"step\", url = \"http://{host}:{ivr_actual}/ivr/step\" }}")
    print("    [routes.match]")
    print("    \"to.user\" = \"*88\"")
    print()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[IVR] shutting down...")
        server.shutdown()


# ── Self-test ────────────────────────────────────────────────────────────────

def _http_post_json(url: str, payload: dict, timeout: float = 5.0) -> dict:
    import urllib.request

    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url, data=data, headers={"Content-Type": "application/json"}, method="POST"
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _ws_client_echo(url: str, samples: list) -> list:
    import socket as _s
    from urllib.parse import urlsplit as _us

    parts = _us(url)
    sock = _s.create_connection((parts.hostname, parts.port or 80), timeout=5.0)
    try:
        sock.sendall(
            (
                f"GET {parts.path or '/'} HTTP/1.1\r\n"
                f"Host: {parts.hostname}:{parts.port or 80}\r\n"
                "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                "Sec-WebSocket-Version: 13\r\n\r\n"
            ).encode("ascii")
        )
        f = sock.makefile("rb", 0)
        status = f.readline().decode("latin-1").strip()
        if "101" not in status:
            raise RuntimeError(f"WS handshake failed: {status}")
        while True:
            line = f.readline().decode("latin-1").strip()
            if not line:
                break
        f.close()
        payload = struct.pack("<%dh" % len(samples), *samples)
        _write_frame(sock, 0x2, payload, mask=True)
        frame = _read_frame(sock)
        if frame is None:
            raise RuntimeError("no echo frame")
        _opcode, data = frame
        return list(struct.unpack("<%dh" % (len(data) // 2), data))
    finally:
        try:
            sock.close()
        except OSError:
            pass


def _ws_client_play(url: str, timeout: float = 15.0) -> bytes:
    """Connect to a WS `/play` endpoint, collect binary PCM16 frames until the
    server closes, and return the concatenated payload bytes."""
    import socket as _s
    from urllib.parse import urlsplit as _us

    parts = _us(url)
    sock = _s.create_connection((parts.hostname, parts.port or 80), timeout=timeout)
    received = bytearray()
    try:
        sock.settimeout(timeout)
        sock.sendall(
            (
                f"GET {parts.path or '/'} HTTP/1.1\r\n"
                f"Host: {parts.hostname}:{parts.port or 80}\r\n"
                "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                "Sec-WebSocket-Version: 13\r\n\r\n"
            ).encode("ascii")
        )
        f = sock.makefile("rb", 0)
        status = f.readline().decode("latin-1").strip()
        if "101" not in status:
            raise RuntimeError(f"WS handshake failed: {status}")
        while True:
            line = f.readline().decode("latin-1").strip()
            if not line:
                break
        f.close()
        while True:
            frame = _read_frame(sock)
            if frame is None:
                break
            opcode, payload = frame
            if opcode == 0x8:  # close frame → server done
                break
            if opcode == 0x2:  # binary PCM16
                received.extend(payload)
    finally:
        try:
            sock.close()
        except OSError:
            pass
    return bytes(received)


def _check_prompts(prompts_dir: Path) -> list:
    failures = []
    for name in PROMPT_COPY:
        wav = prompts_dir / f"{name}.wav"
        if not wav.is_file():
            failures.append(f"missing prompt {wav.name}")
            continue
        tag, ch, rate, bits, _ = wav_info(wav)
        if (tag, ch, rate, bits) != (6, 1, 8000, 8):
            failures.append(
                f"{wav.name}: expected PCMA(6)/mono/8k/8bit, got tag={tag} ch={ch} rate={rate} bits={bits}"
            )
        rms = rms_of_alaw_wav(wav)
        if rms < 100:
            failures.append(f"{wav.name}: silent (rms={rms:.0f})")
    return failures


def self_test(ivr_port: int, ws_port: int, host: str) -> int:
    print("=" * 72)
    print("Unified IVR self-test — no SIP stack needed")
    print("=" * 72)

    failures = []
    failures.extend(_check_prompts(PROMPTS_DIR))

    _, ws_actual = start_ws_echo(ws_port)
    server = _ThreadedHTTPServer(("127.0.0.1", ivr_port), UnifiedIvrHandler)
    ivr_actual = server.server_address[1]
    UnifiedIvrHandler.base_url = f"http://{host}:{ivr_actual}"
    UnifiedIvrHandler.ws_endpoint = f"ws://{host}:{ws_actual}"
    UnifiedIvrHandler.ws_play_endpoint = f"ws://{host}:{ws_actual}/play"
    UnifiedIvrHandler.prompts_dir = PROMPTS_DIR
    srv_thread = threading.Thread(target=server.serve_forever, daemon=True)
    srv_thread.start()
    time.sleep(0.3)

    base = f"http://127.0.0.1:{ivr_actual}"
    url = f"{base}/ivr/step"
    sid = "selftest_%d" % int(time.time())
    trace = []
    _counter = {"n": 0}

    def step_on(sid2, label, event):
        resp = _http_post_json(url, {
            "session_id": sid2, "caller": "1001", "callee": "2000", "event": event,
        })
        trace.append((label, event.get("type"), resp.get("type"), resp))
        return resp

    def fresh_session(label):
        """New call → POST /ivr/step/start then session_start (welcome)."""
        _counter["n"] += 1
        sid2 = f"{sid}_{_counter['n']}"
        _http_post_json(f"{base}/ivr/step/start",
                        {"session_id": sid2, "caller": "1001", "callee": "2000"})
        step_on(sid2, label + "-start", {"type": "session_start"})
        return sid2

    # 1. session_start → interruptible welcome file prompt
    r = step_on(sid, "welcome", {"type": "session_start"})
    if r.get("type") != "prompt" or not r.get("interruptible"):
        failures.append(f"welcome: expected interruptible prompt, got {r}")
    if not (r.get("file") or "").endswith("/audio/welcome.wav"):
        failures.append(f"welcome: file should be a served PCMA wav URL, got {r.get('file')}")

    # 2. audio_complete (welcome finished) → menu prompt
    r = step_on(sid, "menu", {"type": "audio_complete"})
    if not (r.get("file") or "").endswith("/audio/menu.wav"):
        failures.append(f"menu: expected menu file prompt, got {r}")

    # 3. DTMF branches — each on a fresh session (state machine is sequential)
    r = step_on(fresh_session("b1"), "b1-dtmf1", {"type": "dtmf", "digit": "1"})
    if r.get("type") != "prompt" or not (r.get("tts_text") or "").startswith("The current time is"):
        failures.append(f"dtmf-1: expected tts_text time prompt, got {r}")

    r = step_on(fresh_session("b2"), "b2-dtmf2", {"type": "dtmf", "digit": "2"})
    if r.get("type") != "transfer" or r.get("target") != "2001":
        failures.append(f"dtmf-2: expected transfer 2001, got {r}")

    r = step_on(fresh_session("b3"), "b3-dtmf3", {"type": "dtmf", "digit": "3"})
    if r.get("type") != "queue" or r.get("target") != "sales":
        failures.append(f"dtmf-3: expected queue sales, got {r}")

    r = step_on(fresh_session("b4"), "b4-dtmf4", {"type": "dtmf", "digit": "4"})
    if r.get("type") != "voip_bridge" or not r.get("create_room_uri", "").startswith(f"ws://{host}:"):
        failures.append(f"dtmf-4: expected voip_bridge to WS, got {r}")
    if not (r.get("create_room_uri") or "").endswith("/play"):
        failures.append(f"dtmf-4: expected bridge to /play endpoint, got {r.get('create_room_uri')}")
    if r.get("return_app") != "ivr" or r.get("return_target") != "main":
        failures.append(f"dtmf-4: expected return_app/return_target, got {r}")

    r = step_on(fresh_session("b0"), "b0-dtmf0", {"type": "dtmf", "digit": "0"})
    if r.get("type") != "play_and_hangup" or r.get("code") != 200:
        failures.append(f"dtmf-0: expected play_and_hangup(200), got {r}")

    # 4. invalid ×3 on one fresh session → play_and_hangup
    sid_invalid = fresh_session("inv")
    for i in range(3):
        r = step_on(sid_invalid, f"inv-{i}", {"type": "dtmf", "digit": "9"})
    if r.get("type") != "play_and_hangup":
        failures.append(f"invalid×3: expected play_and_hangup, got {r}")

    # 5. WS PCM16 echo round-trip
    samples = [(i * 256) & 0xFFFF for i in range(80)]
    samples = [s if s < 32768 else s - 65536 for s in samples]
    echoed = _ws_client_echo(f"ws://127.0.0.1:{ws_actual}", samples)
    if echoed != samples:
        failures.append("WS PCM16 echo mismatch")
    print(f"[trace] WS echo: sent {len(samples)} samples, got {len(echoed)} "
          f"({'OK' if echoed == samples else 'MISMATCH'})")

    time.sleep(0.2)
    snap = ECHO_STATS.snapshot()
    print(f"[trace] WS stats: {snap['connections']} conn(s), "
          f"{snap['frames_received']} frame(s)")

    # 6. WS /play endpoint: server streams bridge-demo.wav as PCM16, then closes.
    play_url = f"ws://127.0.0.1:{ws_actual}/play"
    play_bytes = _ws_client_play(play_url)
    if len(play_bytes) < 160:
        failures.append(f"WS /play received too little PCM16 ({len(play_bytes)} bytes)")
    elif len(play_bytes) % 2 != 0:
        failures.append(f"WS /play PCM16 length not even ({len(play_bytes)} bytes)")
    else:
        n = len(play_bytes) // 2
        pcm = struct.unpack("<%dh" % n, play_bytes)
        rms = (sum(s * s for s in pcm) / n) ** 0.5
        print(f"[trace] WS /play: received {n} PCM16 samples, RMS {rms:.0f}")
        if rms < 100:
            failures.append(f"WS /play audio silent (rms={rms:.0f})")

    # ── Print the trace table ──────────────────────────────────────────────
    print("\n" + "-" * 72)
    print("IVR STEP TRACE")
    print("-" * 72)
    print("%-22s %-18s %-14s %s" % ("label", "event", "action", "note"))
    print("-" * 72)
    for label, ev, act, node in trace:
        note = ""
        if act == "prompt":
            note = (node.get("file") or node.get("tts_text") or "")[:44]
        elif act == "transfer":
            note = "→ " + node.get("target", "")
        elif act == "queue":
            note = "→ queue " + node.get("target", "")
        elif act == "voip_bridge":
            note = "→ " + node.get("create_room_uri", "")
        elif act == "play_and_hangup":
            note = f"code={node.get('code')}"
        print("%-22s %-18s %-14s %s" % (label, ev or "-", act or "-", note))
    print("-" * 72)

    server.shutdown()
    if failures:
        print("\nFAILED:")
        for f in failures:
            print("  ✗", f)
        return 1
    print("\nALL CHECKS PASSED ✓")
    return 0


# ── Unit tests ───────────────────────────────────────────────────────────────

class TestIvrSession(unittest.TestCase):
    def setUp(self):
        self.sess = IvrSession(
            "1001", "2000", "http://127.0.0.1:8080", "ws://127.0.0.1:9090"
        )

    def test_start_returns_interruptible_welcome_file(self):
        node = self.sess.next_action({"type": "session_start"})
        self.assertEqual(node["type"], "prompt")
        self.assertTrue(node["interruptible"])
        self.assertTrue(node["file"].endswith("/audio/welcome.wav"))

    def test_welcome_audio_complete_returns_menu(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "audio_complete"})
        self.assertEqual(node["type"], "prompt")
        self.assertTrue(node["file"].endswith("/audio/menu.wav"))

    def test_dtmf_1_returns_tts_time_prompt(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf", "digit": "1"})
        self.assertEqual(node["type"], "prompt")
        self.assertTrue(node.get("tts_text", "").startswith("The current time is"))

    def test_dtmf_2_returns_transfer(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf", "digit": "2"})
        self.assertEqual(node["type"], "transfer")
        self.assertEqual(node["target"], "2001")

    def test_dtmf_3_returns_queue(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf", "digit": "3"})
        self.assertEqual(node["type"], "queue")
        self.assertEqual(node["target"], "sales")

    def test_dtmf_4_returns_voip_bridge(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf", "digit": "4"})
        self.assertEqual(node["type"], "voip_bridge")
        self.assertEqual(node["create_room_uri"], "ws://127.0.0.1:9090/play")
        self.assertEqual(node["return_app"], "ivr")
        self.assertEqual(node["return_target"], "main")

    def test_dtmf_0_returns_play_and_hangup(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf", "digit": "0"})
        self.assertEqual(node["type"], "play_and_hangup")
        self.assertEqual(node["code"], 200)
        self.assertTrue(node["prompt"].endswith("/audio/goodbye.wav"))

    def test_invalid_digit_returns_invalid_prompt(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf", "digit": "9"})
        self.assertEqual(node["type"], "prompt")
        self.assertTrue(node["file"].endswith("/audio/invalid.wav"))

    def test_invalid_digit_three_times_play_and_hangup(self):
        self.sess.next_action({"type": "session_start"})
        for _ in range(3):
            node = self.sess.next_action({"type": "dtmf", "digit": "9"})
        self.assertEqual(node["type"], "play_and_hangup")

    def test_error_event_returns_static_menu(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "error", "reason": "TTS unavailable"})
        # Never return tts_text on an error event (would loop).
        self.assertEqual(node["type"], "prompt")
        self.assertIsNone(node.get("tts_text"))
        self.assertTrue(node["file"].endswith("/audio/menu.wav"))

    def test_unknown_event_hangup(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "unknown_event"})
        self.assertEqual(node["type"], "hangup")


class TestAlaw(unittest.TestCase):
    def test_alaw2linear_known_values(self):
        # Verified against the standard G.711 A-law → linear table
        # (FFmpeg libavcodec / ITU-T G.711 reference).
        self.assertEqual(alaw2linear(0xD5), 8)       # A-law silence / near-zero
        self.assertEqual(alaw2linear(0x55), -8)       # softest negative step
        self.assertEqual(alaw2linear(0x2A), -32256)   # loud negative
        self.assertEqual(alaw2linear(0xAA), 32256)    # loud positive
        self.assertEqual(alaw2linear(0x60), -1376)    # moderate negative
        # 0x7F ≠ 0xFF → decoder is not degenerate.
        self.assertNotEqual(alaw2linear(0x7F), alaw2linear(0xFF))

    def test_alaw_round_trip_bounded_error(self):
        for code in range(256):
            dec = alaw2linear(code)
            # Decoder output stays within 16-bit range.
            self.assertGreaterEqual(dec, -32768)
            self.assertLessEqual(dec, 32767)


class TestWavInfo(unittest.TestCase):
    def test_wav_info_parses_generated_prompt(self):
        wav = PROMPTS_DIR / "menu.wav"
        if not wav.is_file():
            self.skipTest("prompts not generated")
        tag, ch, rate, bits, _ = wav_info(wav)
        self.assertEqual((tag, ch, rate, bits), (6, 1, 8000, 8))
        self.assertGreater(rms_of_alaw_wav(wav), 100)


class TestWsFrames(unittest.TestCase):
    def test_write_frame_small_payload(self):
        class FakeSock:
            def __init__(self):
                self.buf = bytearray()

            def sendall(self, b):
                self.buf.extend(b)

        s = FakeSock()
        _write_frame(s, 0x2, b"\x01\x00\x02\x00")
        self.assertEqual(s.buf[0], 0x82)  # FIN + binary
        self.assertEqual(s.buf[1], 4)
        self.assertEqual(bytes(s.buf[2:]), b"\x01\x00\x02\x00")

    def test_write_frame_text(self):
        class FakeSock:
            def __init__(self):
                self.buf = bytearray()

            def sendall(self, b):
                self.buf.extend(b)

        s = FakeSock()
        _write_frame(s, 0x1, b'{"type":"dtmf","digit":"1"}')
        self.assertEqual(s.buf[0], 0x81)  # FIN + text
        self.assertEqual(s.buf[1], 27)


class TestSipInfoBuilders(unittest.TestCase):
    def test_make_ivr_exec_shape(self):
        body = json.loads(make_ivr_exec(ivr_params={"mode": "step", "url": "http://x/step"}))
        self.assertEqual(body["action"], "ivr.exec")
        self.assertEqual(body["params"]["app"], "ivr")
        self.assertTrue(body["params"]["hold_agent"])

    def test_make_app_start_stop(self):
        body = json.loads(make_app_start(app_name="voicemail", app_params={"mailbox": "1000"}))
        self.assertEqual(body["action"], "app.start")
        self.assertEqual(body["params"]["app_name"], "voicemail")
        stop = json.loads(make_app_stop())
        self.assertEqual(stop["action"], "app.stop")

    def test_make_hold(self):
        body = json.loads(make_hold(leg_id="caller"))
        self.assertEqual(body["action"], "hold")
        self.assertEqual(body["params"]["leg_id"], "caller")


# ── CLI / entry point ────────────────────────────────────────────────────────

def main(argv=None):
    p = argparse.ArgumentParser(description="Unified IVR example provider")
    p.add_argument("ivr_port", nargs="?", type=int, default=8080,
                   help="HTTP provider + audio port (default 8080)")
    p.add_argument("--ws-port", type=int, default=0,
                   help="WebSocket echo port (default 0 = auto-pick)")
    p.add_argument("--host", default="127.0.0.1",
                   help="Host used in audio/bridge URLs (default 127.0.0.1)")
    p.add_argument("--self-test", action="store_true",
                   help="run the in-process self-test and exit")
    p.add_argument("--print-sip-info", action="store_true",
                   help="print SIP INFO body builders demo and exit")
    p.add_argument("--generate-prompts", action="store_true",
                   help="(re)generate the PCMA prompt wavs (edge-cli + ffmpeg) and exit")
    args = p.parse_args(argv)

    if args.generate_prompts:
        generate_prompts(PROMPTS_DIR)
        return 0
    if args.print_sip_info:
        print_sip_info_demos()
        return 0
    if args.self_test:
        return self_test(args.ivr_port, args.ws_port, args.host)
    serve(args.ivr_port, args.ws_port, args.host)
    return 0


if __name__ == "__main__":
    sys.exit(main())
