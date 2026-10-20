#!/usr/bin/env python3
"""
kubeTrunk 中继路由服务（route_decision）参考实现 —— 样例脚本。

对应《基于RustPBX的kubeTrunk服务(RTM)技术方案》§4.3：以独立 HTTP 服务
实现 kubeTrunk 分层路由裁决，作为后续 KubeTrunkDaemon（Java）落地时的
裁决语义/配置模型参考原型 —— 本脚本仅依赖标准库，无第三方依赖。

配置模型对齐真实前端数据格式（rtm-web-lite RULES_MAP，2026-10-16 生产数据）：
以 dr_carrier（中继组表）为核心 —— 配置字段命名 route_points（路由点位）/
rule_list（carriers/rules 是 OpenSIPS 术语，对非专业人员不友好，已改名）；
rule_list 为入口过滤条件、gw_list 为出口值。
dr_rules 入口过滤含主被叫两个条件：主叫 = 主叫组引用（组内为主叫号码或
主叫号码前缀记录，支持多组命中）或直接主叫记录；被叫 = 必须单条前缀记录
（不支持数组）。pri_prefix 为出局码（命中后加到被叫前发往网关，
drouting.c push_gw_for_usage/build_ruri 证实），不是主叫匹配条件。

接入方式（RustPBX 现有协议，无需改 Rust 代码）：

    [proxy.http_router]
    url = "http://127.0.0.1:9612/route/decision"
    timeout_ms = 500
    fallback_to_static = false      # fail-closed 全动态路由（方案 §4.1）

呼叫跟踪（本服务附加能力）：[rwi_webhook] url 指向本服务 POST /rwi ——
接收 RustPBX 推送的 RWI 呼叫事件（call_created/call_hangup/call_error 等，
协议 {rwi, event_id, timestamp, call_id, event_type, event{...}}，2xx 送达、
event_id 幂等），与路由决策按 call_id 聚合；GET /trace/<call_id> 查询单次
呼叫的合并跟踪记录（决策层/命中中继/出局目标 + 事件时间线）。

事件链展示（与 examples/yunaoai/extension_decision_service.py 完全一致）：/rwi 同时
把事件归一成 CCM 风格（status_map + KTM_<session_id> X-cid 推导）存入
CCM_CALLS，GET /events 供共享前端（本目录 webui.html，"RustPBX 呼叫事件
查看器"）轮询展示；GET / 与 /ui 出同一份页面，POST /events/clear 清空。

请求体（HttpCallRouter，src/proxy/routing/http.rs）：

    {
      "call_id": "...", "session_id": "...",
      "from": "\"cc9803280017\" <sip:cc9803280017@localhost>;tag=...",
      "to": "<sip:13800000000@localhost>",
      "source_addr": "172.28.5.223:38910",      # 对端 ip:port（ Via received ）
      "direction": "inbound" | "internal",      # RustPBX 视角：中继来话/本地发起
      "method": "INVITE", "uri": "sip:13800000000@localhost",
      "headers": {...}, "body": "..."
    }

响应体（HttpResponsePayload）：

    forward: { action, targets: [{"callee": "..", "trunk": ".."} | URI],
               strategy: "sequential", caller?, headers?, allow_codecs?,
               timeout?, max_ring_time?, media_proxy?, rtp_config? }
    reject:  { action: "reject", status, reason }

网关 → trunk 注册表自动同步（本服务附加能力）：启动时（与 POST
/config/reload 时）先把 RustPBX trunk 数据库里的当前网关清掉（同名或同
dest 的旧条目、网关改名残留一并删除），再按配置全新写入（console API
Bearer 认证，PUT 创建）+ AMI POST /ami/v1/reload/trunks 强制生效（DB 重生成
trunks.generated.toml + 更新注册表）。网关名即 trunk 名（结构化 targets
的前提，两配置文件统一命名）；字段全量映射：dest→sip_server、transport→sip_transport、
direction（默认 bidirectional）、max_load→max_concurrent（-1 不限省略）、
max_cps、allowed_ips、username/password→auth_、outbound_proxy，
profile/external_ip/bind_ip/codec/ringback/media_mode 等进 metadata.sbc
（整体替换，组装完整 {"sbc": {...}}）。同步失败非致命：targets 降级纯
URI（呼叫仍可达，trunk 盖章不生效），/health 可见同步状态。

裁决流程（分层匹配，2026-10-16 设计确认 —— 取代 rtmsip 的"主叫组优先"顺序）：

    来源准入不在本服务：由 RustPBX [proxy.acl] / trunk inbound_hosts 在路由
    前拒绝（等价 rtmsip auth_check 的防盗打职能），本服务只做路由裁决，
    不再做核心网/接入网分类。

    1. 主被叫都符合：主叫命中规则的主叫过滤（主叫组记录或直接主叫记录）
       且 被叫前缀命中
    2. 被叫符合：无主叫过滤的规则中被叫前缀命中（跨主叫，不预筛方向）
    3. 主叫符合：主叫命中主叫过滤的空前缀规则（组内兜底，等价 rtmsip
       noprefix；主叫多组命中时按主叫记录最长优先）
       —— 层内：被叫前缀最长 → priority 降序 → 主叫记录最长 → 配置顺序
    4. 默认路由（主被叫均未命中才走，等价 rtmsip 方向默认组 cr1/cr2 的
       职责替换）：按发起方 User-Agent 识别内部服务（internal_services，
       配置了 source_cidrs 的条目要求 UA+IP 双命中，防白名单来源伪造 UA
       走默认出局）→ default_for="internal" 的兜底组；否则 →
       default_for="external" 的兜底组。兜底组内按被叫前缀选规则
       （空前缀规则匹配全部被叫）。UA 未识别记日志，运营侧可见。
    5. 号码变换：strip 剥码、pri_prefix 出局码加到被叫前、区号加 0
       （add_prefix0，可选默认关）、主叫 sitecode 循环剥离。
    6. targets：出口 gw_list → 结构化 targets（{"callee", "trunk"}，网关名
       即 RustPBX trunk 名，启动时自动同步写入见上） +
       strategy="sequential"（等价 use_next_gw 失败换下一网关；停用网关
       跳过；max_load 并发控制由 RustPBX trunk 配置承接）。同步失败降级
       纯 URI targets（呼叫仍可达，trunk 盖章不生效）。

rtmsip 语义对照（drouting.c do_routing，源码核实 2026-10-16）：

    rtmsip 组内为两级匹配 —— "//优先主叫 + 被叫前缀 匹配"（get_prefix）→
    "//被叫前缀没有，则主叫 + 空被叫前缀 匹配"（new_check_rt / noprefix），
    失败回退方向默认组（呼出 1 / 呼入 2）。本设计将"主叫符合"（空前缀）
    排到"被叫符合"之后（被叫精确度优先），并用 User-Agent 兜底替代方向
    默认组 —— 方向判定只发生在主被叫均未命中的第 4 层。

配置模型（方案 §5.3 {sitecode}.json）：ROUTE_CONFIG 环境变量指定 JSON 文件，
未指定用内置样例配置（字段值取自 3201 站点生产数据）；POST /config/reload
热加载（等价 dbt_reload）。号段表（aaa_phone_prefix，cfg_phone_prifix 等价）
单独文件：与主配置同目录 aaa_phone_prefix.json，主配置不内联 area_codes
（默认空，文件缺失/非法时区号查询直接落空）。

用法：

    python3 examples/yunaoai/route_decision_service.py [--listen 127.0.0.1:9612]
    ROUTE_CONFIG=/path/to/trunk.json python3 examples/yunaoai/route_decision_service.py
"""

