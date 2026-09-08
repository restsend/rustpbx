# 中继与队列

> 译文说明：按英文原文完整翻译，所有代码/配置示例（含注释）原样保留。字段枚举和实现状态以原文为准；本次未将翻译与代码修订混在一起。

## 中继（`[proxy.trunks]`）

连接外部 SIP 提供商的网关。在 `[proxy.trunks]` 映射或独立文件中配置。

```toml
[proxy.trunks.provider_a]
dest = "sip:sip.provider.com:5060"
# Optional failover
backup_dest = "sip:backup.provider.com"

# Authentication
username = "myuser"
password = "mypassword"

# Capacity
max_calls = 50
max_cps = 5          # Calls per second
weight = 10          # Relative weight for load balancing

# Traffic Control
direction = "outbound"       # inbound, outbound, bidirectional
inbound_hosts = ["203.0.113.50"] # Whitelist IPs
```

### 中继字段

| 字段 | 类型 | 默认值 | 说明 |
|-------|------|---------|-------------|
| `dest` | string | 必填 | 网关的 SIP URI |
| `backup_dest` | string | 无 | 故障切换 SIP URI |
| `username` / `password` | string | 无 | SIP 认证凭据 |
| `codec` | [string] | `[]` | 允许的编解码器（别名：`allow_codecs`、`audio_codecs`） |
| `transport` | string | 无 | 覆盖传输协议（例如 `"tcp"`） |
| `max_calls` | int | 无 | 最大并发通话数 |
| `max_cps` | int | 无 | 每秒最大呼叫数 |
| `weight` | int | 无 | 负载均衡权重 |
| `direction` | string | `"bidirectional"` | `inbound`、`outbound`、`bidirectional` |
| `inbound_hosts` | [string] | `[]` | 入站呼叫来源 IP 白名单 |
| `disabled` | bool | `false` | 禁用中继而不删除它 |
| `country` | string | 无 | 号码规范化所用国家代码 |
| `did_numbers` | [string] | `[]` | 此中继拥有的 DID 号码（用于入站路由） |
| `call_id_mode` | string | 无 | Call-ID 改写：`"prefix"`、`"suffix"`、`"none"` |
| `rewrite_hostport` | bool | `true` | 改写出站 Contact 头中的 host:port |
| `recording` | table | 无 | 中继级录音策略覆盖 |
| `ringback` | table | 无 | 中继级回铃音覆盖 |
| `max_ring_time` | int | 无 | 中继级最大振铃/建立时长（秒）；超时未接听以 408 拒绝。`0` 禁用此中继的振铃超时。对通过该中继路由的呼叫覆盖全局 `[proxy] max_ring_time` |
| `external_ip` | string | 无 | 覆盖此中继通话腿在 SDP `c=`/`o=` 行和 ICE 候选中公布的 IP，替代配置档/全局 RTP 外部 IP。当中继在 Tailscale/WireGuard 等覆盖网络终结，需要与公网 NAT 不同的公布地址时尤其重要 |
| `bind_ip` | string | 无 | 覆盖此中继通话腿 RTP socket 绑定的本地 IP，替代配置档/全局 RTP 绑定 IP |
| `profile` | string | 无 | 主配置 `[[network_profile]]` 的配置档 ID（Console：中继 **Media Option → Network profile**）。成组应用 RTP/SDP 和 SIP Contact 设置；设置了中继级 `external_ip` / `bind_ip` 时仍由其覆盖 |
| `header_passthrough` | table | 无 | 控制原 INVITE 的哪些自定义头转发到该中继的出站 INVITE。`mode` 为 `"all"`（默认）、`"whitelist"` 或 `"blacklist"`；`whitelist`/`blacklist` 为头名称列表（不区分大小写）。标准 SIP 头（`Via`/`From`/`To`/`Call-ID`/`CSeq`/`Contact`/…）始终不转发。未设置（默认）时不向外部中继转发任何自定义头；内部目标（同 Realm/已注册/home-proxy）始终全部转发，除非路由的 `with_original_headers` 覆盖 |

### 自定义头透传

控制是否将**原始入站 INVITE** 的自定义头（例如 `X-CRM-Ticket-Id`）复制到被叫腿的出站 INVITE。每个被叫目标按以下顺序解析：

1. **内部目标**（同 Realm、已注册 AOR 或 home-proxy）→ 转发所有自定义头。
2. **外部中继目标** → 使用中继的 `header_passthrough` 配置；未设置则不转发。
3. **HTTP 动态路由器** → 响应中的 `with_original_headers` 覆盖上述设置（`true` = 全部转发，`false` = 全不转发）。见 [04-routing.md](04-routing_zh.md)。

这适用于每条被叫腿：直接拨号、并行分叉、队列坐席腿、转接以及应用发起的腿（在会话内解析的目标回退到上述按目标解析方式）。

中继配置示例：

