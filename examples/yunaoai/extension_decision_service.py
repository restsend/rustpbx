#!/usr/bin/env python3
"""
RustPBX external HTTP service — routing + user/password management.
中文名：分机注册服务（extension_decision）。
Standalone example service (no 3rd-party deps).

This single service owns two jobs for RustPBX (both are built-in features,
no rust changes required):

 1) HttpCallRouter (`[proxy.http_router]`) -> POST /route
    Decides the routing for every call instead of static route files.
    It knows the extension table (see config/extension.json) and asks the PBX
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

Config (gw + ext tables live outside the code):
    config/extension.json next to this script (override with the
    EXTENSION_CONFIG env var) holds the extension table ("extensions":
    {extension: password}), the trunk table ("trunks", synced into the
    rustpbx database at startup, see below) and the outbound trunk name
    ("outbound_trunk"). POST /config/reload re-reads the file (and
    re-syncs trunks) without a restart. No embedded copy: a bad/missing
    file aborts startup rather than serving an empty dialplan.

Usage:
    python3 examples/yunaoai/extension_decision_service.py [--listen 127.0.0.1:9090]

    RUSTPBX_TOKEN below is hard-coded and must match the
    [[console.api_tokens]].token value in config.toml.
"""

import argparse
import ipaddress
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
# Extension + trunk tables live in config/extension.json (extracted out of
# the code — single source of truth for /route "does the number exist",
# /auth digest passwords, and the trunk registry synced into rustpbx).
# Populated by load_extension_config() at startup and on POST /config/reload.
# --------------------------------------------------------------------------- #
USERS: dict[str, str] = {}
TRUNKS: dict[str, dict] = {}

RUSTPBX_HTTP = os.environ.get("RUSTPBX_HTTP", "http://127.0.0.1:8080")
# Bearer token for the console API — must match [[console.api_tokens]] in
# config.toml (kept unscoped = full/superuser access). Hard-coded on purpose.
RUSTPBX_TOKEN = "nmb3BC7PERvThZhm6bL63B8wjD29pVz_"
# Local realm the PBX rewrites inbound trunk callees to (matches route rule to.host).
LOCAL_REALM = "localhost"
# Outbound default trunk, referenced BY NAME via structured targets
# ({"callee", "trunk"}): RustPBX resolves the trunk from its registry
# (synced at startup, see sync_trunks_to_pbx) and applies the full trunk
# config. Value comes from config/extension.json ("outbound_trunk").
OUTBOUND_TRUNK = ""
# gw + ext 配置文件：默认与本脚本同目录的 config/extension.json，
# 可用环境变量 EXTENSION_CONFIG 覆盖。
EXTENSION_CONFIG = os.environ.get("EXTENSION_CONFIG") or os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "config", "extension.json")


def trunk_target(name: str, user: str):
    """Resolve a trunk BY NAME to a structured route `targets` entry.

    http_router targets support {callee, trunk} structured objects: the
    callee is qualified against the trunk dest and RustPBX applies the full
    trunk config (next-hop/transport/credentials, SDP external_ip, ringback,
    header passthrough, CPS/concurrency gates, CDR attribution). The trunk
    must exist in the rustpbx registry (synced at startup, see
    sync_trunks_to_pbx). Raises KeyError if the trunk is not in TRUNKS.

    Sync failure degrades (SYNC_STATE.ok=False): plain URI target
    `sip:<user>@<dest-host:port>` — the call still routes by host:port,
    without trunk treatment.
    """
    if name not in TRUNKS:
        raise KeyError(name)
    if SYNC_STATE.get("ok"):
        return {"callee": user, "trunk": name}
    dest = TRUNKS[name]["dest"]
    # dest looks like "sip:100.88.0.42:5090" -> strip the "sip:" scheme.
    host_port = dest.split(":", 1)[-1] if ":" in dest else dest
    return f"sip:{user}@{host_port}"
