#!/usr/bin/env python3
"""restsend-call MINI 模式 + bootstrap 全流程集成验证（真实 rustpbx）。

部署（参照 config.toml.dev 风格的 CC 配置）：
  * desk.acw.url → mock CRM（全局兜底 ACW）
  * screen_pop 规则 popup_url → mock CRM（全局兜底弹屏，auto_open_on_ringing）
  * SG support metadata: acw_url / screen_pop_url（General tab 字段，SG 级）
  * agent 1001 extras: acw_url / screen_pop_url（Advanced tab 字段，坐席级，
    优先级最高 —— 断言 mock CRM 收到的是坐席级 URL）
  * outbound policy 两条线路规则（prefix 9/self 默认 + prefix 0/021…）绑 1001
  * SG quick contacts：typed 技能组 + IVR + 坐席条目

restsend-pc 以 --mini + bootstrap（static token）真实注册并拉取 agent config：
  A. mini contacts 面板截图（typed 徽标、特性码不外显）
  B. mini dialpad 面板截图（外呼线路选择器 = 策略 2 条线路）
  C. 来话（sipbot 1003 呼 1001）→ 振铃弹屏（mock CRM 收到坐席级 render URL）
     → mini 振铃态截图 → 自动接听 → 挂断 → ACW（mock CRM 收到坐席级 acw URL）
     → mini ACW 态截图
  D. /cmd/dialout 拨 *82main（IVR 特性码）→ 通话中截图 → IVR 超时挂断
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse

RUSTPBX = Path("/Users/pi/workspace/rs/rustpbx")
RESTSEND = Path("/Users/pi/workspace/rs/restsend-call")
PC_BIN = RESTSEND / "target/debug/restsend-pc"
OUT = RUSTPBX / "screenshots/cc-mini-shots"
TMP = Path("/tmp/opencode/cc-mini-shots")

sys.path.insert(0, str(RUSTPBX / "regression"))
import helpers as h  # noqa: E402
from helpers.pbx_server import PbxApiClient, PbxServer  # noqa: E402

TOKEN = "mini-api-token"
MOCK_HITS: list[str] = []
MOCK_HITS_LOCK = threading.Lock()


# ── mock CRM ─────────────────────────────────────────────────────────────────
class MockCrm(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        with MOCK_HITS_LOCK:
            MOCK_HITS.append(self.path)
        body = b"<html><body><h1>Mock CRM</h1></body></html>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):  # silence
        pass


def mock_hits(prefix: str) -> list[str]:
    with MOCK_HITS_LOCK:
        return [h for h in MOCK_HITS if h.startswith(prefix)]


def wait_mock_hit(prefix: str, timeout: float = 12.0) -> str | None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        hits = mock_hits(prefix)
        if hits:
            return hits[0]
        time.sleep(0.3)
    return None


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


MOCK_PORT = free_port()
MOCK = f"http://127.0.0.1:{MOCK_PORT}/mockcrm"


# ── rustpbx boot + seed ──────────────────────────────────────────────────────
async def seed(client: PbxApiClient) -> None:
    seed = await client.seed_default_agents()
    assert seed.get("agents", 0) >= 1, f"agent seeding failed: {seed}"

    # SG General tab fields (metadata): SG-level ACW + ringing popup URL,
    # plus typed quick contacts on the desk fragment.
    resp = await client.put("/api/cc/skill-groups/support", {
        "skill_group_id": "support",
        "display_name": "售后支持组",
        "skills_required": ["support"],
        "metadata": {
            "acw_url": f"{MOCK}/acw?sg=support&call=${{callId}}",
            "screen_pop_url": f"{MOCK}/pop?sg=support&caller=${{caller}}",
        },
        "extra": {
            "desk": {
                "type": "text",
                "value": json.dumps({"transfer": {"quickContacts": [
                    {"agentId": "support", "type": "skill_group"},
                    {"agentId": "main", "label": "主 IVR", "type": "ivr"},
                    {"agentId": "1002", "label": "售后-李四", "number": "1002"},
                ]}}),
            }
        },
    })
    assert resp.get("success") is True or resp.get("skill_group_id"), f"sg update failed: {resp}"

    # Agent 1001 Advanced tab fields (extras): agent-level URLs win.
    agent = await client.get("/api/cc/agents/1001")
    skills = agent.get("skills")
    if isinstance(skills, dict):
        skills = skills.get("list") or ["support", "sales"]
    resp = await client.put("/api/cc/agents/1001", {
        "display_name": agent.get("display_name") or "1001",
        "skills": skills or ["support", "sales"],
        "extra": {
            "acw_url": {"type": "text", "value": f"{MOCK}/acw?agent=1001&call=${{callId}}"},
            "screen_pop_url": {"type": "text", "value": f"{MOCK}/pop?agent=1001&caller=${{caller}}"},
        },
    })
    assert resp.get("success") is True or resp.get("agent_id"), f"agent update failed: {resp}"

    # Outbound policy: two line rules (prefix + available caller number).
    resp = await client.post("/api/cc/outbound-policies", {
        "name": "mini-lines",
        "policy": {
            "enabled": True,
            "lines": [
                {"id": "line-mobile", "label": "Mobile 外线", "prefix": "9",
                 "caller": "self", "default": True, "allowed_prefixes": ["1"]},
                {"id": "line-ld", "label": "长途", "prefix": "0",
                 "caller": "02188886666"},
            ],
        },
        "agents": ["1001"],
    })
    assert resp.get("success") is True, f"policy create failed: {resp}"

    # Delivery sanity: lines replaced by the policy, QC enriched, typed rows.
    config = await client.get("/api/cc/agents/1001/config")
    desk = config.get("desk") or {}
    lines = desk.get("lines") or []
    assert [l.get("id") for l in lines] == ["line-mobile", "line-ld"], f"lines: {lines}"
    assert lines[0].get("default") is True and lines[0].get("caller") == "self", lines[0]
    qc = ((desk.get("transfer") or {}).get("quickContacts")) or []
    by_id = {c.get("agentId"): c for c in qc}
    assert by_id["support"]["number"] == "*81support", by_id
    assert by_id["main"]["number"] == "*82main" and by_id["main"]["type"] == "ivr", by_id
    assert by_id["1002"]["number"] == "1002", by_id
    print("[seed] config ok:",
          f"lines={[l.get('id') for l in lines]} qc={sorted(by_id)}")


def append_desk_section(server: PbxServer) -> None:
    """Append the [cc.desk] ACW / screen-pop section to the generated cc.toml
    (global fallbacks; the SG / agent values come from the DB)."""
    cc_toml = server.work_dir / "config" / "cc" / "cc.toml"
    content = cc_toml.read_text(encoding="utf-8")
    content += f"""

