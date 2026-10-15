#!/usr/bin/env python3
"""
RustPBX external HTTP service — routing + user/password management.
Standalone example service (no 3rd-party deps).

This single service owns two jobs for RustPBX (both are built-in features,
no rust changes required):

 1) HttpCallRouter (`[proxy.http_router]`) -> POST /route
    Decides the routing for every call instead of static route files.
    It knows the extension table (see USERS below) and asks the PBX
    (over the console API) whether a known extension is currently online:

      - FS/trunk inbound:
          number not in the table ............ reject 404 (number does not exist)
          known + online ..................... forward  (PBX rings registered device)
          known + offline .................... reject 480 (user offline)
      - internal (local-originated) => 无脑转 FS trunk

 2) HttpUserBackend (`[[proxy.user_backends]] type = "http"`) -> POST /auth
    Brands/serves extension numbers AND passwords. On a REGISTER/INVITE the
    PBX sends us the username and we must answer with the matching SipUser
    (whose `password` is seed for the Digest challenge) or a "not_found".

Online status is fetched through the rustpbx console API (NOT AMI):

    POST {RUSTPBX_HTTP}/api/diagnostics/locator/lookup
    Authorization: Bearer <token>            # from [[console.api_tokens]]
    body: {"user": "879831001"}              # resolves to sip:<user>@localhost
    -> {"total": N, ...}   N > 0 == online

config.toml additions (restart required):

    [console]
    [[console.api_tokens]]
    token = "<same token>"                   # keep scopes empty = full access

    [proxy.http_router]
    url = "http://127.0.0.1:9090/route"
    timeout_ms = 3000
    fallback_to_static = false

    [[proxy.user_backends]]
    type = "http"
    url = "http://127.0.0.1:9090/auth"
    method = "POST"
    username_field = "username"

Usage:
    python3 examples/http_router_service.py [--listen 127.0.0.1:9090]

    RUSTPBX_TOKEN below is hard-coded and must match the
    [[console.api_tokens]].token value in config.toml.
"""

import argparse
import json
import re
import sys
import os
import time
import urllib.request
import urllib.error
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# --------------------------------------------------------------------------- #
# Extension + password table (your external account manager replaces this).
# key: extension number, value: plaintext password.
# This is the SINGLE source of truth: it drives both /route (does the
# number exist?) and /auth (digest password), and can be fed by any backend
# (DB, LDAP, CRM, ...) instead of a hard-coded dict.
# --------------------------------------------------------------------------- #
USERS = {
    "879831001": "123456",
    "879831002": "123456",
}

RUSTPBX_HTTP = os.environ.get("RUSTPBX_HTTP", "http://127.0.0.1:8080")
# Bearer token for the console API — must match [[console.api_tokens]] in
# config.toml (kept unscoped = full/superuser access). Hard-coded on purpose.
RUSTPBX_TOKEN = "nmb3BC7PERvThZhm6bL63B8wjD29pVz_"
# Local realm the PBX rewrites inbound trunk callees to (matches route rule to.host).
LOCAL_REALM = "localhost"
# Outbound default trunk, referenced BY NAME. The actual next-hop IP/port is
# looked up from TRUNKS where the dest is recorded — change routing here means
# updating TRUNKS, no hard-coded sockets to keep in sync.
OUTBOUND_TRUNK = "yacc55-fs"


def trunk_target(name: str, user: str) -> str:
    """Resolve a trunk BY NAME to a forward `targets` SIP URI.

    http_router forward `targets` are plain SIP URIs (no `@trunkname`
    syntax), but rustpbx matches the outbound request's host/port against each
    trunk's `dest` to apply the trunk behaviour (SDP external_ip, ringback,
    header passthrough). So we build `sip:<user>@<dest-host:port>` here and
    let rustpbx re-associate it with the named trunk at send time.
    Raises KeyError if the trunk is not defined in TRUNKS.
    """
    dest = TRUNKS[name]["dest"]
    # dest looks like "sip:100.88.0.42:5090" -> strip the "sip:" scheme.
    host_port = dest.split(":", 1)[-1] if ":" in dest else dest
    return f"sip:{user}@{host_port}"
