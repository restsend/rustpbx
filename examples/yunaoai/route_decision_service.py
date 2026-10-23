#!/usr/bin/env python3
"""
kubeTrunk 中继路由服务（route_decision）参考实现 —— 独立 HTTP 服务。

对应《基于RustPBX的kubeTrunk服务(RTM)技术方案》§4.3：以独立 HTTP 服务
实现 kubeTrunk 分层路由裁决，作为后续 KubeTrunkDaemon（Java）落地时的
裁决语义/配置模型参考原型；仅依赖标准库，无第三方依赖。

【配置强制外部化】本脚本【不内置任何默认配置】：启动必须以 ROUTE_CONFIG
（或 --config）指定 JSON 配置文件，否则直接退出（fail-fast）。避免内置
兜底配置在误启/忘配时悄悄接管路由。号段表（aaa_phone_prefix，cfg_phone_prifix
等价）为独立文件：与主配置同目录 aaa_phone_prefix.json。

配置模型（{sitecode}.json，以 route_group 为核心，字段对齐 rtm-web-lite
RULES_MAP 前端生产数据）：
    gateways          网关表：gw_list 的出口解析目标。分三种类型（关键特性）：
                        · IP 对等连接型：dest=sip:<host>:<port>，双端直接对接
                        · 主动注册型：dest=sip:<host>:<port> + register_enabled/
                          username/password，本端主动注册出去，正常同步 trunk
                        · 被动注册型：dest=sip:0.0.0.0（对端注册进本端即本机
                          分机）；网关名=分机号、password=入呼注册密码，自动
                          进 USERS 分机账本，不进 trunk 注册表
    caller_groups     主叫组（dr_groups 等价）：组内为主叫号码/前缀记录
    route_points      路由点位（dr_carriers 等价，核心）：point_id 标记、
                      rule_list=入口过滤（主叫过滤 + 被叫前缀），gw_list=出口
    internal_services 已知内部发起 INVITE 的服务（第 4 层 UA 识别）
    area_codes        区号表（cfg_phone_prifix 等价）：手机号前 7 位 → 区号
    webrtc            WebRTC 匿名呼叫段（向 route_points 看齐的列表模型）：
                      一条 = 一个账号组点位，account/accounts 多账号、
                      rule_list 白名单入口（每条 rule 独立 prefix/strip/
                      pri_prefix）、gw_list 出口多网关、allowed_ips 来源准入

WebRTC JWT 匿名呼叫（方案 B，HttpTokenAuthBackend）：
    预设的 WebRTC 账号（webrtc 列表条目内的 account/accounts，默认单账号 8899）
    不存静态密码；呼叫前由 POST /token/webrtc 现场签发短期 JWT 作为"密码"，
    经 X-Auth-Token 头随 REGISTER/INVITE 携带。JWT 特性：HS256 签名、userId
    绑定 WebRTC 账号、exp 短时效（默认 60s，由服务端统一控制，客户端不可
    指定）、jti 一次性防重放（/auth 消费后登记，重放拒绝）。WebRTC 外呼按
    命中的 webrtc 条目决策：先过 allowed_ips 来源准入 → 仅放行 rule_list
    白名单前缀并完全跳过 L1~L4 路由决策（不允许其他路由兜底）→ 命中后
    做该 rule 的 strip/pri_prefix 号码变换 → 转发到该条目 gw_list 出口
    （多网关）。

接入方式（RustPBX 现有协议，无需改 Rust 代码）：
    [proxy.http_router]
    url = "http://127.0.0.1:9612/route/decision"
    timeout_ms = 500
    fallback_to_static = false      # fail-closed 全动态路由（方案 §4.1）

请求体（HttpCallRouter，src/proxy/routing/http.rs）：
    { "call_id", "session_id", "from", "to", "source_addr",
      "direction": "inbound"|"internal", "method": "INVITE", "uri",
      "headers", "body" }

响应体（HttpResponsePayload）：
    forward: { action, targets: [{"callee": "..", "trunk": ".."} | URI],
               strategy: "sequential", caller?, headers?, allow_codecs?,
               timeout?, max_ring_time?, media_proxy?, rtp_config? }
    reject:  { action: "reject", status, reason }

网关 → trunk 注册表自动同步（附加能力）：启动时（与 POST /config/reload）
先把 RustPBX trunk 数据库里旧的网关条目清理（同名/同 dest 旧条目、改名残留
一并删除），再按配置全新写入（console API Bearer 认证，PUT 创建）+ AMI
POST /ami/v1/reload/trunks 强制生效。仅"非被动型"网关参与同步；被动型网关
（分机）不进 trunk 注册表，其历史同名 trunk 一并清理。网关名即 trunk 名；
同步失败非致命：targets 降级纯 URI（呼叫仍可达，trunk 盖章不生效），
/health 可见同步状态。

裁决流程（分层匹配，2026-10-16 设计确认，仅普通出局呼叫走此流程；WebRTC
呼叫不走，见上）：
    1. 主被叫都符合：主叫命中规则主叫过滤 且 被叫前缀命中
    2. 被叫符合：无主叫过滤的规则中被叫前缀命中（跨主叫）
    3. 主叫符合：主叫命中空前缀规则（组内兜底，等价 rtmsip noprefix）
       —— 层内：被叫前缀最长 → priority 降序 → 主叫记录最长 → 配置顺序
    4. 默认路由（主被叫均未命中）：按发起方 User-Agent 识别内部服务
       （internal_services，防白名单来源伪造 UA 走默认出局）→ internal/
       external 兜底组；兜底组内按被叫前缀选规则
    5. 号码变换：strip 剥码、pri_prefix 出局码加被叫前、区号加 0、主叫
       sitecode 循环剥离
    6. targets：出口 gw_list → 结构化 targets + strategy="sequential"

呼叫跟踪（附加能力）：[rwi_webhook] url 指向本服务 POST /rwi，接收 RustPBX
推送的 RWI 呼叫事件（call_created/call_hangup/call_error 等，2xx 送达、event_id
幂等），按 call_id 与路由决策聚合；GET /trace/<call_id> 查询单次呼叫跟踪。

事件链展示（与 extension_decision_service.py 一致）：/rwi 同时把事件归一成
CCM 风格存入 CCM_CALLS，GET /events 供 webui.html 轮询展示；共享前端
GET / 与 /ui 出同一份页面，POST /events/clear 清空；WebRTC 呼叫前端
GET /webrtc（webrtc_phone.html），同源 JsSIP 于 GET /static/jssip.min.js。

用法：
    python3 examples/yunaoai/route_decision_service.py --config examples/yunaoai/config/3201.json
    ROUTE_CONFIG=/path/to/{sitecode}.json python3 examples/yunaoai/route_decision_service.py
"""

import argparse
import base64
import hashlib
import hmac
import ipaddress
import json
import os
import re
import secrets
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# 路由配置（{sitecode}.json 等价物）一律由外部文件提供，本脚本【不内置任何
# 默认配置】：启动时必须以 ROUTE_CONFIG（或 --config）指定 JSON 文件，否则
# 直接退出（fail-fast），避免内置兜底配置在误启/忘配时悄悄接管路由。
# 以 route_group 为核心：
#
#   gateways          网关表（sbc_dr_gateways 等价）：gw_list 的出口解析目标
#   caller_groups     主叫组（sbc_dr_groups 等价）：组内为主叫号码/前缀记录
#   route_points      路由点位（sbc_dr_carriers 等价，核心）：point_id 标记、
#                     rule_list=入口过滤（主叫过滤 + 被叫单前缀），gw_list=出口
#   internal_services 已知内部发起 INVITE 的服务（第 4 层 UA 识别）
#   area_codes        区号表（cfg_phone_prifix 等价）：手机号前 7 位 → 区号
#   webrtc            向 route_points 看齐的列表：一条 = 一个账号组点位，
#                     account/accounts 多账号、rule_list 白名单入口、
#                     gw_list 出口多网关、allowed_ips 来源准入、
#                     strip + pri_prefix 号码变换

# 生效配置（load_config() 填充）。模块级单例：与 KubeTrunkDaemon 内存裁决
# 配置一致的形态（方案 §4.3 "全动态：内存裁决，无静态兜底"）。
CONFIG: dict = {}
# 当前装载配置文件路径（/config/reload 复用；用 --config 启动时 ROUTE_CONFIG
# 环境变量可能为空，必须记下确切实参）。
CONFIG_FILE: str = ""

# 分机账本（{extension: password}）：来自 gateways 表中"被动注册型"网关
# （dest=sip:0.0.0.0，对方注册进来即本机分机），供 `/auth`（HttpUserBackend，
# REGISTER/INVITE Digest 鉴权）使用。load_config()/reload 时经
# `_apply_extensions()` 从 cfg["gateways"] 同步。
USERS: dict[str, str] = {}
# 本地 realm：RustPBX /auth 应答里返回，PBX 用它做 Digest 挑战域。
LOCAL_REALM = "localhost"

# ---- WebRTC 会话（方案 B：HttpTokenAuthBackend 一次性 JWT） ----
# WebRTC 账号：一个预设的特殊电话号码，自身不存静态密码；WebRTC 呼叫时由
# POST /token/webrtc 现场签发短期 JWT 作为"密码"。RustPBX 在 REGISTER/
# INVITE 检测到 X-Auth-Token 时不再发 401/407，而是把 token 转发给本服务
# /auth（校验该字段），认证即通过。JWT 声明：userId=WebRTC 账号、exp=有效期、
# jti=一次性 nonce（/auth 消费后记入 KEYED_NONCES 防重放）。
WEBRTC_ACCOUNT = os.environ.get("RUSTPBX_WEBRTC_ACCOUNT", "8899")
JWT_SECRET = os.environ.get("RUSTPBX_JWT_SECRET", "change-me-webrtc-jwt-secret")
JWT_ISSUER = os.environ.get("RUSTPBX_JWT_ISSUER", "yunaoai-webrtc")
JWT_AUDIENCE = os.environ.get("RUSTPBX_JWT_AUDIENCE", "rustpbx-webrtc")
# WebRTC JWT 默认存活秒数，默认 60s（覆盖 1 次 REGISTER + 1 次 INVITE 窗口即可；
# 且配合下方 KEYED_NONCES 的 jti 一次性防重放，保证"短时效 + 单次有效"）。
JWT_MAX_AGE = int(os.environ.get("RUSTPBX_JWT_MAX_AGE", "60"))
# 一次性 nonce 防重放登记表（{jti: 消费时间戳}），上限 KEYED_ONCE_MAX 条。
KEYED_NONCES: dict[str, float] = {}
KEYED_ONCE_MAX = 10000
# X-Auth-Token 转发到 /auth 的 form 字段名（= config.toml sip_headers 元素）。
WEBRTC_TOKEN_FIELD = "X-Auth-Token"


def _b64url_decode(s: str) -> bytes:
    """Base64URL 解码，RFC 7515 规范补齐 padding。失败返回 b""。"""
    pad = "=" * (-len(s) % 4)
    try:
        return base64.urlsafe_b64decode((s + pad).encode("utf-8"))
    except Exception:
        return b""


def _b64url_encode(raw: bytes) -> str:
    """Base64URL 编码（无 padding），RFC 7515。"""
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("utf-8")


def _webrtc_known_accounts(cfg: dict = None) -> set:
    """WebRTC JWT 可用的全部合法账号：各 webrtc 位点（rule.caller_group 组内
    主叫）并集，兜底 WEBRTC_ACCOUNT。方案 A 下账号不再硬编码单个，而是随
    webrtc 段 rule_list 引用的 caller_group 聚合。"""
    cfg = CONFIG if cfg is None else cfg
    known = set()
    for entry in webrtc_entries(cfg):
        known.update(webrtc_entry_callers(cfg, entry))
    if WEBRTC_ACCOUNT:
        known.add(str(WEBRTC_ACCOUNT))
    return known


def decode_jwt(token: str):
    """解析并校验 HS256 JWT，返回 (claims_dict | None)。

    校验内容：三段结构、HS256 签名（共享 JWT_SECRET）、exp/tiat/nbf、
    iss/aud 白名单、WebRTC 账号绑定（userId 必须属于配置中各 webrtc 位点的
    合法账号集合，见 _webrtc_known_accounts）。任何一步失败返回 None。"""
    if not token or not isinstance(token, str):
        return None
    try:
        header_b64, payload_b64, sig_b64 = token.split(".")
    except ValueError:
        return None
    header = _b64url_decode(header_b64)
    payload = _b64url_decode(payload_b64)
    signature = _b64url_decode(sig_b64)
    if not header or not payload or not signature:
        return None
    try:
        header = json.loads(header)
        claims = json.loads(payload)
    except ValueError:
        return None
    # 仅接受 HS256 签名算法（防 alg='none'/HS384 混淆攻击）。
    if header.get("alg") != "HS256":
        return None
    signing_input = f"{header_b64}.{payload_b64}".encode("utf-8")
    expect = hmac.new(JWT_SECRET.encode("utf-8"), signing_input,
                      hashlib.sha256).digest()
    if not hmac.compare_digest(signature, expect):
        return None
    now = time.time()
    if claims.get("exp") is not None and float(claims["exp"]) <= now:
        return None
    if claims.get("nbf") is not None and float(claims["nbf"]) > now + 60:
        return None
    if claims.get("iat") is not None and now - float(claims["iat"]) > JWT_MAX_AGE * 2:
        return None
    if JWT_ISSUER and claims.get("iss") != JWT_ISSUER:
        return None
    if JWT_AUDIENCE and claims.get("aud") != JWT_AUDIENCE:
        return None
    if str(claims.get("userId") or "") not in _webrtc_known_accounts():
        return None
    return claims


