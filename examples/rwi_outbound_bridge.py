#! /usr/bin/env python3
"""RWI outbound → voip_bridge demo — stdlib only.

Demonstrates the complete outbound (originated call) voip_bridge flow that was
previously broken ("Cannot transfer leg caller: invalid state Initializing"):

  1. `call.originate` with an inline `record` option (recording auto-starts
     once the callee answers)
  2. wait for `call_answered` (+ `record_started`)
  3. `call.transfer` the caller leg to a `voip_bridge:` WebSocket endpoint —
     this example also *runs* that endpoint: a local PCM16 echo server
  4. bidirectional audio: the echo server receives PCM16 binary frames
     (call → WS) and echoes them back (WS → call); DTMF arrives as JSON text
  5. `call.hangup` → `call_hangup`, and the recording chain finishes:
     `record_stopped` → `recording_metadata_available` → `record_end`

Run (needs a rustpbx with [rwi] enabled, media_proxy anchoring and [recording]
enabled for the metadata events):

    python3 examples/rwi_outbound_bridge.py \\
        --rwi ws://127.0.0.1:18080/rwi/v1 --token test-api-key-e2e \\
        --dest sip:1002@127.0.0.1 --bridge-port 9101

Self-test (frame codec only, no PBX):

    python3 examples/rwi_outbound_bridge.py --self-test

Unit tests:

    python3 -m unittest examples/rwi_outbound_bridge.py

See docs/bridge.md §6 (RWI 呼叫流程) and docs/rwi.md §5.3–5.4 for protocol
details.
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
from urllib.parse import urlsplit

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


# ---------------------------------------------------------------------------
# Minimal WebSocket framing (shared by client and server sides)
# ---------------------------------------------------------------------------

def _recv_exactly(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def _read_frame(sock):
    """Return (opcode, payload) or None on EOF/close-error."""
    hdr = _recv_exactly(sock, 2)
    if hdr is None:
        return None
    opcode = hdr[0] & 0x0F
    masked = (hdr[1] & 0x80) != 0
    length = hdr[1] & 0x7F
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
    """Server→client frames unmasked; client→server frames MUST be masked."""
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


# ---------------------------------------------------------------------------
# Bridge echo server (what rustpbx connects to after the transfer)
# ---------------------------------------------------------------------------

class BridgeEchoServer(threading.Thread):
    """Accepts rustpbx's voip_bridge connection; echoes PCM16, prints DTMF."""

    def __init__(self, host="127.0.0.1", port=9101, sample_rate=8000):
        super().__init__(daemon=True)
        self.host, self.port, self.sample_rate = host, port, sample_rate
        self.lock = threading.Lock()
        self.connections = 0
        self.pcm_bytes = 0
        self.echoed_bytes = 0
        self.dtmf = []
        self.served = threading.Event()
        self._srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._srv.bind((host, port))
        self._srv.listen(4)
        self.port = self._srv.getsockname()[1]
        self._stop = threading.Event()

    @property
    def ws_url(self):
        return f"ws://{self.host}:{self.port}/ws"

    def stats(self):
        with self.lock:
            return dict(connections=self.connections, pcm_bytes=self.pcm_bytes,
                        echoed_bytes=self.echoed_bytes, dtmf=list(self.dtmf))

    def run(self):
        self.served.set()
        self._srv.settimeout(0.3)
        while not self._stop.is_set():
            try:
                conn, addr = self._srv.accept()
            except socket.timeout:
                continue
            threading.Thread(target=self._serve, args=(conn, addr),
                             daemon=True).start()

    def stop(self):
        self._stop.set()
        try:
            self._srv.close()
        except OSError:
            pass

    # -- one bridge connection --------------------------------------------

    def _serve(self, conn, addr):
        try:
            if self._handshake(conn) is None:
                return
            with self.lock:
                self.connections += 1
            print(f"[bridge] rustpbx connected from {addr}")
            while True:
                frame = _read_frame(conn)
                if frame is None:
                    break
                opcode, payload = frame
                if opcode == 0x8:            # close
                    _write_frame(conn, 0x8, b"")
                    break
                if opcode == 0x9:            # ping
                    _write_frame(conn, 0xA, payload)
                    continue
                if opcode == 0xA:            # pong
                    continue
                if opcode == 0x1:            # text → DTMF JSON (call → WS)
                    try:
                        msg = json.loads(payload.decode("utf-8"))
                    except (json.JSONDecodeError, UnicodeDecodeError):
                        msg = None
                    if isinstance(msg, dict) and msg.get("type") == "dtmf":
                        digit = msg.get("digit", "?")
                        leg = msg.get("leg_id", "?")
                        with self.lock:
                            self.dtmf.append(digit)
                        print(f"[bridge] DTMF '{digit}' from leg {leg}")
                    else:
                        print(f"[bridge] text: {payload[:120]!r}")
                    continue
                if opcode == 0x2:            # binary → PCM16 echo (WS → call)
                    with self.lock:
                        self.pcm_bytes += len(payload)
                    _write_frame(conn, 0x2, payload)
                    with self.lock:
                        self.echoed_bytes += len(payload)
                    n = len(payload) // 2
                    if n:
                        samples = struct.unpack("<%dh" % n, payload[: n * 2])
                        peak = max(abs(s) for s in samples)
                        print(f"[bridge] PCM {len(payload)} bytes "
                              f"(~{len(payload) * 1000 // (2 * self.sample_rate)} ms, "
                              f"peak {peak}) → echoed back")
        except (OSError, struct.error):
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass

    @staticmethod
    def _handshake(sock):
        f = sock.makefile("rb", 0)
        key = None
        try:
            start = f.readline().decode("latin-1").strip()
            while True:
                line = f.readline().decode("latin-1").strip()
                if not line:
                    break
                if line.lower().startswith("sec-websocket-key:"):
                    key = line.split(":", 1)[1].strip()
        finally:
            f.close()
        if not start.startswith("GET ") or not key:
            return None
        accept = base64.b64encode(
            hashlib.sha1((key + WS_GUID).encode("ascii")).digest()
        ).decode("ascii")
        sock.sendall((
            "HTTP/1.1 101 Switching Protocols\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
        ).encode("ascii"))
        return accept