# --------------------------------------------------------------------------- #
# SIP trunk table (synced into rustpbx database at startup).
# rustpbx runs in database-config mode (generated_db=true), so file-based
# trunks under config/trunks/* are NEVER loaded — they must be written into
# the console database. We own that here so the example is self-sufficient.
#
# Each entry maps 1:1 to a `PUT/GET /api/sip-trunk` record:
#   allow_ips   -> inbound_hosts (source IP that marks the call as inbound,
#                                  i.e. no auth challenge)
#   dest        -> sip_server
#   external_ip -> SDP media IP advertised to the peer (Tailscale here)
#   ringback    -> tones played on reject_with_tone (404/480)
# --------------------------------------------------------------------------- #
TRUNKS = {
    "yacc55-fs": {
        "display_name": "FS Trunk (yacc55)",
        "direction": "bidirectional",
        "dest": "sip:100.88.0.42:5090",
        "transport": "udp",
        "allow_ips": "100.88.0.42/32",
        "external_ip": "100.88.0.1",
        # 出站 offer 只允许 G.711（PCMA/PCMU），去掉 G722/opus，避免线路侧 488。
        "audio_codecs": ["PCMA", "PCMU"],
        "ringback": {
            "notfound": "sounds/failure-notfound-zh.wav",
            "offline": "sounds/failure-offline-zh.wav",
        },
    },
}
# 回呼前缀：外呼外显为 98089+分机号；呼入若带此前缀则剥离后再找分机。
CALLBACK_PREFIX = "98089"
# clpss 风格 SITECODE 剥号：分机号本身可能带站点码前缀(如 3201001 = 3201+001)，
# 出呼时循环剥离直到不再匹配（支持叠加如 32013201xxx）。默认为空=不剥；
# 由 clpss-daemon 按环境变量 SITE_CODE 写入实际值。
SITECODE = os.environ.get("SITE_CODE", "")
# clpss 风格 CCM_CALL 事件：内部记录保存（内存 FIFO），供 clpss-daemon / 高可用
# 话务转移侧消费。当前仅打印并保存，不做网络上报。
CCM_CALLS_CAP = 500     # 最多保留最近 N 通完整呼叫（每通 invite→200→bye 链不截断）
CCM_CALLS: dict[str, list[dict]] = {}   # callId -> 该通所有事件（按时间序）
CCM_CALLS_ORDER: list[str] = []         # callId 插入序 FIFO，用于淘汰最旧的完整呼叫

# X-cid 呼叫关联约定：
# 出呼声（上游无 X-cid）生成 KSR_<session_id>，其中 session_id 由 RustPBX
# /route 请求直接提供（与该通话 RWI 事件的 session_id 完全一致）。因此
# RWI 事件端可用同样的 KSR_<session_id> 推导出 X-cid，无需任何状态存储。
# 注：http_router Forward 分支返回的 headers 不会进 RWI sip_headers，但
# 本服务不依赖 sip_headers——直接用确定性的 session_id 即可。