[desk.screen_pop]
auto_open_on_ringing = true

[desk.acw]
url = "{MOCK}/acw?global=1&call=${{callId}}"
wrapup_time_secs = 10

[[desk.screen_pop.rules]]
name = "global-popup"
is_default = true
priority = 90
popup_url = "{MOCK}/pop?global=1&caller=${{caller}}"
auto_open = true
"""
    cc_toml.write_text(content, encoding="utf-8")
    print("[pbx] desk acw/screen-pop section appended")


# ── restsend-pc driver ───────────────────────────────────────────────────────
STATUS_PORT = 18159


def write_client_env() -> None:
    shutil.rmtree(TMP, ignore_errors=True)
    (TMP / "home").mkdir(parents=True)
    (TMP / "data").mkdir(parents=True)
    OUT.mkdir(parents=True, exist_ok=True)

    bootstrap = TMP / "desk.bootstrap.json"
    bootstrap.write_text(json.dumps({
        "version": 1,
        "base_url": f"http://127.0.0.1:{PBX_HTTP}/api",
        "auth": {"type": "bearer", "source": "static", "static_token": TOKEN},
        "login_presence": "idle",
        "acw": {"wrapup_secs": 10},
    }, indent=2))

    prefs = {
        "accounts": [{
            "name": "分机 1001",
            "enabled": True,
            "auth": "password",
            "user": "1001",
            "local_uri": f"sip:1001@127.0.0.1:{PBX_SIP + 20}",
            "registrar": f"sip:127.0.0.1:{PBX_SIP}",
            "password": "123456",
            "media": "rtp",
            "transport": "udp",
            "path": "",
            "ice_servers": "",
            "ice_relay_only": False,
            "ice_enabled": True,
        }],
        "active_account": 0,
        "update": {"url": "", "frequency": "never", "last_check": 0},
        "audio": {"input_device": "", "output_device": "", "ringtone_device": "",
                  "codecs": [{"id": i, "enabled": True} for i in range(5)]},
        "video": {"camera": "", "mock_file": ""},
        "alerts": {"ring_enabled": False, "connect_tone": False, "hangup_tone": False},
        "call": {"auto_answer": True, "auto_answer_secs": 2,
                 "auto_acw": False, "dnd": False},
        "record": {"enabled": False},
        "enterprise": {"acw_enabled": True, "team_enabled": True,
                       "perf_enabled": True, "csat_enabled": True},
    }
    (TMP / "data" / "prefs.json").write_text(
        json.dumps(prefs, ensure_ascii=False, indent=2), encoding="utf-8")


def pc_env(extra: dict[str, str]) -> dict[str, str]:
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
    return env


def reset_bootguard() -> None:
    bs = TMP / "data" / "boot_state.json"
    if bs.exists():
        bs.unlink()


def spawn_pc(name: str, extra_env: dict[str, str], args: list[str]) -> subprocess.Popen:
    reset_bootguard()
    return subprocess.Popen(
        [str(PC_BIN), *args], env=pc_env(extra_env), cwd=str(RESTSEND),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def wait_registered(timeout: float = 20.0) -> bool:
    deadline = time.time() + timeout
    url = f"http://127.0.0.1:{STATUS_PORT}/status"
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                body = r.read().decode()
                if '"agent_id":"1001"' in body:
                    return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def cmd_shot(name: str, timeout: float = 10.0) -> Path:
    path = OUT / f"{name}.png"
    url = f"http://127.0.0.1:{STATUS_PORT}/cmd/shot?path={path}"
    req = urllib.request.Request(url, data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=5) as r:
        r.read()
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists() and path.stat().st_size > 10_000:
            print(f"[shot] {name} ok ({path.stat().st_size} bytes)")
            return path
        time.sleep(0.3)
    raise RuntimeError(f"shot {name} not written")


async def main() -> int:
    if not PC_BIN.exists():
        print(f"missing {PC_BIN}")
        return 1

    # mock CRM
    mock_srv = ThreadingHTTPServer(("127.0.0.1", MOCK_PORT), MockCrm)
    threading.Thread(target=mock_srv.serve_forever, daemon=True).start()

    global PBX_SIP, PBX_HTTP
    PBX_SIP, PBX_HTTP = free_port(), free_port()
    work = RUSTPBX / "target/cc-mini-shots"
    server = PbxServer(
        host="127.0.0.1", sip_port=PBX_SIP, http_port=PBX_HTTP, rwi_token=TOKEN,
        work_dir=work, project_root=RUSTPBX,
    )
    server.config_builder.database_url = f"sqlite://{work}/cc-mini.db?mode=rwc"
    # A non-agent SIP user for the inbound caller (agents must stay idle so
    # the ACD dispatches to restsend-call 1001).
    server.config_builder.add_memory_users(["2001"])
    server.config_builder.add_ivr("main", """[ivr]
