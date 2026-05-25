#! /usr/bin/env python3
"""JSON-RPC Router — upstream HTTP server for rustpbx SBC JSON-RPC rules.

Demonstrates:
  - Receiving a JSON-RPC call routing request from rustpbx
  - Parsing the MiniJinja-rendered body (call.callee, call.caller, etc.)
  - Applying simple number translation / rewrite logic
  - Returning a JSON response that rustpbx evaluates via success_when

Usage:
  python3 examples/jsonrpc_router.py [port]

Corresponding rustpbx JSON-RPC config (sbc_jsonrpc.toml):
  enabled = true
  timeout_ms = 5000

  [[rules]]
  name = "number_translate"
  enabled = true

  [rules.match_group]
  logic = "all"

  [[rules.match_group.conditions]]
  field = "callee"
  op = "starts_with"
  value = "0086"

  [rules.upstream]
  method = "POST"
  url = "http://localhost:9001/route"
  body = '{"number":"{{ call.callee }}","caller":"{{ call.caller }}"}'

  [rules.upstream.headers]

  [rules.response]
  success_when = "json.code == 0"
  callee_rewrite = "sip:{{ json.rewritten_number }}@proxy.example.com"
  caller_rewrite = "sip:{{ json.rewritten_caller }}@proxy.example.com"
  reject_status = 403
  reject_reason = ""
  passthrough_original_headers = true
  inject_headers = []

Test with curl (simulating what rustpbx sends after MiniJinja rendering):
  curl -X POST http://localhost:9001/route \
    -H 'Content-Type: application/json' \
    -d '{"number":"008613800138000","caller":"sip:1001@pbx.example.com"}'
"""

import json
import re
import sys
from http.server import HTTPServer, BaseHTTPRequestHandler

try:
    PORT = int(sys.argv[1])
except (IndexError, ValueError):
    PORT = 9001

# ── Number Translation Rules ──────────────────────────────────────────────
# Maps country code prefixes to E.164 transformations

TRANSLATION_RULES = [
    # China: 0086 → +86, strip leading 0 after country code
    {"prefix": "0086", "country": "+86", "strip_zeros": True},
    # US: 001 → +1
    {"prefix": "001", "country": "+1", "strip_zeros": False},
    # UK: 0044 → +44
    {"prefix": "0044", "country": "+44", "strip_zeros": False},
]


def translate_number(number: str) -> dict:
    """Translate an international number to E.164 format.

    Returns a dict with routing instructions for rustpbx.
    """
    number = number.strip()

    for rule in TRANSLATION_RULES:
        if number.startswith(rule["prefix"]):
            local = number[len(rule["prefix"]):]
            if rule["strip_zeros"] and local.startswith("0"):
                local = local[1:]
            e164 = f"{rule['country']}{local}"
            return {
                "code": 0,
                "translated": True,
                "rewritten_number": e164,
                "rewritten_caller": f"+86{e164}",
                "country": rule["country"],
                "local_number": local,
            }

    # No match — reject
    return {
        "code": 1,
        "translated": False,
        "rewritten_number": number,
        "rewritten_caller": "",
        "country": "",
        "local_number": "",
        "error": "unsupported_prefix",
    }


# ── HTTP Handler ──────────────────────────────────────────────────────────


class RouterHandler(BaseHTTPRequestHandler):

    def do_POST(self):
        path = self.path.rstrip("/")

        if path == "/route":
            self._handle_route()
        elif path == "/health":
            self._send_json(200, {"status": "ok"})
        else:
            self._send_json(404, {"error": "not_found", "path": path})

    def _handle_route(self):
        """POST /route — main routing endpoint called by rustpbx."""
        body = self._read_body()

        number = body.get("number", "")
        caller = body.get("caller", "")

        result = translate_number(number)

        if result["code"] == 0:
            print(f"  [ROUTE] {caller} → {number}")
            print(f"     ↳ translated to {result['rewritten_number']} ({result['country']})")
        else:
            print(f"  [ROUTE] {caller} → {number} — REJECTED ({result.get('error')})")

        self._send_json(200, {
            "code": result["code"],
            "translated": result["translated"],
            "rewritten_number": result["rewritten_number"],
            "rewritten_caller": result["rewritten_caller"],
            "country": result["country"],
            "local_number": result["local_number"],
            "error": result.get("error"),
        })

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
        sys.stderr.write(f"[JSON-RPC Router] {args[0]} {args[1]} {args[2]}\n")


# ── Main ──────────────────────────────────────────────────────────────────


def main():
    server = HTTPServer(("", PORT), RouterHandler)
    print(f"[JSON-RPC Router] Running on http://0.0.0.0:{PORT}")
    print(f"[JSON-RPC Router] Endpoint: POST /route")
    print()
    print(f"  Simulate a call routing request:")
    print(f"    curl -X POST http://localhost:{PORT}/route \\")
    print(f"      -H 'Content-Type: application/json' \\")
    print(f"      -d '{{\"number\":\"008613800138000\",\"caller\":\"sip:1001@pbx.example.com\"}}'")
    print()
    print(f"  With matching rustpbx JSON-RPC config, the result will:")
    print(f"    - Rewrite callee to sip:{chr(36)}{{ json.rewritten_number }}@proxy.example.com")
    print(f"    - Rewrite caller to sip:{chr(36)}{{ json.rewritten_caller }}@proxy.example.com")
    print(f"    - Condition json.code == 0 → accept (else reject with 403)")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[JSON-RPC Router] Shutting down...")
        server.server_close()


# ── Unit Tests ────────────────────────────────────────────────────────────

import unittest


class TestTranslateNumber(unittest.TestCase):

    def test_china_mobile(self):
        r = translate_number("008613800138000")
        self.assertEqual(r["code"], 0)
        self.assertEqual(r["rewritten_number"], "+8613800138000")
        self.assertTrue(r["translated"])

    def test_china_landline_strip_zero(self):
        r = translate_number("00860215551234")
        self.assertEqual(r["code"], 0)
        self.assertEqual(r["rewritten_number"], "+86215551234")

    def test_us_number(self):
        r = translate_number("0012125550198")
        self.assertEqual(r["code"], 0)
        self.assertEqual(r["rewritten_number"], "+12125550198")

    def test_uk_number(self):
        r = translate_number("00442079461234")
        self.assertEqual(r["code"], 0)
        self.assertEqual(r["rewritten_number"], "+442079461234")

    def test_unsupported_prefix(self):
        r = translate_number("0090123456")
        self.assertEqual(r["code"], 1)
        self.assertFalse(r["translated"])
        self.assertIn("error", r)

    def test_empty_number(self):
        r = translate_number("")
        self.assertEqual(r["code"], 1)
        self.assertFalse(r["translated"])


class TestRouterHandler(unittest.TestCase):
    """Integration-style tests using the handler directly."""

    def setUp(self):
        self.handler = RouterHandler

    def test_translate_china(self):
        body = json.dumps({"number": "008613800138000", "caller": "sip:1001@pbx.com"})
        result = json.loads(body)
        r = translate_number(result["number"])
        self.assertEqual(r["code"], 0)
        self.assertEqual(r["rewritten_number"], "+8613800138000")

    def test_reject_unknown(self):
        result = json.loads('{"number":"999123","caller":"sip:u@h.com"}')
        r = translate_number(result["number"])
        self.assertEqual(r["code"], 1)


if __name__ == "__main__":
    main()
