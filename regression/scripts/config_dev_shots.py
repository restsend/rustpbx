#!/usr/bin/env python3
"""Verify the shipped `config.toml.dev` runs the full CC quick-route /
ACW / screen-pop / outbound-lines flow with restsend-call mini mode.

Uses the REAL repo config files:
  * `--conf config.toml.dev`  (users 1001/1002, console token `dev`, addon cc)
  * `config/cc/cc.toml`       (agents/skill-groups files + appended desk
                               section: ACW / screen-pop / quick-route codes)
  * `config/ivr/main.toml`    (IVR target for *82main)

The dev sqlite DB is backed up before the run and restored afterwards.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
import sys
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

RUSTPBX = Path("/Users/pi/workspace/rs/rustpbx")
RESTSEND = Path("/Users/pi/workspace/rs/restsend-call")
PC_BIN = RESTSEND / "target/debug/restsend-pc"
OUT = RUSTPBX / "screenshots/cc-dev-shots"
TMP = Path("/tmp/opencode/cc-dev-shots")
CONF = RUSTPBX / "config.toml.dev"
DB = RUSTPBX / "rustpbx.sqlite3"

TOKEN = "dev"                      # [[console.api_tokens]] in config.toml.dev
PBX_HTTP = 8082                    # http_addr in config.toml.dev
PBX_SIP = 15060                    # udp_port in config.toml.dev
STATUS_PORT = 18259
MOCK_PORT = 18444
MOCK = f"http://127.0.0.1:{MOCK_PORT}/mockcrm"

MOCK_HITS: list[str] = []
LOCK = threading.Lock()


class MockCrm(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        with LOCK:
            MOCK_HITS.append(self.path)
        body = b"<html><body><h1>Mock CRM (config.toml.dev)</h1></body></html>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def wait_mock(prefix: str, timeout: float = 30.0) -> str | None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with LOCK:
            hits = [h for h in MOCK_HITS if h.startswith(prefix)]
        if hits:
            return hits[0]
        time.sleep(0.4)
    return None


async def seed(client) -> None:
    """Console-side configuration for agent 1001 (mirrors the console UI):
    SG General metadata (ACW + ringing popup), SG quick contacts (typed),
    agent extras (ACW + ringing popup), outbound policy (line rules)."""
    resp = await client.put("/api/cc/skill-groups/support", {
        "skill_group_id": "support",
        "display_name": "售后支持组",
        "skills_required": ["support"],
        "metadata": {
            "acw_url": f"{MOCK}/acw?sg=support&call=${{callId}}",
            "screen_pop_url": f"{MOCK}/pop?sg=support&caller=${{caller}}",
        },
        "extra": {
            "desk": {"type": "text", "value": json.dumps({"transfer": {"quickContacts": [
                {"agentId": "support", "type": "skill_group"},
                {"agentId": "main", "label": "主 IVR", "type": "ivr"},
                {"agentId": "1002", "label": "同事-李四", "number": "1002"},
            ]}})},
        },
    })
    assert resp.get("success") is True or resp.get("skill_group_id"), f"sg failed: {resp}"

    agent = await client.get("/api/cc/agents/1001")
    skills = agent.get("skills")
    if isinstance(skills, dict):
        skills = skills.get("list") or ["support", "sales"]
    resp = await client.put("/api/cc/agents/1001", {
        "display_name": agent.get("display_name") or "1001",
        "skills": skills or ["support", "sales"],
        "extra": {
            "acw_url": {"type": "text", "value": f"{MOCK}/acw?agent=1001&call=${{callId}}"},
            "screen_pop_url": {"type": "text",
                               "value": f"{MOCK}/pop?agent=1001&caller=${{caller}}"},
        },
    })
    assert resp.get("success") is True or resp.get("agent_id"), f"agent failed: {resp}"

    resp = await client.post("/api/cc/outbound-policies", {
        "name": "dev-lines",
        "policy": {"enabled": True, "lines": [
            {"id": "line-mobile", "label": "Mobile 外线", "prefix": "9",
             "caller": "self", "default": True, "allowed_prefixes": ["1"]},
            {"id": "line-ld", "label": "长途", "prefix": "0", "caller": "02188886666"},
        ]},
        "agents": ["1001"],
    })
    assert resp.get("success") is True, f"policy failed: {resp}"

    config = await client.get("/api/cc/agents/1001/config")
    desk = config.get("desk") or {}
    lines = desk.get("lines") or []
    assert [l.get("id") for l in lines] == ["line-mobile", "line-ld"], f"lines: {lines}"
    sp = desk.get("screenPop") or {}
    assert sp.get("autoOpenOnRinging") is True, f"autoOpenOnRinging not delivered: {sp}"
    print("[seed] config ok: policy lines + typed quick contacts delivered")


def write_client_env() -> None:
    shutil.rmtree(TMP / "home", ignore_errors=True)
    shutil.rmtree(TMP / "data", ignore_errors=True)
    (TMP / "home").mkdir(parents=True)
    (TMP / "data").mkdir(parents=True)
    OUT.mkdir(parents=True, exist_ok=True)
    TMP.mkdir(parents=True, exist_ok=True)
    (TMP / "desk.bootstrap.json").write_text(json.dumps({
        "version": 1,
        "base_url": f"http://127.0.0.1:{PBX_HTTP}/api",
        "auth": {"type": "bearer", "source": "static", "static_token": TOKEN},
        "login_presence": "idle",
    }, indent=2))
    prefs = {
        "accounts": [{
            "name": "分机 1001", "enabled": True, "auth": "password",
            "user": "1001",
            "local_uri": f"sip:1001@127.0.0.1:{PBX_SIP + 30}",
            "registrar": f"sip:127.0.0.1:{PBX_SIP}",
            "password": "123456", "media": "rtp", "transport": "udp",
            "ice_servers": "", "ice_relay_only": False, "ice_enabled": True,
        }],
        "active_account": 0,
        "update": {"url": "", "frequency": "never", "last_check": 0},
        "audio": {"input_device": "", "output_device": "", "ringtone_device": "",
                  "codecs": [{"id": i, "enabled": True} for i in range(5)]},
        "video": {"camera": "", "mock_file": ""},
        "alerts": {"ring_enabled": False, "connect_tone": False, "hangup_tone": False},
        "call": {"auto_answer": True, "auto_answer_secs": 2, "auto_acw": False, "dnd": False},
        "record": {"enabled": False},
        "enterprise": {"acw_enabled": True, "team_enabled": True,
                       "perf_enabled": True, "csat_enabled": True},
    }
    (TMP / "data" / "prefs.json").write_text(
        json.dumps(prefs, ensure_ascii=False, indent=2), encoding="utf-8")


def spawn_pc(name: str, extra: dict[str, str], args: list[str]) -> subprocess.Popen:
    env = os.environ.copy()
    env.update({
        "HOME": str(TMP / "home"),
        "RESTSEND_PC_DATA": str(TMP / "data"),
        "RESTSEND_DESK_CONFIG": str(TMP / "desk.bootstrap.json"),
        "RESTSEND_PC_DEVICE": "tone",
        "RESTSEND_PC_STATUS_PORT": str(STATUS_PORT),
        "RUST_LOG": "warn",
    })
    env.update(extra)
    bs = TMP / "data" / "boot_state.json"
    if bs.exists():
        bs.unlink()
    return subprocess.Popen([str(PC_BIN), *args], env=env, cwd=str(RESTSEND),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def wait_registered(timeout: float = 25.0) -> bool:
    deadline = time.time() + timeout
    url = f"http://127.0.0.1:{STATUS_PORT}/status"
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                if '"agent_id":"1001"' in r.read().decode():
                    return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def cmd_shot(name: str) -> Path:
    path = OUT / f"{name}.png"
    req = urllib.request.Request(
        f"http://127.0.0.1:{STATUS_PORT}/cmd/shot?path={path}", data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=5) as r:
        r.read()
    deadline = time.time() + 10
    while time.time() < deadline and not (path.exists() and path.stat().st_size > 10_000):
        time.sleep(0.3)
    ok = path.exists() and path.stat().st_size > 10_000
    print(f"[shot] {name}: {'ok' if ok else 'MISSING'}")
    return path if ok else None


async def main() -> int:
    if not PC_BIN.exists():
        print(f"missing {PC_BIN}")
        return 1

    TMP.mkdir(parents=True, exist_ok=True)
    backup = None
    if DB.exists():
        backup = TMP / "rustpbx.sqlite3.bak"
        shutil.copy2(DB, backup)
        print("[db] dev sqlite backed up")

    mock_srv = ThreadingHTTPServer(("127.0.0.1", MOCK_PORT), MockCrm)
    threading.Thread(target=mock_srv.serve_forever, daemon=True).start()

    server_log = open(TMP / "rustpbx.log", "w")
    proc = subprocess.Popen(
        [str(RUSTPBX / "target/debug/rustpbx-cc-e2e"), "--conf", str(CONF)],
        cwd=str(RUSTPBX), stdout=server_log, stderr=subprocess.STDOUT,
        preexec_fn=os.setsid,
    )
    try:
        # readiness
        deadline = time.time() + 60
        ready = False
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{PBX_HTTP}/healthz", timeout=2) as r:
                    if r.status < 500:
                        ready = True
                        break
            except Exception:
                pass
            time.sleep(0.5)
        assert ready, "rustpbx (config.toml.dev) did not become ready"
        print(f"[pbx] config.toml.dev up: http=:{PBX_HTTP} sip=:{PBX_SIP}")

        import aiohttp
        session = aiohttp.ClientSession()
        try:
            client = type("C", (), {})()
            client.http_url = f"http://127.0.0.1:{PBX_HTTP}"
            client._token = TOKEN

            async def _req(method, path, body=None):
                headers = {"Authorization": f"Bearer {TOKEN}",
                           "Content-Type": "application/json"}
                async with session.request(method, client.http_url + path,
                                           headers=headers,
                                           json=body) as r:
                    if r.status >= 400:
                        raise RuntimeError(f"{method} {path} -> {r.status}: {await r.text()}")
                    return json.loads(await r.text())

            client.get = lambda path: _req("GET", path)
            client.post = lambda path, body: _req("POST", path, body)
            client.put = lambda path, body: _req("PUT", path, body)
            await seed(client)
        finally:
            await session.close()

        write_client_env()

        # live mini flow: idle → inbound *81support → ringing pop → answer → ACW
        proc_pc = spawn_pc("live", {}, ["--mini"])
        assert wait_registered(30), "restsend-pc did not register"
        await asyncio.sleep(3)
        cmd_shot("mini-idle")

        caller_log = open(TMP / "caller.log", "w")
        caller = subprocess.Popen(
            ["/Users/pi/.cargo/bin/sipbot", "call",
             "-t", f"sip:*81support@127.0.0.1:{PBX_SIP}",
             "--username", "alice", "--password", "123456",
             "--codecs", "pcmu", "--hangup", "8", "-v",
             "-a", "127.0.0.1:25501"],
            stdout=caller_log, stderr=subprocess.STDOUT)
        pop = wait_mock("/mockcrm/pop?agent=1001", timeout=30)
        assert pop, f"ringing screen-pop URL not opened. hits={MOCK_HITS}"
        cmd_shot("mini-ringing")
        acw = wait_mock("/mockcrm/acw?agent=1001", timeout=30)
        assert acw, f"ACW URL not opened. hits={MOCK_HITS}"
        assert "sg=" not in acw, f"agent-level ACW must win: {acw}"
        time.sleep(1.5)
        cmd_shot("mini-acw")
        caller.terminate()

        # outbound via the policy line prefix: dial 9 + an allowed destination
        dial = f"http://127.0.0.1:{STATUS_PORT}/cmd/dialout?calleeno=91380013800"
        req = urllib.request.Request(dial, data=b"", method="POST")
        with urllib.request.urlopen(req, timeout=5) as r:
            r.read()
        await asyncio.sleep(4)
        cmd_shot("mini-outbound")
        proc_pc.terminate()
        time.sleep(1)
        if proc_pc.poll() is None:
            proc_pc.kill()

        print("\nmock CRM hits:")
        for hit in MOCK_HITS:
            print(f"  {hit}")
        return 0
    finally:
        try:
            os.killpg(os.getpgid(proc.pid), 15)
        except Exception:
            pass
        time.sleep(1)
        mock_srv.shutdown()
        if backup:
            shutil.copy2(backup, DB)
            print("[db] dev sqlite restored")


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