import argparse
import ipaddress
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# --------------------------------------------------------------------------- #
# 内置样例配置（{sitecode}.json 等价物，字段值取自 3201 站点生产数据）。
# 真实部署由 KubeTrunkDaemon 从 S3 /config/trunk/{sitecode}.json 拉取后渲染，
# 本脚本经 ROUTE_CONFIG 指向同一份 JSON。以 route_group 为核心：
#
#   gateways          网关表（sbc_dr_gateways 等价）：gw_list 的出口解析目标
#   caller_groups     主叫组（sbc_dr_groups 等价）：组内为主叫号码/前缀记录
#   route_points      路由点位（sbc_dr_carriers 等价，核心）：point_id 标记、
#                     rule_list=入口过滤（主叫过滤 + 被叫单前缀），gw_list=出口
#   internal_services 已知内部发起 INVITE 的服务（第 4 层 UA 识别）
#   area_codes        区号表（cfg_phone_prifix 等价）：手机号前 7 位 → 区号
# --------------------------------------------------------------------------- #
DEFAULT_CONFIG = {
    "sitecode": "3201",
    # 网关表（sbc_dr_gateways 等价）：type=internal(对内)/external(对外)；
    # max_load 并发上限（-1 不限，等价 RTM maxLoad）；enabled=False 停用
    # （等价探测摘除）
    "gateways": {
        "gw-ccm": {"dest": "sip:172.28.5.223:38910", "transport": "udp",
                   "type": "internal", "max_load": -1},
        "gw-clpss": {"dest": "sip:172.28.5.223:8880", "transport": "udp",
                     "type": "internal", "max_load": -1},
        "gw3299": {"dest": "sip:100.88.0.36:38897", "transport": "udp",
                   "type": "external", "max_load": 100, "enabled": False},
        "gw-vos": {"dest": "sip:8.136.48.226:5566", "transport": "udp",
                   "type": "external", "max_load": 100,
                   "area_code": "025", "add_prefix0": False},
        "gw-digital": {"dest": "sip:100.88.9.9:5060", "transport": "udp",
                       "type": "external", "max_load": 1, "enabled": False},
        "gw-jinhua": {"dest": "sip:100.88.8.8:5060", "transport": "udp",
                      "type": "external", "max_load": 100},
    },
    # 主叫组（sbc_dr_groups 等价）：组内为主叫号码或主叫号码前缀记录，
    # 一个主叫可同时命中多个组（等价 get_all_group_id 多组返回）
    "caller_groups": {
        "848": ["cc98032"],          # vos中国：cc98032 前缀段
        "850": ["cc9803280017"],     # 内部测试：精确号码（比 848 前缀更具体）
        "851": ["cc9803298016"],     # 北京固话
        "854": ["cc9803298014"],     # 内部测试手机号
    },
    # 路由组（sbc_dr_carriers 等价，以 route_group 为核心）：
    #   rule_list = 入口过滤条件 —— caller_group（主叫组引用）或 caller（直接
    #           主叫记录）+ prefix（被叫，必须单条前缀记录，不支持数组；
    #           空 = 匹配全部被叫）；pri_prefix=出局码；priority 同层排序
    #   gw_list = 出口值（按 gateways 解析，sequential 依次尝试）
    #   default_for = 兜底组标记（external=外部来话兜底，internal=内部来话
    #           兜底；等价 rtmsip cr1/cr2 方向默认组的职责替换）
    #   group_type = internal（gw_list 连接 internal 网关）/ external（连接
    #           external 网关），与 gateway type 同一词汇
    # 样例 values 取自 3201 生产数据（cr4/cr6/cr7/cr10 等）
    "route_points": [
        # —— 呼入被叫规则（等价 groupid=0 规则：无主叫过滤 + 被叫前缀）——
        {"point_id": "net_beijing_3201", "group_type": "internal",
         "gw_list": ["gw-ccm"],
         "rule_list": [{"prefix": "3201", "strip": 4}]},
        {"point_id": "conference", "group_type": "internal",
         "gw_list": ["gw-ccm"],
         "rule_list": [{"prefix": "30", "strip": 0}]},
        {"point_id": "ivr", "group_type": "internal",
         "gw_list": ["gw-ccm"],
         "rule_list": [{"prefix": "31", "strip": 0}]},
        {"point_id": "agent_gw2", "group_type": "internal",
         "gw_list": ["gw-clpss"],
         "rule_list": [{"prefix": "87", "strip": 0},
                   {"prefix": "320187", "strip": 4},
                   {"prefix": "1133", "strip": 0},
                   {"prefix": "32011133", "strip": 4}]},
        # 出局码 33 删码（KubePhone 分机呼出 → 33+号码 → 剥 2 位转发）；
        # gw3299 停用 → 自动跳过（等价探测摘除）
        {"point_id": "outbound-33", "group_type": "external",
         "gw_list": ["gw3299", "gw-vos"],
         "rule_list": [{"prefix": "33", "strip": 2}]},
        # —— 自定义业务组（等价 cr4/cr6/cr7/cr10）：主叫组过滤 + 空被叫前缀
        #    + 出局码（pri_prefix 加到被叫前发给网关）——
        {"point_id": "vos", "group_type": "external",
         "gw_list": ["gw-vos"],
         "rule_list": [{"prefix": "", "caller_group": "848",
                    "pri_prefix": "cc98032", "priority": 9}]},
        {"point_id": "internal-test", "group_type": "external",
         "gw_list": ["gw-vos"],
         "rule_list": [{"prefix": "", "caller_group": "850",
                    "pri_prefix": "cc9803280017", "priority": 9}]},
        {"point_id": "bj-fixed", "group_type": "external",
         "gw_list": ["gw-vos"],
         "rule_list": [{"prefix": "", "caller_group": "851",
                    "pri_prefix": "cc9803298016", "priority": 9}]},
        {"point_id": "test-mobile", "group_type": "external",
         "gw_list": ["gw-vos"],
         "rule_list": [{"prefix": "", "caller_group": "854",
                    "pri_prefix": "cc9803298014", "priority": 9}]},
        # —— 兜底组（等价 rtmsip cr1/cr2 方向默认组的职责替换）——
        # 外部来话兜底 → 转核心（等价 cr2：空被叫前缀匹配全部被叫）
        {"point_id": "internal-default", "group_type": "internal",
         "default_for": "external",
         "gw_list": ["gw-ccm"],
         "rule_list": [{"prefix": "", "strip": 0}]},
        # 内部服务来话兜底 → 默认出局（生产可置空 gw_list 实现 fail-closed，
        # 等价真实 cr1：接入网默认组无网关 → 500）
        {"point_id": "external-default", "group_type": "external",
         "default_for": "internal",
         "gw_list": ["gw-vos", "gw-jinhua"],
         "rule_list": [{"prefix": "", "strip": 0}]},
    ],
    # 第 4 层内部服务识别：user_agent 大小写不敏感子串匹配；source_cidrs
    # 可选加固（UA 与来源 IP 双命中才认内部，防白名单来源伪造 UA 防盗打）
    "internal_services": [
        {"user_agent": "CCM", "source_cidrs": ["172.28.5.0/24"]},
        {"user_agent": "kubePhone"},
    ],
}