def mint_webrtc_jwt(ttl: int = JWT_MAX_AGE, account: str = None) -> dict:
    """签发现场一次性 WebRTC JWT（HS256）。返回 {token, account, expires_in}。

    account 未指定时取配置里第一个可用 WebRTC 账号（向后兼容 WEBRTC_ACCOUNT）。
    有效期固定由 ttl（服务端）决定，不接受客户端指定。"""
    if not account:
        account = default_webrtc_account()
    account = str(account).strip()
    now = int(time.time())
    jti = secrets.token_hex(12)
    claims = {
        "userId": account, "sub": account,
        "iss": JWT_ISSUER, "aud": JWT_AUDIENCE,
        "jti": jti, "iat": now, "exp": now + ttl,
    }
    header = json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode()
    payload = json.dumps(claims, separators=(",", ":")).encode()
    signing_input = f"{_b64url_encode(header)}.{_b64url_encode(payload)}"
    sig = hmac.new(JWT_SECRET.encode("utf-8"), signing_input.encode(),
                   hashlib.sha256).digest()
    token = f"{signing_input}.{_b64url_encode(sig)}"
    return {"token": token, "account": account,
            "expires_in": ttl, "expires_at": now + ttl}


def consume_webrtc_nonce(claims: dict) -> bool:
    """一次性 nonce（jti）防重放：已消费则拒绝，未消费则登记。

    返回 True 表示首次使用允许；False 表示重放。登记表超上限时只清老化项。
    """
    jti = str(claims.get("jti") or "")
    if not jti:
        return False
    now = time.time()
    if jti in KEYED_NONCES:
        return False
    if len(KEYED_NONCES) >= KEYED_ONCE_MAX:
        cutoff = now - JWT_MAX_AGE * 4
        for k in [k for k, v in KEYED_NONCES.items() if v < cutoff]:
            KEYED_NONCES.pop(k, None)
    KEYED_NONCES[jti] = now
    return True


def is_passive_gateway(gw: dict) -> bool:
    """判定"被动注册型"网关：无真实对端地址（dest 为空，或 sip:0.0.0.0）。

    网关分三种类型：
      - IP 对等：dest=sip:host:port，双端直接对接（如 yacc55-fs / lvcx）；
      - 主动注册：dest=sip:host:port + register_enabled/username/password，
        本端注册出去（如 ya-pbx）；
      - 被动注册：dest=sip:0.0.0.0（或留空），对端注册进本端 → 本服务扫描到
        后把它解析为该网关名对应的分机号（进 USERS 账本），供入呼鉴权与
        出口转发使用，且不进 RustPBX trunk 注册表。
    """
    dest = ((gw or {}).get("dest") or "").strip()
    return not dest or dest == "sip:0.0.0.0"


def _apply_extensions(cfg: dict) -> None:
    """把 gateways 表中"被动注册型"网关解析成本地分机账本 USERS。

    不再使用独立的 `extensions` 字段：被动注册型网关（dest=sip:0.0.0.0）
    即代表一台对方注册进来的分机。分机号优先取网关内的 `username` 字段
    （对方实际注册时用的用户名）；缺失则 fallback 到网关名。网关 password
    = 入呼注册密码。/auth（分机鉴权）与路由裁决共用同一份配置单例，
    /config/reload 热加载后账本一并更新。"""
    USERS.clear()
    for name, gw in (cfg.get("gateways") or {}).items():
        if is_passive_gateway(gw):
            ext = str(gw.get("username") or name).strip()
            USERS[ext] = str(gw.get("password") or "")
    if USERS:
        log("auth", f"被动注册分机账本已同步: {len(USERS)} 个 {sorted(USERS)}")

# 呼叫跟踪：路由决策 + RWI 呼叫事件按 call_id 聚合（内存环形上限）。
# RustPBX [rwi_webhook] url 指向本服务 POST /rwi，推送 call_created/
# call_hangup/call_error 等生命周期事件（协议：{rwi, event_id, timestamp,
# call_id, event_type, event{...}}，2xx 即送达，event_id 幂等去重）；
# GET /trace/<call_id> 查询单次呼叫的 决策+事件 合并跟踪记录。
TRACE: dict = {}            # call_id -> [record]
TRACE_SESSION: dict = {}    # session_id -> call_id（RWI 事件 envelope 的 call_id
                            #   是 RFC 7989 session_id，需二次索引关联）
TRACE_CORR: dict = {}       # corr_id/xcid -> call_id（决策 corr_id 与 RWI 事件 xcid
                            #   同值桥接 —— WebRTC/JsSIP 的随机 Call-ID 与 RustPBX 的
                            #   session 号不同，靠 X-cid 这一对同值把它们并入同一卡片）
CALL_XCID: dict = {}        # call_id -> xcid（决策阶段 /route 沿用网页 X-CID 确立，
                            #   供后续每条 RWI 事件沿用；防 200/bye 等事件头缺 X-CID
                            #   时掉回 KTM_ 造成同呼叫 xcid 跳变）
TRACE_MAX_CALLS = 2000      # 最多保留的呼叫数（内存保护）
TRACE_MAX_RECORDS = 64      # 单呼叫最多记录数


def trace_record(call_id: str, kind: str, session_id=None, **fields) -> None:
    """按 call_id 聚合跟踪记录（kind=decision 路由决策 / kind=rwi 呼叫事件）。

    session_id 可选：RWI 事件 envelope 的 call_id 是 RustPBX 的 RFC 7989
    session_id（与 /route/decision payload 的 session_id 同值），登记映射后
    /trace/<任意一侧 id> 均可查到同一份跟踪记录。"""
    if not call_id:
        return
    if session_id and session_id != call_id:
        TRACE_SESSION[session_id] = call_id
    # corr_id（= 出口 X-cid，与 RWI 事件 xcid 同值）登记桥接，供 /events 和
    # /trace 按 corr_id/xcid 把决策与事件并入同一呼叫。
    corr = fields.get("corr_id")
    if corr:
        TRACE_CORR[corr] = call_id
        CALL_XCID[call_id] = corr
    records = TRACE.setdefault(call_id, [])
    records.append({"kind": kind, "ts": time.time_ns() // 1_000,
                    "session_id": session_id, **fields})
    if len(records) > TRACE_MAX_RECORDS:
        records.pop(0)
    if len(TRACE) > TRACE_MAX_CALLS:
        TRACE.pop(next(iter(TRACE)))


# CCM_CALL 事件链（与 examples/yunaoai/extension_decision_service.py 同源实现）：
# /rwi 收到的 RWI 呼叫事件归一成 CCM 风格存入，供共享前端 GET /events 展示。
CCM_CALLS_CAP = 500     # 最多保留最近 N 通完整呼叫（每通 invite→200→bye 链不截断）
CCM_CALLS: dict = {}    # callId -> 该通所有事件（按时间序）
CCM_CALLS_ORDER: list = []  # callId 插入序 FIFO，用于淘汰最旧的完整呼叫

# 共享前端（webui.html）：从 extension_decision_service.py 剥离，两服务公用同一份
# （GET / 与 /ui 出该页面，数据端点 GET /events、POST /events/clear）。
WEBUI_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "webui.html")
# WebRTC 呼叫前端（webrtc_phone.html）：GET /webrtc 出该页面。自包含：自动向
# /token/webrtc 申请 JWT，带 X-Auth-Token 完成注册并发起/挂断呼叫。
WEBRTC_PHONE_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "webrtc_phone.html")
# WebRTC 呼叫前端同源 JsSIP（本地化，避免依赖外部 CDN）：GET /static/jssip.min.js
JSSIP_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "static", "jssip.min.js")


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
    log("ccm-event", json.dumps(event, ensure_ascii=False))


