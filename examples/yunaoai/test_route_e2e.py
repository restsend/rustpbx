#!/usr/bin/env python3
"""kubeTrunk route_decision_service HTTP 接口端到端验证 —— 全场景覆盖.

用法:
    python3 examples/yunaoai/test_route_e2e.py

覆盖矩阵:
    [webrtc]      caller_group 命中 → 白名单前缀 → 正确 webrtc 条目/gw_list
    [webrtc]      caller 不属 webrtc group → 降级到 route_points 兜底
    [webrtc]      source_addr 非法/缺失/IPv6 → allowed_ips 白名单拒答
    [webrtc]      caller 纯 SIP URI / 匿名 → parse_user 正确提取主叫
    [webrtc]      被动网关 → network_profile_id + rtp_config.external_ip 传递
    [route]       caller_group 命中 cg-8120 / cg-8196 → 正确 route_point
    [route]       caller 无组 → identify_internal 判定 → default_for 兜底
    [route]       source_addr 非法 → route_points 不校验 allowed_ips 正常 forward
    [method]      method != INVITE → action=not_handled
    [edge]        callee 空/畸形/字段缺失 → reject 484
"""
import json, urllib.request, urllib.error, sys
from datetime import datetime

URL = "http://127.0.0.1:9612/route/decision"
BASE_CFG = "examples/yunaoai/config/3201.json"
TEST_CFG = "examples/yunaoai/config/test_3202.json"

