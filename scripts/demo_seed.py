#!/usr/bin/env python3
"""Seed demo client-telemetry data + produce UI screenshots for the
releases page "客户端上报" tab.

Run against a fresh server started from the verify worktree.
"""
import gzip
import json
import os
import sqlite3
import sys
import time
import urllib.parse
import urllib.request

BASE = os.environ.get("TELEMETRY_BASE", "http://127.0.0.1:8099")
WORKTREE = os.environ.get(
    "WORKTREE",
    "/private/var/folders/yr/3_mgdw995s7bb4grvcmsk1d00000gn/T/opencode/rustpbx-verify",
)
DB = os.path.join(WORKTREE, "rustpbx-demo.sqlite3")

# ── 1. simulate live client reports via POST /api/report ────────────────

CONTEXT = {
    "agent_id": "1001",
    "account_name": "前台-张三",
    "auth": "digest",
    "sip_user": "1001",
    "registrar_host": "pbx.example.com",
    "media": "audio",
    "transport": "wss",
    "remote_uri": "sip:1002@pbx.example.com",
    "in_call": False,
    "safe_mode": False,
    "version": "0.5.1",
    "build": "2026-10-18",
    "build_time": "20261018101200",
    "git_commit": "4ff67af0",
    "os": "macos",
    "arch": "aarch64",
    "hostname": "zhangsan-mbp",
    "mac": "aa:bb:cc:dd:ee:01",
    "cpu_cores": "10",
    "mem_total_mb": "16384",
    "mem_avail_mb": "4096",
    "cti_host": "pbx.example.com",
}


def context_prefix(call_id=None, note=None):
    ctx = dict(CONTEXT)
    ctx["call_id"] = call_id or ""
    summary = json.dumps(ctx, indent=2, ensure_ascii=False)
    head = f"--- restsend context ---\n{summary}\n--- end context ---\n\n"
    if note:
        head += f"note: {note}\n\n"
    return head


def report(event, kind="task", call_id=None, note=None, log_lines=None, version=None):
    body = context_prefix(call_id, note)
    body += "\n".join(log_lines or ["2026-10-20 10:00:00 INFO sample log line"])
    gz = gzip.compress(body.encode())
    params = {
        "kind": kind,
        "event": event,
        "version": version or CONTEXT["version"],
        "build": CONTEXT["build"],
        "build_time": CONTEXT["build_time"],
        "git_commit": CONTEXT["git_commit"],
        "os": CONTEXT["os"],
        "arch": CONTEXT["arch"],
        "hostname": CONTEXT["hostname"],
        "mac": CONTEXT["mac"],
        "cpu_cores": CONTEXT["cpu_cores"],
        "mem_total_mb": CONTEXT["mem_total_mb"],
        "mem_avail_mb": CONTEXT["mem_avail_mb"],
        "agent_id": CONTEXT["agent_id"],
        "account_name": CONTEXT["account_name"],
        "auth": CONTEXT["auth"],
        "media": CONTEXT["media"],
        "transport": CONTEXT["transport"],
        "sip_user": CONTEXT["sip_user"],
        "registrar": CONTEXT["registrar_host"],
        "ts": str(int(time.time())),
    }
    if call_id:
        params["call_id"] = call_id
    if note:
        params["note"] = note
    qs = "&".join(f"{k}={urllib.parse.quote(str(v))}" for k, v in params.items())
    req = urllib.request.Request(
        f"{BASE}/api/report?{qs}", data=gz,
        headers={"Content-Type": "application/gzip"}, method="POST",
    )
    with urllib.request.urlopen(req, timeout=10) as resp:
        out = json.loads(resp.read())
        assert out.get("ok"), out
        return out


print("POST /api/report smoke ...")
report("diag_run", kind="diag", note="mini 快设诊断")
report("panic", kind="crash_day", call_id="cid-demo-001", note="panic: called `Option::unwrap()` on a `None` value",
       log_lines=[
           "2026-10-20 09:59:58 INFO call established cid=cid-demo-001",
           "2026-10-20 09:59:59 DEBUG rtp stream up",
           "2026-10-20 10:00:01 ERROR panic: called `Option::unwrap()` on a `None` value",
           "stack backtrace:",
           "   0: pc_engine::media::on_packet",
           "   1: pc_engine::run",
       ])
report("link_lost", note="ws closed 1006")
report("update_check", kind="task") if False else None
print("smoke ok")

# ── 2. seed richer history directly into sqlite (7 days × events) ───────

import urllib.parse  # noqa: E402
print("seeding history into", DB)
conn = sqlite3.connect(DB)
cur = conn.cursor()
cur.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='rustpbx_runtime_snapshots'")
assert cur.fetchone(), "snapshots table missing — server did not migrate?"

EVENTS = [
    # (event, weight, abnormal)
    ("update_check", 40, False),
    ("diag_run", 6, False),
    ("update_success", 8, False),
    ("update_progress", 10, False),
    ("manual_upload", 2, False),
    ("call_abnormal", 7, True),
    ("link_lost", 5, True),
    ("login_fail", 3, True),
    ("dial_fail", 2, True),
    ("update_download_fail", 2, True),
    ("panic", 1, True),
]
VERSIONS = [("0.5.1", 0.55), ("0.5.0", 0.25), ("0.4.3", 0.15), ("0.3.9", 0.05)]
AGENTS = ["1001", "1002", "1003", "1007", "1010"]
OSES = ["windows", "macos", "windows", "windows", "macos"]

import random
random.seed(42)
now = int(time.time())
rows = []
for day in range(6, -1, -1):
    base = now - day * 86400
    for event, weight, abnormal in EVENTS:
        # abnormal events get more frequent on older versions & recent days
        n = weight + random.randint(0, 3)
        if abnormal and day <= 2:
            n += weight // 2
        for _ in range(n):
            ver = random.choices([v for v, _ in VERSIONS], [w for _, w in VERSIONS])[0]
            i = random.randrange(len(AGENTS))
            ts = base + random.randint(8 * 3600, 20 * 3600)
            created = time.strftime('%Y-%m-%dT%H:%M:%S', time.gmtime(ts)) + f'.{random.randint(100000, 999999)}+00:00'
            data = {
                "kind": "task",
                "os": OSES[i],
                "arch": "x86_64" if OSES[i] == "windows" else "aarch64",
                "agent_id": AGENTS[i],
                "sip_user": AGENTS[i],
                "account_name": f"坐席-{AGENTS[i]}",
                "registrar": "pbx.example.com",
                "transport": "wss",
                "note": None,
                "ts": ts,
                "ip": f"192.168.1.{10 + i}",
                "storage_key": None,
                "body_preview": f"sample {event} log body\n2026-10-20 10:00:00 INFO demo",
                "body_truncated": False,
                "body_is_gzip": True,
                "received_by": None,
            }
            rows.append((
                f"cr:{event}",            # kind
                created,                  # created_at
                4096 + random.randint(0, 100000),  # num1 = body bytes
                1 if random.random() < 0.2 else 0, # num2 = in_call
                0,                        # num3 = safe_mode
                0.0,                      # num4
                json.dumps(data, ensure_ascii=False),  # data
                ver,                      # instance_id = version
            ))
random.shuffle(rows)
cur.executemany(
    """INSERT INTO rustpbx_runtime_snapshots
       (kind, created_at, num1, num2, num3, num4, data, instance_id)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
    rows,
)
conn.commit()
cur.execute("SELECT COUNT(*) FROM rustpbx_runtime_snapshots WHERE kind LIKE 'cr:%'")
print("seeded cr: rows:", cur.fetchone()[0])
conn.close()
print("done")
