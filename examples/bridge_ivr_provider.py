#! /usr/bin/env python3
"""Bridge IVR Provider + WebSocket PCM16 echo server — stdlib only.

Demonstrates the full IVR → bridge transfer flow:

  1. IVR step provider exposes POST /ivr/step
     - session_start → welcome prompt → dtmf_menu
     - press "1" → returns a TERMINAL ``bridge`` ActionNode that bridges
       the call's audio to a WebSocket endpoint speaking raw PCM16
     - press "2" → announces current time (prompt)
     - press "0" → hangup
  2. A stdlib WebSocket server (raw socket, no third-party deps) acts as the
     "external VoIP service". It echoes any binary PCM16 frame back to the
     client — i.e. RustPBX receives its own audio. This is the same shape the
     Rust E2E test (src/proxy/tests/test_bridge_e2e.rs) uses.

Topology once the bridge is up:

  Caller (WebRTC/RTP)  ◄──►  RustPBX SipSession  ◄──►  WebSocket (PCM16)  ◄──►  this echo server

Run (defaults: IVR HTTP on 8080, WS echo on 9090):

    python3 examples/bridge_ivr_provider.py

Run with custom ports:

    python3 examples/bridge_ivr_provider.py 8080 9090

Self-test (drives the IVR flow against the provider + pings the WS server
with a PCM16 frame, printing a step-by-step trace — no SIP stack required):

    python3 examples/bridge_ivr_provider.py --self-test

Unit tests:

    python3 -m unittest examples/bridge_ivr_provider.py
"""

import argparse
import base64
import hashlib
import json
import os
import struct
import sys
import threading
import time
from http.server import HTTPServer, BaseHTTPRequestHandler
from socketserver import ThreadingMixIn
from threading import Lock
from urllib.parse import urlsplit

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

WELCOME_TEXT = "Welcome to the VoIP bridge demo. Press 1 to bridge the call to an external service, press 2 for the time, press 0 to hang up."


# ── IVR session state machine ────────────────────────────────────────────────


class IvrSession:
    """Per-call state machine.

    rustpbx calls POST /ivr/step on every event; we reply with the next
    ActionNode. See docs/ivr_step_protocol_en.md for the protocol and
    docs/bridge.md for the bridge terminal action.
    """

    def __init__(self, caller: str, callee: str, ws_endpoint: str):
        self.caller = caller
        self.callee = callee
        self.ws_endpoint = ws_endpoint
        self._step = "start"
        self._menu_retries = 0

    def next_action(self, event: dict) -> dict:
        ev_type = event.get("type", "")

        if self._step == "start":
            # Welcome prompt chained (no provider round-trip) into a DTMF menu.
            self._step = "wait_menu"
            return {
                "type": "prompt",
                "tts_text": WELCOME_TEXT,
                "interruptible": True,
                "step_id": "welcome",
                "step_name": "欢迎语",
                "next": {
                    "type": "dtmf_menu",
                    "greeting_text": "Press 1, 2 or 0",
                    "timeout_ms": 8000,
                    "max_retries": 3,
                    "entries": {
                        "1": action_bridge(self.ws_endpoint),
                        "2": {"type": "prompt", "tts_text": _time_text()},
                        "0": action_hangup(),
                    },
                },
            }

        if self._step == "wait_menu":
            # Reached only when the menu forwards an event here (entries empty
            # path) — kept for compatibility with the plain-dtmf provider style.
            if ev_type == "dtmf":
                digit = event.get("digit", "")
                if digit == "1":
                    return action_bridge(self.ws_endpoint)
                if digit == "2":
                    return action_prompt(tts_text=_time_text())
                if digit == "0":
                    return action_hangup()
                self._menu_retries += 1
                if self._menu_retries >= 3:
                    return action_hangup()
                return action_prompt(tts_text="Invalid option. " + WELCOME_TEXT,
                                     interruptible=True)
            if ev_type == "dtmf_timeout":
                return action_hangup()

        return action_hangup()


# ── ActionNode builders ──────────────────────────────────────────────────────


def action_prompt(tts_text=None, file=None, interruptible=False):
    node = {"type": "prompt", "interruptible": interruptible}
    if tts_text:
        node["tts_text"] = tts_text
    if file:
        node["file"] = file
    return node


def action_hangup():
    return {"type": "hangup"}


