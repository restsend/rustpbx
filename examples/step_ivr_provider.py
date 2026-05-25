#! /usr/bin/env python3
"""Step-by-step IVR Provider — stdlib only (http.server, json, threading).

Demonstrates the external provider protocol for rustpbx step-mode IVR.
Supports: TTS welcome prompt, get current time (press 1), transfer to
human agent (press 2).

Run:
    python3 examples/step_ivr_provider.py [port]

Test:
    curl -X POST http://localhost:8080/ivr/step \
         -H "Content-Type: application/json" \
         -d '{"session_id":"test","event":{"type":"session_start"},"caller":"1001","callee":"2000"}'

Unit tests:
    python3 -m unittest examples/step_ivr_provider.py
"""

import json
import sys
import time
from http.server import HTTPServer, BaseHTTPRequestHandler
from threading import Lock

try:
    PORT = int(sys.argv[1])
except (IndexError, ValueError):
    PORT = 8080

# ── Session Manager ──────────────────────────────────────────────────────────

WELCOME_TEXT = "IVR step, press 1 to get current time, press 2 to transfer to a human agent"


class IvrSession:
    """Per-call state machine. rustpbx calls POST /ivr/step on each event,
    we return the next ActionNode."""

    def __init__(self, caller, callee):
        self.caller = caller
        self.callee = callee
        self._step = "start"
        self._menu_retries = 0

    def next_action(self, event):
        ev_type = event.get("type", "")

        if self._step == "start":
            self._step = "wait_menu"
            return action_prompt(
                tts_text=WELCOME_TEXT,
                interruptible=True,
            )

        if self._step == "wait_menu":
            if ev_type == "dtmf":
                digit = event.get("digit", "")
                if digit == "1":
                    now = time.strftime("%Y-%m-%d %H:%M:%S")
                    return action_prompt(
                        tts_text=f"The current time is {now}",
                    )
                elif digit == "2":
                    return action_transfer("agent")
                else:
                    self._menu_retries += 1
                    if self._menu_retries >= 3:
                        return action_hangup()
                    return action_prompt(
                        tts_text="Invalid option. " + WELCOME_TEXT,
                        interruptible=True,
                    )
            elif ev_type == "dtmf_timeout":
                return action_hangup()
            elif ev_type == "audio_complete":
                return action_prompt(
                    tts_text=WELCOME_TEXT,
                    interruptible=True,
                )

        return action_hangup()


# ── ActionNode builders ──────────────────────────────────────────────────────


def action_prompt(file=None, tts_text=None, interruptible=False, next=None):
    node = {"type": "prompt", "interruptible": interruptible}
    if tts_text:
        node["tts_text"] = tts_text
    if file:
        node["file"] = file
    if next is not None:
        node["next"] = next
    return node


def action_transfer(target):
    return {"type": "transfer", "target": target}


def action_hangup():
    return {"type": "hangup"}


# ── HTTP Server ──────────────────────────────────────────────────────────────


class IvrStepHandler(BaseHTTPRequestHandler):
    """Handle POST /ivr/step — the main provider endpoint."""

    sessions = {}
    sessions_lock = Lock()

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
        """POST /ivr/step/start — called when a new call enters the IVR."""
        body = self._read_body()
        session_id = body.get("session_id", "")
        caller = body.get("caller", "")
        callee = body.get("callee", "")
        with self.sessions_lock:
            self.sessions[session_id] = IvrSession(caller, callee)
        self._send_json(200, {"status": "ok"})

    def _handle_end(self):
        """POST /ivr/step/end — called when the IVR session ends."""
        body = self._read_body()
        session_id = body.get("session_id", "")
        with self.sessions_lock:
            self.sessions.pop(session_id, None)
        self._send_json(200, {"status": "ok"})

    def _handle_step(self):
        """POST /ivr/step — called on each IVR step."""
        body = self._read_body()
        session_id = body.get("session_id", "")

        with self.sessions_lock:
            session = self.sessions.get(session_id)

        if session is None:
            session = IvrSession(
                body.get("caller", ""),
                body.get("callee", ""),
            )
            with self.sessions_lock:
                self.sessions[session_id] = session

        event = body.get("event", {"type": "session_start"})
        node = session.next_action(event)
        self._send_json(200, node)

    def _read_body(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length > 0 else b"{}"
        return json.loads(raw) if raw else {}

    def _send_json(self, status, data):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode("utf-8"))

    def log_message(self, format, *args):
        sys.stderr.write(f"[IVR Provider] {args[0]} {args[1]} {args[2]}\n")


# ── Server Start ─────────────────────────────────────────────────────────────


def main():
    server = HTTPServer(("", PORT), IvrStepHandler)
    print(f"[IVR Provider] Running on http://0.0.0.0:{PORT}")
    print(f"[IVR Provider] Endpoints:")
    print(f"  POST /ivr/step       — main provider endpoint")
    print(f"  POST /ivr/step/start — session start notification")
    print(f"  POST /ivr/step/end   — session end notification")
    print(f"")
    print(f"  Test with:")
    print(f"    curl -X POST http://localhost:{PORT}/ivr/step \\")
    print(f"      -H 'Content-Type: application/json' \\")
    print(f"      -d '{{\"session_id\":\"call_001\",\"event\":{{\"type\":\"session_start\"}},\"caller\":\"1001\",\"callee\":\"2000\"}}'")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[IVR Provider] Shutting down...")
        server.server_close()


# ── Unit Tests ────────────────────────────────────────────────────────────────


import unittest


class TestIvrSession(unittest.TestCase):

    def setUp(self):
        self.sess = IvrSession("1001", "2000")

    def test_start_returns_interruptible_prompt(self):
        node = self.sess.next_action({"type": "session_start"})
        self.assertEqual(node["type"], "prompt")
        self.assertTrue(node["interruptible"])
        self.assertIn("IVR step", node.get("tts_text", ""))

    def test_dtmf_1_returns_time_prompt(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf", "digit": "1"})
        self.assertEqual(node["type"], "prompt")
        self.assertIn("current time", node.get("tts_text", ""))

    def test_dtmf_2_returns_transfer(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf", "digit": "2"})
        self.assertEqual(node["type"], "transfer")
        self.assertEqual(node["target"], "agent")

    def test_invalid_digit_returns_prompt(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf", "digit": "9"})
        self.assertEqual(node["type"], "prompt")
        self.assertTrue(node["interruptible"])
        self.assertIn("Invalid option", node.get("tts_text", ""))

    def test_invalid_digit_three_times_hangup(self):
        self.sess.next_action({"type": "session_start"})
        for _ in range(3):
            node = self.sess.next_action({"type": "dtmf", "digit": "9"})
        self.assertEqual(node["type"], "hangup")

    def test_dtmf_timeout_returns_hangup(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "dtmf_timeout"})
        self.assertEqual(node["type"], "hangup")

    def test_audio_complete_returns_welcome_prompt(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "audio_complete"})
        self.assertEqual(node["type"], "prompt")
        self.assertTrue(node["interruptible"])

    def test_unknown_event_hangup(self):
        self.sess.next_action({"type": "session_start"})
        node = self.sess.next_action({"type": "unknown_event"})
        self.assertEqual(node["type"], "hangup")


if __name__ == "__main__":
    main()