```toml
[proxy.trunks.provider_a]
header_passthrough = { mode = "all" }                # forward all custom headers
# header_passthrough = { mode = "whitelist", whitelist = ["X-Smart2Agent", "X-SmartParams"] }
# header_passthrough = { mode = "blacklist", blacklist = ["X-Token"] }
```

标准 SIP 头始终不转发；此规则只作用于自定义（非标准）头。

### 中继注册

适用于需要向外注册的中继：

```toml
[proxy.trunks.sip_provider]
dest = "sip:sip.provider.com:5060"
username = "myuser"
password = "mypassword"

# SIP registration (register at this trunk)
register_enabled = true
register_expires = 3600
# register_extra_headers = { "X-Client-ID" = "my-pbx" }
```

### 中继健康检查

可选的中继可用性健康监控：

```toml
[proxy.trunks.provider_a]
dest = "sip:sip.provider.com:5060"

health_check_enabled = true
health_check_interval_secs = 30   # Probe every 30s
health_check_probe_count = 3      # Fail after 3 failed probes
health_check_fallback_trunk = "backup-provider"  # Auto-failover
```

### 高级中继设置

```toml
[proxy.trunks.provider_a]
dest = "sip:sip.provider.com:5060"

# Call Admission Control
cac_policy = "loss_based"         # "loss_based" or "reject"
overflow_threshold = 90           # Trigger CAC at 90% capacity

# Media handling
media_mode = "auto"               # "auto", "none", "bypass", "force_transcode"
                                  # - auto: bridge only for app/queue flows
                                  # - none: no media proxy (SDP passthrough, RTP direct)
                                  # - bypass: SDP rewrite only, RTP direct
                                  # - force_transcode: always bridge through PBX
video_policy = "pass_through"      # "passthrough", "strip", "transcode"

# Per-trunk network profile (multi-path egress; see 01-platform.md)
profile = "overlay"               # [[network_profile]] id

# Per-trunk IP override (for overlay networks like Tailscale/WireGuard)
# These override the selected profile when set.
external_ip = "100.64.10.1"
bind_ip = "100.64.10.2"

# See [06-media-recording.md](06-media-recording.md) for the full media proxy
# reference, including latching, trunk-level vs server-level configuration,
# and recommended combinations for NAT / overlay scenarios.

# SIP header manipulation
header_rules = [
    { action = "add", name = "X-Client-ID", value = "rustpbx" },
    { action = "remove", name = "X-Internal-Info" },
]

# Forward original custom headers to this trunk's outgoing INVITE.
# Unset (default) -> forward nothing; internal destinations forward everything.
header_passthrough = { mode = "all" }            # all custom headers
# header_passthrough = { mode = "whitelist", whitelist = ["X-Smart2Agent", "X-SmartParams"] }
# header_passthrough = { mode = "blacklist", blacklist = ["X-Token"] }

# Number normalization
incoming_from_user_prefix = ""    # Strip prefix from inbound caller
incoming_to_user_prefix = ""      # Strip prefix from inbound callee
```

## 队列（`[proxy.queues]`）

呼叫分配逻辑（ACD）。