def action_bridge(ws_endpoint: str, headers=None, timeout_ms=10000):
    """Terminal action: bridge call audio to a WebSocket endpoint as PCM16.

    rustpbx turns this into a blind transfer target
    ``bridge:<ws_endpoint>?codec=pcm&samplerate=8000&_hdr_*=&timeout_ms=``
    and runs connect_bridge() in src/proxy/proxy_call/sip_session/transfer.rs.
    """
    return {
        "type": "bridge",
        "create_room_uri": ws_endpoint,
        "headers": headers or {"X-Source": "ivr-voip-bridge-example"},
        "timeout_ms": timeout_ms,
        "step_id": "bridge",
        "step_name": "VoIP 桥接",
    }


def _time_text() -> str:
    return "The current time is " + time.strftime("%Y-%m-%d %H:%M:%S")


# ── stdlib WebSocket PCM16 echo server ───────────────────────────────────────
# Minimal RFC 6455 implementation: handshake + binary frame echo.
# No third-party dependencies — mirrors the zero-deps convention of
# examples/step_ivr_provider.py.


class _EchoStats:
    def __init__(self):
        self.lock = Lock()
        self.connections = 0
        self.frames_received = 0
        self.bytes_received = 0
        self.last_samples = None  # last decoded PCM16 i16 list (for tests)

    def snapshot(self):
        with self.lock:
            return {
                "connections": self.connections,
                "frames_received": self.frames_received,
                "bytes_received": self.bytes_received,
                "last_samples": self.last_samples,
            }


ECHO_STATS = _EchoStats()


def _ws_handshake(sock) -> bool:
    """Read the HTTP upgrade request, write the 101 response. Returns False on
    any protocol error so the caller can close the socket."""
    f = sock.makefile("rb", 0)
    headers = {}
    try:
        start_line = f.readline().decode("latin-1").strip()
        if not start_line.startswith("GET "):
            return False
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
        return False
    accept = base64.b64encode(
        hashlib.sha1((key + WS_GUID).encode("ascii")).digest()
    ).decode("ascii")

    resp = (
        "HTTP/1.1 101 Switching Protocols\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
    ).encode("ascii")
    sock.sendall(resp)
    return True


