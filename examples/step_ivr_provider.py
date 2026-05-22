#! /usr/bin/env python3
"""Step-by-step IVR Provider — stdlib only (http.server, json, threading).

Demonstrates the external provider protocol for rustpbx step-mode IVR.

Run:
    python3 examples/step_ivr_provider.py [port]

Test:
    curl -X POST http://localhost:8080/ivr/step \\
         -H "Content-Type: application/json" \\
         -d '{"session_id":"test","event":{"type":"session_start"},"caller":"1001","callee":"2000"}'
"""

import json
import sys
import time
from http.server import HTTPServer, BaseHTTPRequestHandler
from threading import Lock

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8080

# ── Session Manager ──────────────────────────────────────────────────────────

class IvrSession:
    """Per-call state machine. rustpbx calls POST /ivr/step on each event,
    we return the next ActionNode."""

    def __init__(self, caller, callee):
        self.caller = caller
        self.callee = callee
        self._step = "start"
        self._menu_retries = 0

    def next_action(self, event):
        """Determine next ActionNode based on current step + event."""
        ev_type = event.get("type", "")

        # ── Step 1: initial greeting ──
        if self._step == "start":
            self._step = "wait_menu"
            return action_prompt(
                "sounds/ivr/welcome.wav",
                interruptible=True,
                next=action_dtmf_menu(
                    greeting="sounds/ivr/menu.wav",
                    entries={
                        "1": action_transfer("2001"),
                        "2": action_queue("support"),
                        "0": action_transfer("operator"),
                    },
                    timeout_action=action_prompt(
                        "sounds/ivr/timed_out.wav",
                        next=action_repeat(),
                    ),
                    invalid_action=action_prompt(
                        "sounds/ivr/invalid.wav",
                        next=action_repeat(),
                    ),
                ),
            )

        # ── Step 2: menu DTMF processing ──
        if self._step == "wait_menu":
            if ev_type == "dtmf":
                digit = event.get("digit", "")
                if digit == "1":
                    return action_transfer("2001")
                elif digit == "2":
                    return action_queue("support")
                elif digit == "0":
                    return action_transfer("operator")
                else:
                    self._menu_retries += 1
                    if self._menu_retries >= 3:
                        return action_hangup()
                    return action_prompt(
                        "sounds/ivr/invalid.wav",
                        next=action_repeat(),
                    )
            elif ev_type == "dtmf_timeout":
                return action_prompt(
                    "sounds/ivr/timed_out.wav",
                    next=action_repeat(),
                )

        # ── Unknown step / event: hangup ──
        return action_hangup()


# ── ActionNode builders ──────────────────────────────────────────────────────

def action_prompt(file, tts_text=None, interruptible=False, next=None):
    node = {"type": "prompt", "file": file, "interruptible": interruptible}
    if tts_text:
        node["tts_text"] = tts_text
        del node["file"]
    if next is not None:
        node["next"] = next
    return node


def action_dtmf_menu(entries, greeting="sounds/ivr/menu.wav",
                     timeout_action=None, invalid_action=None,
                     timeout_ms=5000, max_retries=3):
    node = {
        "type": "dtmf_menu",
        "greeting": greeting,
        "timeout_ms": timeout_ms,
        "max_retries": max_retries,
        "entries": entries,
    }
    if timeout_action is not None:
        node["timeout_action"] = timeout_action
    if invalid_action is not None:
        node["invalid_action"] = invalid_action
    return node


def action_transfer(target):
    return {"type": "transfer", "target": target}


def action_queue(target):
    return {"type": "queue", "target": target}


def action_hangup():
    return {"type": "hangup"}


def action_repeat():
    return {"type": "repeat"}


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

    # ── Session endpoints ──────────────────────────────────────────────

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

        # If session not found, create a new one
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

    # ── Helpers ────────────────────────────────────────────────────────

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
        # Quiet logging: prefix with [IVR Provider]
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


if __name__ == "__main__":
    main()