分配给队列坐席的呼叫遵循相同的[自定义头透传](#自定义头透传)规则：原始 INVITE 自定义头转发到内部坐席腿；外部坐席中继按其 `header_passthrough` 配置处理。

```toml
[proxy.queues.support_main]
name = "General Support"
accept_immediately = true
passthrough_ringback = false
# acd_policy = "default"       # Reference to ACD policy (CC addon)

# Hold Music
[proxy.queues.support_main.hold]
audio_file = "sounds/hold_music.wav"
loop_playback = true

# Distribution Strategy
[proxy.queues.support_main.strategy]
mode = "sequential" # or "parallel" (ring-all)
wait_timeout_secs = 20

[[proxy.queues.support_main.strategy.targets]]
uri = "sip:1001@local"
label = "Alice"

[[proxy.queues.support_main.strategy.targets]]
uri = "sip:1002@local"
label = "Bob"

# Fallback (if no agents answer)
[proxy.queues.support_main.fallback]
redirect = "sip:voicemail@local" # or a queue URI, e.g. "queue:overflow?overflow_group=..." (embedded query params are honored)
# failure_code = 486
# failure_reason = "No agents available"

# Voice prompts (played to caller while waiting)
# [proxy.queues.support_main.voice_prompts]
# estimated_wait = "sounds/estimated_wait.wav"
# position = "sounds/position.wav"
# periodic = "sounds/thank_you.wav"
# periodic_interval_secs = 60
```

### 队列字段

| 字段 | 类型 | 默认值 | 说明 |
|-------|------|---------|-------------|
| `name` | string | 无 | 显示名称 |
| `acd_policy` | string | 无 | ACD 策略名称（CC 插件） |
| `accept_immediately` | bool | `false` | 在坐席接听前接受通话（200 OK） |
| `passthrough_ringback` | bool | `false` | 将被叫回铃音转发给主叫 |
| `hold` | table | 无 | 保持音乐配置 |
| `strategy` | table | 必填 | 分配策略（模式、目标、超时） |
| `fallback` | table | 无 | 无坐席接听时的回退 |
| `voice_prompts` | table | 无 | 等待期间的语音公告 |

### 队列转接查询参数

通过 `queue:<name>` 将通话转入队列时，可附加查询参数，在运行时覆盖队列配置。

**`?return_app=<app>&return_target=<target>`**——覆盖回退动作：没有可用坐席时转到应用（例如 `ivr`、`voicemail`、`queue`、`conference`），替代已配置的回退。注意：旧写法 `return_ivr=` **不会**被解析：

```
queue:support?return_app=ivr&return_target=main_menu
```

**`?target=<value>`**——用指定值覆盖队列配置的坐席目标。支持 `skillgroup:<id>`（通过 AgentRegistry 解析）或 SIP URI。支持多个 `&target=` 参数，按顺序拨号：

```
queue:support?target=skillgroup:sales                            # Single skill group
queue:support?target=sip:agent@pbx.com                           # Single SIP agent
queue:support?target=skillgroup:sales&target=skillgroup:support  # Multiple targets (sequential)
```

**`?overflow_group=<id>`**——【溢出覆盖】为本通通话指定溢出目标技能组。可重复指定，也支持逗号分隔值。存在时会**替换**本通通话的整个升级时间线，不会与技能组配置的 `overflow_groups` 合并：

```
queue:support?overflow_group=support_l2                          # Single overflow group
queue:support?overflow_group=support_l2,support_l3               # Comma-separated list
queue:support?overflow_group=support_l2&overflow_group=support_l3
```

**`?overflow_after=<secs>`**——溢出触发阈值，单位为秒（覆盖升级步骤的阈值）。未指定 `overflow_group` 时，改写从注册表合成的计划阈值。

**`?overflow_wait=<secs>`**——队列最大等待时间（`max_wait_secs`），单位为秒；到期后主叫被路由至回退/返回应用。它与 `overflow_after` 不同：前者限制总排队时间，后者安排溢出升级时间。

**`?overflow_mode=<replace|cumulative>`**——溢出模式：`replace`（重新拨入新组）或 `cumulative`（将新组坐席加入现有坐席，公平轮询）。省略时默认为 `cumulative`。

**优先级：** URI 参数 > ACD 策略（`overflow.escalation_timeline`）> 技能组 `overflow_groups` + `max_wait_secs`。只覆盖 URI 中实际出现的字段，其余回退到注册表合成的计划。仅当队列拨号目标是技能组（`?target=skillgroup:...` 或队列自身技能组策略）时，溢出参数才生效。

**组合用法：**

```
queue:support?target=skillgroup:vip&overflow_group=support_l2&overflow_group=support_l3&overflow_after=30&overflow_mode=cumulative&overflow_wait=120&return_app=ivr&return_target=main_menu
```

### 队列溢出升级（技能组队列）

队列拨号目标为技能组（`skill-group:{id}`，通过 `strategy.targets` 或 `?target=skillgroup:...`）时，CC 插件会为其生成升级计划：起初仅在**主技能组**内调度；达到配置的排队等待阈值后，将候选集合**扩大**到溢出组，并在并集上公平排序（轮询，计数器每通通话推进一次）。新坐席与主组坐席一起振铃（累积模式）。

两个配置来源，按优先关系说明如下：

**1. 技能组 `overflow_groups` + `max_wait_secs`**（最简单，不需要 ACD 策略）。每个溢出组在该组 `max_wait_secs` 阈值处成为一个升级步骤，采用累积加公平模式：

```toml
# skill_groups TOML (or the /api/cc/skill-groups API — same fields)
[[skill_groups]]
skill_group_id = "support"
skills_required = ["support"]
overflow_groups = ["support_l2", "support_l3"]  # widen targets
max_wait_secs = 30                              # widen threshold (per queue wait)
```

**2. ACD 策略 `overflow.escalation_timeline`**（显式配置；当技能组 `acd_policy` 引用定义了时间线的策略时，优先使用）：

```toml
[policies.p1.overflow]
mode = "cumulative"          # cumulative = ring alongside; replace = redial

[[policies.p1.overflow.escalation_timeline]]
threshold_secs = 20
skill_group_id = "support_l2"
fair = true                  # widen with round-robin ordering (default false)
```

| 字段 | 类型 | 默认值 | 说明 |
|-------|------|---------|-------------|
| `threshold_secs` | int | 必填 | 触发此步骤前的排队等待秒数 |
| `skill_group_id` | string | 必填 | 扩展到的技能组 |
| `fair` | bool | `false` | 在扩展后的并集上轮询排序 |
| `mode` | string | `"replace"` | `cumulative`（一起振铃）或 `replace`（重新拨号） |