def _rwi_payload(incoming: dict) -> dict:
    """展开 RWI webhook 的嵌套 envelope，返回扁平事件体（与
    examples/yunaoai/extension_decision_service.py 同源实现）。

    RustPBX [rwi_webhook] POST 到 /rwi 的实际结构为（rwi/webhook.rs）：
      { "rwi": "1.0", "event_id": "...", "timestamp": "...",
        "call_id": "...",           # 顶层冗余一份 call_id
        "event_type": "call_created",   # 顶层：事件类型唯一真源
        "event": {                  # 真正的扁平 payload（event_type 被摘除）
            "call_id": "...", "caller": "...", "callee": "...",
            "direction": "inbound"|"outbound"|"local", "trunk": "...",
            "session_id": "...", "sip_headers": {...},
            "reason"/"hangup_by"/"sip_status"/"duration_secs": ...  # hangup only
        }
      }
    归一化以嵌套 event 为准，顶层 call_id/event_type 作兜底，
    兼容扁平与嵌套两种投递形态。"""
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
    """接收 RustPBX RWI webhook push（与 examples/yunaoai/extension_decision_service.py
    同一实现）：展开 envelope 后归一成 clpss CCM_CALL 风格存入 CCM_CALLS，
    供共享前端 /events 展示完整事件链（invite→180→200→bye）。"""
    payload = _rwi_payload(payload)
    event_type = payload.get("event") or payload.get("event_type") or ""
    if not event_type:
        log("rwi", f"no event_type in payload: {payload}")
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

    # 呼入场景上游真实带的 X-cid 会出现在原始 INVITE（经 sip_headers 透传），
    # 优先认它；否则用与 /route 阶段相同的 KTM_<session_id> 确定性推导，
    # 无需任何状态存储。
    sip_headers = payload.get("sip_headers")
    header_xcid = None
    if isinstance(sip_headers, dict):
        xcid_key = next((k for k in sip_headers if k.lower() == "x-cid"), None)
        header_xcid = sip_headers.get(xcid_key) if xcid_key else None
    # 网页传入的 X-CID 优先；事件头若未带（如 200/bye），则沿用决策阶段 /route
    # 已确立的同值 xcid，最后才回落到统一的 session_id 确定性推导 —— 保证同呼叫
    # 内所有事件 xcid 不跳变。
    xcid = header_xcid or CALL_XCID.get(call_id) or f"KTM_{session_id}"
    if header_xcid and xcid != f"KTM_{session_id}":
        CALL_XCID[call_id] = header_xcid

    ccm_event = {
        "xcid": xcid,
        "callId": call_id,
        "sessionId": session_id,
        "ani": payload.get("caller") or "",
        "dnis": payload.get("callee") or "",
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


def log(tag: str, msg: str) -> None:
    print(f"[route-decision] {tag} {msg}", file=sys.stderr)


# --------------------------------------------------------------------------- #
# 配置加载与校验（等价 KubeTrunkDaemon 渲染前校验，方案 §5.4）
# --------------------------------------------------------------------------- #
def validate_config(cfg: dict) -> list:
    """加载前校验，返回错误列表（非空则拒绝加载并告警）。"""
    errors = []
    gateways = cfg.get("gateways") or {}
    for name, g in gateways.items():
        if is_passive_gateway(g):
            # 被动注册型（dest 为空 或 sip:0.0.0.0 = 对方注册进来当分机）：
            # 无需真实对端地址，只等对方 REGISTER 进来；必须带入呼密码。
            if not g.get("register_enabled"):
                errors.append(f"passive gateway '{name}' needs register_enabled=true")
            if not str(g.get("password") or "").strip():
                errors.append(f"passive gateway '{name}' missing password (inbound register)")
        elif not str(g["dest"]).startswith("sip:"):
            errors.append(f"gateway '{name}' dest must be a sip: URI: {g['dest']}")

    # 主叫组：记录必须为非空字符串（号码或前缀）
    caller_groups = cfg.get("caller_groups") or {}
    for grp, records in caller_groups.items():
        if not records or not isinstance(records, list):
            errors.append(f"caller_group '{grp}' must be a non-empty list")
            continue
        for rec in records:
            if not str(rec).strip():
                errors.append(f"caller_group '{grp}' has empty record")

    # 路由组：id 唯一、gw_list 引用存在、规则被叫为单前缀（不支持数组）、
    # 主叫组引用存在、default_for 每方向至多一个
    seen_groups = set()
    seen_defaults = set()
    for grp in cfg.get("route_points") or []:
        gid = grp.get("point_id")
        if not gid:
            errors.append("route_point missing point_id")
            continue
        if gid in seen_groups:
            errors.append(f"route_group '{gid}' duplicated")
        seen_groups.add(gid)
        for g in grp.get("gw_list") or []:
            if g not in gateways:
                errors.append(f"route_group '{gid}' references unknown gateway '{g}'")
        dflt = grp.get("default_for")
        if dflt:
            if dflt not in ("internal", "external"):
                errors.append(f"route_group '{gid}' invalid default_for '{dflt}'")
            elif dflt in seen_defaults:
                errors.append(f"route_group '{gid}': duplicate default_for '{dflt}'")
            seen_defaults.add(dflt)
        for i, r in enumerate(grp.get("rule_list") or []):
            prefix = r.get("prefix", "")
            if isinstance(prefix, list):
                # 被叫过滤必须单条前缀记录，不支持数组（与前端数据格式一致）
                errors.append(f"route_group '{gid}' rule_list[{i}] prefix must be a "
                              f"single prefix string, not an array")
            cg = r.get("caller_group")
            if cg and cg not in caller_groups:
                errors.append(f"route_group '{gid}' rule_list[{i}] references unknown "
                              f"caller_group '{cg}'")

    # internal_services：user_agent 缺失的条目无法匹配，拒绝加载
    for i, svc in enumerate(cfg.get("internal_services") or []):
        if not str(svc.get("user_agent") or "").strip():
            errors.append(f"internal_services[{i}] missing user_agent")

    # WebRTC 呼叫（方案 A）：webrtc 段与 route_points 完全同构（point_id/
    # group_type/gw_list/rule_list），仅额外多 allowed_ips 来源准入；不再有
    # account/accounts —— 主叫 → 位点的归属靠规则 rule_list 的 caller_group
    # （组内为主叫号码/前缀记录），走独立白名单路由，不参与 L1~L4 裁决。
    seen_webrtc_points = set()
    for w in webrtc_entries(cfg):
        wid = webrtc_entry_tag(w)
        if wid in seen_webrtc_points:
            errors.append(f"webrtc point '{wid}' duplicated")
        seen_webrtc_points.add(wid)
        if not webrtc_entry_groups(w):
            errors.append(f"webrtc point '{wid}' must define caller_group in "
                          "rule_list (webrtc mirrors route_points)")
        rl = w.get("rule_list")
        if not isinstance(rl, list) or not rl:
            errors.append(f"webrtc point '{wid}' rule_list must be a non-empty list")
        else:
            for i, r in enumerate(rl):
                px = r.get("prefix")
                if not isinstance(px, str) or not str(px).strip():
                    if isinstance(px, list):
                        errors.append(f"webrtc point '{wid}' rule_list[{i}] "
                                      "prefix must be a single string, not a list")
                    else:
                        errors.append(f"webrtc point '{wid}' rule_list[{i}] "
                                      "missing/blank prefix")
                try:
                    int(r.get("strip") or 0)
                except (TypeError, ValueError):
                    errors.append(f"webrtc point '{wid}' rule_list[{i}] "
                                  "strip must be an integer")
                cg = (r.get("caller_group") or "").strip()
                if cg and cg not in caller_groups:
                    errors.append(f"webrtc point '{wid}' rule_list[{i}] references "
                                  f"unknown caller_group '{cg}'")
        aips = w.get("allowed_ips")
        if aips is not None and not isinstance(aips, list):
            errors.append(f"webrtc point '{wid}' allowed_ips must be a list")
        for g in w.get("gw_list") or []:
            if g not in gateways:
                errors.append(f"webrtc point '{wid}' gw_list references "
                              f"unknown gateway '{g}'")
    return errors


def load_phone_prefix(config_path: str) -> dict:
    """号段表（aaa_phone_prefix，cfg_phone_prifix 等价）单独文件：与主配置
    同目录的 aaa_phone_prefix.json（11 位手机号取前 7 位 → 区号）。

    支持两种格式（现网导出文件内容不动、直接解析）：
    - 现网导出（JSON-Lines，每行一条记录）：取记录的 prefix → areaCode
      字段（真实全量号段表，53 万行；由 KTM 从 S3 单独拉取，独立于站点
      路由配置）；
    - 样例格式（单个 string->string 对象 {前缀: 区号}）。
    文件缺失/非法时返回 {}（area_codes 默认空，主配置不再内联该字段）。"""
    if not config_path:
        return {}
    ac_path = os.path.join(os.path.dirname(os.path.abspath(config_path)),
                           "aaa_phone_prefix.json")
    try:
        with open(ac_path, "r", encoding="utf-8") as f:
            content = f.read()
    except OSError as exc:
        log("config", f"phone_prefix file {ac_path} failed: {exc} — fallback inline")
        return {}
    table = None
    try:
        loaded = json.loads(content)
    except ValueError:
        loaded = None
    if isinstance(loaded, dict) and all(
            isinstance(v, str) for v in loaded.values()):
        table = loaded                      # 样例格式：string->string 对象
    else:
        table = {}                          # 现网导出：JSON-Lines 逐行解析
        bad = 0
        for line in content.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except ValueError:
                bad += 1
                continue
            if isinstance(rec, dict) and rec.get("prefix") and rec.get("areaCode"):
                table[str(rec["prefix"])] = str(rec["areaCode"])
            else:
                bad += 1
        if not table:
            log("config", f"phone_prefix file {ac_path} invalid (no usable "
                          f"records) — fallback inline")
            return {}
        if bad:
            log("config", f"phone_prefix {ac_path}: skipped {bad} unusable lines")
    log("config", f"loaded phone_prefix {ac_path} ({len(table)} prefixes)")
    return table


def load_config(path: str = None) -> dict:
    """从外部 JSON 文件加载配置（强制）。

    配置来源：优先显式 path（--config），否则 ROUTE_CONFIG 环境变量。两者
    都未指定/文件缺失/校验非法 → 抛 ValueError（fail-fast，无内置兜底），
    由调用方（main() 启动即退出；/config/reload 返回 4xx 保留旧配置）。

    手机号段表单独文件：同目录 aaa_phone_prefix.json，主配置不再内联
    area_codes 字段（默认空，号段表是唯一来源）。
    等价 rtm-daemon 的 TDB 生成 + dbt_reload 链路收敛为一次文件读取。"""
    path = (path or "").strip() or os.environ.get("ROUTE_CONFIG", "").strip()
    if not path:
        raise ValueError("no route config: set ROUTE_CONFIG or pass --config")
    try:
        with open(path, "r", encoding="utf-8") as f:
            cfg = json.load(f)
    except (OSError, ValueError) as exc:
        raise ValueError(f"failed to load {path}: {exc}") from exc
    # 号段表（aaa_phone_prefix）单独文件，存在则覆盖内联字段
    # 号段表（aaa_phone_prefix）单独文件是唯一来源，主配置不内联（默认空）
    cfg["area_codes"] = load_phone_prefix(path)
    errors = validate_config(cfg)
    if errors:
        raise ValueError("invalid config %s: %s"
                         % (path, "; ".join(errors)))
    log("config", f"loaded {path} sitecode={cfg.get('sitecode')} "
                  f"route_points={len(cfg.get('route_points') or [])} "
                  f"gateways={len(cfg.get('gateways') or {})} "
                  f"extensions={sum(1 for g in (cfg.get('gateways') or {}).values() if is_passive_gateway(g))}")
    _apply_extensions(cfg)
    cfg["_webrtc_table"] = build_webrtc_table(cfg)
    cfg["_route_table"] = build_route_points_table(cfg)
    return cfg


# --------------------------------------------------------------------------- #
# RustPBX trunk 注册表同步（gateways 表 → RustPBX trunk 数据库，自动生效）
#
# 结构化 targets {"callee", "trunk"} 要求网关名是 RustPBX trunk 注册表条目：
# 启动时（与 POST /config/reload 时）把加载配置的 gateways 表全部网关自动
# 同步写入 RustPBX trunk 数据库 —— 先清后加：DELETE 当前网关（同名或同
# dest 的旧条目、网关改名残留一并清掉）→ PUT 按配置全新创建 + AMI
# POST /ami/v1/reload/trunks 强制生效（DB 重生成 trunks.generated.toml +
# 更新注册表）。同步失败非致命：告警后 targets 降级纯 URI（见 gateway_targets）。
# --------------------------------------------------------------------------- #
RUSTPBX_HTTP = os.environ.get("RUSTPBX_HTTP", "http://127.0.0.1:8080")
# console API token（config.toml [[console.api_tokens]]，scopes 空 = 全量权限；
# 可用环境变量覆盖，勿外泄本文件）。
RUSTPBX_TOKEN = os.environ.get("RUSTPBX_TOKEN", "nmb3BC7PERvThZhm6bL63B8wjD29pVz_")
TRUNK_SYNC_ATTEMPTS = 3     # 启动同步重试次数（覆盖 RustPBX 晚于本服务就绪的顺序差）

# 同步状态（/health 可见）：ok=False 时 gateway_targets 降级纯 URI targets。
SYNC_STATE = {"ok": False, "at": 0.0, "detail": "not run"}

# 网关配置键 → SipTrunkForm / metadata.sbc 映射。支持全部 trunk 字段：
#   表单字段：dest→sip_server、transport→sip_transport、direction（默认
#   bidirectional）、max_load→max_concurrent（-1 不限省略）、max_cps、
#   allowed_ips/inbound_hosts、username/password→auth_username/auth_password、
#   outbound_proxy、description、register_enabled/register_expires
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


def trunk_form_from_gateway(name: str, gw: dict) -> dict:
    """网关配置 → SipTrunkForm（console PUT/PATCH /api/sip-trunk 请求体）。"""
    form = {
        "name": name,
        "display_name": gw.get("display_name") or name,
        "sip_server": gw.get("dest", ""),
        "sip_transport": gw.get("transport") or "udp",
        "direction": gw.get("direction") or "bidirectional",
        "is_active": gw.get("enabled") is not False,
        "register_enabled": gw.get("register_enabled") or False,
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
        if gw.get(src) is not None:
            form[dst] = gw[src]
    # 并发上限：max_load（-1/缺省 = 不限，省略字段）
    max_load = gw.get("max_load")
    if max_load is not None and int(max_load) >= 0:
        form["max_concurrent"] = int(max_load)
    # 入呼 ACL（来源准入自动入 ACL）：显式 allowed_ips / inbound_hosts /
    # allow_ips 优先；未配置时从 dest 主机 IP 推导（同一台网关 Box，来话
    # 来源 = 出话目的，等价 rtmsip sbc_address 来源准入）—— trunk
    # inbound_hosts 会进 acl_inbound_trunks 前缀树，命中即绕过全局 ACL 并
    # 标记 TrunkContext（source_trunk + 入呼媒体修复），所有网关自动 ACL。
    acl_sources = None
    for key in ("allowed_ips", "inbound_hosts", "allow_ips"):
        val = gw.get(key)
        if isinstance(val, list) and val:
            acl_sources = [str(v) for v in val]
            break
        if isinstance(val, str) and val.strip():
            acl_sources = [v.strip() for v in val.split(",") if v.strip()]
            break
    if acl_sources is None:
        dest = gw.get("dest") or ""
        host = dest.split(":", 1)[-1] if ":" in dest else dest
        host = host.split("@")[-1].split(";")[0].split(":", 1)[0].strip()
        try:
            ip = ipaddress.ip_address(host)
            acl_sources = [f"{ip}/{32 if ip.version == 4 else 128}"]
        except ValueError:
            # 域名 dest 无法推导（对端源 IP 不可知），须显式配置 allowed_ips
            log("trunk-sync", f"gateway '{name}' dest '{dest}' host '{host}' "
                              f"is not an IP — no auto ACL")
    if acl_sources:
        form["allowed_ips"] = ",".join(acl_sources)
    # 其余 SBC 类键 → metadata.sbc（form.metadata Some 时整体替换，必须
    # 组装完整 metadata JSON；recording→recording_enabled、ice_lite 一并入库）
    sbc = {}
    for src, dst in GATEWAY_SBC_KEYS:
        val = gw.get(src)
        if val is not None:
            sbc[dst] = val
    if gw.get("recording") is not None:
        sbc["recording_enabled"] = gw["recording"]
    if gw.get("ice_lite") is not None:
        sbc["ice_lite"] = bool(gw["ice_lite"])
    if sbc:
        form["metadata"] = json.dumps({"sbc": sbc}, ensure_ascii=False)
    return form


def _console_request(method: str, path: str, payload=None, timeout: int = 5):
    """console API 认证请求（Bearer token），返回解码 JSON。异常向上抛。"""
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


def _dest_host_port(dest: str) -> str:
    """'sip:100.88.0.42:5090' → '100.88.0.42:5090'（同 dest 比对用）。"""
    s = (dest or "").split(";", 1)[0].strip()
    if s.lower().startswith("sips:"):
        s = s[5:]
    elif s.lower().startswith("sip:"):
        s = s[4:]
    return s.split("@")[-1]


def _lookup_contact_hostport(ext: str) -> str:
    """查 RustPBX locator API，拿分机 ext 当前注册的 host:port。

    GET /api/diagnostics/locator/lookup → records[0].destination
    destination 形如 "UDP 100.88.0.204:38890"，抽出末尾 host:port。
    查不到（离线 / 未注册）返回 "" —— 调用方应降级为不带 trunk 的
    {"callee": ext}，让 RustPBX extension_locator 自己处理。"""
    try:
        data = _console_request(
            "POST", "/api/diagnostics/locator/lookup", {"user": ext}, timeout=3,
        )
    except Exception as exc:
        log("locator", f"lookup '{ext}' failed: {exc}")
        return ""
    records = (data or {}).get("records") or []
    if not records:
        return ""
    dest = (records[0].get("destination") or "").strip()
    return dest.split()[-1] if dest else ""


def _existing_trunks() -> dict:
    """现有 trunk 名 → DB 行（id/sip_server 等；全量分页拉取，只取真实 DB 行）。"""
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


def sync_trunks_to_rustpbx(cfg: dict) -> dict:
    """gateways 表 → RustPBX trunk 数据库 先清后加 + AMI 强制生效。

    先 DELETE 数据库里的当前网关（与配置同名、或同 dest 的旧条目——网关
    改名后的残留一并清掉），再 PUT 按配置全新创建，最后 POST
    /ami/v1/reload/trunks（从 DB 重生成 trunks.generated.toml + 更新注册
    表）。幂等，启动与 /config/reload 调用。失败非致命：更新 SYNC_STATE
    （/health 可见），targets 降级纯 URI。"""
    gateways = cfg.get("gateways") or {}
    if not gateways:
        SYNC_STATE.update(ok=True, at=time.time(), detail="no gateways in config")
        return SYNC_STATE
    try:
        existing = _existing_trunks()
    except Exception as exc:
        SYNC_STATE.update(ok=False, at=time.time(), detail=f"list failed: {exc}")
        log("trunk-sync", f"skipped (console unreachable): {exc}")
        return SYNC_STATE

    # 清掉当前网关：与配置网关同名（含被动注册型残留），或 dest 指向同一台
    # 对端（改名残留）。被动注册型（dest=0.0.0.0）只作分机用、不进 trunk，
    # 因此排除在"需建 trunk"集合外——其历史同名 trunk 一并清理。
    active = {n: g for n, g in gateways.items() if not is_passive_gateway(g)}
    cfg_dests = {_dest_host_port(g.get("dest", "")) for g in active.values()}
    deleted, created, failed = [], [], []
    skip_create = set()
    for name, item in existing.items():
        item_dest = _dest_host_port(item.get("sip_server") or "")
        if name not in active and not (item_dest and item_dest in cfg_dests):
            continue
        trunk_id = item.get("id")
        if trunk_id is None:
            continue
        try:
            _console_request("DELETE", f"/api/sip-trunk/{trunk_id}")
        except Exception as exc:
            detail = ""
            if isinstance(exc, urllib.error.HTTPError):
                detail = exc.read().decode("utf-8", errors="replace")
            failed.append(f"{name}: delete {exc}")
            log("trunk-sync", f"delete '{name}' failed: {exc} {detail}")
            # 删除失败时不再重建（同名牌仍在，必撞唯一约束）
            if name in active:
                skip_create.add(name)
            continue
        deleted.append(name)
        log("trunk-sync", f"cleared trunk '{name}' (id={trunk_id})")

    for name, gw in active.items():
        if name in skip_create:
            continue
        form = trunk_form_from_gateway(name, gw)
        try:
            resp = _console_request("PUT", "/api/sip-trunk", form)
        except Exception as exc:
            detail = ""
            if isinstance(exc, urllib.error.HTTPError):
                detail = exc.read().decode("utf-8", errors="replace")
            failed.append(f"{name}: {exc}")
            log("trunk-sync", f"create '{name}' failed: {exc} {detail}")
            continue
        created.append(name)
        log("trunk-sync", f"created trunk '{name}' -> {resp}")

    if created or deleted:
        # 强制生效（AMI reload：DB → trunks.generated.toml → 注册表）
        try:
            reload = _console_request("POST", "/ami/v1/reload/trunks", {})
            log("trunk-sync", f"hot-reload: {reload}")
        except Exception as exc:
            log("trunk-sync", f"reload after sync failed: {exc}")

    ok = not failed
    detail = f"deleted={deleted} created={created} failed={failed}"
    SYNC_STATE.update(ok=ok, at=time.time(), detail=detail)
    log("trunk-sync", f"done ok={ok} {detail}")
    return SYNC_STATE


def sync_trunks_with_retry(cfg: dict) -> None:
    """启动同步：重试数次，覆盖 RustPBX 晚于本服务就绪的顺序差。"""
    for attempt in range(1, TRUNK_SYNC_ATTEMPTS + 1):
        state = sync_trunks_to_rustpbx(cfg)
        if state.get("ok"):
            return
        if attempt < TRUNK_SYNC_ATTEMPTS:
            time.sleep(2)
    log("trunk-sync", f"gave up after {TRUNK_SYNC_ATTEMPTS} attempts — "
                      f"targets degrade to plain URIs")


# --------------------------------------------------------------------------- #
# 工具函数（与 examples/yunaoai/extension_decision_service.py 保持同源实现）
# --------------------------------------------------------------------------- #
_SIP_USER_RE = re.compile(r"sip:([^@;\s<>]+)", re.IGNORECASE)


def handle_auth(form: dict) -> tuple:
    """HttpUserBackend / HttpTokenAuthBackend 端点：RustPBX 在 REGISTER/INVITE
    时 POST username/realm/request_uri（有 X-Auth-Token 时额外转发），应答
    SipUser（含 Digest 密码种子）或 not_found。返回 (status_code, payload_dict)。

    两条分支：
    1. form 带 `X-Auth-Token`（WebRTC JWT 一次性认证）：校验 JWT + 消费一次性
       nonce 后直接放行，不再走静态密码。对应 RustPBX `[[proxy.user_backends]]
       type="http" + token_header="X-Auth-Token" + sip_headers=["X-Auth-Token"]`。
    2. 普通分机：账本（USERS）来自 gateways 中"被动注册型"网关（对方注册
       进来当分机），走 Digest 挑战。

    与 examples/yunaoai/extension_decision_service.py 的 handle_auth 契约
    一致，只是账本（USERS）改由被动注册型网关解析而来。"""
    token = (form.get(WEBRTC_TOKEN_FIELD) or "").strip()
    if token:
        return handle_webrtc_token(token, form)
    user = (form.get("username") or "").strip()
    password = USERS.get(user)
    if password is None:
        log("auth", f"user '{user}' NOT FOUND")
        return 404, {"reason": "not_found"}
    ordered = list(USERS)
    log("auth", f"user '{user}' FOUND (realm={form.get('realm')!r})")
    return 200, {
        "id": ordered.index(user) + 1,
        "enabled": True,
        "username": user,
        "password": password,
        "realm": LOCAL_REALM,
        "allow_guest_calls": False,
    }


def handle_webrtc_token(token: str, form: dict) -> tuple:
    """WebRTC 临时 JWT 一次性认证分支：验签 + exp/iss/aud + 绑定 WebRTC 账号
    + 消费 nonce。

    返回 (status, {SipUser}) 形态与 handle_auth 一致。JWT 有效则"以 JWT 里的
    userId 为主叫"返回 SipUser；异常返回 not_found / 401 让 RustPBX 回退或拒绝。
    """
    claims = decode_jwt(token)
    if claims is None:
        log("auth", "webrtc JWT 校验失败（签名/过期/iss/aud 不符）")
        return 404, {"reason": "not_found"}
    if not consume_webrtc_nonce(claims):
        log("auth", f"webrtc JWT 重放拒绝(jti={claims.get('jti')})")
        return 401, {"reason": "invalid_credentials"}
    webrtc = str(claims["userId"])
    realm = form.get("realm") or LOCAL_REALM
    log("auth", f"webrtc JWT 通过 → WebRTC 账号 {webrtc} (realm={realm!r})")
    return 200, {
        "id": 0,
        "enabled": True,
        "username": webrtc,
        "password": "",
        "realm": realm,
        "allow_guest_calls": False,
    }


def parse_user(uri: str):
    """Extract the user part from a SIP Request-URI / To / From header.

    兼容三种形态：纯 URI、无 scheme 的 user@host、From 头（显示名+尖括号+参数）。
    """
    s = (uri or "").strip()
    if not s:
        return None
    m = re.search(r"<([^>]*)>", s)
    candidate = m.group(1) if m else s
    m = _SIP_USER_RE.search(candidate)
    if m:
        return m.group(1)
    m = re.match(r"^([^@;\s<>]+)(?:@[^;\s<>]+)?;?", candidate.strip())
    return m.group(1) if m else None


def source_ip(source_addr) -> str:
    """"1.2.3.4:5060" -> "1.2.3.4"（兼容 IPv6 字面量 [::1]:5060）。"""
    s = (source_addr or "").strip()
    if s.startswith("["):
        return s[1:s.find("]")] if "]" in s else s
    return s.rsplit(":", 1)[0] if ":" in s else s


def ip_in_cidr(ip: str, cidr: str) -> bool:
    """标准库实现的最小 CIDR 匹配（仅 IPv4；IPv6 未命中直接 False）。"""
    try:
        net, bits = cidr.split("/", 1)
        bits = int(bits)
    except ValueError:
        return ip == cidr
    try:
        ipb = tuple(int(x) for x in ip.split("."))
        netb = tuple(int(x) for x in net.split("."))
    except ValueError:
        return False  # IPv6 暂不展开
    if len(ipb) != 4 or len(netb) != 4:
        return False
    mask = (0xFFFFFFFF << (32 - bits)) & 0xFFFFFFFF if bits else 0
    ipv = (ipb[0] << 24) | (ipb[1] << 16) | (ipb[2] << 8) | ipb[3]
    netv = (netb[0] << 24) | (netb[1] << 16) | (netb[2] << 8) | netb[3]
    return (ipv & mask) == (netv & mask)


def header_value(payload: dict, name: str) -> str:
    """按名字大小写不敏感取请求头（RustPBX headers 原样透传）。"""
    for k, v in (payload.get("headers") or {}).items():
        if k.lower() == name.lower():
            return v
    return ""


# --------------------------------------------------------------------------- #
# 路由裁决（route_decision）—— 每个 step 对应未来 KubeTrunkDaemon 的一个
# Java 模块；语义与方案 §4.3 裁决伪码一一对应。
# --------------------------------------------------------------------------- #
def resolve_caller_groups(caller: str, cfg: dict) -> dict:
    """主叫 → 命中的主叫组集合 {group: 最长命中记录长度}。

    caller_groups 组内为主叫号码/前缀记录，一个主叫可同时命中多个组
    （等价 rtmsip get_all_group_id 多组返回，drouting.c:2227）。"""
    hits = {}
    for grp, records in (cfg.get("caller_groups") or {}).items():
        best = 0
        for rec in records or []:
            rec = str(rec)
            if rec and caller.startswith(rec) and len(rec) > best:
                best = len(rec)
        if best:
            hits[grp] = best
    return hits


def match_rule(caller: str, callee: str, group_hits: dict, cfg: dict, trail=None):
    """分层匹配：第 1 层主被叫都符合 → 第 2 层被叫符合 → 第 3 层主叫符合。

    遍历所有 route_group 的 rule_list（rule_list=入口过滤，gw_list=出口；default_for 兜底组
    不参与 1~3 层，仅供第 4 层）：
      第 1 层：主叫命中主叫过滤（主叫组记录/直接记录）且非空被叫前缀命中
      第 2 层：无主叫过滤的规则中被叫前缀命中（跨主叫、不预筛方向）
      第 3 层：主叫命中主叫过滤的空前缀规则（等价 rtmsip noprefix 组内兜底）
    层序优先；层内排序：被叫前缀最长 → priority 降序 → 主叫记录最长（更
    精确主叫优先）→ 配置顺序。等价 rtmsip do_routing 组内两级匹配
    （get_prefix → new_check_rt），但把"主叫符合"排到"被叫符合"之后 ——
    被叫精确度优先（rtmsip 中主叫组的空前缀规则会截胡被叫精确匹配）。
    返回 (route_group, rule, layer)；全层未命中返回 (None, None, None)。
    trail 为列表时逐条追加评估轨迹（决策记录 steps 字段，界面可查看
    计算过程：跳过原因 / 每条规则命中或落选原因 / 最优选择）。"""
    layer_rank = {"L1": 2, "L2": 1, "L3": 0}
    best = None       # (rank, route_group, rule, layer)
    for gi, grp in enumerate(cfg.get("route_points") or []):
        if grp.get("enabled") is False or grp.get("default_for"):
            if trail is not None:
                trail.append(f"[{grp.get('point_id')}] 跳过："
                             + ("已停用" if grp.get("enabled") is False
                                else "兜底组（仅第 4 层）"))
            continue                     # 兜底组只参与第 4 层
        for ri, rule in enumerate(grp.get("rule_list") or []):
            prefix = (rule.get("prefix") or "").strip()
            cg = rule.get("caller_group")
            direct = str(rule.get("caller") or "").strip()
            if cg:
                cg_len = group_hits.get(cg, 0)
            elif direct:
                cg_len = len(direct) if caller.startswith(direct) else 0
            else:
                cg_len = None          # 无主叫过滤
            callee_hit = bool(prefix) and callee.startswith(prefix)
            tag = f"[{grp.get('point_id')}#{ri}] prefix='{prefix}'"
            tag += (f" caller_group={cg}" if cg
                    else (f" caller='{direct}'" if direct else " 无主叫过滤"))
            if cg_len:                                  # 主叫命中
                layer = "L1" if callee_hit else "L3"
                if trail is not None:
                    trail.append(f"{tag} → 命中 {layer}（"
                                 + ("主叫+被叫前缀" if callee_hit else "主叫命中，空前缀") + "）")
            elif cg_len is None:                        # 无主叫过滤 → 第 2 层
                if not callee_hit:
                    if trail is not None:
                        trail.append(f"{tag} → 落选：被叫 '{callee}' 不匹配前缀")
                    continue
                layer = "L2"           # 空前缀无主叫过滤也归第 2 层（建议改用兜底组表达）
                if trail is not None:
                    trail.append(f"{tag} → 命中 L2（被叫前缀，无主叫过滤）")
            else:                                       # 主叫过滤存在但未命中
                if trail is not None:
                    why = (f"主叫组 {cg} 未命中" if cg
                           else f"主叫 '{caller}' 不匹配 caller '{direct}'")
                    trail.append(f"{tag} → 落选：{why}")
                continue
            rank = (layer_rank[layer], len(prefix),
                    int(rule.get("priority") or 0), cg_len or 0,
                    -(gi * 10000 + ri))
            if best is None or rank > best[0]:
                best = (rank, grp, rule, layer)
    if trail is not None:
        if best:
            b_grp, b_rule, b_layer = best[1], best[2], best[3]
            trail.append(f"最优：{b_grp.get('point_id')}（{b_layer}，"
                         f"前缀='{(b_rule.get('prefix') or '').strip()}'）")
        else:
            trail.append("最优：无 —— 1~3 层全部落选，进第 4 层兜底")
    if best:
        return best[1], best[2], best[3]
    return None, None, None


def find_default_route_group(side: str, cfg: dict):
    """第 4 层：按方向取兜底组（default_for 标记，等价 rtmsip cr1/cr2）。"""
    for grp in cfg.get("route_points") or []:
        if grp.get("enabled") is not False and grp.get("default_for") == side:
            return grp
    return None


def match_default_rule(route_group: dict, callee: str):
    """兜底组内按被叫前缀选规则（最长前缀 → priority → 配置顺序；
    空前缀规则匹配全部被叫，等价 rtmsip 默认组 do_routing(1)/(2) 语义）。"""
    best = None
    best_key = None
    for i, rule in enumerate(route_group.get("rule_list") or []):
        prefix = rule.get("prefix", "") or ""
        if not callee.startswith(prefix):
            continue
        key = (len(prefix), int(rule.get("priority") or 0), -i)
        if best is None or key > best_key:
            best, best_key = rule, key
    return best


def rule_detail(rule: dict):
    """命中规则的过滤/变换细节（决策记录附带，界面可查看判断过程）。
    仅收录非空字段；prefix="" 为"匹配全部被叫"，保留。"""
    if not rule:
        return None
    return {k: rule[k] for k in ("prefix", "caller_group", "caller",
                                 "priority", "strip", "pri_prefix")
            if rule.get(k) is not None}


def identify_internal(payload: dict, cfg: dict):
    """第 4 层：按发起方 User-Agent 识别内部服务（方向判定挪到兜底层）。

    user_agent 大小写不敏感子串匹配；配置了 source_cidrs 的条目要求 UA 与
    来源 IP 双命中（外部白名单来源伪造 UA 走不出默认出局 —— 防盗打）。
    UA 未识别返回 False（落外部兜底组），由调用方记日志保持运营可见。
    返回 (是否内部, 判定依据) —— 依据随决策记录写入，界面可查看判断过程。"""
    ua = header_value(payload, "User-Agent")
    ip = source_ip(payload.get("source_addr"))
    if not ua:
        return False, "UA 缺失 → 外部"
    ua_hit_no_ip = None
    for i, svc in enumerate(cfg.get("internal_services") or []):
        pat = str(svc.get("user_agent") or "").strip().lower()
        if pat and pat not in ua.lower():
            continue
        cidrs = svc.get("source_cidrs") or []
        if cidrs and not any(ip_in_cidr(ip, c) for c in cidrs):
            ua_hit_no_ip = i        # UA 命中但 IP 不在网段，继续看其他条目
            continue
        return True, (f"UA '{ua}' 命中 internal_services[{i}]" +
                      (f" + 来源 {ip} 在网段" if cidrs else "（条目未配 source_cidrs）"))
    if ua_hit_no_ip is not None:
        return False, (f"UA '{ua}' 命中 internal_services[{ua_hit_no_ip}] "
                       f"但来源 {ip} 不在网段（防盗打）→ 外部")
    return False, f"UA '{ua}' 未识别 → 外部"


def apply_rule_transforms(rule: dict, callee: str) -> str:
    """号码变换：strip 剥码 + pri_prefix 出局码加到被叫前
    （等价 push_gw_for_usage → build_ruri：pri_prefix + (被叫 - strip)）。"""
    out = callee
    strip = int(rule.get("strip") or 0)
    if strip > 0:
        out = out[strip:]
    pri = (rule.get("pri_prefix") or "").strip()
    if pri:
        out = pri + out
    return out


def apply_area_prefix0(callee: str, gateway: dict, cfg: dict) -> str:
    """区号加 0（do_num_convs 的 add_prefix0 等价，可选默认关）。

    网关 add_prefix0=true 且被叫为 11 位手机号（1xx）时：取前 7 位查
    area_codes（cfg_phone_prifix 等价），区号与网关 area_code 不一致 → 加 "0"。"""
    if not gateway.get("add_prefix0"):
        return callee
    if len(callee) == 11 and callee.startswith("1"):
        area = (cfg.get("area_codes") or {}).get(callee[:7])
        if area and area != gateway.get("area_code"):
            log("num-convs", f"area add0 callee={callee} area={area} "
                             f"gw_area={gateway.get('area_code')}")
            return "0" + callee
    return callee


def strip_caller_sitecode(caller: str, cfg: dict) -> str:
    """主叫 sitecode 循环剥离（分机号带站点码前缀 3201001=3201+001，
    支持叠加 32013201xxx）。sitecode 为空不剥。"""
    sitecode = str(cfg.get("sitecode") or "")
    if not sitecode:
        return caller
    stripped = caller
    while len(stripped) > len(sitecode) and stripped.startswith(sitecode):
        stripped = stripped[len(sitecode):]
    return stripped


def gateway_targets(callee: str, gwlist, cfg: dict):
    """出口 gw_list → (targets 列表, rtp_config 字典, network_profile_id str|None)。

    返回 tuple：
      - targets：结构化或纯 URI targets 列表，供 RustPBX http_router 转发。
      - rtp_config：收集到的媒体覆盖配置（external_ip/bind_ip），如果
        gw_list 中网关配置了 external_ip/profile 则有值，否则 {}。
        调用方应将其合并进 HTTP 响应顶层 rtp_config 字段，让 RustPBX
        应用对应的媒体宣告地址（如 tailscale overlay 上对端可达的 IP）。
      - network_profile_id：gw_list 里第一个带 profile 的网关的 profile 名
        （如 "tailscale"），让 RustPBX 把对应的 network_profile 应用到本
        次呼叫的 media（包括 sip_contact、bind_ip、rtp 端口等）。被动注册型
        网关（纯 URI target 不带 trunk）也能拿到正确的 SIP Contact 地址。

    RustPBX http_router 已支持 {callee, trunk} 结构化 targets：网关名即
    RustPBX trunk 注册表条目（本服务启动时把 gateways 表同步写入，见
    sync_trunks_to_rustpbx），callee 限定到该 trunk 的 dest host:port，
    由 RustPBX 按 trunk 配置盖章（next-hop/transport/凭据、媒体 hints、
    CPS 并发闸门、CDR 归因）——等价静态路由的 trunk 承接。
    多网关按序展开 + strategy="sequential" = 等价 use_next_gw 失败换下一
    网关；停用网关跳过（等价探测摘除）。
    同步失败降级（SYNC_STATE.ok=False）：生成 `sip:<callee>@<dest-host:port>`
    纯 URI targets —— 呼叫仍按 host:port 可达，trunk 盖章不生效。

    被动注册型网关（对方 FS 注册进来当分机）：cfg.gateways 中 dest=0.0.0.0，
    其 username 字段 = 对方实际注册的分机号（同步进 USERS 账本）。
    对这类网关，先实时调 RustPBX locator API 查该分机的动态 Contact
    host:port → 拼 `sip:<原始callee>@<host:port>` 纯 URI target。
    这样 Request-URI 里的 user 就是用户真正拨的号（FS 能知道是 8798001
    还是 8798555），host 直接路由到 FS 注册地址。
    locator 查不到（对方离线）则降级为 {"callee": ext} —— RustPBX
    extension_locator 自己处理（也会因为离线而失败，但保持优雅降级）。"""
    targets = []
    rtp_config = {}
    network_profile_id = None
    for name in gwlist or []:
        gw = (cfg.get("gateways") or {}).get(name)
        if gw and gw.get("external_ip"):
            rtp_config.setdefault("external_ip", gw["external_ip"])
        if gw and gw.get("bind_ip"):
            rtp_config.setdefault("bind_ip", gw["bind_ip"])
        if gw and gw.get("profile") and network_profile_id is None:
            network_profile_id = gw["profile"]
        if gw and is_passive_gateway(gw):
            ext = str(gw.get("username") or name).strip()
            hp = _lookup_contact_hostport(ext)
            if hp:
                targets.append(f"sip:{callee}@{hp}")
            else:
                log("targets", f"passive gw '{name}' ext={ext} not registered, "
                               f"fallback to {{callee: {ext}}}")
                targets.append({"callee": ext})
            continue
        if name in USERS:
            if SYNC_STATE.get("ok"):
                targets.append({"callee": name})
            else:
                targets.append(f"sip:{name}@{LOCAL_REALM or 'localhost'}")
            continue
        if not gw:
            log("targets", f"gateway '{name}' missing, skip")
            continue
        if gw.get("enabled") is False:
            log("targets", f"gateway '{name}' disabled, skip")
            continue
        if SYNC_STATE.get("ok"):
            targets.append({"callee": callee, "trunk": name})
        else:
            dest = gw["dest"]
            host_port = dest.split(":", 1)[-1] if ":" in dest else dest
            targets.append(f"sip:{callee}@{host_port}")
    return targets, rtp_config, network_profile_id


def resolve_xcid(payload: dict) -> str:
    """X-cid 呼叫关联约定（与 examples/yunaoai/extension_decision_service.py 同源）：
    上游带 X-cid 沿用（跨节点转发），否则生成 KTM_<session_id> —— session_id
    由 RustPBX /route 请求直接携带，与 RWI 事件完全一致，两端无需状态映射。"""
    xcid = header_value(payload, "X-cid")
    if xcid:
        return xcid
    session_id = payload.get("session_id") or payload.get("call_id") or ""
    return f"KTM_{session_id}"


def resolve_webrtc_xtag(payload: dict) -> str:
    """WebRTC 发起呼叫的关联标识（X-cid / User-to-User 共用）。前缀 KRTC_，
    与普通路由的 KTM_ 区隔；优先沿用上游已带 X-cid（跨节点转发），否则本地
    生成 KRTC_<call_id|session_id>（保证与决策 trace/corr_id 可关联）。"""
    xcid = header_value(payload, "X-cid")
    if xcid:
        return xcid
    key = payload.get("call_id") or payload.get("session_id")
    if not key:
        import time as _t
        key = f"{int(_t.time() * 1000)}-{int(_t.monotonic() * 1e6) % 100000}"
    return f"KRTC_{key}"


def uui_business_value() -> str:
    """User-to-User（UUI）头值：仅业务保留默认字段（RFC 7433 分号参数）。
    默认 vdn=52000111，可通过配置项 uui_vdn 覆盖；呼叫关联由独立的 X-cid
    头承担，不在此混入关联标识。下游业务系统读取该默认值用于呼入路由。"""
    return str(CONFIG.get("uui_vdn") or "52000111")


def webrtc_entries(cfg: dict) -> list:
    """webrtc 配置归一（向 route_points 看齐的列表模型）：每条 level 一个账号
    组点位；兼容旧单对象格式（dict 视为单元素列表）。"""
    webrtc = cfg.get("webrtc")
    if isinstance(webrtc, dict):          # 旧格式：单 account 对象
        return [webrtc]
    return list(webrtc or [])


def _expand_entries(src_list: list, kind: str) -> list:
    """把条目数组展平成三元组表（caller_group, prefix → gw_list）。"""
    table = []
    for src in src_list:
        if src.get("enabled") is False:
            continue
        for ri, rule in enumerate(src.get("rule_list") or []):
            table.append({
                "kind": kind,
                "source": src.get("point_id") or src.get("group_type") or kind,
                "rule_index": ri,
                "caller_group": (rule.get("caller_group") or "").strip() or None,
                "caller": (rule.get("caller") or "").strip() or None,
                "prefix": (rule.get("prefix") or "").strip(),
                "gw_list": list(src.get("gw_list") or []),
                "strip": rule.get("strip"),
                "pri_prefix": rule.get("pri_prefix"),
                "priority": int(rule.get("priority") or 0),
                "default_for": src.get("default_for"),
                "allowed_ips": src.get("allowed_ips"),
                "enabled": True,
            })
    return table


def build_webrtc_table(cfg: dict) -> list:
    """webrtc 路由表：独立于 route_points，只服务 webrtc 分机。"""
    table = _expand_entries(webrtc_entries(cfg), "webrtc")
    log("route-table", f"webrtc_table built {len(table)} entries")
    return table


def build_route_points_table(cfg: dict) -> list:
    """route_points 路由表：独立于 webrtc，服务普通分机。"""
    table = _expand_entries(cfg.get("route_points") or [], "route")
    log("route-table", f"route_table built {len(table)} entries")
    return table


def _find_in_table(table: list, caller: str, callee: str,
                   group_hits: dict, trail=None):
    """从指定路由表中找最优三元组（caller 命中 + callee 前缀）。

    只有 callee 前缀匹配的条目才能参与竞争，rank 按 (前缀长度, priority,
    caller 强度) 排序，最长前缀优先。"""
    candidates = [t for t in table if t["enabled"] and not t.get("default_for")]
    best = None

    for t in candidates:
        caller_len = 0
        if t["caller_group"]:
            caller_len = group_hits.get(t["caller_group"], 0)
        elif t["caller"]:
            caller_len = len(t["caller"]) if caller.startswith(t["caller"]) else 0
        else:
            caller_len = None

        callee_hit = bool(t["prefix"]) and callee.startswith(t["prefix"])
        if not callee_hit:
            continue

        if caller_len:
            layer = "L1"
        elif caller_len is None:
            layer = "L2"
        else:
            continue

        rank = (len(t["prefix"]), t["priority"], caller_len or 0)
        if best is None or rank > best[0]:
            best = (rank, t, layer)

        if trail is not None:
            trail.append(f"  [{t['source']}#{t['rule_index']}] "
                         f"cg={t['caller_group']!r} callee_prefix={t['prefix']!r} "
                         f"caller_hit={'Y' if caller_len else 'N'} "
                         f"callee_hit=Y -> {layer}")

    if best:
        return best[1], best[2]
    return None, None


def webrtc_entry_tag(entry: dict) -> str:
    """webrtc 位点标识：与 route_points 同构用 point_id；无则退回 group_type。"""
    return str(entry.get("point_id") or entry.get("group_type") or "webrtc")


def webrtc_entry_groups(entry: dict) -> list:
    """该 webrtc 位点 rule_list 引用的 caller_group 名（去重保留顺序）。

    方案 A：webrtc 段完全照抄 route_points（无 account/accounts），主叫 → 位点
    的归属判定靠规则 rule_list 的 caller_group（组内为主叫号码/前缀记录）。"""
    out, seen = [], set()
    for r in entry.get("rule_list") or []:
        g = (r.get("caller_group") or "").strip()
        if g and g not in seen:
            seen.add(g)
            out.append(g)
    return out


def webrtc_entry_callers(cfg: dict, entry: dict) -> list:
    """该 webrtc 位点可接纳的主叫集合：各 rule.caller_group 组内记录并集（去重）。"""
    out, seen = [], set()
    cg = cfg.get("caller_groups") or {}
    for g in webrtc_entry_groups(entry):
        for rec in cg.get(g) or []:
            rec = str(rec)
            if rec and rec not in seen:
                seen.add(rec)
                out.append(rec)
    return out


def is_webrtc_caller(cfg: dict, caller: str) -> bool:
    """caller 是否归属任何 webrtc 条目（仅按 caller_group 判定，不管 callee）。"""
    hits = resolve_caller_groups(caller, cfg)
    for entry in webrtc_entries(cfg):
        if entry.get("enabled") is False:
            continue
        if any(hits.get(g, 0) for g in webrtc_entry_groups(entry)):
            return True
    return False


def find_webrtc_entry(cfg: dict, caller: str, callee: str = ""):
    """遍历 webrtc 条目（独立竞争，按配置顺序）。

    传 callee 时：caller_group 命中 AND callee 前缀命中 → 第一个命中的返回。
    全条目遍历完未命中 → None。
    不传 callee：仅按 caller_group → 第一个命中的返回。"""
    hits = resolve_caller_groups(caller, cfg)
    for entry in webrtc_entries(cfg):
        if entry.get("enabled") is False:
            continue
        if not any(hits.get(g, 0) for g in webrtc_entry_groups(entry)):
            continue
        if callee:
            if webrtc_match_rule(entry, callee) is not None:
                return entry
        else:
            return entry
    return None


def find_all_webrtc_entries(cfg: dict, caller: str) -> list:
    """返回所有 caller 归属的 webrtc 条目列表（同 caller_group 可能挂多个条目）。"""
    hits = resolve_caller_groups(caller, cfg)
    result = []
    for entry in webrtc_entries(cfg):
        if entry.get("enabled") is False:
            continue
        if any(hits.get(g, 0) for g in webrtc_entry_groups(entry)):
            result.append(entry)
    return result


def default_webrtc_account(cfg: dict = None) -> str:
    """签发 JWT 的默认 WebRTC 账号：第一个可用位点的首名主叫，回退环境变量。"""
    cfg = CONFIG if cfg is None else cfg
    for entry in webrtc_entries(cfg):
        c = webrtc_entry_callers(cfg, entry)
        if c:
            return c[0]
    return WEBRTC_ACCOUNT


def webrtc_account_prefixes(cfg: dict, acc) -> list:
    """WebRTC 账号可外呼的被叫前缀：所有归属位点 rule_list 的 prefix 列表聚合去重。"""
    entries = find_all_webrtc_entries(cfg, str(acc))
    pre, seen = [], set()
    for entry in entries:
        for r in entry.get("rule_list") or []:
            p = str(r.get("prefix") or "")
            if p not in seen:
                seen.add(p)
                pre.append(p)
    return pre


def webrtc_account_rules(cfg: dict, acc) -> list:
    """WebRTC 账号聚合的完整入口规则：所有归属位点 rule_list 的 prefix / strip /
    pri_prefix / caller_group / gw_list 聚合去重。每条 rule 带上所属条目 gw_list，
    供前端展示"前缀→出口"关联（不再需要单独的 meta 点信息）。"""
    entries = find_all_webrtc_entries(cfg, str(acc))
    out, seen = [], set()
    for entry in entries:
        gw_list = [str(g) for g in (entry.get("gw_list") or [])]
        for r in entry.get("rule_list") or []:
            prefix = str(r.get("prefix") or "")
            key = (prefix, int(r.get("strip") or 0),
                   str(r.get("pri_prefix") or ""), str(r.get("caller_group") or ""))
            if key in seen:
                continue
            seen.add(key)
            out.append({
                "prefix": prefix,
                "strip": int(r.get("strip") or 0),
                "pri_prefix": str(r.get("pri_prefix") or ""),
                "caller_group": str(r.get("caller_group") or ""),
                "gw_list": gw_list,
            })
    return out


def webrtc_entry_meta(cfg: dict, acc):
    """WebRTC 账号归属信息：所有归属条目列表，每条带 point_id + group_type + gw_list。
    同 caller_group 可能挂多个条目（合并配置后不同前缀走不同网关）。"""
    entries = find_all_webrtc_entries(cfg, str(acc))
    if not entries:
        return None
    return [{
        "point_id": str(e.get("point_id") or e.get("group_type") or ""),
        "group_type": str(e.get("group_type") or ""),
        "gw_list": [str(g) for g in (e.get("gw_list") or [])],
    } for e in entries]


def webrtc_entry_allows_source(entry: dict, ip: str) -> bool:
    """准入策略：allowed_ips 为空 = 不限制来源；否则来源 IP 必须命中其一网段。"""
    aips = entry.get("allowed_ips") or []
    if not aips:
        return True
    return any(ip_in_cidr(ip, c) for c in aips if str(c).strip())


def webrtc_match_rule(entry: dict, callee: str, caller: str = "", cfg: dict = None):
    """webrtc 条目内 rule_list 白名单匹配（对齐 route_points：rule_list=入口过滤，
    gw_list=出口）。逐条按前缀匹配，取最长前缀（相同长度时：若传 caller+cfg，
    优先选 caller_group 命中主叫的那条；否则保留配置顺序先命中的）。
    命中返回该 rule（其 strip/pri_prefix 用于号码变换），未命中 None。"""
    best = None
    best_len = -1
    best_cg_match = False
    caller_hits = resolve_caller_groups(caller, cfg) if caller and cfg else {}
    for i, rule in enumerate(entry.get("rule_list") or []):
        prefix = (rule.get("prefix") or "").strip()
        if not prefix or not callee.startswith(prefix):
            continue
        cg = (rule.get("caller_group") or "").strip()
        cg_hits = caller_hits.get(cg, 0) if cg else True
        if len(prefix) > best_len:
            best, best_len, best_cg_match = rule, len(prefix), bool(cg_hits)
        elif len(prefix) == best_len:
            if cg_hits and not best_cg_match:
                best, best_cg_match = rule, True
            elif best is None:
                best = rule
    return best


def webrtc_route_decision(payload: dict, cfg: dict, caller: str, callee: str,
                          entry: dict) -> dict:
    """WebRTC 呼叫的独立路由策略（方案 A）。

    与普通呼叫不同：WebRTC 会话（临时 JWT 认证）的主叫【不走 L1~L4 路由
    裁决】，而是按配置段 `webrtc` 的白名单直接决策：
      - webrtc 段与 route_points **完全同构**：point_id/group_type/rule_list
        /gw_list，仅额外多 allowed_ips=来源准入；不含 account/accounts。
      - 主叫 → 位点归属靠位点 rule_list 的 caller_group（组内为主叫号码/
        前缀记录），命中该位点即由 entry 驱动后续决策；
      - callee 命中 rule_list（白名单前缀）→ 转发到 gw_list（通常指向注册
        分机 / 出局中继），仅此一条出口，不做任何默认兜底；
      - callee 未命中任何白名单前缀 → 拒绝，防止 WebRTC 会话被用于拨打
        任意号码（仅放行配置前缀，其余一律 4xx）。
    该分支在 route_decision() 入口处按主叫归属命中 webrtc 条目触发，返回
    结构与普通 forward/reject 一致，RustPBX 无需区分。

    entry 为当前命中的 webrtc 点位，完全对齐 route_points：rule_list=白名单
    入口（每条 rule 独立 prefix/strip/pri_prefix/caller_group 号码变换与主叫
    过滤），gw_list=出口（多网关），外加 allowed_ips=来源准入。命中 rule 后
    用该 rule 的 strip/pri_prefix 变换被叫，再经条目 gw_list 出口。"""
    entry_tag = webrtc_entry_tag(entry)
    matched_rule = webrtc_match_rule(entry, callee, caller, cfg)
    matched = (matched_rule.get("prefix") or "").strip() if matched_rule else None

    # 来源信息诊断：raw_addr = RustPBX 捕获的 WS 对端原始地址（可能含端口），
    # src_ip   = 从中解析出的 IP（准入判定的依据），xff = 透传的真实客户端
    # 来源（仅记录展示，不参与准入判定，避免伪造 XFF 绕过白名单）。
    raw_addr = payload.get("source_addr") or ""
    src_ip = source_ip(raw_addr)
    xff = (header_value(payload, "X-Forwarded-For")
           or str(payload.get("x-forwarded-for") or "")).split(",")[0].strip()

    # 准入策略：来源 IP 未命中 allowed_ips → 拒绝（防盗打，防外部借 Webrtc
    # 会话发起任意呼叫）。
    if not webrtc_entry_allows_source(entry, src_ip):
        log("decision", f"webrtc caller={caller} raw_addr={raw_addr!r} "
                        f"src={src_ip} xff={xff!r} entry={entry_tag} → 准入拒绝")
        trace_record(payload.get("call_id") or "", "decision",
                     session_id=payload.get("session_id"),
                     layer="webrtc", callee=callee, caller=caller,
                     entry=entry_tag, source_addr=raw_addr, src_ip=src_ip,
                     xff=xff, corr_id=resolve_webrtc_xtag(payload), action="reject",
                     status=403, reason="source not allowed for webrtc calls")
        return {"action": "reject", "status": 403,
                "reason": "source not allowed for webrtc calls"}

    log("decision", f"webrtc caller={caller} raw_addr={raw_addr!r} src={src_ip} "
                    f"xff={xff!r} callee={callee} matched_rule={matched!r}")
    trace_record(payload.get("call_id") or "", "decision",
                 session_id=payload.get("session_id"),
                 layer="webrtc", callee=callee, caller=caller,
                 entry=entry_tag, source_addr=raw_addr, src_ip=src_ip, xff=xff,
                 corr_id=resolve_webrtc_xtag(payload),
                 rule_list=entry.get("rule_list"), matched_prefix=matched)
    if matched_rule is None:
        return {"action": "reject", "status": 403,
                "reason": "prefix not allowed for webrtc calls"}

    # caller_group 主叫组过滤（对齐 route_points：命中规则可带 caller_group，
    # 组内为主叫号码/前缀记录；仅放行属于该组的主叫，否则 403，语义与
    # route_points 的主叫过滤完全一致）。
    _cg = (matched_rule.get("caller_group") or "").strip()
    if _cg:
        _hits = resolve_caller_groups(caller, cfg)
        if not _hits.get(_cg, 0):
            log("decision", f"webrtc caller={caller} callee={callee} "
                            f"entry={entry_tag} caller_group={_cg} → 主叫组未命中")
            trace_record(payload.get("call_id") or "", "decluision",
                         session_id=payload.get("session_id"),
                         layer="webrtc", callee=callee, caller=caller,
                         entry=entry_tag, matched_prefix=matched,
                         caller_group=_cg, group_hits=_hits,
                         corr_id=resolve_webrtc_xtag(payload),
                         action="reject", status=403,
                         reason="caller not allowed by caller_group")
            return {"action": "reject", "status": 403,
                    "reason": "caller not allowed by caller_group"}

    # 号码变换：取该命中 rule 的 strip/pri_prefix（对齐 route_points），
    # 然后经该条目 gw_list 出口（多网关按序展开）。
    callee2 = apply_rule_transforms(matched_rule, callee)
    targets, rtp_config, network_profile_id = gateway_targets(callee2, entry.get("gw_list"), cfg)
    if not targets:
        trace_record(payload.get("call_id") or "", "decision",
                     session_id=payload.get("session_id"),
                     layer="webrtc", status=500, callee=callee2, caller=caller,
                     entry=entry_tag, reason="no usable gateway")
        return {"action": "reject", "status": 500, "reason": "No GW Route found"}

    resp = {
        "action": "forward",
        "targets": targets,
        "strategy": "sequential",
        # X-cid：跨节点自定义呼叫关联；User-to-User（UUI）是标准 SIP 头，
        # 出口 trunk 的 INVITE 会带它，被叫侧 PBX 可凭同一值关联回主叫判定。
        # WebRTC 发起统一用 KRTC_ 前缀（区别于普通路由的 KTM_）。
        "headers": {"X-cid": resolve_webrtc_xtag(payload),
                    "User-to-User": uui_business_value()},
    }
    if rtp_config:
        resp["rtp_config"] = rtp_config
    if network_profile_id:
        resp["network_profile_id"] = network_profile_id
    _corr = resolve_webrtc_xtag(payload)
    trace_record(payload.get("call_id") or "", "decision",
                 session_id=payload.get("session_id"),
                 layer="webrtc", action="forward",
                 callee=callee2, caller=caller, entry=entry_tag, targets=targets,
                 corr_id=_corr, matched_prefix=matched,
                 strip=matched_rule.get("strip"),
                 pri_prefix=matched_rule.get("pri_prefix"))
    return resp


def route_decision(payload: dict) -> dict:
    """主流程：分层匹配 + 默认路由 + 号码变换 + targets。"""
    cfg = CONFIG or {}

    method = (payload.get("method") or "").upper()
    if method and method != "INVITE":
        return {"action": "not_handled"}

    uri = payload.get("uri") or payload.get("to") or ""
    callee = parse_user(uri)
    caller = parse_user(payload.get("from") or "") or ""
    log("decision", f"call_id={payload.get('call_id')} method={method} "
                    f"src={payload.get('source_addr')} dir={payload.get('direction')} "
                    f"ua={header_value(payload, 'User-Agent')!r} "
                    f"caller={caller} callee={callee}")

    # $rU 为空 → 484（等价 rtmsip 484 Address Incomplete）。parse_user 对
    # "sip:@host" 这类无 user 的畸形 R-URI 会回退匹配出 "sip"/"tel" 伪值，
    # 一并按空处理（KubeTrunkDaemon 用正规 URI 解析，无此问题）。
    if not callee or callee.lower().rstrip(":") in ("sip", "sips", "tel"):
        trace_record(payload.get("call_id") or "", "decision",
                     session_id=payload.get("session_id"),
                     layer="reject", status=484, callee=callee, caller=caller)
        return {"action": "reject", "status": 484, "reason": "address incomplete"}

    webrtc_table = cfg.get("_webrtc_table") or []
    route_table = cfg.get("_route_table") or []
    group_hits = resolve_caller_groups(caller, cfg)

    # WebRTC 呼叫：caller 归属 webrtc caller_group → 只查 webrtc_table。
    # 命中 → 构造 fake entry 走 webrtc_route_decision；
    # 不命中 → 直接 403 reject，绝不掉 route_points 兜底。
    if webrtc_table:
        webrtc_groups = set()
        for t in webrtc_table:
            if t["caller_group"]:
                webrtc_groups.add(t["caller_group"])
        if any(group_hits.get(g, 0) for g in webrtc_groups):
            trail = []
            t3, _layer = _find_in_table(
                webrtc_table, caller, callee, group_hits, trail=trail)
            if t3 is not None:
                fake_entry = {
                    "point_id": t3["source"],
                    "gw_list": t3["gw_list"],
                    "allowed_ips": None,
                    "rule_list": [{
                        "prefix": t3["prefix"],
                        "strip": t3["strip"],
                        "pri_prefix": t3["pri_prefix"],
                        "caller_group": t3["caller_group"],
                    }],
                }
                fake_entry.update({
                    k: None for k in ("caller",)
                })
                for src in webrtc_entries(cfg):
                    if src.get("point_id") == t3["source"]:
                        fake_entry["allowed_ips"] = src.get("allowed_ips")
                        break
                trace_record(payload.get("call_id") or "", "decision",
                             session_id=payload.get("session_id"),
                             layer="webrtc-branch", entry=t3["source"],
                             steps=trail)
                return webrtc_route_decision(
                    payload, cfg, caller, callee, fake_entry)
            trace_record(payload.get("call_id") or "", "decision",
                         session_id=payload.get("session_id"),
                         layer="webrtc-reject", callee=callee, caller=caller,
                         reason="caller is webrtc but no callee prefix match",
                         steps=trail)
            return {"action": "reject", "status": 403,
                    "reason": "prefix not allowed for webrtc calls"}

    # 普通路由：从聚合表筛 kind="route" 条目。不命中 → 兜底组。
    steps = ["主叫组解析: " + (", ".join(f"{g}(长度{v})" for g, v in group_hits.items())
                              or "无")]
    t3, layer = _find_in_table(
        route_table, caller, callee, group_hits, trail=steps)
    details = {"group_hits": group_hits, "steps": steps}

    if t3 is None:
        internal, why_internal = identify_internal(payload, cfg)
        details["internal_check"] = why_internal
        steps.append("兜底方向判定: " + why_internal)
        side = "internal" if internal else "external"
        layer = f"L4-{side}"
        default_group = None
        for src in cfg.get("route_points") or []:
            if src.get("enabled") is not False and src.get("default_for") == side:
                default_group = src
                break
        default_rule = None
        if default_group:
            best_k = None
            for r in default_group.get("rule_list") or []:
                pre = (r.get("prefix") or "").strip()
                if callee.startswith(pre):
                    k = (len(pre), int(r.get("priority") or 0))
                    if best_k is None or k > best_k:
                        best_k, default_rule = k, r
            if default_rule:
                steps.append(f"兜底组: {default_group.get('point_id')}，"
                             f"prefix='{(default_rule.get('prefix') or '').strip()}'")
            else:
                steps.append(f"兜底组: {default_group.get('point_id')}，"
                             f"无匹配规则")
            t3 = {
                "source": default_group.get("point_id"),
                "gw_list": list(default_group.get("gw_list") or []),
                "prefix": (default_rule.get("prefix") or "").strip() if default_rule else "",
                "strip": default_rule.get("strip") if default_rule else None,
                "pri_prefix": default_rule.get("pri_prefix") if default_rule else None,
                "caller_group": None,
                "kind": "route",
            }
        if t3 is None:
            log("decision", f"no default route_group/rule for {side} — reject 500")
            trace_record(payload.get("call_id") or "", "decision",
                         session_id=payload.get("session_id"),
                         layer=layer, status=500, callee=callee, caller=caller,
                         reason="no default route_group/rule", **details)
            return {"action": "reject", "status": 500,
                    "reason": "No GW Route found"}
    else:
        log("decision", f"hit table route t3={t3['source']} layer={layer} "
                        f"prefix='{t3['prefix']}' gw={t3['gw_list']}")

    callee2 = apply_rule_transforms(
        {"strip": t3["strip"], "pri_prefix": t3["pri_prefix"]}, callee)
    targets, rtp_config, network_profile_id = gateway_targets(callee2, t3["gw_list"], cfg)
    if not targets:
        trace_record(payload.get("call_id") or "", "decision",
                     session_id=payload.get("session_id"),
                     layer=layer, status=500, callee=callee2, caller=caller,
                     reason="no usable gateway", source=t3["source"],
                     **details)
        return {"action": "reject", "status": 500, "reason": "No GW Route found"}

    resp = {
        "action": "forward",
        "targets": targets,
        "strategy": "sequential",
        "headers": {"X-cid": resolve_xcid(payload),
                    "User-to-User": uui_business_value()},
    }
    if rtp_config:
        resp["rtp_config"] = rtp_config
    if network_profile_id:
        resp["network_profile_id"] = network_profile_id

    log("decision", f"forward callee2={callee2} targets={targets} rtp={rtp_config}")
    trace_record(payload.get("call_id") or "", "decision",
                 session_id=payload.get("session_id"),
                 layer=layer, callee=callee2, caller=caller,
                 source=t3["source"], targets=targets, rtp_config=rtp_config,
                 matched_prefix=t3["prefix"], strip=t3["strip"],
                 pri_prefix=t3["pri_prefix"], **details)
    return resp


# --------------------------------------------------------------------------- #
# HTTP 服务（stdlib，与 examples/yunaoai/extension_decision_service.py 同一形态）
# --------------------------------------------------------------------------- #
class RouteDecisionHandler(BaseHTTPRequestHandler):
    def _json(self, status, obj):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "Content-Type, X-Auth-Token")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_OPTIONS(self):
        # 预检：允许跨域读取（页面在 file:// 或异域托管时也能调 /token、/auth）
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "Content-Type, X-Auth-Token")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _read_body(self) -> bytes:
        """兼容 Content-Length 与 chunked 两种编码（reqwest 可能发 chunked）。"""
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

    def do_POST(self):
        global CONFIG  # config/reload 及 /token/webrtc 读取/赋值，须先声明再引用
        path = self.path.split("?")[0].rstrip("/")
        if path.endswith("/token/webrtc"):
            # WebRTC 会话：向 (预) 客户端签发一次性短期 JWT 作为密码。
            # 支持多账号：body 可带 {"account": "..."} 指定签发给哪个 WebRTC
            # 账号（必须是某条 webrtc 条目内账号），缺省取第一条目的默认账号。
            # 有效期由服务器统一指定（JWT_MAX_AGE / RUSTPBX_JWT_MAX_AGE），
            # 不接受客户端传入，避免前端自行拉长授权窗口。
            raw = self._read_body()
            want = None
            try:
                data = json.loads(raw.decode("utf-8")) if raw else {}
                if isinstance(data, dict):
                    want = str(data.get("account") or "").strip() or None
            except ValueError:
                want = None
            if want is not None:
                known = set()
                for e in webrtc_entries(CONFIG):
                    known.update(webrtc_entry_callers(CONFIG, e))
                if want not in known:
                    return self._json(403, {"ok": False,
                                            "reason": f"unknown webrtc account '{want}'"})
            return self._json(200, mint_webrtc_jwt(JWT_MAX_AGE, account=want))
        if path.endswith("/auth"):
            # HttpUserBackend：分机 REGISTER/INVITE 鉴权。兼容 reqwest 发的
            # application/json 与 form-urlencoded 两种 body（对齐
            # extension_decision_service.py 的 _handle_auth）。账本来自
            # gateways 中"被动注册型"网关（_apply_extensions 已同步进 USERS）。
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
            return self._json(status, payload)
        if path.endswith("/events/clear"):
            # 共享前端 "清空记录" 按钮（与 http_router_service.py 同一契约）。
            # 统一视图数据源有两处：呼叫事件（CCM_CALLS）+ 路由决策
            # （TRACE/TRACE_SESSION），一并清空，否则决策卡片会残留。
            CCM_CALLS.clear()
            CCM_CALLS_ORDER.clear()
            TRACE.clear()
            TRACE_SESSION.clear()
            return self._json(200, {"ok": True})
        if path.endswith("/route/decision") or path.endswith("/route"):
            raw = self._read_body()
            try:
                payload = json.loads(raw.decode("utf-8"))
            except ValueError:
                return self._json(200, {"action": "reject", "status": 400,
                                        "reason": "bad json"})
            if not isinstance(payload, dict):
                return self._json(200, {"action": "reject", "status": 400,
                                        "reason": "bad payload"})
            try:
                return self._json(200, route_decision(payload))
            except Exception as exc:  # 裁决异常兜底：fail-closed 拒绝，不放行
                import traceback
                traceback.print_exc()
                return self._json(200, {"action": "reject", "status": 500,
                                        "reason": f"decision error: {exc}"})
        if path.endswith("/config/reload"):
            try:
                new_cfg = load_config(CONFIG_FILE or None)
            except ValueError as exc:
                # 加载/校验失败：保留旧配置（运行中不崩溃），显式 400 告警
                log("config", f"reload REJECTED: {exc}")
                return self._json(400, {"ok": False, "reason": str(exc)})
            CONFIG = new_cfg
            # 配置变更后重新同步网关表（upsert 幂等；新增网关自动入 ACL）
            sync_trunks_to_rustpbx(CONFIG)
            return self._json(200, {"ok": True, "sitecode": CONFIG.get("sitecode"),
                                    "trunk_sync": SYNC_STATE.get("ok")})
        if path.endswith("/rwi"):
            # RWI webhook 接收端（呼叫跟踪 + 前端事件链）：RustPBX
            # [rwi_webhook] 推送呼叫生命周期事件，2xx 即视为送达。
            # trace_record 聚合进 /trace 调试记录；handle_rwi 归一成 CCM
            # 风格存入 CCM_CALLS 供共享前端 GET /events 展示（处理与
            # http_router_service.py 完全一致）。
            raw = self._read_body()
            try:
                ev_payload = json.loads(raw.decode("utf-8"))
            except ValueError:
                return self._json(400, {"reason": "bad json"})
            if not isinstance(ev_payload, dict):
                return self._json(400, {"reason": "bad payload"})
            flat = _rwi_payload(ev_payload)
            trace_record(str(flat.get("call_id") or ""), "rwi",
                         event_type=flat.get("event_type"),
                         session_id=flat.get("session_id"),
                         direction=flat.get("direction"),
                         sip_status=flat.get("sip_status"),
                         reason=flat.get("reason"),
                         routing_path=flat.get("routing_path"),
                         routing_target=flat.get("routing_target"),
                         trunk=flat.get("trunk"))
            handle_rwi(ev_payload)
            log("rwi", f"call_id={flat.get('call_id')} "
                       f"event={flat.get('event_type')} "
                       f"session_id={flat.get('session_id')} "
                       f"sip_status={flat.get('sip_status')} reason={flat.get('reason')}")
            return self._json(200, {"ok": True})
        return self._json(404, {"reason": "not found"})

    def do_GET(self):
        path = self.path.split("?")[0].rstrip("/")
        cfg = CONFIG or {}
        if path == "/webrtc/accounts":
            # WebRTC 可用账号列表（供前端下拉选择）。默认=第一可用账号，
            # 顺序=配置 items 展开；配置外账号由前端自行追加测试项。
            # 附带 prefixes：每个账号支持的被叫前缀（归属位点 rule_list）。
            accounts, dflt = [], default_webrtc_account(cfg)
            prefixes = {}
            rules = {}
            metas = {}
            for entry in webrtc_entries(cfg):
                for acc in webrtc_entry_callers(cfg, entry):
                    if acc not in accounts:
                        accounts.append(acc)
                        prefixes[acc] = webrtc_account_prefixes(cfg, acc)
                        rules[acc] = webrtc_account_rules(cfg, acc)
                        metas[acc] = webrtc_entry_meta(cfg, acc)
            if accounts and dflt not in accounts:
                accounts.insert(0, dflt)
            if dflt not in prefixes:
                prefixes[dflt] = webrtc_account_prefixes(cfg, dflt)
                rules[dflt] = webrtc_account_rules(cfg, dflt)
                metas[dflt] = webrtc_entry_meta(cfg, dflt)
            return self._json(200, {"accounts": accounts, "default": dflt,
                                    "prefixes": prefixes, "rules": rules,
                                    "meta": metas})
        if path in ("/static/jssip.min.js", "/jssip.min.js"):
            # WebRTC 呼叫前端同源 JsSIP：本地加载，不依赖外部 CDN
            try:
                body = open(JSSIP_PATH, "rb").read()
            except OSError:
                return self._json(404, {"reason": "jssip.min.js missing"})
            self.send_response(200)
            self.send_header("Content-Type", "application/javascript; charset=utf-8")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path in ("/webrtc", "/phone"):
            # WebRTC 呼叫前端（webrtc_phone.html）
            try:
                body = open(WEBRTC_PHONE_PATH, "rb").read()
            except OSError:
                return self._json(404, {"reason": "webrtc_phone.html missing"})
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path in ("", "/ui"):
            # 共享前端（webui.html）：与 http_router_service.py 公用同一份页面
            try:
                with open(WEBUI_PATH, "rb") as f:
                    body = f.read()
            except OSError:
                return self._json(404, {"reason": "webui.html missing"})
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path in ("/health", "/healthz"):
            # 存活探针（等价 KubeTrunkDaemon 健康检查目标，方案 §6）
            return self._json(200, {
                "status": "ok",
                "service_name": "中继路由服务",
                "service": "kubeTrunk-route-decision",
                "sitecode": cfg.get("sitecode"),
                "route_points": len(cfg.get("route_points") or []),
                "gateways": sorted((cfg.get("gateways") or {}).keys()),
                "internal_services": len(cfg.get("internal_services") or []),
                "extensions": len(USERS),
                "trunk_sync_ok": SYNC_STATE.get("ok"),
                "trunk_sync_detail": SYNC_STATE.get("detail"),
            })
        if path.endswith("/events"):
            # 共享前端数据端点（与 http_router_service.py 同一响应结构）。
            # 呼叫事件 + 路由决策合并展示：同一通话的决策记录（TRACE，按
            # payload call_id 存）经 session_id 二次索引（TRACE_SESSION）与
            # 呼叫事件（CCM_CALLS，按 RWI envelope 的 session_id 存）合进
            # 同一呼叫 —— 决策行 kind=decision 在前、事件行 kind=event 在后
            # （时间序）。分机服务无决策跟踪，同构响应自然只出事件行，
            # 两服务共用同一前端渲染。
            calls = {}
            corr2cid = {}
            for key, recs in TRACE.items():
                for r in recs:
                    if r.get("kind") == "decision":
                        calls.setdefault(key, []).append(r)
                        cr = r.get("corr_id")
                        if cr:
                            corr2cid.setdefault(cr, key)
            for sid, evts in CCM_CALLS.items():
                # 先按 session_id，再按 corr_id/xcid 桥接（决策 corr_id 与 RWI
                # 事件 xcid 同值；WebRTC 的 JsSIP Call-ID ≠ RustPBX session 号，
                # 只能靠后者合并进同一卡片）。
                cid = (TRACE_SESSION.get(sid)
                       or corr2cid.get((evts[0] or {}).get("xcid")) or sid)
                calls.setdefault(cid, []).extend(
                    {"kind": "event", **e} for e in evts)
            return self._json(200, {
                "call_cap": CCM_CALLS_CAP,
                "call_count": len(calls),
                "total_events": sum(len(v) for v in calls.values()),
                "calls": calls,
            })
        if path.endswith("/config"):
            return self._json(200, cfg)
        if path == "/trace" or path.startswith("/trace/"):
            # 呼叫跟踪查询：
            #   按关联键：/trace/<call_id|session_id|corr_id>
            #   按主被叫：/trace?caller=&callee=&corr_id=（支持只填其一，用于在
            #             未发起呼叫、仅决策被拦截时仍能按主/被叫查到记录并显示主被叫）
            call_id = path[len("/trace"):].strip("/")
            qs = {}
            if "?" in self.path:
                from urllib.parse import parse_qs
                qs = parse_qs(self.path.split("?", 1)[1])
            if call_id:
                return self._json(200, {"call_id": call_id,
                                        "records": (TRACE.get(call_id)
                                                    or TRACE.get(TRACE_SESSION.get(call_id) or "") or [])})
            want_caller = (qs.get("caller") or [""])[0]
            want_callee = (qs.get("callee") or [""])[0]
            want_corr = (qs.get("corr_id") or [""])[0]
            if not (want_caller or want_callee or want_corr):
                return self._json(200, {"call_id": "", "records": []})
            hits = []
            for cid, recs in TRACE.items():
                for r in recs:
                    caller = str(r.get("caller") or "")
                    callee = str(r.get("callee") or "")
                    cr = str(r.get("corr_id") or "")
                    if want_caller and caller != want_caller:
                        continue
                    if want_callee and callee != want_callee:
                        continue
                    if want_corr and cr != want_corr:
                        continue
                    hits.append({"call_id": cid, **r})
            hits.sort(key=lambda h: h.get("ts") or 0, reverse=True)
            return self._json(200, {"call_id": "", "search": True,
                                    "caller": want_caller, "callee": want_callee,
                                    "corr_id": want_corr, "records": hits})
        return self._json(404, {"reason": "not found"})

    def log_message(self, fmt, *args):
        log("http", self.address_string() + " " + (fmt % args))