name = "main"
ivr_mode = "tree"

[ivr.root]
greeting_text = "Welcome."
timeout_ms = 8000
max_retries = 1
timeout_action = { type = "hangup" }
""")
    server.prepare(webhook_url="", build=False)
    append_desk_section(server)
    server.start(timeout=90)
    print(f"[pbx] up: sip={server.sip_addr} http={server.http_url}")

    import aiohttp
    session = aiohttp.ClientSession()
    try:
        client = PbxApiClient(session, server.http_url, TOKEN)
        assert await client.ensure_console_auth(), "console auth failed"
        await seed(client)
    finally:
        await session.close()

    write_client_env()

    # ── C: live flow — idle → ringing(弹屏) → in-call → ACW ──
    proc = spawn_pc("live", {}, ["--mini"])
    registered = wait_registered(25)
    print(f"[pc] registered={registered}")
    await asyncio.sleep(3)  # agent config fetched → lines/QC/screenPop applied
    cmd_shot("mini-idle")

    from helpers.sipbot import SipBotProcess
    print("[flow] dialing *81support (quick contact path)...")
    caller = SipBotProcess(name="caller-2001")
    caller.start_caller(
        target=f"sip:*81support@{server.sip_addr}",
        username="2001",
        password="123456",
        register=True,
        proxy=f"127.0.0.1:{PBX_SIP}",
        hangup=7,
        addr="127.0.0.1:25461",
    )
    # ringing: screen-pop render URL opens (agent-level wins over SG/global)
    pop = wait_mock_hit("/mockcrm/pop?agent=1001", timeout=30)
    assert pop, f"ringing screen-pop URL not opened. hits={MOCK_HITS}"
    cmd_shot("mini-ringing")
    assert not mock_hits("/mockcrm/pop?sg="), "SG-level pop must NOT win over agent-level"

    # auto-answer (2s) → connected; caller hangs up at 7s → ACW opens
    acw = wait_mock_hit("/mockcrm/acw?agent=1001", timeout=25)
    assert acw, f"ACW URL not opened. hits={MOCK_HITS}"
    assert "sg=" not in acw, f"agent-level ACW must win: {acw}"
    time.sleep(1.5)
    cmd_shot("mini-acw")
    caller.terminate()

    # ── D: outbound via /cmd/dialout to the IVR feature code ──
    dial = f"http://127.0.0.1:{STATUS_PORT}/cmd/dialout?calleeno=%2A82main"
    req = urllib.request.Request(dial, data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=5) as r:
        r.read()
    await asyncio.sleep(4)
    cmd_shot("mini-outbound-call")
    proc.terminate()
    time.sleep(1)
    if proc.poll() is None:
        proc.kill()

    # ── A/B: headless panel shots — warm agent-session kv cache from the live
    # run makes the panels render real data instantly.
    panels = [
        ("mini-contacts", {"RESTSEND_PC_MINI_PANEL": "contacts",
                           "RESTSEND_PC_SHOT": str(OUT / "mini-contacts.png"),
                           "RESTSEND_PC_STATUS_PORT": "18161"}, ["--mini"]),
        ("mini-dialpad", {"RESTSEND_PC_MINI_PANEL": "dialpad",
                          "RESTSEND_PC_SHOT": str(OUT / "mini-dialpad.png"),
                          "RESTSEND_PC_STATUS_PORT": "18162"}, ["--mini"]),
    ]
    for name, extra, args in panels:
        proc = spawn_pc(name, extra, args)
        deadline = time.time() + 25
        while time.time() < deadline and not (OUT / f"{name}.png").exists():
            time.sleep(0.4)
        time.sleep(0.5)
        if proc.poll() is None:
            proc.terminate()
        p = OUT / f"{name}.png"
        print(f"[pc] {name}: {'OK' if p.exists() and p.stat().st_size > 8000 else 'MISSING'}")

    server.stop()
    mock_srv.shutdown()

    print("\nmock CRM hits:")
    for hit in MOCK_HITS:
        print(f"  {hit}")
    # 1920x48 mini-bar strips: 11px labels vanish in scaled previews —
    # emit a 3x crop of the leftmost dropdowns for human verification.
    try:
        from PIL import Image
    except ImportError:
        Image = None
    for f in sorted(OUT.glob("mini-*.png")):
        if Image is None:
            break
        im = Image.open(f)
        if im.width > 800:
            crop = im.crop((0, 0, 320, im.height))
            crop = crop.resize((crop.width * 3, crop.height * 3), Image.LANCZOS)
            crop.save(f.with_suffix(".left.png"))
    print("\nscreenshots:")
    for p in sorted(OUT.glob("*.png")):
        print(f"  {p} ({p.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