def _reload(cfg_path):
    req = urllib.request.Request(
        "http://127.0.0.1:9612/config/reload",
        data=json.dumps({"config_file": cfg_path}).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            body = json.loads(r.read())
            if not body.get("ok"):
                print(f"{RED}⚠ reload FAILED: {body}{RESET}")
                return False
            return True
    except Exception as e:
        print(f"{RED}⚠ reload error: {e}{RESET}")
        return False

def _sip_uri_or_wrap(value, host="127.0.0.1"):
    if not value: return ""
    v = str(value)
    if v.startswith(("sip:", "sips:", "tel:")) or "@" in v:
        return v if v.startswith(("sip:", "sips:", "tel:")) else f"sip:{v}"
    return f"sip:{v}@{host}"

def call(caller=None, callee=None, is_webrtc=True,
         source_addr="127.0.0.1:15060", ua=None, method=None,
         extra=None, **payload_kw):
    p = {"call_id": f"e2e-{abs(hash(str(caller)+str(callee)))%99999}",
         "session_id": f"sess-{abs(hash(str(callee)))%99999}",
         "from": _sip_uri_or_wrap(caller),
         "to": _sip_uri_or_wrap(callee),
         "source_addr": source_addr,
         "headers": {}}
    if caller is not None: p["caller"] = caller
    if callee is not None: p["callee"] = callee
    if is_webrtc: p["headers"]["X-cid"] = f"WRTC_e2e_{abs(hash(str(caller)+str(callee)))%99999}"
    if ua: p["headers"]["User-Agent"] = ua
    if method: p["method"] = method
    if extra: p.update(extra)
    p.update(payload_kw)
    try:
        req = urllib.request.Request(URL, data=json.dumps(p).encode(),
                                      headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=3) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        try: body = json.loads(e.read())
        except: body = {"http_error": e.code}
        return e.code, body
    except Exception as e:
        return 0, {"error": str(e)}

GREEN, RED, YELLOW, CYAN, BOLD, RESET, DIM = (
    "\033[32m","\033[31m","\033[33m","\033[36m","\033[1m","\033[0m","\033[2m")

class Case:
    def __init__(self, tag, name, **kwargs):
        self.tag = tag
        self.name = name
        self.kwargs = kwargs
        self.caller = kwargs.get("caller", "")
        self.callee = kwargs.get("callee", "")
        self.is_webrtc = kwargs.get("is_webrtc", True)
        self.source_addr = kwargs.get("source_addr", "127.0.0.1:15060")
        self.ua = kwargs.get("ua")
        self.method = kwargs.get("method")
        self.expect_action = kwargs.pop("expect_action", "forward")
        self.expect_npid = kwargs.pop("expect_npid", None)
        self.expect_npid_absent = kwargs.pop("expect_npid_absent", False)
        self.expect_targets_contain = kwargs.pop("expect_targets_contain", None)
        self.expect_rtp_external_ip = kwargs.pop("expect_rtp_external_ip", None)
        self.expect_status = kwargs.pop("expect_status", None)
        self.expect_reason_contains = kwargs.pop("expect_reason_contains", None)
        self.expect_callee2_contains = kwargs.pop("expect_callee2_contains", None)
        self.expect_callee2_exact = kwargs.pop("expect_callee2_exact", None)
        self.passed = True
        self.errs = []
        self.st, self.body = None, None

    def run(self):
        self.st, self.body = call(**self.kwargs)
        b = self.body
        if self.expect_status is not None and b.get("status") != self.expect_status:
            self.passed = False
            self.errs.append(f"status={b.get('status')!r}!={self.expect_status}")
        if self.expect_action and b.get("action") != self.expect_action:
            self.passed = False
            self.errs.append(f"action={b.get('action')!r}!={self.expect_action!r}")
        if self.expect_reason_contains:
            reason = b.get("reason") or ""
            if self.expect_reason_contains not in reason:
                self.passed = False
                self.errs.append(f"reason缺{self.expect_reason_contains!r}, 实际={reason!r}")
        npid = b.get("network_profile_id")
        if self.expect_npid and npid != self.expect_npid:
            self.passed = False
            self.errs.append(f"npid={npid!r}!={self.expect_npid!r}")
        if self.expect_npid_absent and npid is not None:
            self.passed = False
            self.errs.append(f"npid应为空, 实际={npid!r}")
        if self.expect_targets_contain:
            flat = json.dumps(b.get("targets") or [])
            if self.expect_targets_contain not in flat:
                self.passed = False
                self.errs.append(f"targets缺{self.expect_targets_contain!r}: {b.get('targets')}")
        if self.expect_rtp_external_ip:
            rtp = b.get("rtp_config") or {}
            got = rtp.get("external_ip")
            if got != self.expect_rtp_external_ip:
                self.passed = False
                self.errs.append(f"rtp.ext={got!r}!={self.expect_rtp_external_ip!r}")
        if self.expect_callee2_contains or self.expect_callee2_exact:
            tgts = b.get("targets") or []
            first_callee = None
            for t in tgts:
                if isinstance(t, dict):
                    first_callee = t.get("callee")
                    break
                if isinstance(t, str) and t.startswith("sip:"):
                    import urllib.parse as _up
                    callee_part = _up.urlparse(t).path.lstrip("/").split("@")[0]
                    if callee_part:
                        first_callee = callee_part
                        break
            if self.expect_callee2_exact and first_callee != self.expect_callee2_exact:
                self.passed = False
                self.errs.append(f"callee2={first_callee!r}!={self.expect_callee2_exact!r}")
            if self.expect_callee2_contains and first_callee is not None \
                    and self.expect_callee2_contains not in first_callee:
                self.passed = False
                self.errs.append(f"callee2缺{self.expect_callee2_contains!r}: {first_callee!r}")

cases = [
    # ╔═══════════════════════════════════════════════════════════════════════╗
    # ║  GROUP W: webrtc 路由 — caller_group + 白名单前缀 + 混合网关         ║
    # ╠═══════════════════════════════════════════════════════════════════════╣
    # ║  A. 正常命中                                                          ║
    # ║  B. reject 分支（越权前缀 / 非法来源）                                ║
    # ║  C. caller 不属任何 webrtc group → 直接拦截（不降级 route_points）     ║
    # ║  D. allowed_ips / source_addr 边界                                    ║
    # ║  E. 输入解析（caller 纯 URI / 匿名）                                  ║
    # ╚═══════════════════════════════════════════════════════════════════════╝

    # --- W-A: webrtc 正常命中 (8) ---
    Case("W-A1", "被动网关 reg879830001 → 8798 ⭐核心修复点",
         caller="8899", callee="8798", expect_npid="tailscale",
         expect_targets_contain="100.88.0.204",
         expect_rtp_external_ip="100.88.0.1"),
    Case("W-A2", "被动网关 → 8798001 长号分机",
         caller="8899", callee="8798001", expect_npid="tailscale",
         expect_targets_contain="100.88.0.204"),
    Case("W-A3", "ya-pbx+yacc55-fs → 98088 混合出口 tailscale",
         caller="8899", callee="980880001", expect_npid="tailscale",
         expect_targets_contain="yacc55-fs"),
    Case("W-A4", "ya-pbx+yacc55-fs → 138前缀",
         caller="8899", callee="13800138000", expect_npid="tailscale"),
    Case("W-A5", "IP对等 liusy → 1551 tailscale",
         caller="8812", callee="1551", expect_npid="tailscale",
         expect_targets_contain="liusy"),
    Case("W-A6", "lvcx → 138前缀 cg-webrtc-b 无profile",
         caller="8812", callee="13800138000", expect_npid_absent=True,
         expect_targets_contain="lvcx"),
    Case("W-A7", "lvcx → 159前缀",
         caller="8812", callee="15912345678", expect_npid_absent=True),
    Case("W-A8", "lvcx → 178前缀",
         caller="8812", callee="17812345678", expect_npid_absent=True),

    # --- W-B: webrtc reject 分支 (4) ---
    Case("W-B1", "caller=8899(cg-a) → 1551(cg-b前缀) → reject",
         caller="8899", callee="1551", expect_action="reject",
         expect_reason_contains="prefix not allowed"),
    Case("W-B2", "caller=8812(cg-b) → 8798(cg-a前缀) → reject",
         caller="8812", callee="8798", expect_action="reject",
         expect_reason_contains="prefix not allowed"),
    Case("W-B3", "未知前缀 55555555 → reject",
         caller="8899", callee="55555555", expect_action="reject",
         expect_reason_contains="prefix not allowed"),
    Case("W-B4", "非法来源IP 8.8.8.8 → allowed_ips拒答",
         caller="8899", callee="8798", source_addr="8.8.8.8:12345",
         expect_action="reject", expect_reason_contains="source not allowed"),

    # --- W-C: caller 不属 webrtc group → 直接拦截 reject (3) NEW ---
    Case("W-C1", "caller=6666∉任何webrtc cg → 拦截 reject",
         caller="6666", callee="7000",
         expect_action="reject", expect_reason_contains="caller not allowed"),
    Case("W-C2", "caller=6666∉webrtc cg + CCM + src=100.88 → 仍拦截 reject(不看方向)",
         caller="6666", callee="7000", ua="CCM", source_addr="100.88.0.1:15060",
         expect_action="reject", expect_reason_contains="caller not allowed"),
    Case("W-C3", "caller=8120∈cg-8120(route_points专属,非webrtc组) → 拦截 reject",
         caller="8120", callee="7000",
         expect_action="reject", expect_reason_contains="caller not allowed"),

    # --- W-D: allowed_ips / source_addr 边界 (3) NEW ---
    Case("W-D1", "source_addr=空字符串 → source_ip='' → allowed_ips拒答",
         caller="8899", callee="8798", source_addr="",
         expect_action="reject", expect_reason_contains="source not allowed"),
    Case("W-D2", "source_addr=完全缺失(不传) → 默认 '' → allowed_ips拒答",
         caller="8899", callee="8798", source_addr=None,
         expect_action="reject", expect_reason_contains="source not allowed"),
    Case("W-D3", "source_addr=IPv6 [::1]:15060 → source_ip不在allowed_ips → 拒答",
         caller="8899", callee="8798", source_addr="[::1]:15060",
         expect_action="reject", expect_reason_contains="source not allowed"),

    # --- W-E: 输入解析 caller 纯 URI / 匿名 (2) NEW ---
    Case("W-E1", "caller=sip:8899@evil.com(纯URI) → parse_user提取8899 → 正确webrtc路由 → tailscale",
         caller="sip:8899@evil.com", callee="8798",
         expect_npid="tailscale", expect_targets_contain="100.88.0.204"),
    Case("W-E2", "caller=sip:8899@evil.com → ya-pbx+yacc55-fs 混合出口",
         caller="sip:8899@evil.com", callee="13800138000",
         expect_npid="tailscale", expect_targets_contain="yacc55-fs"),

    # ╔═══════════════════════════════════════════════════════════════════════╗
    # ║  GROUP R: route_points 普通路由                                       ║
    # ╠═══════════════════════════════════════════════════════════════════════╣
    # ║  C. caller_group 精确命中                                             ║
    # ║  D. 兜底分支（identify_internal + default_for 交叉配对）              ║
    # ║  E. route_points 不校验 allowed_ips + 空前缀规则全覆盖                ║
    # ╚═══════════════════════════════════════════════════════════════════════╝

    # --- R-C: caller_group 精确命中 (3) ---
    Case("R-C1", "caller=8120 → cg-8120 → internal-8120 → lvcx",
         caller="8120", callee="7000", is_webrtc=False,
         expect_npid_absent=True, expect_targets_contain="lvcx"),
    Case("R-C2", "caller=8120001(startswith) → cg-8120 → lvcx",
         caller="8120001", callee="7000", is_webrtc=False,
         expect_targets_contain="lvcx"),
    Case("R-C3", "caller=8196 → cg-8196 → internal-8196-5000 → lvcx",
         caller="8196", callee="7000", is_webrtc=False,
         expect_targets_contain="lvcx"),

    # --- R-D: 兜底分支 identify_internal + default_for 交叉配对 (4) ---
    Case("R-D1", "CCM+src=100.88 → internal=True → default_for=internal → ya-pbx 🔍",
         caller="6666", callee="7000", is_webrtc=False,
         ua="CCM", source_addr="100.88.0.1:15060",
         expect_targets_contain="ya-pbx", expect_npid_absent=True),
    Case("R-D2", "kubePhone src=127.0.0.1(不在source_cidrs=100.88) → 防盗打→false → 外部兜底→lvcx",
         caller="6666", callee="7000", is_webrtc=False,
         ua="kubePhone", expect_targets_contain="lvcx"),
    Case("R-D3", "BadUA → identify_internal=False → 外部兜底→lvcx",
         caller="6666", callee="7000", is_webrtc=False,
         ua="BadUA/1.0", expect_targets_contain="lvcx", expect_npid_absent=True),
    Case("R-D4", "无UA → identify_internal=False → 外部兜底→lvcx",
         caller="6666", callee="7000", is_webrtc=False,
         expect_targets_contain="lvcx"),

    # --- R-E: route_points allowed_ips + 空前缀全覆盖 (4) NEW ---
    Case("R-E1", "caller=8120 + callee=任意(99999999) → cg-8120空前缀规则→lvcx",
         caller="8120", callee="99999999", is_webrtc=False,
         expect_targets_contain="lvcx"),
    Case("R-E2", "caller=8120 + callee=8798(webrtc前缀) → route_points空前缀→lvcx(不走被动网关!)",
         caller="8120", callee="8798", is_webrtc=False,
         expect_targets_contain="lvcx", expect_npid_absent=True),
    Case("R-E3", "caller=8120 + source=8.8.8.8 → route_points不校验allowed_ips → 正常forward→lvcx",
         caller="8120", callee="7000", is_webrtc=False,
         source_addr="8.8.8.8:12345", expect_targets_contain="lvcx"),
    Case("R-E4", "caller=6666 + directory + callee=8798 → 兜底lvcx(不会走webrtc被动网关!)",
         caller="6666", callee="8798", is_webrtc=False,
         expect_targets_contain="lvcx", expect_npid_absent=True),

    # ╔═══════════════════════════════════════════════════════════════════════╗
    # ║  GROUP E: directory route_type — 同一 caller 跨路由形态对比          ║
    # ╚═══════════════════════════════════════════════════════════════════════╝
    Case("E-1", "无wrtc X-cid → route_points默认 → 8798→lvcx",
         caller="8899", callee="8798", is_webrtc=False,
         expect_targets_contain="lvcx", expect_npid_absent=True),
    Case("E-2", "无wrtc X-cid → route_points默认 → 1551→lvcx",
         caller="8812", callee="1551", is_webrtc=False,
         expect_targets_contain="lvcx", expect_npid_absent=True),
    Case("E-3", "无wrtc X-cid → route_points → 1551→lvcx (普通路由无caller_group限制)",
         caller="8899", callee="1551", is_webrtc=False,
         expect_targets_contain="lvcx", expect_npid_absent=True),

    # ╔═══════════════════════════════════════════════════════════════════════╗
    # ║  GROUP M: method 过滤 — 非 INVITE → not_handled                      ║
    # ╚═══════════════════════════════════════════════════════════════════════╝
    Case("M-1", "method=OPTIONS → action=not_handled",
         caller="8899", callee="8798", method="OPTIONS",
         expect_action="not_handled", expect_npid_absent=True),
    Case("M-2", "method=BYE → action=not_handled",
         caller="8899", callee="8798", method="BYE",
         expect_action="not_handled"),
    Case("M-3", "method=REGISTER → action=not_handled",
         caller="8899", callee="8798", method="REGISTER",
         expect_action="not_handled"),

    # ╔═══════════════════════════════════════════════════════════════════════╗
    # ║  GROUP F: 边缘场景 — 畸形输入 / 字段缺失                             ║
    # ╚═══════════════════════════════════════════════════════════════════════╝
    Case("F-1", "callee=空字符串 → reject 484 address incomplete",
         caller="8899", callee="", expect_action="reject", expect_status=484,
         expect_reason_contains="address incomplete"),
    Case("F-2", "callee=字段完全缺失(sipprefix='' → 伪值) → reject 484",
         caller="8899", callee=None, expect_action="reject", expect_status=484,
         expect_reason_contains="address incomplete"),
    Case("F-3", "to=sip:@127.0.0.1 畸形RURI → reject 484",
         caller="8899", callee=None, expect_action="reject", expect_status=484),
    Case("F-4", "caller=空 + directory → 无 group 命中 → 兜底 lvcx",
         caller="", callee="7000", is_webrtc=False,
         expect_targets_contain="lvcx"),
    Case("F-5", "caller=完全缺失 + from='' → parse_user='' → 兜底 lvcx",
         caller=None, callee="7000", is_webrtc=False,
         expect_targets_contain="lvcx"),
    Case("F-6", "source_addr=非法格式 invalid-format → source_ip='' → 拒答",
         caller="8899", callee="8798", source_addr="invalid-format",
         expect_action="reject", expect_reason_contains="source not allowed"),
]

print()
print(f"{BOLD}{'='*112}{RESET}")
print(f"{BOLD}  kubeTrunk route_decision_service HTTP 接口端到端验证  |  {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}{RESET}")
print(f"{BOLD}  URL: {URL}{RESET}")
print(f"{BOLD}  Phase 1: {BASE_CFG} (基准配置)  |  Phase 2: {TEST_CFG} (扩充场景配置){RESET}")
print(f"{BOLD}{'='*112}{RESET}")

# ===== Phase 1: 3201.json 基准 =====
if not _reload(BASE_CFG):
    print(f"{RED}FATAL: 无法加载 {BASE_CFG}, 中止{RESET}"); sys.exit(1)
print(f"\n{BOLD}📂 Phase 1: 3201.json 基准配置 ({len(cases)} 用例){RESET}")

for c in cases: c.run()

group_map = {
    "W": ("webrtc",
          {"A": "A. 正常命中", "B": "B. reject 越权/非法来源",
           "C": "C. caller不属webrtc group → 降级route_points兜底 🆕",
           "D": "D. allowed_ips/source_addr边界 🆕",
           "E": "E. 输入解析 🆕"}),
    "R": ("route_points 普通路由",
          {"C": "C. caller_group精确命中", "D": "D. 兜底分支 identify_internal + default_for交叉配对",
           "E": "E. route_points不校验allowed_ips + 空前缀全覆盖 🆕"}),
    "E": ("directory route_type", {}),
    "M": ("method 过滤 🆕", {}),
    "F": ("边缘场景", {}),
}
cur_group = ""; cur_sub = ""
for c in cases:
    g, s = c.tag.split("-", 1)
    sub = s.split("-", 1)[0]
    if g != cur_group:
        cur_group = g
        title, subs = group_map.get(g, (g, {}))
        print(f"\n{BOLD}{'─'*3} {title} {'─'*(90-len(title)-6)}{RESET}")
        cur_sub = ""
    if subs and sub != cur_sub:
        cur_sub = sub
        print(f"{DIM}  {subs.get(sub, sub)}{RESET}")
    tag = f"{GREEN}✅{RESET}" if c.passed else f"{RED}❌{RESET}"
    b = c.body or {}
    act = b.get("action"); npid = b.get("network_profile_id")
    tgts = b.get("targets"); rtp = b.get("rtp_config") or {}
    reason = b.get("reason", "")
    st = b.get("status")
    extra = ""
    if c.ua: extra += f" UA={c.ua!r}"
    if c.method: extra += f" method={c.method!r}"
    print(f"{tag} {c.tag:6s} {CYAN}{c.name}{RESET}")
    print(f"       caller={c.caller!r} callee={c.callee!r} "
          f"→ HTTP{c.st} status={st} action={act}{extra}")
    print(f"       npid={npid!r} rtp.ext={rtp.get('external_ip')!r} targets={tgts}")
    if reason: print(f"       reason={YELLOW}{reason}{RESET}")
    if not c.passed:
        for e in c.errs: print(f"       {RED}⚠ {e}{RESET}")

passed_p1 = sum(1 for c in cases if c.passed)
total_p1 = len(cases)

# ===== Phase 2: test_3202.json 扩充场景 =====
phase2_cases = [
    # ╔═══════════════════════════════════════════════════════════════════════╗
    # ║  GROUP T: 测试配置专用场景（config/test_3202.json）                 ║
    # ╠═══════════════════════════════════════════════════════════════════════╣
    # ║  S. strip / pri_prefix 号码变换                                      ║
    # ║  G. disabled gateway / mixed gw_list 过滤                            ║
    # ║  E. webrtc / route_points entry enabled=false → 跳过                 ║
    # ║  C. rule.caller 直接匹配（非 caller_group）                          ║
    # ║  P. priority + 最长前缀对比                                          ║
    # ║  Z. 全部网关 disabled → targets=[] → reject                          ║
    # ╚═══════════════════════════════════════════════════════════════════════╝

    # --- T-S: strip / pri_prefix ---
    Case("T-S1", "webrtc cg-b 10123456 prefix=10 strip=2 pri=00 → callee2=00123456",
         caller="8812", callee="10123456",
         expect_callee2_exact="00123456", expect_targets_contain="gw-nostrip"),
    Case("T-S2", "webrtc cg-b 20123456 prefix=20 strip=0 pri=001 → callee2=00120123456",
         caller="8812", callee="20123456",
         expect_callee2_exact="00120123456"),
    Case("T-S3", "route_points caller=7777 callee=712345 prefix=7 strip=1 pri=0086 → callee2=008612345",
         caller="7777", callee="712345", is_webrtc=False,
         expect_callee2_exact="008612345"),
    Case("T-S4", "route_points caller=7777 callee=812345 prefix=8 strip=3 pri=0 → callee2=0345",
         caller="7777", callee="812345", is_webrtc=False,
         expect_callee2_exact="0345"),

    # --- T-G: disabled gateway + mixed gw_list ---
    Case("T-G1", "webrtc gw_list=[gw-disabled(enabled=false)+lvcx] → disabled被过滤 → 只命中lvcx",
         caller="8812", callee="301234", expect_targets_contain="lvcx",
         expect_targets_contain_not_exist="gw-disabled"),

    # --- T-Z: all gw disabled → targets=[] → reject ---
    Case("T-Z1", "webrtc gw_list=[gw-disabled only] → targets=[] → reject 'No GW Route'",
         caller="8812", callee="501234", expect_action="reject",
         expect_reason_contains="No GW Route"),

    # --- T-E: entry enabled=false → 被跳过 ---
    Case("T-E1", "webrtc webrtc-entry-disabled.enabled=false → 该条目被跳过 → 99xxx不在webrtc_table → 降级reject",
         caller="8812", callee="991234", expect_action="reject",
         expect_reason_contains="prefix not allowed"),

    # --- T-C: route_points rule.caller 直接匹配 ---
    Case("T-C1", "route_points caller=7777 callee=661234 → rp-callermatch prefix=66 caller=7777 直接命中",
         caller="7777", callee="661234", is_webrtc=False,
         expect_targets_contain="gw-prefix"),
    Case("T-C2", "route_points caller=7777001 callee=671234 → prefix=67 caller=7777001 精确匹配",
         caller="7777001", callee="671234", is_webrtc=False,
         expect_targets_contain="gw-prefix"),
    Case("T-C3", "route_points caller=6666 callee=681234 → 不命中caller=7777的66规则 → 命中prefix=68 caller=null（无caller过滤）",
         caller="6666", callee="681234", is_webrtc=False,
         expect_targets_contain="gw-prefix"),

    # --- T-R: route_points enabled=false → 跳过 → 兜底 ---
    Case("T-R1", "route_points rp-disabled.enabled=false → 555123不命中 → default兜底lvcx",
         caller="6666", callee="555123", is_webrtc=False,
         expect_targets_contain="lvcx"),

    # --- T-P: priority 最长前缀优先 (priority 是 tiebreaker 非决定因子!) ---
    Case("T-P1", "route_points callee=123456 → prefix='12' 2字符 > prefix='1' 1字符 → 最长优先（不看priority）",
         caller="6666", callee="123456", is_webrtc=False,
         expect_targets_contain="lvcx"),

    # --- T-B: 基础 webrtc 场景在 test_3202 里也能命中 ---
    Case("T-B1", "test_3202 里 webrtc-reg 也有8798前缀 → 被动网关 reg879830001 → 纯URI",
         caller="8899", callee="8798",
         expect_npid="tailscale", expect_targets_contain="100.88.0.204",
         expect_rtp_external_ip="100.88.0.1"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  W-CGL  webrtc: caller_group 记录长度竞争                                ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("W-CGL1", "webrtc 同前缀70 caller=8812 → cg-len-b['881']=3 > cg-len-a['88']=2 → 长记录优先 → gw-nostrip",
         caller="8812", callee="701234", expect_targets_contain="gw-nostrip"),
    Case("W-CGL2", "webrtc 同前缀70 caller=88 → cg-len-b['881']=3 不命中(88不以881开头!) → 只剩 cg-len-a → gw-nostrip",
         caller="88", callee="701234", expect_targets_contain="gw-nostrip"),
    Case("W-CGL3", "webrtc 同前缀70 caller=881 → cg-len-a['88']=2命中 AND cg-len-b['881']=3命中 → 长记录赢 → gw-nostrip",
         caller="881", callee="701234", expect_targets_contain="gw-nostrip"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  W-CVCG webrtc: rule.caller 精确匹配 vs rule.caller_group               ║
    # ║           (同长度同前缀同priority → 配置顺序先入表者赢)                   ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("W-CVCG1", "webrtc 同前缀71 caller=8899 → caller='8899'(4) vs cg['8899'](4) → 同rank → rule.caller先入表 → 赢 → gw-nostrip",
         caller="8899", callee="711234", expect_targets_contain="gw-nostrip"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-L1   route_points: L1主叫+被叫 跨层优先于 L2被叫匹配                  ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-L1-1", "rp-layer-test caller=7777 callee=100xxx → 命中cg-strip-demo → L1 layer优先 → gw-prefix",
         caller="7777", callee="100123", is_webrtc=False, expect_targets_contain="gw-prefix"),
    Case("R-L1-2", "rp-layer-test caller=6666 callee=100xxx → 不命中cg-strip-demo → 只剩L2无主叫过滤 → 也命中gw-prefix",
         caller="6666", callee="100999", is_webrtc=False, expect_targets_contain="gw-prefix"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-CVCG route_points: rule.caller 精确 vs rule.caller_group (同长度)    ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-CVCG1", "rp-callervscg-compete caller=7777 → caller='7777'(4) vs cg-strip-demo['7777'](4) → 同L1同prefix同len → 配置顺序caller先赢",
         caller="7777", callee="200123", is_webrtc=False, expect_targets_contain="gw-prefix"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-PRIO route_points: 同层同prefix priority 大的赢                       ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-PRIO1", "rp-priority-same-layer callee=300xxx → 同L2同prefix prio10>prio1 → prio10赢 → pri_prefix=P10 → callee2含P10",
         caller="6666", callee="300999", is_webrtc=False, expect_callee2_contains="P10"),
    Case("R-PRIO2", "rp-priority-same-layer callee=300xxx caller=7777 → 同上 → prio10赢 → callee2含P10",
         caller="7777", callee="300123", is_webrtc=False, expect_callee2_contains="P10"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-L3   route_points: L3 空前缀 + caller_group 主叫命中                  ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-L3-1", "rp-l3-empty caller=5566 callee=9999 → cg-multi-a['55','5566'] → 5566最长=4 → L3 → lvcx",
         caller="5566", callee="9999", is_webrtc=False, expect_targets_contain="lvcx"),
    Case("R-L3-2", "rp-l3-empty caller=55 callee=任意 → cg-multi-a['55']=2命中 → L3 → lvcx",
         caller="55", callee="8888", is_webrtc=False, expect_targets_contain="lvcx"),
    Case("R-L3-3", "caller=9999 callee=9999 → 不命中任何cg → 1~3层全落选 → 第4层external-default → lvcx",
         caller="9999", callee="9999", is_webrtc=False,
         expect_targets_contain="lvcx", expect_npid_absent=True),
    Case("R-L3-4", "caller=9999 UA=CCM src=100.88 callee=9999 → identify_internal=True → side=internal → external-default → ya-pbx",
         caller="9999", callee="9999", is_webrtc=False,
         expect_targets_contain="ya-pbx", expect_npid_absent=True,
         ua="CCM", source_addr="100.88.0.5:15060"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-LEN  route_points: caller_len vs cg_len 竞争（更精确主叫优先）        ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-LEN-1", "rp-cg-vs-callerlen caller=6666 callee=400xxx → cg-compete-a['6666']=4命中, caller='666600'不匹配 → cg赢",
         caller="6666", callee="400123", is_webrtc=False, expect_targets_contain="gw-prefix"),
    Case("R-LEN-2", "rp-cg-vs-callerlen caller=6666001 callee=400xxx → cg['6666']=4 AND caller='666600'=6 → caller_len 6赢!",
         caller="6666001", callee="400123", is_webrtc=False, expect_targets_contain="gw-prefix"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-CGM  route_points: caller_group 多记录最长优先                        ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-CGM1", "cg-multi-a caller=556699 callee=9999 → ['55'(2), '5566'(4)] → 5566最长=4优先 → L3 → lvcx",
         caller="556699", callee="9999", is_webrtc=False, expect_targets_contain="lvcx"),
    Case("R-CGM2", "cg-multi-a caller=551234 callee=9999 → ['55'(2)] → 只有55命中(551234.startswith('55')=True, 不以5566开头!) → len=2 → L3 → lvcx",
         caller="551234", callee="9999", is_webrtc=False, expect_targets_contain="lvcx"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  W-OVR  webrtc: 同caller多cg重叠命中 → 最长记录优先                      ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("W-OVR1", "caller=8812(同时命中cg-len-a['88']和cg-len-b['881']) callee=701234 → 长记录=3赢",
         caller="8812", callee="701234", expect_targets_contain="gw-nostrip"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-SUB  route_points: caller 子串匹配（caller=前缀+更多号码）              ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-SUB1", "rp-callermatch-test caller=7777999 callee=661234 → caller='7777'是7777999的前缀吗? 不! 是7777999.startswith('7777')=True → 命中 → gw-prefix",
         caller="7777999", callee="661234", is_webrtc=False, expect_targets_contain="gw-prefix"),
    Case("R-SUB2", "rp-callermatch-test caller=7777001PLUS callee=671234 → caller='7777001'前缀匹配 → 命中 → gw-prefix",
         caller="777700199", callee="671234", is_webrtc=False, expect_targets_contain="gw-prefix"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-NOMATCH route_points: caller不匹配rule.caller但匹配同层前缀规则       ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-NOM1", "rp-callermatch-test caller=8120 callee=681234 → 8120非webrtc组走route_points → rule.caller=null无主叫过滤 → L2前缀68 → gw-prefix",
         caller="8120", callee="681234", is_webrtc=False, expect_targets_contain="gw-prefix"),
    Case("R-NOM2", "rp-callermatch-test caller=7777 callee=681234 → 跳过caller='7777'的66规则, 跳过caller='7777001'的67规则 → 命中caller=null的68规则 → gw-prefix",
         caller="7777", callee="681234", is_webrtc=False, expect_targets_contain="gw-prefix"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-NOFALL route_points: L1落选但同rp里L2前缀也不匹配 → 找其他rp          ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-NOF1", "rp-strip-test callee=555 callee≠'7'且≠'8'且≠'9' → rp-strip-test落选, 跳过rp-disabled → 其他rp也不命中 → 默认兜底",
         caller="7777", callee="555", is_webrtc=False, expect_targets_contain="lvcx"),
    Case("R-NOF2", "rp-callermatch-test callee=5678(不命中66/67/68/7/8/9/1/12) → 所有rp都不命中 → 默认兜底 lvcx",
         caller="7777", callee="5678", is_webrtc=False, expect_targets_contain="lvcx"),

    # ╔═════════════════════════════════════════════════════════════════════════╗
    # ║  R-SAME  route_points: 同层同prefix同priority → 配置顺序(rule index) 决胜 ║
    # ╚═════════════════════════════════════════════════════════════════════════╝
    Case("R-SAME1", "rp-callervscg-compete 同L1同prefix同prio同len → 配置先caller后cg → caller='7777'先赢",
         caller="7777", callee="200999", is_webrtc=False, expect_targets_contain="gw-prefix"),
]

# 在 Case 类加上 expect_targets_contain_not_exist 支持（gw-disabled 不应出现）
for c in phase2_cases:
    if "expect_targets_contain_not_exist" in c.kwargs:
        setattr(c, "expect_targets_contain_not_exist", c.kwargs.pop("expect_targets_contain_not_exist"))

if not _reload(TEST_CFG):
    print(f"{RED}FATAL: 无法加载 {TEST_CFG}, 中止{RESET}"); sys.exit(1)
print(f"\n{BOLD}📂 Phase 2: test_3202.json 扩充配置 ({len(phase2_cases)} 用例){RESET}")

for c in phase2_cases: c.run()

# 手动添加 contain_not_exist 断言
for c in phase2_cases:
    want = getattr(c, "expect_targets_contain_not_exist", None)
    if want and c.passed:
        flat = json.dumps(c.body.get("targets") or [])
        if want in flat:
            c.passed = False
            c.errs.append(f"targets不应包含{want!r} 但存在: {c.body.get('targets')}")

# ===== Phase 2 输出 =====
phase2_passed = sum(1 for c in phase2_cases if c.passed)
phase2_total = len(phase2_cases)

group_map_p2 = {
    "T-S": ("T-S strip/pri_prefix 号码变换", {}),
    "T-G": ("T-G disabled gateway / mixed gw_list", {}),
    "T-Z": ("T-Z 全部网关 disabled → targets=[]", {}),
    "T-E": ("T-E entry enabled=false → 跳过", {}),
    "T-C": ("T-C rule.caller 直接匹配", {}),
    "T-R": ("T-R route_points enabled=false → 兜底", {}),
    "T-P": ("T-P priority + 最长前缀优先", {}),
    "T-B": ("T-B 基础 webrtc 场景回归", {}),
    "W-CGL": ("W-CGL webrtc caller_group 记录长度竞争", {}),
    "W-CVCG": ("W-CVCG webrtc caller精确 vs caller_group", {}),
    "W-OVR": ("W-OVR webrtc 多cg重叠命中", {}),
    "R-L1": ("R-L1 route_points L1主叫+被叫 跨层优先", {}),
    "R-CVCG": ("R-CVCG rp caller精确 vs caller_group同长度", {}),
    "R-PRIO": ("R-PRIO rp 同层priority竞争", {}),
    "R-L3": ("R-L3 rp L3空前缀+caller_group + 第4层默认兜底", {}),
    "R-LEN": ("R-LEN rp caller_len vs cg_len 精确主叫优先", {}),
    "R-CGM": ("R-CGM rp caller_group多记录最长优先", {}),
    "R-SUB": ("R-SUB rp caller子串前缀匹配", {}),
    "R-NOM": ("R-NOM rp rule.caller不命中但同层无过滤命中", {}),
    "R-NOF": ("R-NOF rp全部不命中 → 默认兜底", {}),
    "R-SAME": ("R-SAME rp 同层同prio同len → 配置顺序决胜", {}),
}

for c in phase2_cases:
    key = c.tag.rstrip("0123456789")
    if key not in group_map_p2:
        for full_key in group_map_p2:
            if c.tag.startswith(full_key + "-"):
                key = full_key
                break
        else:
            continue
    title, subs = group_map_p2[key]
    cur_sub = ""
    prev_tag = phase2_cases[phase2_cases.index(c)-1].tag if phase2_cases.index(c) > 0 else ""
    if not prev_tag.startswith(key):
        print(f"\n{BOLD}{'─'*3} {title} {'─'*(90-len(title)-6)}{RESET}")
    tag_str = f"{GREEN}✅{RESET}" if c.passed else f"{RED}❌{RESET}"
    b = c.body or {}
    print(f"{tag_str} {c.tag:6s} {CYAN}{c.name}{RESET}")
    print(f"       caller={c.caller!r} callee={c.callee!r} source={'wrtc' if c.is_webrtc else 'dir'} "
          f"→ HTTP{c.st} action={b.get('action')}")
    print(f"       npid={b.get('network_profile_id')!r} rtp.ext={(b.get('rtp_config') or {}).get('external_ip')!r} targets={b.get('targets')}")
    r2 = b.get("reason")
    if r2: print(f"       reason={YELLOW}{r2}{RESET}")
    if not c.passed:
        for e in c.errs: print(f"       {RED}⚠ {e}{RESET}")

# ===== 总统计 =====
all_cases = cases + phase2_cases
all_passed = passed_p1 + phase2_passed
all_total = total_p1 + phase2_total
all_pct = all_passed * 100 // all_total if all_total else 0
bar_len = 40
bar_passed = bar_len * all_passed // all_total if all_total else 0
bar = "█" * bar_passed + "░" * (bar_len - bar_passed)
color = GREEN if all_passed == all_total else (YELLOW if all_passed >= all_total*0.8 else RED)

print()
print(f"{BOLD}{'═'*112}{RESET}")
print(f"  {BOLD}Phase 1 (3201.json) {RESET} {passed_p1}/{total_p1}  "
      f"|  {BOLD}Phase 2 (test_3202.json) {RESET} {phase2_passed}/{phase2_total}")
print(f"{BOLD}{'═'*112}{RESET}")
print(f"  {color}{bar}{RESET}  {BOLD}{all_passed}/{all_total}{RESET} 通过  ({all_pct}%)")
print(f"{BOLD}{'═'*112}{RESET}")

# 清理：reload 回基准配置
_reload(BASE_CFG)
sys.exit(0 if all_passed == all_total else 1)