def main():
    parser = argparse.ArgumentParser(description="kubeTrunk route_decision 样例服务")
    parser.add_argument("--listen",
                        default=os.environ.get("ROUTE_LISTEN", "127.0.0.1:9612"))
    parser.add_argument("--config", default=os.environ.get("ROUTE_CONFIG", ""),
                        help="路由配置文件（{sitecode}.json），必填；也可用 ROUTE_CONFIG 环境变量")
    args = parser.parse_args()
    host, _, port = args.listen.rpartition(":")

    global CONFIG
    # 记录启动使用的确切配置路径，供 /config/reload 复用（reload 不再回退
    # 到 ROUTE_CONFIG 环境变量 —— 用 --config 启动时它可能为空）。
    global CONFIG_FILE
    CONFIG_FILE = (args.config or os.environ.get("ROUTE_CONFIG", "")).strip()
    # 配置强制外部提供：缺省/缺失/非法 → fail-fast 退出，不启动
    # （避免内置兜底配置在误启/忘配时悄悄接管路由）。
    try:
        CONFIG = load_config(CONFIG_FILE or None)
    except ValueError as exc:
        sys.stderr.write(f"[route-decision] FATAL: {exc}\n")
        sys.exit(1)

    # 网关 → RustPBX trunk 注册表自动同步（结构化 targets + 自动 ACL 的
    # 前提；失败非致命，targets 降级纯 URI，/health 可见）
    sync_trunks_with_retry(CONFIG)

    server = ThreadingHTTPServer((host, int(port)), RouteDecisionHandler)
    log("main", f"listening on {host}:{port}")
    log("main", f"sitecode={CONFIG.get('sitecode')} "
                f"internal_services={len(CONFIG.get('internal_services') or [])}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()