# 生效配置（load_config() 填充）。模块级单例：与 KubeTrunkDaemon 内存裁决
# 配置一致的形态（方案 §4.3 "全动态：内存裁决，无静态兜底"）。
CONFIG: dict = {}

# 呼叫跟踪：路由决策 + RWI 呼叫事件按 call_id 聚合（内存环形上限）。
# RustPBX [rwi_webhook] url 指向本服务 POST /rwi，推送 call_created/
# call_hangup/call_error 等生命周期事件（协议：{rwi, event_id, timestamp,
# call_id, event_type, event{...}}，2xx 即送达，event_id 幂等去重）；
# GET /trace/<call_id> 查询单次呼叫的 决策+事件 合并跟踪记录。
TRACE: dict = {}            # call_id -> [record]
TRACE_SESSION: dict = {}    # session_id -> call_id（RWI 事件 envelope 的 call_id
                            #   是 RFC 7989 session_id，需二次索引关联）
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
    xcid = None
    if isinstance(sip_headers, dict):
        xcid_key = next((k for k in sip_headers if k.lower() == "x-cid"), None)
        xcid = sip_headers.get(xcid_key) if xcid_key else None
    xcid = xcid or f"KTM_{session_id}"

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
        if not g.get("dest"):
            errors.append(f"gateway '{name}' missing dest")
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