def _recv_exactly(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def _read_frame(sock):
    """Read one WebSocket frame. Returns (opcode, payload) or None on close/error."""
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
    """Write one WebSocket frame. Server→client is unmasked; client→server
    must set ``mask=True`` (RFC 6455). Handles the 7/16/64-bit length forms."""
    out = bytearray()
    out.append(0x80 | (opcode & 0x0F))  # FIN=1
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
        sock.settimeout(15.0)
        if not _ws_handshake(sock):
            return
        with ECHO_STATS.lock:
            ECHO_STATS.connections += 1
        print(f"[WS Echo] connection from {addr} (total {ECHO_STATS.connections})")
        while True:
            frame = _read_frame(sock)
            if frame is None:
                break
            opcode, payload = frame
            if opcode == 0x8:  # close
                _write_frame(sock, 0x8, b"")
                break
            if opcode == 0x9:  # ping
                _write_frame(sock, 0xA, payload)  # pong
                continue
            if opcode == 0xA:  # pong
                continue
            if opcode == 0x2 or opcode == 0x1:  # binary / text
                with ECHO_STATS.lock:
                    ECHO_STATS.frames_received += 1
                    ECHO_STATS.bytes_received += len(payload)
                    if opcode == 0x2 and len(payload) % 2 == 0:
                        ECHO_STATS.last_samples = list(
                            struct.unpack("<%dh" % (len(payload) // 2), payload)
                        )
                # Echo back as a binary frame (PCM16 round-trip).
                _write_frame(sock, 0x2, payload)
    except (OSError, struct.error):
        pass
    finally:
        try:
            sock.close()
        except OSError:
            pass


def _ws_serve(port: int):
    import socket

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(16)
    print(f"[WS Echo] listening on ws://0.0.0.0:{port} (raw PCM16 binary echo)")
    while True:
        try:
            sock, addr = srv.accept()
        except OSError:
            break
        t = threading.Thread(target=_echo_conn, args=(sock, addr), daemon=True)
        t.start()


def start_ws_echo(port: int):
    t = threading.Thread(target=_ws_serve, args=(port,), daemon=True)
    t.start()
    return t


# ── IVR HTTP step provider ───────────────────────────────────────────────────


class _ThreadedHTTPServer(ThreadingMixIn, HTTPServer):
    daemon_threads = True


class IvrStepHandler(BaseHTTPRequestHandler):
    sessions: dict = {}
    sessions_lock = Lock()
    ws_endpoint = "ws://127.0.0.1:9090"

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
                body.get("caller", ""), body.get("callee", ""), self.ws_endpoint
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
                body.get("caller", ""), body.get("callee", ""), self.ws_endpoint
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
        payload = json.dumps(data, ensure_ascii=False, separators=(",", ":"))
        print(f"{prefix} {payload}")

    def log_message(self, *args):
        pass


def serve(ivrr_port: int, ws_port: int):
    IvrStepHandler.ws_endpoint = f"ws://127.0.0.1:{ws_port}"
    start_ws_echo(ws_port)
    server = _ThreadedHTTPServer(("0.0.0.0", ivrr_port), IvrStepHandler)
    print(f"[IVR] step provider on http://0.0.0.0:{ivrr_port}/ivr/step")
    print(f"[IVR] bridge target = {IvrStepHandler.ws_endpoint}")
    print("[IVR] Endpoints:")
    print("  POST /ivr/step         — main provider endpoint")
    print("  POST /ivr/step/start   — session start (fire-and-forget)")
    print("  POST /ivr/step/end     — session end   (fire-and-forget)")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[IVR] shutting down...")
        server.shutdown()


# ── Self-test: drive the IVR flow + verify the WS echo round-trip ────────────


def _http_post_json(url: str, payload: dict, timeout: float = 5.0) -> dict:
    import urllib.request

    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url, data=data, headers={"Content-Type": "application/json"}, method="POST"
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _ws_client_echo(url: str, samples: list) -> list:
    """Connect to the echo server as a client, send PCM16, return echoed samples."""
    import socket
    from urllib.parse import urlsplit

    parts = urlsplit(url)
    host = parts.hostname
    port = parts.port or 80
    sock = socket.create_connection((host, port), timeout=5.0)
    try:
        sock.sendall(
            (
                f"GET {parts.path or '/'} HTTP/1.1\r\n"
                f"Host: {host}:{port}\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                "Sec-WebSocket-Version: 13\r\n\r\n"
            ).encode("ascii")
        )
        # read 101 response
        f = sock.makefile("rb", 0)
        status = f.readline().decode("latin-1").strip()
        if "101" not in status:
            raise RuntimeError(f"WS handshake failed: {status}")
        while True:
            line = f.readline().decode("latin-1").strip()
            if not line:
                break
        f.close()
        # send masked binary frame (client→server MUST mask per RFC 6455)
        payload = struct.pack("<%dh" % len(samples), *samples)
        _write_frame(sock, 0x2, payload, mask=True)
        # read echoed frame
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


def self_test(ivrr_port: int, ws_port: int):
    """Run provider + WS server in-process, drive the flow, print a trace."""
    print("=" * 72)
    print("Bridge IVR self-test — no SIP stack needed")
    print("=" * 72)
    IvrStepHandler.ws_endpoint = f"ws://127.0.0.1:{ws_port}"
    start_ws_echo(ws_port)
    server = _ThreadedHTTPServer(("127.0.0.1", ivrr_port), IvrStepHandler)
    srv_thread = threading.Thread(target=server.serve_forever, daemon=True)
    srv_thread.start()
    time.sleep(0.3)

    url = f"http://127.0.0.1:{ivrr_port}/ivr/step"
    sid = "selftest_%d" % int(time.time())
    trace = []
    failures = []

    def step(label, event):
        resp = _http_post_json(url, {
            "session_id": sid, "caller": "1001", "callee": "2000",
            "event": event,
        })
        trace.append((label, event.get("type"), resp.get("type"), resp))
        return resp

    # 1. session_start → prompt + dtmf_menu (entries hold the bridge)
    resp = step("initial", {"type": "session_start"})
    assert resp["type"] == "prompt", resp
    menu = resp.get("next", {})
    assert menu.get("type") == "dtmf_menu", menu
    entry1 = menu.get("entries", {}).get("1", {})
    assert entry1.get("type") == "bridge", entry1
    print(f"[trace] session_start -> {resp['type']} (next={menu['type']}, "
          f"entry['1']={entry1['type']} uri={entry1.get('create_room_uri')})")

    # 2. Verify the bridge ActionNode shape matches the protocol doc.
    uri = entry1.get("create_room_uri", "")
    if f":{ws_port}" not in uri:
        failures.append(f"ws port mismatch in create_room_uri: {uri}")
    if "timeout_ms" not in entry1:
        failures.append("timeout_ms missing on bridge node")

    # 3. Drive dtmf "2" through a fresh session to exercise the time branch.
    sid2 = sid + "_b"
    _http_post_json(f"http://127.0.0.1:{ivrr_port}/ivr/step/start",
                    {"session_id": sid2, "caller": "1001", "callee": "2000"})
    r1 = _http_post_json(url, {"session_id": sid2, "event": {"type": "session_start"}})
    trace.append(("time-branch-start", "session_start", r1.get("type"), r1))
    r2 = _http_post_json(url, {"session_id": sid2, "event": {"type": "dtmf", "digit": "2"}})
    trace.append(("time-branch-dtmf2", "dtmf", r2.get("type"), r2))

    # 4. WS echo round-trip with a 10ms PCMU-style PCM16 frame (80 samples @8k).
    samples = [(i * 256) & 0xFFFF for i in range(80)]
    samples = [s if s < 32768 else s - 65536 for s in samples]
    echoed = _ws_client_echo(f"ws://127.0.0.1:{ws_port}", samples)
    if echoed != samples:
        failures.append("WS PCM16 echo mismatch")
    print(f"[trace] WS echo: sent {len(samples)} samples, got {len(echoed)} "
          f"({'OK' if echoed == samples else 'MISMATCH'})")

    time.sleep(0.2)
    snap = ECHO_STATS.snapshot()
    print(f"[trace] WS stats: {snap['connections']} conn(s), "
          f"{snap['frames_received']} frame(s), {snap['bytes_received']} bytes")

    if snap["connections"] < 1 or snap["frames_received"] < 1:
        failures.append("WS echo server did not record the client connection")

    # ── Print the trace table ────────────────────────────────────────────────
    print("\n" + "-" * 72)
    print("IVR STEP TRACE")
    print("-" * 72)
    print("%-22s %-16s %-14s %s" % ("label", "event", "action", "note"))
    print("-" * 72)
    for label, ev, act, node in trace:
        note = ""
        if act == "bridge":
            note = "→ " + node.get("create_room_uri", "")
        elif act == "prompt":
            note = (node.get("tts_text") or node.get("file") or "")[:40]
        print("%-22s %-16s %-14s %s" % (label, ev or "-", act or "-", note))
    print("-" * 72)
    print("WS ECHO  sent=%d  echoed=%d  match=%s"
          % (len(samples), len(echoed), echoed == samples))
    print("WS STATS conn=%d frames=%d bytes=%d"
          % (snap["connections"], snap["frames_received"], snap["bytes_received"]))
    print("-" * 72)

    if failures:
        print("\nFAILED:")
        for f in failures:
            print("  ✗", f)
        server.shutdown()
        return 1
    print("\nALL CHECKS PASSED ✓")
    server.shutdown()
    return 0


# ── Unit tests ───────────────────────────────────────────────────────────────


import unittest


class TestIvrSession(unittest.TestCase):
    def setUp(self):
        self.sess = IvrSession("1001", "2000", "ws://127.0.0.1:9090")

    def test_start_returns_prompt_chained_to_menu(self):
        node = self.sess.next_action({"type": "session_start"})
        self.assertEqual(node["type"], "prompt")
        nxt = node["next"]
        self.assertEqual(nxt["type"], "dtmf_menu")
        self.assertIn("1", nxt["entries"])

    def test_menu_entry_1_is_bridge(self):
        node = self.sess.next_action({"type": "session_start"})
        e1 = node["next"]["entries"]["1"]
        self.assertEqual(e1["type"], "bridge")
        self.assertEqual(e1["create_room_uri"], "ws://127.0.0.1:9090")
        self.assertGreater(e1["timeout_ms"], 0)
        self.assertIn("headers", e1)

    def test_menu_entry_0_is_hangup(self):
        node = self.sess.next_action({"type": "session_start"})
        self.assertEqual(node["next"]["entries"]["0"]["type"], "hangup")

    def test_bridge_uri_from_env_override(self):
        s = IvrSession("1", "2", "wss://relay.example.com/ws")
        node = s.next_action({"type": "session_start"})
        self.assertEqual(
            node["next"]["entries"]["1"]["create_room_uri"],
            "wss://relay.example.com/ws",
        )


class TestWsFrames(unittest.TestCase):
    def test_write_frame_small_payload(self):
        # Build a frame using the server writer helpers, then parse length.
        import io

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


def main():
    p = argparse.ArgumentParser(description="Bridge IVR provider + WS echo")
    p.add_argument("ivrr_port", nargs="?", type=int, default=8080)
    p.add_argument("ws_port", nargs="?", type=int, default=9090)
    p.add_argument("--self-test", action="store_true",
                   help="drive the IVR flow + WS echo in-process and exit")
    args = p.parse_args()

    if args.self_test:
        sys.exit(self_test(args.ivrr_port, args.ws_port))
    serve(args.ivrr_port, args.ws_port)


if __name__ == "__main__":
    main()