# --------------------------------------------------------------------------- #
# SIP trunk 表（gw）+ 分机表（ext）的真正存放处：config/extension.json，
# 由 load_extension_config() 加载（启动与 POST /config/reload）。
# rustpbx 运行在 database-config 模式（generated_db=true），config/trunks/*
# 文件中继永不加载 —— trunk 必须写入 console 数据库才生效（本服务启动时
# upsert + AMI reload，见 sync_trunks_to_pbx）。
# 同步为先清后加：DELETE 当前网关（同名或同 dest 的旧条目/改名残留）→
# PUT 按配置全新创建（FULL trunk field mapping, see trunk_form_from_trunk):
# dest→sip_server, transport→sip_transport, direction, max_load→max_concurrent,
# max_cps, username/password→auth_*, outbound_proxy, and the SBC keys
# (profile/external_ip/bind_ip/audio_codecs/ringback/media_mode/...) into
# metadata.sbc. 来源准入自动入 ACL：allow_ips/allowed_ips 优先，未配置时从
# dest 主机 IP 推导 → trunk inbound_hosts（绕过全局 ACL + TrunkContext）。
# --------------------------------------------------------------------------- #
def load_extension_config() -> bool:
    """从 EXTENSION_CONFIG（默认 <脚本目录>/config/extension.json）加载
    gw（trunks）+ ext（extensions）表，整体替换模块级 USERS/TRUNKS/
    OUTBOUND_TRUNK。加载或校验失败返回 False 并保留旧配置（/config/reload
    路径；启动路径由 main() 直接退出）。"""
    try:
        with open(EXTENSION_CONFIG, "r", encoding="utf-8") as f:
            cfg = json.load(f)
    except (OSError, ValueError) as exc:
        print(f"[config] failed to load {EXTENSION_CONFIG}: {exc}", file=sys.stderr)
        return False
    extensions = cfg.get("extensions")
    trunks = cfg.get("trunks")
    if not isinstance(extensions, dict) or not all(
            isinstance(k, str) and isinstance(v, str)
            for k, v in extensions.items()):
        print(f"[config] invalid 'extensions' in {EXTENSION_CONFIG} "
              f"(want {{extension: password}})", file=sys.stderr)
        return False
    if not isinstance(trunks, dict) or not all(
            isinstance(k, str) and isinstance(v, dict)
            for k, v in trunks.items()):
        print(f"[config] invalid 'trunks' in {EXTENSION_CONFIG} "
              f"(want {{name: {dest, transport, ...}}})", file=sys.stderr)
        return False
    outbound = cfg.get("outbound_trunk")
    if outbound is None:
        # 未指定时默认取表内第一条 trunk
        outbound = next(iter(trunks), "")
    elif trunks and outbound not in trunks:
        print(f"[config] invalid 'outbound_trunk' '{outbound}' (not in trunks)",
              file=sys.stderr)
        return False
    global USERS, TRUNKS, OUTBOUND_TRUNK
    USERS = dict(extensions)
    TRUNKS = dict(trunks)
    OUTBOUND_TRUNK = outbound
    print(f"[config] loaded {EXTENSION_CONFIG} extensions={len(USERS)} "
          f"trunks={list(TRUNKS)} outbound_trunk={OUTBOUND_TRUNK!r}", file=sys.stderr)
    return True
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
# 出呼声（上游无 X-cid）生成 KPM_<session_id>，其中 session_id 由 RustPBX
# /route 请求直接提供（与该通话 RWI 事件的 session_id 完全一致）。因此
# RWI 事件端可用同样的 KPM_<session_id> 推导出 X-cid，无需任何状态存储。
# 注：http_router Forward 分支返回的 headers 不会进 RWI sip_headers，但
# 本服务不依赖 sip_headers——直接用确定性的 session_id 即可。

# 共享前端（webui.html）：页面已从本文件剥离，与本目录 route_decision_service.py
# 公用同一份（GET / 与 /ui 都出该页面）。数据端点契约：GET /events、
# POST /events/clear（前端 "RustPBX 呼叫事件查看器"）。
WEBUI_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "webui.html")

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


# 网关/trunk 配置键 → SipTrunkForm / metadata.sbc 映射（与
# route_decision_service.py 同源）。支持全部 trunk 字段：
#   表单字段：dest→sip_server、transport→sip_transport、direction（默认
#   bidirectional）、max_load→max_concurrent（-1 不限省略）、max_cps、
#   username/password→auth_username/auth_password、outbound_proxy、
#   register_enabled/register_expires
#   metadata.sbc（form.metadata 整体替换 —— 必须组装完整 {"sbc": {...}}）：
#   external_ip/profile/bind_ip/audio_codecs/video_codecs/ringback/
#   media_mode/video_policy/header_passthrough/recording_enabled/ice_lite
# 注：rewrite_hostport 无表单字段（DB 列，创建/覆盖保持现值）
GATEWAY_SBC_KEYS = (
    ("external_ip", "external_ip"),
    ("profile", "profile"),
    ("bind_ip", "bind_ip"),
    ("codec", "audio_codecs"),
    ("audio_codecs", "audio_codecs"),
    ("video_codecs", "video_codecs"),
    ("ringback", "ringback"),
    ("media_mode", "media_mode"),
    ("video_policy", "video_policy"),
    ("header_passthrough", "header_passthrough"),
    ("call_id_mode", "call_id_mode"),
    ("cac_policy", "cac_policy"),
    ("overflow_threshold", "overflow_threshold"),
)