_UI_HTML = r"""<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>RustPBX 事件查看器</title>
<style>
  * { box-sizing: border-box; }
  body { margin: 0; font-family: -apple-system, "PingFang SC", "Microsoft YaHei", sans-serif;
         background: #f5f6fa; color: #1a1a2e; font-size: 14px; }
  header { background: linear-gradient(135deg,#1e3c72,#2a5298); color:#fff;
           padding: 16px 24px; display:flex; align-items:center; gap:24px; }
  header h1 { margin:0; font-size:18px; font-weight:600; letter-spacing:.5px; }
  .stats { display:flex; gap:20px; margin-left:auto; }
  .stat { background:rgba(255,255,255,.15); padding:6px 14px; border-radius:20px; font-size:13px; }
  .stat b { font-size:16px; margin-right:4px; }
  toolbar { display:flex; gap:12px; padding:12px 24px; background:#fff;
           border-bottom:1px solid #e1e4ea; align-items:center; position:sticky; top:0; z-index:10; }
  toolbar input { flex:1; max-width:400px; padding:8px 14px; border:1px solid #d0d4dc;
                  border-radius:8px; font-size:14px; outline:none; transition:border .2s; }
  toolbar input:focus { border-color:#2a5298; }
  toolbar button { padding:8px 16px; border:1px solid #d0d4dc; background:#fff;
                   border-radius:8px; cursor:pointer; font-size:13px; transition:all .15s; }
  toolbar button:hover { border-color:#2a5298; color:#2a5298; }
  toolbar button.primary { background:#2a5298; color:#fff; border-color:#2a5298; }
  toolbar button.primary:hover { background:#1e3c72; }
  .refresh-indicator { display:flex; align-items:center; gap:6px; font-size:12px; color:#6b7280; }
  .dot { width:8px; height:8px; border-radius:50%; background:#22c55e; animation:pulse 2s infinite; }
  @keyframes pulse { 0%,100%{opacity:1} 50%{opacity:.4} }
  main { padding:16px 24px; display:flex; flex-direction:column; gap:10px; }
  .call-card { background:#fff; border-radius:10px; border:1px solid #e1e4ea;
               overflow:hidden; transition:box-shadow .15s; }
  .call-card:hover { box-shadow:0 2px 12px rgba(0,0,0,.06); }
  .call-head { padding:10px 16px; display:flex; align-items:center; gap:12px;
               cursor:pointer; user-select:none; background:#fafbfe; }
  .call-head:hover { background:#f0f3fa; }
  .chevron { font-size:10px; color:#9ca3af; transition:transform .2s; }
.call-card.open .chevron { transform:rotate(90deg); }
.call-ts { font-family:"SF Mono",Menlo,monospace; font-size:13px; font-weight:600; color:#1e3c72; width:185px; flex-shrink:0; white-space:nowrap; }
  .call-ani { color:#059669; font-weight:500; }
  .call-arrow { color:#9ca3af; }
  .call-dnis { color:#dc2626; font-weight:500; }
  .call-xcid { margin-left:auto; font-family:"SF Mono",Menlo,monospace; font-size:12px;
               color:#6b7280; background:#f3f4f6; padding:2px 8px; border-radius:10px; max-width:260px;
               overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  .call-states { display:flex; gap:4px; }
  .state-pill { font-size:11px; padding:2px 8px; border-radius:10px; font-weight:500; }
  .st-invite   { background:#dbeafe; color:#1d4ed8; }
  .st-180      { background:#e0e7ff; color:#4338ca; }
  .st-200      { background:#dcfce7; color:#15803d; }
  .st-bye      { background:#f3f4f6; color:#6b7280; }
  .st-no_answer{ background:#fef3c7; color:#b45309; }
  .st-busy     { background:#fee2e2; color:#b91c1c; }
  .st-error    { background:#fee2e2; color:#b91c1c; }
  .card-body { display:none; padding:12px 16px; border-top:1px solid #e1e4ea; background:#fff; }
  .call-card.open .card-body { display:block; }
  .event-row { display:flex; align-items:flex-start; gap:10px; padding:6px 0;
             border-bottom:1px dashed #eef0f5; font-size:13px; }
.event-row:last-child { border-bottom:none; }
.event-ts { flex-shrink:0; font-family:"SF Mono",Menlo,monospace; font-size:12px; color:#6b7280; width:125px; white-space:nowrap; }
.event-badge { flex-shrink:0; width:56px; text-align:center; }
  .event-badge .state-pill { display:inline-block; }
  .event-json { flex:1; font-family:"SF Mono",Menlo,monospace; font-size:12px;
                color:#4b5563; word-break:break-all; background:#f9fafb; padding:6px 10px;
                border-radius:6px; line-height:1.6; }
  .empty { text-align:center; padding:60px 20px; color:#9ca3af; }
  .empty .big { font-size:48px; margin-bottom:10px; }
</style>
</head>
<body>
<header>
  <h1>📞 RustPBX 呼叫事件查看器</h1>
  <div class="stats">
    <div class="stat">呼叫 <b id="s-calls">0</b>/<span id="s-cap">0</span></div>
    <div class="stat">事件 <b id="s-events">0</b></div>
    <div class="stat" id="s-latest">—</div>
  </div>
</header>
<toolbar>
  <input id="q" type="text" placeholder="搜索 callId / xcid / ani / dnis..." />
  <button id="btn-refresh" class="primary">🔄 立即刷新</button>
  <button id="btn-toggle">⏸ 暂停自动刷新</button>
  <button id="btn-clear">🗑 清空记录</button>
  <div class="refresh-indicator"><span class="dot"></span><span id="auto-label">自动刷新 3s</span></div>
</toolbar>
<main id="list"><div class="empty"><div class="big">📭</div>暂无事件，发起一通呼叫看看</div></main>

<script>
let autoRefresh = true;
let refreshInterval = 3000;
let timer = null;
let countdown = null;
let countdownVal = 0;
let expanded = new Set();

function stateClass(s) {
  const m = {invite:'st-invite', '180':'st-180', '200':'st-200', bye:'st-bye',
    no_answer:'st-no_answer', busy:'st-busy', error:'st-error'};
  return m[s] || 'st-bye';
}

function fmtTime(us, withDate) {
  if (!us) return '';
  const d = new Date(Math.floor(us / 1000));       // 微秒 → 毫秒给 Date
  const pad = n => String(n).padStart(2, '0');
  const pad3 = n => String(n).padStart(3, '0');
  const time = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad3(d.getMilliseconds())}${String(us % 1000).padStart(3, '0')}`;
  if (withDate === false) return time;
  return `${pad(d.getMonth()+1)}/${pad(d.getDate())} ${time}`;
}

async function load() {
  try {
    const r = await fetch('/events');
    const d = await r.json();
    render(d);
  } catch(e) { console.error(e); }
  if (autoRefresh) startCountdown();
}

function render(d) {
  document.getElementById('s-calls').textContent = d.call_count;
  document.getElementById('s-cap').textContent = d.call_cap;
  document.getElementById('s-events').textContent = d.total_events;

  const q = document.getElementById('q').value.trim().toLowerCase();
  const main = document.getElementById('list');
  const callIds = Object.keys(d.calls);

  if (callIds.length === 0) {
    main.innerHTML = '<div class="empty"><div class="big">📭</div>暂无事件，发起一通呼叫看看</div>';
    return;
  }

  const sortedIds = Object.keys(d.calls).sort((a, b) => {
    const ta = (d.calls[a][0] || {}).ts || 0;
    const tb = (d.calls[b][0] || {}).ts || 0;
    return tb - ta;
  });

  const items = [];
  for (const cid of sortedIds) {
    const evts = d.calls[cid];
    const first = evts[0] || {};
    const last = evts[evts.length-1] || {};
    // 折叠重复状态：统计各 status 出现次数，`180×2` 一目了然。
    const stateCounts = {};
    evts.forEach(e => { stateCounts[e.status] = (stateCounts[e.status]||0) + 1; });
    const xcid = first.xcid || '';
    const ani = first.ani || '';
    const dnis = first.dnis || '';

    if (q) {
      const hay = (cid + xcid + ani + dnis).toLowerCase();
      if (!hay.includes(q)) continue;
    }

    const isOpen = expanded.has(cid);
    const statesHtml = Object.keys(stateCounts).map(s =>
      `<span class="state-pill ${stateClass(s)}" title="出现 ${stateCounts[s]} 次">${s}${stateCounts[s] > 1 ? '×' + stateCounts[s] : ''}</span>`).join(' ');

    const startTime = fmtTime(first.ts);

    let bodyHtml = '';
    if (isOpen) {
      bodyHtml = '<div class="card-body">' + evts.map((e,i) => {
        const j = {...e}; delete j.source; delete j.ts;
        return `<div class="event-row">
          <div class="event-ts">${fmtTime(e.ts, false)}</div>
          <div class="event-badge"><span class="state-pill ${stateClass(e.status)}">${e.status}</span></div>
          <div class="event-json">${JSON.stringify(j)}</div>
        </div>`;
      }).join('') + '</div>';
    }

    items.push(`
      <div class="call-card ${isOpen ? 'open' : ''}" data-cid="${cid}">
        <div class="call-head" onclick="toggle('${cid}')">
          <span class="chevron">▶</span>
          <span class="call-ts">${startTime}</span>
          <span class="call-ani">${ani}</span>
          <span class="call-arrow">→</span>
          <span class="call-dnis">${dnis}</span>
          <span class="call-xcid" title="${xcid}">${xcid}</span>
          <div class="call-states">${statesHtml}</div>
        </div>
        ${bodyHtml}
      </div>`);
  }

  main.innerHTML = items.length ? items.join('') :
    '<div class="empty"><div class="big">🔍</div>没有匹配的呼叫</div>';

  // 显示最新一通呼叫的状态（sortedIds 已按时间倒序，首条即最新）
  const latestCid = sortedIds[0];
  const latestEvts = d.calls[latestCid] || [];
  const latest = latestEvts[latestEvts.length - 1] || {};
  if (latest && Object.keys(latest).length) {
    document.getElementById('s-latest').textContent =
      '最新: ' + (latest.status || '') + ' ' + latestCid + ' (' + latestEvts.length + '条)';
  }
}

function toggle(cid) {
  if (expanded.has(cid)) expanded.delete(cid);
  else expanded.add(cid);
  load();
}

function tickCountdown() {
  const el = document.getElementById('auto-label');
  if (!el) return;
  countdownVal--;
  if (countdownVal <= 0) {
    el.textContent = '刷新中...';
  } else {
    el.textContent = countdownVal + 's 后刷新';
  }
}
function startCountdown() {
  stopCountdown();
  countdownVal = Math.ceil(refreshInterval / 1000);
  tickCountdown();
  countdown = setInterval(tickCountdown, 1000);
}
function stopCountdown() {
  if (countdown) clearInterval(countdown);
  countdown = null;
}

function startAuto() {
  stopAuto();
  timer = setInterval(load, refreshInterval);
  startCountdown();
  document.getElementById('btn-toggle').textContent = '⏸ 暂停自动刷新';
}
function stopAuto() {
  if (timer) clearInterval(timer);
  timer = null;
  stopCountdown();
  document.getElementById('auto-label').textContent = '已暂停';
  document.getElementById('btn-toggle').textContent = '▶ 开启自动刷新';
}

document.getElementById('btn-refresh').onclick = load;
document.getElementById('btn-toggle').onclick = () => {
  autoRefresh = !autoRefresh;
  if (autoRefresh) startAuto(); else stopAuto();
};
document.getElementById('btn-clear').onclick = async () => {
  if (!confirm('确定清空所有事件记录？')) return;
  const r = await fetch('/events/clear', {method:'POST'});
  if (r.ok) { expanded.clear(); load(); }
};
let searchTimer;
document.getElementById('q').oninput = () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(load, 200);
};

load();
if (autoRefresh) startAuto();
</script>
</body>
</html>"""