def load_config() -> dict:
    """从 ROUTE_CONFIG 指定的 JSON 文件加载配置，未指定用内置样例。

    手机号段表单独文件：同目录 aaa_phone_prefix.json，主配置不再内联
    area_codes 字段（默认空，号段表是唯一来源）。
    等价 rtm-daemon 的 TDB 生成 + dbt_reload 链路收敛为一次文件读取。"""
    path = os.environ.get("ROUTE_CONFIG", "")
    if path:
        try:
            with open(path, "r", encoding="utf-8") as f:
                cfg = json.load(f)
        except (OSError, ValueError) as exc:
            log("config", f"failed to load {path}: {exc} — keep current config")
            return CONFIG or DEFAULT_CONFIG
    else:
        cfg = DEFAULT_CONFIG
    # 号段表（aaa_phone_prefix）单独文件，存在则覆盖内联字段
    # 号段表（aaa_phone_prefix）单独文件是唯一来源，主配置不内联（默认空）
    cfg["area_codes"] = load_phone_prefix(path)
    errors = validate_config(cfg)
    if errors:
        for e in errors:
            log("config", f"invalid: {e}")
        log("config", f"REJECTED config from {path or '<embedded>'} — keep current")
        return CONFIG or DEFAULT_CONFIG  # 非法配置拒绝加载，保留旧配置（§8）
    log("config", f"loaded {path or '<embedded>'} sitecode={cfg.get('sitecode')} "
                  f"route_points={len(cfg.get('route_points') or [])} "
                  f"gateways={len(cfg.get('gateways') or {})}")
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

    # 清掉当前网关：与配置网关同名，或 dest 指向同一台对端（改名残留）
    cfg_dests = {_dest_host_port(gw.get("dest", "")) for gw in gateways.values()}
    deleted, created, failed = [], [], []
    skip_create = set()
    for name, item in existing.items():
        item_dest = _dest_host_port(item.get("sip_server") or "")
        if name not in gateways and not (item_dest and item_dest in cfg_dests):
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
            if name in gateways:
                skip_create.add(name)
            continue
        deleted.append(name)
        log("trunk-sync", f"cleared trunk '{name}' (id={trunk_id})")

    for name, gw in gateways.items():
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
    """出口 gw_list → 结构化 targets 列表（{"callee", "trunk"}）。

    RustPBX http_router 已支持 {callee, trunk} 结构化 targets：网关名即
    RustPBX trunk 注册表条目（本服务启动时把 gateways 表同步写入，见
    sync_trunks_to_rustpbx），callee 限定到该 trunk 的 dest host:port，
    由 RustPBX 按 trunk 配置盖章（next-hop/transport/凭据、媒体 hints、
    CPS 并发闸门、CDR 归因）——等价静态路由的 trunk 承接。
    多网关按序展开 + strategy="sequential" = 等价 use_next_gw 失败换下一
    网关；停用网关跳过（等价探测摘除）。
    同步失败降级（SYNC_STATE.ok=False）：生成 `sip:<callee>@<dest-host:port>`
    纯 URI targets —— 呼叫仍按 host:port 可达，trunk 盖章不生效。"""
    targets = []
    for name in gwlist or []:
        gw = (cfg.get("gateways") or {}).get(name)
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
    return targets