# 同步状态（/health 可见）：ok=False 时 trunk_target 降级纯 URI targets。
SYNC_STATE = {"ok": False, "at": 0.0, "detail": "not run"}
TRUNK_SYNC_ATTEMPTS = 3     # 启动同步重试次数（覆盖 RustPBX 晚于本服务就绪的顺序差）


def trunk_form_from_trunk(name: str, cfg: dict) -> dict:
    """TRUNKS 条目 → SipTrunkForm（console PUT/PATCH /api/sip-trunk 请求体），
    支持全部 trunk 字段。来源准入自动入 ACL：显式 allow_ips/allowed_ips/
    inbound_hosts 优先，未配置时从 dest 主机 IP 推导（同一台网关 Box，来话
    来源 = 出话目的，等价 rtmsip sbc_address）—— trunk inbound_hosts 会进
    acl_inbound_trunks 前缀树，命中即绕过全局 ACL 并标记 TrunkContext，
    所有网关自动 ACL。"""
    form = {
        "name": name,
        "display_name": cfg.get("display_name") or name,
        "sip_server": cfg.get("dest", ""),
        "sip_transport": cfg.get("transport") or "udp",
        "direction": cfg.get("direction") or "bidirectional",
        "is_active": cfg.get("enabled") is not False,
        "register_enabled": cfg.get("register_enabled") or False,
    }
    for src, dst in (
        ("description", "description"),
        ("max_cps", "max_cps"),
        ("max_call_duration", "max_call_duration"),
        ("outbound_proxy", "outbound_proxy"),
        ("username", "auth_username"),
        ("password", "auth_password"),
        ("register_expires", "register_expires"),
    ):
        if cfg.get(src) is not None:
            form[dst] = cfg[src]
    # 并发上限：max_load（-1/缺省 = 不限，省略字段）
    max_load = cfg.get("max_load")
    if max_load is not None and int(max_load) >= 0:
        form["max_concurrent"] = int(max_load)
    acl_sources = None
    for key in ("allowed_ips", "inbound_hosts", "allow_ips"):
        val = cfg.get(key)
        if isinstance(val, list) and val:
            acl_sources = [str(v) for v in val]
            break
        if isinstance(val, str) and val.strip():
            acl_sources = [v.strip() for v in val.split(",") if v.strip()]
            break
    if acl_sources is None:
        dest = cfg.get("dest") or ""
        host = dest.split(":", 1)[-1] if ":" in dest else dest
        host = host.split("@")[-1].split(";")[0].split(":", 1)[0].strip()
        try:
            ip = ipaddress.ip_address(host)
            acl_sources = [f"{ip}/{32 if ip.version == 4 else 128}"]
        except ValueError:
            # 域名 dest 无法推导（对端源 IP 不可知），须显式配置 allowed_ips
            print(f"[trunk-sync] trunk '{name}' dest '{dest}' host '{host}' is "
                  f"not an IP — no auto ACL", file=sys.stderr)
    if acl_sources:
        form["allowed_ips"] = ",".join(acl_sources)
    # 其余 SBC 类键 → metadata.sbc（form.metadata Some 时整体替换，必须
    # 组装完整 metadata JSON；recording→recording_enabled、ice_lite 一并入库）
    sbc = {}
    for src, dst in GATEWAY_SBC_KEYS:
        val = cfg.get(src)
        if val is not None:
            sbc[dst] = val
    if cfg.get("recording") is not None:
        sbc["recording_enabled"] = cfg["recording"]
    if cfg.get("ice_lite") is not None:
        sbc["ice_lite"] = bool(cfg["ice_lite"])
    if sbc:
        form["metadata"] = json.dumps({"sbc": sbc}, ensure_ascii=False)
    return form