_SIP_USER_RE = re.compile(r"sip:([^@;\s<>]+)", re.IGNORECASE)


def parse_user(uri: str):
    """Extract the user part from a SIP Request-URI / To / From header.

    兼容三种形态：
      - "sip:user@host"                     (纯 URI)
      - "user@host"                         (无 sip: scheme)
      - '"Name" <sip:user@host>;tag=...'    (From 头：显示名+尖括号+参数)
    找不到则返回 None。
    """
    s = uri.strip()
    if not s:
        return None
    # 优先取尖括号内的 SIP URI（From 头形态），否则在整个串里找 sip:。
    m = re.search(r"<([^>]*)>", s)
    candidate = m.group(1) if m else s
    m = _SIP_USER_RE.search(candidate)
    if m:
        return m.group(1)
    # 无 "sip:" scheme 的 user@host 兜底
    m = re.match(r"^([^@;\s<>]+)(?:@[^;\s<>]+)?;?", candidate.strip())
    return m.group(1) if m else None


def strip_caller_sitecode(from_user: str) -> str:
    """Mirror clpss route[strip_caller_sitecode]:
    分机号本身可能带 sitecode 前缀(如 3201001, 或叠加 32013201xxx)，
    循环剥离直到不再匹配。仅对出呼主叫号生效(与 clpss 调用点一致)。
    SITECODE 为空时直接原值返回(不剥)。
    """
    if not SITECODE:
        return from_user
    stripped = from_user
    while len(stripped) > len(SITECODE) and stripped.startswith(SITECODE):
        stripped = stripped[len(SITECODE):]
    if stripped != from_user:
        print(
            f"[http-router] strip_caller_sitecode orig={from_user} "
            f"stripped={stripped} sitecode={SITECODE}",
            file=sys.stderr,
        )
    return stripped