# ---------------------------------------------------------------------------
# Minimal RWI WebSocket client
# ---------------------------------------------------------------------------

class RwiClient:
    """Blocking RWI client: request/response by action_id + event tap."""

    def __init__(self, url, token, timeout=10.0):
        parts = urlsplit(url)
        host, port = parts.hostname, parts.port or (443 if parts.scheme == "wss" else 80)
        self.sock = socket.create_connection((host, port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        path = parts.path or "/rwi/v1"
        self.sock.sendall((
            f"GET {path}?token={token} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            f"Sec-WebSocket-Protocol: rwi-v1\r\n\r\n"
        ).encode("ascii"))
        f = self.sock.makefile("rb", 0)
        status = f.readline().decode("latin-1").strip()
        while True:
            line = f.readline().decode("latin-1").strip()
            if not line:
                break
        if "101" not in status:
            raise ConnectionError(f"RWI upgrade failed: {status}")

        self.events = []
        self._pending = {}
        self._msg_id = 0
        self._lock = threading.Lock()
        self._rx = threading.Thread(target=self._recv_loop, daemon=True)
        self._rx.start()

    def _recv_loop(self):
        while True:
            try:
                frame = _read_frame(self.sock)
            except (OSError, struct.error):
                break
            if frame is None:
                break
            opcode, payload = frame
            if opcode == 0x8:
                break
            if opcode != 0x1:
                continue
            try:
                data = json.loads(payload.decode("utf-8"))
            except (json.JSONDecodeError, UnicodeDecodeError):
                continue
            if not isinstance(data, dict):
                continue
            aid = data.get("action_id")
            if aid:
                with self._lock:
                    entry = self._pending.pop(aid, None)
                if entry is not None:
                    entry[0].set()
                    entry[1].append(data)
                    continue
            self.events.append(data)
            et = data.get("event_type") or data.get("type")
            if et:
                print(f"[rwi] event: {et}")

    def send(self, action, params=None, timeout=15.0):
        self._msg_id += 1
        aid = f"demo-{self._msg_id}"
        req = {"rwi": "1.0", "action_id": aid, "action": action,
               "params": params or {}}
        entry = (threading.Event(), [])
        with self._lock:
            self._pending[aid] = entry
        _write_frame(self.sock, 0x1, json.dumps(req).encode("utf-8"), mask=True)
        if not entry[0].wait(timeout):
            with self._lock:
                self._pending.pop(aid, None)
            raise TimeoutError(f"RWI {action} timed out after {timeout}s")
        return entry[1][0]

    def wait_event(self, event_type, timeout=15.0):
        """Wait until an event of this type has arrived. Scans the full
        backlog so events that arrived between wait_event calls are found."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            for ev in self.events:
                if (ev.get("event_type") or ev.get("type")) == event_type:
                    return ev
            time.sleep(0.05)
        return None

    def close(self):
        try:
            _write_frame(self.sock, 0x8, b"", mask=True)
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# Demo flow
# ---------------------------------------------------------------------------

def run_demo(args):
    bridge = BridgeEchoServer(port=args.bridge_port,
                              sample_rate=args.sample_rate)
    bridge.start()
    bridge.served.wait(5)
    print(f"[demo] bridge echo server on {bridge.ws_url}")

    rwi = RwiClient(args.rwi, args.token)
    print(f"[demo] RWI connected: {args.rwi}")
    try:
        resp = rwi.send("session.subscribe", {"contexts": ["default"]})
        print(f"[demo] subscribe: {resp.get('status')}")

        call_id = f"ob-demo-{uuid.uuid4().hex[:8]}"

        # 1. Originate with an inline record option (auto-start on answer).
        resp = rwi.send("call.originate", {
            "call_id": call_id,
            "destination": args.dest,
            "caller_id": args.caller,
            "context": "default",
            "timeout_secs": args.ring_timeout,
            "record": {"mode": "mixed", "beep": False, "storage": {"path": ""}},
        })
        print(f"[demo] originate: {resp.get('status')} call_id={call_id}")
        if resp.get("status") != "success":
            print(json.dumps(resp, indent=2, ensure_ascii=False))
            return 1

        ev = rwi.wait_event("call_answered", timeout=args.ring_timeout + 10)
        print(f"[demo] call_answered: {bool(ev)}")
        if not ev:
            return 1
        rec = rwi.wait_event("record_started", timeout=5)
        print(f"[demo] record_started: {bool(rec)}")

        # 2. Transfer the caller leg to the local voip_bridge echo endpoint.
        target = f"voip_bridge:{bridge.ws_url}?samplerate={args.sample_rate}"
        resp = rwi.send("call.transfer", {"call_id": call_id, "target": target})
        print(f"[demo] transfer → voip_bridge: {resp.get('status')}")
        if resp.get("status") != "success":
            print(json.dumps(resp, indent=2, ensure_ascii=False))
            return 1

        # 3. Let audio flow through the bridge (echo round-trip).
        deadline = time.time() + args.bridge_secs
        while time.time() < deadline:
            time.sleep(1.0)
            s = bridge.stats()
            print(f"[demo] bridge stats: {s}")
            if s["pcm_bytes"] >= args.min_pcm_bytes:
                break
        s = bridge.stats()
        print(f"[demo] final bridge stats: {s}")
        if s["pcm_bytes"] < args.min_pcm_bytes:
            print("[demo] WARN: little/no PCM received from the call side")

        # 4. Hang up and observe the recording chain finish.
        rwi.send("call.hangup", {"call_id": call_id})
        for et in ("call_hangup", "record_stopped",
                   "recording_metadata_available", "record_end"):
            ev = rwi.wait_event(et, timeout=15)
            print(f"[demo] {et}: {bool(ev)}")
            if ev:
                print("      " + json.dumps(ev, ensure_ascii=False)[:220])
        return 0
    finally:
        rwi.close()
        bridge.stop()


def self_test():
    """Frame codec round-trip over a real socketpair (masked client → server)."""
    a, b = socket.socketpair()
    payload = struct.pack("<4h", 100, -200, 300, -400)
    _write_frame(a, 0x2, payload, mask=True)          # client → server (masked)
    got = _read_frame(b)
    assert got == (0x2, payload), got
    _write_frame(b, 0x1, b'{"type":"dtmf","digit":"1"}')  # server → client
    got = _read_frame(a)
    assert got[0] == 0x1 and json.loads(got[1])["digit"] == "1"
    a.close()
    b.close()
    print("self-test OK")
    return 0


class FrameTests(unittest.TestCase):
    def test_masked_roundtrip(self):
        a, b = socket.socketpair()
        try:
            for op, data in ((0x1, b"hello"), (0x2, b"\x01\x02\x03"),
                             (0x2, bytes(200))):   # 16-bit extended length
                _write_frame(a, op, data, mask=True)
                self.assertEqual(_read_frame(b), (op, data))
        finally:
            a.close()
            b.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[1])
    ap.add_argument("--rwi", default="ws://127.0.0.1:18080/rwi/v1")
    ap.add_argument("--token", default="test-api-key-e2e")
    ap.add_argument("--dest", default="sip:1002@127.0.0.1")
    ap.add_argument("--caller", default="sip:demo@127.0.0.1")
    ap.add_argument("--bridge-port", type=int, default=9101)
    ap.add_argument("--sample-rate", type=int, default=8000)
    ap.add_argument("--ring-timeout", type=int, default=30)
    ap.add_argument("--bridge-secs", type=int, default=8,
                    help="max seconds to run the bridge echo")
    ap.add_argument("--min-pcm-bytes", type=int, default=3200,
                    help="stop the bridge once this many PCM bytes arrived")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()
    if args.self_test:
        return self_test()
    return run_demo(args)


if __name__ == "__main__":
    sys.exit(main())