def _dest_host_port(dest: str) -> str:
    """'sip:100.88.0.42:5090' → '100.88.0.42:5090'（同 dest 比对用）。"""
    s = (dest or "").split(";", 1)[0].strip()
    if s.lower().startswith("sips:"):
        s = s[5:]
    elif s.lower().startswith("sip:"):
        s = s[4:]
    return s.split("@")[-1]


def _existing_trunks() -> dict:
    """Return {trunk_name: db_item} for trunks currently held in the rustpbx
    database (all pages; only real DB rows — read-only file snapshots are
    not listed in items)."""
    by_name = {}
    page = 1
    while True:
        data = _console_request(
            "POST", "/api/sip-trunk",
            {"page": page, "per_page": 100, "filters": {}},
        )
        for item in data.get("items", []):
            name = item.get("name")
            if name and name not in by_name:
                by_name[name] = item
        if page >= int(data.get("total_pages") or 1):
            break
        page += 1
    return by_name


def sync_trunks_to_pbx() -> dict:
    """
    Push the TRUNKS table into the rustpbx console database — clear then
    re-add: DELETE the current trunks matching by name or by dest (same
    peer host:port — renamed leftovers are cleaned up too), then create
    them fresh from the table, then hot-reload (AMI /reload/trunks: DB →
    trunks.generated.toml → registry) so inbound trunk matching (no auth
    for FS-originated calls) and the auto-ACL (inbound_hosts) pick up any
    changes. Safe to run at every startup (idempotent). Failures are
    non-fatal: SYNC_STATE is updated (/health shows it) and trunk_target
    degrades to plain URI targets.
    """
    if not TRUNKS:
        SYNC_STATE.update(ok=True, at=time.time(), detail="no trunks in table")
        return SYNC_STATE
    try:
        existing = _existing_trunks()
    except Exception as exc:
        SYNC_STATE.update(ok=False, at=time.time(), detail=f"list failed: {exc}")
        print(f"[trunk-sync] skipped (console unreachable): {exc}", file=sys.stderr)
        return SYNC_STATE

    # 清掉当前网关：与配置网关同名，或 dest 指向同一台对端（改名残留）
    cfg_dests = {_dest_host_port(cfg.get("dest", "")) for cfg in TRUNKS.values()}
    deleted, created, failed = [], [], []
    skip_create = set()
    for name, item in existing.items():
        item_dest = _dest_host_port(item.get("sip_server") or "")
        if name not in TRUNKS and not (item_dest and item_dest in cfg_dests):
            continue
        trunk_id = item.get("id")
        if trunk_id is None:
            continue
        try:
            _console_request("DELETE", f"/api/sip-trunk/{trunk_id}")
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode("utf-8", errors="replace")
            failed.append(f"{name}: delete HTTP {exc.code}")
            print(f"[trunk-sync] delete '{name}' failed HTTP {exc.code}: {detail}",
                  file=sys.stderr)
            # 删除失败时不再重建（同名牌仍在，必撞唯一约束）
            if name in TRUNKS:
                skip_create.add(name)
            continue
        except Exception as exc:
            failed.append(f"{name}: delete {exc}")
            print(f"[trunk-sync] delete '{name}' error: {exc}", file=sys.stderr)
            if name in TRUNKS:
                skip_create.add(name)
            continue
        deleted.append(name)
        print(f"[trunk-sync] cleared trunk '{name}' (id={trunk_id})", file=sys.stderr)

    for name, cfg in TRUNKS.items():
        if name in skip_create:
            continue
        body = trunk_form_from_trunk(name, cfg)
        try:
            resp = _console_request("PUT", "/api/sip-trunk", body)
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode("utf-8", errors="replace")
            failed.append(f"{name}: HTTP {exc.code}")
            print(f"[trunk-sync] create '{name}' failed HTTP {exc.code}: {detail}",
                  file=sys.stderr)
            continue
        except Exception as exc:
            failed.append(f"{name}: {exc}")
            print(f"[trunk-sync] create '{name}' error: {exc}", file=sys.stderr)
            continue
        created.append(name)
        print(f"[trunk-sync] created trunk '{name}' -> {resp}", file=sys.stderr)

    if created or deleted:
        # 强制生效（AMI reload：DB → trunks.generated.toml → 注册表）
        try:
            reload = _console_request("POST", "/ami/v1/reload/trunks", {})
            print(f"[trunk-sync] hot-reload: {reload}", file=sys.stderr)
        except Exception as exc:
            print(f"[trunk-sync] reload after sync failed: {exc}", file=sys.stderr)

    ok = not failed
    detail = f"deleted={deleted} created={created} failed={failed}"
    SYNC_STATE.update(ok=ok, at=time.time(), detail=detail)
    print(f"[trunk-sync] done ok={ok} {detail}", file=sys.stderr)
    return SYNC_STATE