def check_online(user: str):
    """
    Ask the PBX (console API) whether the given extension is registered/online.

    POST {RUSTPBX_HTTP}/api/diagnostics/locator/lookup  {"user": <user>}
    -> {"total": N, ...}; N > 0 means online.

    Returns True/False; returns None if the request failed (upstream error).
    """
    body = json.dumps({"user": user}).encode("utf-8")
    headers = {"Content-Type": "application/json"}
    if RUSTPBX_TOKEN:
        headers["Authorization"] = f"Bearer {RUSTPBX_TOKEN}"
    req = urllib.request.Request(
        f"{RUSTPBX_HTTP}/api/diagnostics/locator/lookup",
        data=body,
        headers=headers,
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=2) as resp:
            data = json.loads(resp.read().decode("utf-8"))
        return int(data.get("total") or 0) > 0
    except (urllib.error.URLError, urllib.error.HTTPError, OSError, ValueError) as exc:
        print(f"[http-router] online check failed for {user}: {exc}", file=sys.stderr)
        return None


def _console_request(method: str, path: str, payload=None, timeout: int = 5):
    """Send an authenticated request to the rustpbx console API and return
    the decoded JSON body. Raises on HTTP/network errors."""
    headers = {"Content-Type": "application/json"}
    if RUSTPBX_TOKEN:
        headers["Authorization"] = f"Bearer {RUSTPBX_TOKEN}"
    body = json.dumps(payload).encode("utf-8") if payload is not None else None
    req = urllib.request.Request(
        f"{RUSTPBX_HTTP}{path}", data=body, headers=headers, method=method
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read().decode("utf-8")
        return json.loads(raw) if raw else {}


def _existing_trunk_by_name() -> dict:
    """Return {trunk_name: db_id} for trunks currently held in the rustpbx
    database (only real DB rows; read-only file snapshots are ignored)."""
    try:
        data = _console_request(
            "POST", "/api/sip-trunk",
            {"page": 1, "per_page": 100, "filters": {}},
        )
    except Exception as exc:
        print(f"[trunk-sync] failed to list trunks: {exc}", file=sys.stderr)
        return {}
    by_name = {}
    for item in data.get("items", []):
        name = item.get("name")
        if not name:
            continue
        if name in by_name:
            continue  # first row wins (id is stable per name)
        by_name[name] = item.get("id")
    return by_name


def sync_trunks_to_pbx():
    """
    Push the TRUNKS table into the rustpbx console database: create trunks
    that don't exist yet and update the ones that do, then hot-reload so
    inbound trunk matching (no auth for FS-originated calls) picks up any
    changes. Safe to run at every startup (idempotent).
    """
    try:
        existing = _existing_trunk_by_name()
    except Exception as exc:
        print(f"[trunk-sync] skipped (console unreachable): {exc}", file=sys.stderr)
        return

    changed = 0
    for name, cfg in TRUNKS.items():
        metadata = {"sbc": {}}
        if cfg.get("external_ip"):
            metadata["sbc"]["external_ip"] = cfg["external_ip"]
        if cfg.get("ringback"):
            metadata["sbc"]["ringback"] = cfg["ringback"]
        if cfg.get("audio_codecs"):
            metadata["sbc"]["audio_codecs"] = cfg["audio_codecs"]
        body = {
            "name": name,
            "display_name": cfg.get("display_name") or name,
            "direction": cfg.get("direction", "bidirectional"),
            "sip_server": cfg.get("dest", ""),
            "sip_transport": cfg.get("transport", "udp"),
            "allowed_ips": cfg.get("allow_ips", ""),
            "is_active": True,
            "register_enabled": False,
            "metadata": json.dumps(metadata, ensure_ascii=False),
        }
        trunk_id = existing.get(name)
        if trunk_id is not None:
            method, path = "PATCH", f"/api/sip-trunk/{trunk_id}"
            verb = "updated"
        else:
            method, path = "PUT", "/api/sip-trunk"
            verb = "created"
        try:
            resp = _console_request(method, path, body)
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode("utf-8", errors="replace")
            print(f"[trunk-sync] {verb} '{name}' failed HTTP {exc.code}: {detail}",
                  file=sys.stderr)
            continue
        except Exception as exc:
            print(f"[trunk-sync] {verb} '{name}' error: {exc}", file=sys.stderr)
            continue
        changed += 1
        print(f"[trunk-sync] {verb} trunk '{name}' -> {resp}", file=sys.stderr)

    if changed:
        try:
            reload = _console_request("POST", "/ami/v1/reload/trunks", {})
            print(f"[trunk-sync] hot-reload: {reload}", file=sys.stderr)
        except Exception as exc:
            print(f"[trunk-sync] reload after sync failed: {exc}", file=sys.stderr)


def post_ccm_event(event: dict):
    # 微秒时间戳（time.time_ns()//1000）：比毫秒精确 1000×，
    # 且仍在 JS 安全整数范围内，前端可无损解析/排序。
    event.setdefault("ts", time.time_ns() // 1_000)
    group_key = event.get("sessionId") or event.get("callId") or event.get("xcid") or "unknown"
    if group_key not in CCM_CALLS:
        CCM_CALLS[group_key] = []
        CCM_CALLS_ORDER.append(group_key)
    CCM_CALLS[group_key].append(event)
    while len(CCM_CALLS_ORDER) > CCM_CALLS_CAP:
        old_id = CCM_CALLS_ORDER.pop(0)
        CCM_CALLS.pop(old_id, None)
    print(f"[ccm-event] {json.dumps(event, ensure_ascii=False)}", file=sys.stderr)


def _rwi_payload(incoming: dict) -> dict:
    """展开 RWI webhook 的嵌套 envelope，返回扁平事件体。

    RustPBX [rwi_webhook] POST 到 /rwi 的实际结构为（rwi/webhook.rs）：
      {
        "rwi": "1.0",
        "event_id": "...",
        "timestamp": "...",
        "call_id": "...",           # 顶层冗余一份 call_id
        "event_type": "call_created",   # 顶层：事件类型唯一真源
        "event": {                  # 真正的扁平 payload（event_type 被摘除）
            "call_id": "...",
            "caller": "...",        #   ani
            "callee": "...",        #   dnis
            "direction": "inbound"|"outbound"|"local",
            "trunk": "...",
            "session_id": "...",
            "sip_headers": {...},
            "reason"/"hangup_by"/"sip_status"/"duration_secs": ...  # hangup only
        }
      }

    归一化时以嵌套 `event` 为准，顶层 `call_id`/`event_type` 作兜底，
    兼容扁平与嵌套两种投递形态。
    """
    nested = incoming.get("event")
    if isinstance(nested, dict):
        flat = dict(nested)
        # event_type 以顶层唯一真源为准
        event_type = incoming.get("event_type") or flat.get("event_type") or ""
        if event_type:
            flat["event_type"] = event_type
        # 顶层冗余的 call_id 为兜底
        if not flat.get("call_id") and incoming.get("call_id"):
            flat["call_id"] = incoming["call_id"]
        return flat
    return incoming


def handle_rwi(payload: dict) -> None:
    """接收 RustPBX RWI webhook push（呼叫生命周期事件）。

    实际投递为嵌套 envelope（见 _rwi_payload 注释），统一展开扁平后处理。
    最终归一成 clpss CCM_CALL 风格存入 CCM_CALLS（按 callId 分组），和
    /route 里记录的 invite 事件合并，让 clpss-daemon 统一消费完整事件链。
    """
    payload = _rwi_payload(payload)
    event_type = payload.get("event") or payload.get("event_type") or ""
    if not event_type:
        print(f"[rwi] no event_type in payload: {payload}", file=sys.stderr)
        return

    status_map = {
        "call_created": "invite",
        "call_ringing": "180",
        "call_answered": "200",
        "call_hangup": "bye",
    }
    clpss_status = status_map.get(event_type, event_type)

    call_id = payload.get("call_id") or ""
    session_id = payload.get("session_id") or call_id

    # 呼入场景上游真实带的 X-cid 会出现在原始 INVITE，优先认它；
    # 否则（呼出场景本地生成的 X-cid 在 Forward headers，不经过这里）
    # 用与 /route 阶段相同的 KSR_<session_id> 确定性推导，无需状态。
    sip_headers = payload.get("sip_headers")
    xcid = None
    if isinstance(sip_headers, dict):
        xcid_key = next((k for k in sip_headers if k.lower() == "x-cid"), None)
        xcid = sip_headers.get(xcid_key) if xcid_key else None
    xcid = xcid or f"KSR_{session_id}"

    ani = payload.get("caller") or ""
    dnis = payload.get("callee") or ""

    ccm_event = {
        "xcid": xcid,
        "callId": call_id,
        "sessionId": session_id,
        "ani": ani,
        "dnis": dnis,
        "status": clpss_status,
        "dir": payload.get("direction") or "",
        "source": "rwi",
    }
    if event_type == "call_hangup":
        for k in ("reason", "hangup_by", "sip_status", "duration_secs"):
            v = payload.get(k)
            if v is not None:
                ccm_event[k] = v

    post_ccm_event(ccm_event)


def handle_route(payload: dict) -> dict:
    direction = payload.get("direction", "")
    uri = payload.get("uri") or payload.get("to") or ""
    user = parse_user(uri)
    print(f"[http-router] direction={direction} uri={uri} user={user}", file=sys.stderr)

    if not user:
        return {"action": "reject", "status": 400, "reason": "bad request uri"}

    # 呼入（trunk/FS → 本地）若被叫带回呼前缀 98089，剥掉后再找分机。
    if direction == "inbound" and user.startswith(CALLBACK_PREFIX):
        user = user[len(CALLBACK_PREFIX):]
        uri = f"sip:{user}@localhost"

    # --- X-cid 呼叫关联 ---
    # 判定 dir 的核心依据是 "INVITE 里有没有 X-cid"：
    #   - 无 X-cid → 本节点首次见到 → 自己生成 KSR_<id> → dir="outbound"
    #   - 有 X-cid → 跨节点转发过来的 → 沿用 → dir="inbound"
    # 这里结合 RustPBX direction 做交叉判定：上游真带了 X-cid 强制 inbound，
    # 否则按 direction 映射（等价"首次进入"语义）。
    call_id = payload.get("call_id") or ""
    # X-cid 统一以 session_id 为基准（RustPBX 在 /route 请求里直接携带，
    # 与 RWI 事件的 session_id 完全一致）。下游 RWI 端用同样的
    # KSR_<session_id> 推导，两端天然恒等，无需维护任何映射/状态。
    session_id = payload.get("session_id") or call_id
    request_headers = payload.get("headers") or {}
    upstream_has_xcid = False
    xcid = None
    for k, v in request_headers.items():
        if k.lower() == "x-cid":
            xcid = v
            upstream_has_xcid = True
            break
    if not xcid:
        xcid = f"KSR_{session_id}"

    if direction == "internal":
        # 本地呼出 —— 无脑转接 FS trunk（按中继名找 dest，不硬编码 IP）。
        from_user = parse_user(payload.get("from") or "") or user
        caller_out = f"{CALLBACK_PREFIX}{from_user}"
        return {
            "action": "forward",
            "targets": [trunk_target(OUTBOUND_TRUNK, user)],
            # 出站到 FS trunk 只允许 G.711，避免 FS 选定 G722 送给线路侧 488。
            "allow_codecs": ["PCMA", "PCMU"],
            "headers": {"X-cid": xcid},
            "caller": caller_out,
        }

    # trunk(FS) 呼入 —— 留在本地转接分机。
    # FS 侧主叫可能带 sitecode 前缀，剥掉后才是本地分机号，写入
    # 返回体 caller 字段让 RustPBX 改写出站 INVITE 的 From 头。
    from_user = parse_user(payload.get("from") or "") or ""
    clean_from = strip_caller_sitecode(from_user) if from_user else ""
    print(
        f"[http-router][inbound-dbg] direction={direction} from_raw={payload.get('from')!r} "
        f"from_user={from_user!r} clean_from={clean_from!r} uri={uri!r} user={user!r} "
        f"sitecode={SITECODE!r}",
        file=sys.stderr,
    )

    if user not in USERS:
        return {"action": "reject", "status": 404, "reason": "number does not exist"}

    online = check_online(user)
    if online is True:
        resp = {"action": "forward", "targets": [f"sip:{user}@{LOCAL_REALM}"]}
        if clean_from:
            resp["caller"] = clean_from
        return resp
    if online is False:
        return {"action": "reject", "status": 480, "reason": "user offline"}
    # 查询失败 —— 保守处理：仍转接，由 PBX 自行决定（避免误拒真呼叫）
    resp = {"action": "forward", "targets": [f"sip:{user}@{LOCAL_REALM}"]}
    if clean_from:
        resp["caller"] = clean_from
    return resp


def handle_auth(form: dict) -> tuple:
    """
    HttpUserBackend endpoint: on REGISTER/INVITE the PBX POSTs
    username/realm/request_uri. Return 200 + SipUser (with password for the
    Digest challenge) or 404 {"reason":"not_found"}.

    Returns (status_code, payload_dict).
    """
    user = (form.get("username") or "").strip()
    password = USERS.get(user)
    if password is None:
        print(f"[http-auth] user '{user}' NOT FOUND", file=sys.stderr)
        return 404, {"reason": "not_found"}
    print(f"[http-auth] user '{user}' FOUND (realm={form.get('realm')!r})", file=sys.stderr)
    ordered = list(USERS)
    return 200, {
        "id": ordered.index(user) + 1,
        "enabled": True,
        "username": user,
        "password": password,
        "realm": LOCAL_REALM,
        "allow_guest_calls": False,
    }


class RouterHandler(BaseHTTPRequestHandler):
    def _json(self, status, obj):
        body = json.dumps(obj).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self) -> bytes:
        te = (self.headers.get("Transfer-Encoding") or "").lower()
        if "chunked" in te:
            chunks = []
            while True:
                line = self.rfile.readline().strip()
                if not line:
                    break
                size = int(line.split(b";", 1)[0].strip(), 16)
                if size == 0:
                    self.rfile.readline()
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.readline()
            return b"".join(chunks)
        length = int(self.headers.get("Content-Length", 0) or 0)
        return self.rfile.read(length) if length else b""

    def _handle_auth(self):
        raw = self._read_body()
        ctype = self.headers.get("Content-Type", "")
        if ctype.startswith("application/json"):
            try:
                data = json.loads(raw.decode("utf-8")) or {}
            except ValueError:
                data = {}
        else:
            data = {k: v[0] if v else "" for k, v in
                    urllib.parse.parse_qs(raw.decode("utf-8")).items()}
        status, payload = handle_auth(data)
        self._json(status, payload)

    def _handle_route(self):
        raw = self._read_body()
        try:
            payload = json.loads(raw.decode("utf-8"))
        except ValueError:
            return self._json(400, {"action": "reject", "status": 400, "reason": "bad json"})
        result = handle_route(payload) if isinstance(payload, dict) else {
            "action": "reject", "status": 400, "reason": "bad payload"}
        self._json(200, result)

    def _handle_rwi(self):
        try:
            raw = self._read_body()
            print(f"[RWI] body={len(raw)}B", file=sys.stderr, flush=True)
            payload = json.loads(raw.decode("utf-8"))
            print(f"[RWI] parsed ok", file=sys.stderr, flush=True)
            if isinstance(payload, dict):
                handle_rwi(payload)
            print(f"[RWI] handle_rwi done, sending 200", file=sys.stderr, flush=True)
            self._json(200, {"ok": True})
        except Exception as e:
            import traceback
            print(f"[RWI] EXCEPTION: {e}", file=sys.stderr, flush=True)
            traceback.print_exc()
            self._json(500, {"ok": False, "reason": str(e)})

    def do_POST(self):
        print(f"[RAW-POST] path={self.path} headers={dict(self.headers)}", file=sys.stderr, flush=True)
        path = self.path.split("?")[0].rstrip("/")
        if path.endswith("/auth"):
            return self._handle_auth()
        if path.endswith("/rwi"):
            return self._handle_rwi()
        if path.endswith("/events/clear"):
            CCM_CALLS.clear()
            CCM_CALLS_ORDER.clear()
            return self._json(200, {"ok": True})
        return self._handle_route()

    def do_GET(self):
        path = self.path.split("?")[0].rstrip("/")
        if path in ("", "/ui"):
            body = _UI_HTML.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path.endswith("/events"):
            total_events = sum(len(v) for v in CCM_CALLS.values())
            self._json(200, {
                "call_cap": CCM_CALLS_CAP,
                "call_count": len(CCM_CALLS_ORDER),
                "total_events": total_events,
                "calls": {cid: list(CCM_CALLS[cid]) for cid in CCM_CALLS_ORDER},
            })
            return
        # Simple liveness/health probe (used by console HTTP-router "test").
        self._json(200, {"status": "ok", "extensions": list(USERS)})

    def log_message(self, fmt, *args):
        print(f"[http-router] {self.address_string()} " + (fmt % args), file=sys.stderr)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", default=os.environ.get("ROUTER_LISTEN", "127.0.0.1:9090"))
    args = parser.parse_args()
    host, _, port = args.listen.rpartition(":")
    # Push trunk registrations into the rustpbx console DB and hot-reload so
    # inbound trunk matching (no auth for FS-originated calls) is active even
    # on a fresh deployment.
    sync_trunks_to_pbx()
    server = ThreadingHTTPServer((host, int(port)), RouterHandler)
    print(f"[http-router] listening on {host}:{port}", file=sys.stderr)
    print(f"[http-router] rustpbx console API -> {RUSTPBX_HTTP}", file=sys.stderr)
    print(f"[http-router] managed extensions -> {list(USERS)}", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()