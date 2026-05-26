#! /usr/bin/env python3
"""RWI Event Webhook — stdlib only (http.server, json).

Receives RWI webhook POST events from rustpbx and dumps them to stdout
and a timestamped JSON-lines file.

Run:
    python3 examples/rwi_event_webhook.py [port]

rustpbx config (in config.toml):
    [rwi_webhook]
    url = "http://localhost:8080/rwi/event"
    events = []           # empty = receive all events
    # timeout_ms = 5000
    # headers = { Authorization = "Bearer my-token" }

Test with the built-in test endpoint:
    curl -X POST http://localhost:8080/rwi/event \
         -H "Content-Type: application/json" \
         -d '{"rwi":"1.0","sequence":1,"timestamp":1700000000,"call_id":"test","event_type":"call_ringing","event":{"call_ringing":{"call_id":"test","context":{}}}}'
"""

import json
import sys
import time
from datetime import datetime, timezone
from http.server import HTTPServer, BaseHTTPRequestHandler

try:
    PORT = int(sys.argv[1])
except (IndexError, ValueError):
    PORT = 8080

OUTFILE = f"rwi_events_{datetime.now(timezone.utc).strftime('%Y%m%d_%H%M%S')}.jsonl"


class RwiEventHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self._read_body()
        if body is None:
            self._send_json(400, {"error": "invalid_json"})
            return

        event_type = body.get("event_type", "unknown")
        call_id = body.get("call_id", "")
        sequence = body.get("sequence", 0)

        line = json.dumps(body, ensure_ascii=False)
        ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")

        sys.stderr.write(
            f"[RWI Webhook] [{ts}] #{sequence} {event_type} call_id={call_id}\n"
        )
        print(line, flush=True)

        with open(OUTFILE, "a") as f:
            f.write(line + "\n")

        self._send_json(200, {"status": "ok"})

    def do_GET(self):
        if self.path == "/health":
            self._send_json(200, {"status": "ok"})
            return
        self._send_json(404, {"error": "not_found"})

    def _read_body(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length > 0 else b"{}"
        try:
            return json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            return None

    def _send_json(self, status, data):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode("utf-8"))

    def log_message(self, format, *args):
        sys.stderr.write(f"[RWI Webhook] {args[0]} {args[1]} {args[2]}\n")


def main():
    server = HTTPServer(("", PORT), RwiEventHandler)
    print(f"[RWI Webhook] Listening on http://0.0.0.0:{PORT}")
    print(f"[RWI Webhook] Dumping events to {OUTFILE}")
    print(f"")
    print(f"  Configure rustpbx with:")
    print(f"    [rwi_webhook]")
    print(f"    url = \"http://localhost:{PORT}/rwi/event\"")
    print(f"    events = []")
    print(f"")
    print(f"  Test:")
    print(f"    curl -X POST http://localhost:{PORT}/rwi/event \\")
    print(f"      -H 'Content-Type: application/json' \\")
    print(f"      -d '{{\"rwi\":\"1.0\",\"sequence\":1,\"timestamp\":1700000000,\"call_id\":\"test\",\"event_type\":\"call_ringing\",\"event\":{{\"call_ringing\":{{\"call_id\":\"test\",\"context\":{{}}}}}}}}'")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[RWI Webhook] Shutting down...")
        server.server_close()


if __name__ == "__main__":
    main()