def sync_trunks_with_retry() -> None:
    """启动同步：重试数次，覆盖 RustPBX 晚于本服务就绪的顺序差。"""
    for attempt in range(1, TRUNK_SYNC_ATTEMPTS + 1):
        state = sync_trunks_to_pbx()
        if state.get("ok"):
            return
        if attempt < TRUNK_SYNC_ATTEMPTS:
            time.sleep(2)
    print(f"[trunk-sync] gave up after {TRUNK_SYNC_ATTEMPTS} attempts — "
          f"targets degrade to plain URIs", file=sys.stderr)


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
    # 用与 /route 阶段相同的 KPM_<session_id> 确定性推导，无需状态。
    sip_headers = payload.get("sip_headers")
    xcid = None
    if isinstance(sip_headers, dict):
        xcid_key = next((k for k in sip_headers if k.lower() == "x-cid"), None)
        xcid = sip_headers.get(xcid_key) if xcid_key else None
    xcid = xcid or f"KPM_{session_id}"

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
    #   - 无 X-cid → 本节点首次见到 → 自己生成 KPM_<id> → dir="outbound"
    #   - 有 X-cid → 跨节点转发过来的 → 沿用 → dir="inbound"
    # 这里结合 RustPBX direction 做交叉判定：上游真带了 X-cid 强制 inbound，
    # 否则按 direction 映射（等价"首次进入"语义）。
    call_id = payload.get("call_id") or ""
    # X-cid 统一以 session_id 为基准（RustPBX 在 /route 请求里直接携带，
    # 与 RWI 事件的 session_id 完全一致）。下游 RWI 端用同样的
    # KPM_<session_id> 推导，两端天然恒等，无需维护任何映射/状态。
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
        xcid = f"KPM_{session_id}"

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
        if path.endswith("/config/reload"):
            # 重读 extension.json；成功则把（可能变化的）trunk 表重新
            # upsert 进 rustpbx 并 AMI reload 生效。失败保留旧配置。
            ok = load_extension_config()
            state = sync_trunks_to_pbx() if ok else {}
            return self._json(200 if ok else 500, {
                "ok": ok,
                "extensions": list(USERS),
                "trunks": list(TRUNKS),
                "outbound_trunk": OUTBOUND_TRUNK,
                "trunk_sync": state.get("detail") if ok else None,
            })
        if path.endswith("/events/clear"):
            CCM_CALLS.clear()
            CCM_CALLS_ORDER.clear()
            return self._json(200, {"ok": True})
        return self._handle_route()

    def do_GET(self):
        path = self.path.split("?")[0].rstrip("/")
        if path in ("", "/ui"):
            with open(WEBUI_PATH, "rb") as f:
                body = f.read()
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
        # Simple liveness/health probe (used by console HTTP-router "test");
        # service_name 供共享前端动态显示服务名（中文名：分机注册服务）。
        self._json(200, {"status": "ok", "service_name": "分机注册服务",
                         "extensions": list(USERS),
                         "trunk_sync_ok": SYNC_STATE.get("ok"),
                         "trunk_sync_detail": SYNC_STATE.get("detail")})

    def log_message(self, fmt, *args):
        print(f"[http-router] {self.address_string()} " + (fmt % args), file=sys.stderr)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", default=os.environ.get("ROUTER_LISTEN", "127.0.0.1:9090"))
    args = parser.parse_args()
    host, _, port = args.listen.rpartition(":")
    # gw + ext tables come from config/extension.json — required (no
    # embedded copy): abort rather than serve an empty dialplan.
    if not load_extension_config():
        print(f"[main] no usable config at {EXTENSION_CONFIG} — exiting", file=sys.stderr)
        sys.exit(1)
    # Push trunk registrations into the rustpbx console DB and hot-reload so
    # inbound trunk matching (no auth for FS-originated calls) and the
    # auto-ACL (inbound_hosts) are active even on a fresh deployment.
    sync_trunks_with_retry()
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