def resolve_xcid(payload: dict) -> str:
    """X-cid 呼叫关联约定（与 examples/yunaoai/extension_decision_service.py 同源）：
    上游带 X-cid 沿用（跨节点转发），否则生成 KTM_<session_id> —— session_id
    由 RustPBX /route 请求直接携带，与 RWI 事件完全一致，两端无需状态映射。"""
    xcid = header_value(payload, "X-cid")
    if xcid:
        return xcid
    session_id = payload.get("session_id") or payload.get("call_id") or ""
    return f"KTM_{session_id}"


def route_decision(payload: dict) -> dict:
    """主流程：分层匹配 + 默认路由 + 号码变换 + targets。"""
    cfg = CONFIG or DEFAULT_CONFIG

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

    # 第 1~3 层：主被叫都符合 → 被叫符合 → 主叫符合
    group_hits = resolve_caller_groups(caller, cfg)
    # 计算过程轨迹（决策记录 steps 字段）：主叫组解析结果 + 逐条规则
    # 评估顺序（跳过/命中/落选原因）+ 最优选择，界面可完整查看计算过程
    steps = ["主叫组解析: " + (", ".join(f"{g}(长度{v})" for g, v in group_hits.items())
                              or "无")]
    route_group, rule, layer = match_rule(caller, callee, group_hits, cfg, trail=steps)
    # 判定细节（随决策记录写入，界面可查看判断过程）：主叫分组命中、
    # 命中规则过滤条件、L4 方向判定依据、号码变换前后值、计算过程轨迹
    details = {"group_hits": group_hits, "steps": steps}

    # 第 4 层：主被叫均未命中 → User-Agent 判内部/外部 → 兜底组
    if rule is None:
        internal, why_internal = identify_internal(payload, cfg)
        details["internal_check"] = why_internal
        steps.append("第 4 层方向判定: " + why_internal)
        side = "internal" if internal else "external"
        layer = f"L4-{side}"
        route_group = find_default_route_group(side, cfg)
        rule = match_default_rule(route_group, callee) if route_group else None
        if route_group:
            if rule:
                steps.append(f"兜底组: {route_group.get('point_id')}，"
                             f"兜底规则 prefix='{(rule.get('prefix') or '').strip()}'")
            else:
                steps.append(f"兜底组: {route_group.get('point_id')}，无匹配规则")
        else:
            steps.append(f"兜底组: 无（{side} 方向未配置默认组）")
        if not internal:
            # UA 未识别/未配置：运营可见（UA 串随版本升级变化会落到这里）
            log("decision", f"fallback UA not recognized → {side} default "
                            f"(callee={callee} caller={caller})")
        if rule is None:
            log("decision", f"no default route_group/rule for {side} — reject 500")
            trace_record(payload.get("call_id") or "", "decision",
                         session_id=payload.get("session_id"),
                         layer=layer, status=500, callee=callee, caller=caller,
                         reason="no default route_group/rule", **details)
            return {"action": "reject", "status": 500,
                    "reason": "No GW Route found"}
    else:
        log("decision", f"hit layer={layer} route_group={route_group.get('point_id')} "
                        f"groups={sorted(group_hits)}")

    # 号码变换
    details["rule"] = rule_detail(rule)
    details["callee_raw"] = callee
    details["caller_raw"] = caller
    callee2 = apply_rule_transforms(rule, callee)
    first_gw = (cfg.get("gateways") or {}).get((route_group.get("gw_list") or [""])[0]) or {}
    callee2 = apply_area_prefix0(callee2, first_gw, cfg)
    caller2 = strip_caller_sitecode(caller, cfg)

    # targets（出口 gw_list，多网关 sequential = use_next_gw 等价）
    targets = gateway_targets(callee2, route_group.get("gw_list"), cfg)
    if not targets:
        trace_record(payload.get("call_id") or "", "decision",
                     session_id=payload.get("session_id"),
                     layer=layer, status=500, route_group=route_group.get("point_id"),
                     callee=callee2, caller=caller, reason="no usable gateway",
                     **details)
        return {"action": "reject", "status": 500, "reason": "No GW Route found"}

    resp = {
        "action": "forward",
        "targets": targets,
        "strategy": "sequential",
        "headers": {"X-cid": resolve_xcid(payload)},
    }
    if caller2 and caller2 != caller:
        resp["caller"] = caller2
    # 网关编解码策略 → allow_codecs（等价 trunk codec 下发）
    codecs = first_gw.get("audio_codecs")
    if codecs:
        resp["allow_codecs"] = codecs

    log("decision", f"forward layer={layer} route_group={route_group.get('point_id')} "
                    f"callee={callee2} targets={targets} caller={resp.get('caller')}")
    trace_record(payload.get("call_id") or "", "decision",
                 session_id=payload.get("session_id"),
                 layer=layer, action="forward",
                 route_group=route_group.get("point_id"),
                 callee=callee2, caller=resp.get("caller") or caller,
                 targets=targets, **details)
    return resp


# --------------------------------------------------------------------------- #
# HTTP 服务（stdlib，与 examples/yunaoai/extension_decision_service.py 同一形态）
# --------------------------------------------------------------------------- #
class RouteDecisionHandler(BaseHTTPRequestHandler):
    def _json(self, status, obj):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

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
        path = self.path.split("?")[0].rstrip("/")
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
            global CONFIG
            CONFIG = load_config()
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
        cfg = CONFIG or DEFAULT_CONFIG
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
            for key, recs in TRACE.items():
                for r in recs:
                    if r.get("kind") == "decision":
                        calls.setdefault(key, []).append(r)
            for sid, evts in CCM_CALLS.items():
                cid = TRACE_SESSION.get(sid) or sid
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
            # 呼叫跟踪查询：/trace/<call_id> 或 /trace?call_id=<id>
            call_id = path[len("/trace"):].strip("/")
            if not call_id and "?" in self.path:
                from urllib.parse import parse_qs
                qs = parse_qs(self.path.split("?", 1)[1])
                call_id = (qs.get("call_id") or [""])[0]
            records = (TRACE.get(call_id)
                       or TRACE.get(TRACE_SESSION.get(call_id) or "") or [])
            return self._json(200, {"call_id": call_id, "records": records})
        return self._json(404, {"reason": "not found"})

    def log_message(self, fmt, *args):
        log("http", self.address_string() + " " + (fmt % args))


def main():
    parser = argparse.ArgumentParser(description="kubeTrunk route_decision 样例服务")
    parser.add_argument("--listen",
                        default=os.environ.get("ROUTE_LISTEN", "127.0.0.1:9612"))
    args = parser.parse_args()
    host, _, port = args.listen.rpartition(":")

    global CONFIG
    CONFIG = load_config()

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
