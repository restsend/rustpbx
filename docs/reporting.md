# RustPBX 数据报表口径与架构

> 统一报表层（`crate::report` + `cc::report_service`）的指标定义、数据源与 API 参考。
> 所有报表端点共享本文件定义的口径，禁止在 handler 内私自重复定义。

## 1. 数据源

| 表 | 角色 | 说明 |
|---|---|---|
| `rustpbx_call_records` | PBX core CDR 事实表 | 主 CDR = `session_id IS NULL OR session_id = call_id`；`hangup_reason`/`sip_status_code` 为提升后的报表列 |
| `rustpbx_cdr_daily` | core 每日预聚合 | (day, direction, department, trunk) 维度，每 5 分钟重算昨日+今日，幂等 |
| `cc_calls` | CC per-call 事实表（唯一） | 含 direction/hangup_by/answered_at/ended_at/disposition + `(created_at)`、`(queue_id, created_at)`、`(agent_id, created_at)` 索引 |
| `cc_skill_group_hourly_stats` | CC 小时预聚合 | 每小时从 `cc_calls` 校准（自愈），事件增量写入 |
| `cc_queue_snapshots` | 30s 实时快照 | 累计列 + `delta_*` 区间增量列；区间图表必须用 delta |
| `cc_agent_status_history` | 坐席状态流水 | 迁移即一行（离开 from_status 时写入）；`SUM(duration_secs) GROUP BY from_status` = 分状态时长 |
| `cc_alert_history` | 告警历史（含 ack） | `dispatch_alert` 落库 |
| `cc_csat_surveys` | per-survey 满意度 | call_id 唯一 upsert |

## 2. 指标口径

| 指标 | 定义 | 数据源 |
|---|---|---|
| ASR（接通率） | answered / offered × 100 | cc_calls |
| 放弃率 | abandoned / offered × 100 | cc_calls |
| SLA | answered AND wait_time_secs ≤ target / answered × 100 | cc_calls + `cc_skill_groups.sla_target_secs` |
| AWT（平均等待） | SUM(wait_time_secs WHERE answered) / answered | cc_calls |
| AHT（平均处理时长） | SUM(talk_time_secs WHERE answered) / answered | cc_calls |
| 占用率（Occupancy） | (busy + ringing + wrapup) / (busy+ringing+wrapup+idle+away+dnd) × 100 | cc_agent_status_history |
| 小休时长 | SUM(duration_secs) WHERE from_status = 'away'（reason_code = 小休原因） | cc_agent_status_history |
| CSAT | AVG(score)，来自 cc_csat_surveys（明细）/ cc_calls.csat_score（冗余） | cc_csat_surveys |

### SLA 目标解析
- 单队列报表：读 `cc_skill_groups.sla_target_secs`（默认 20s）→ **精确**。
- 跨队列汇总报表：固定 20s 阈值（`AGGREGATE_SLA_TARGET_SECS`）→ **近似**，文档已注明。

### 快照 delta 语义
`cc_queue_snapshots` 的 `calls_offered/answered/abandoned` 是 MetricsCollector **累计值**；
`delta_offered/answered/abandoned/within_target` 是本区间增量（首条为 0，防启动尖峰）。
**区间图表必须聚合 delta 列**；范围内全为 0（迁移前旧数据）时端点自动回退旧行为。

### 集群语义
- 计数器（offered/answered/…）：跨 `instance_id` 行 **SUM**。
- 仪表型（calls_waiting 等）：**MAX/AVG**，且以事件驱动的共享 DB 值为准。

## 3. API

### CC v2 报表（RBAC：`cc_reports:read`）
| 端点 | 参数 | 返回 |
|---|---|---|
| `GET /cc/reports/v2/queue` | from,to(RFC3339 或 YYYY-MM-DD)、bucket=hour/day/week、queue_id、tz=±h 或 +HH:MM | 按 (bucket, queue)：offered/answered/abandoned/transferred/voicemail、avg_wait、avg_handle、sla_pct、asr_pct、abandon_rate_pct |
| `GET /cc/reports/v2/queue/export` | 同上 | CSV |
| `GET /cc/reports/v2/agents` | from,to | 每 agent：calls/answered/talk/avg_handle/wrapup/break/occupancy/csat |
| `GET /cc/reports/v2/agents/export` | 同上 | CSV |

### Core CDR
| 端点 | 说明 |
|---|---|
| `GET /console/api/call-records/export` | 与列表页同过滤参数（camelCase query），游标分批拉取，上限 50,000 行，RFC4180 CSV |

## 4. 时间桶与时区
`crate::report::bucket_index_expr(backend, col, bucket_secs, tz_offset)` 三方言生成：
- SQLite: `FLOOR((CAST(strftime('%s', col) AS INTEGER) + tz)/bucket)`
- MySQL: `FLOOR((UNIX_TIMESTAMP(col) + tz)/bucket)`
- PostgreSQL: `FLOOR((EXTRACT(EPOCH FROM col) + tz)/bucket)`

DAY 桶按 tz 对齐本地零点；`bucket_start_utc(idx, bucket, tz)` 还原桶起点。

## 5. 运维指标（Prometheus）
`/metrics`（observability addon）已覆盖 SIP/media/trunk/CC/cdr/recording 全套 `rustpbx_*` 指标；
`/console/api/metrics/runtime/data` 提供 Media（RTP 收发/丢包/桥接数）与 Voicemail（邮箱数/今日留言）实时 JSON。
Grafana 样例：`docs/grafana/rustpbx-overview.json`。

## 6. 与旧行为的差异
- 旧 `/cc/reports/calls`、`/cc/reports/agents`、`/cc/reports/charts/*` 保留兼容；数据源问题（快照累计值曲线）已在底层修复。
- 旧 `/cc/dashboard/timeseries` 在含 delta 数据的日期改用区间增量。
- SPA reports 页切换到 v2 API 计划在后续前端迭代中完成（后端已就绪）。

## 7. 多 Addon 域报表（/console/reports Tab 化）

统一 API：`GET /console/api/reports/v2/{domain}`（参数 from/to/bucket/tz/direction），每域附 `/export` CSV。
权限：`reports:read`（新点，可运行时授权）OR `cdr:read`；CC 域兼容 `cc_reports:read`。

| Tab | 域 | 数据源 | 说明 |
|---|---|---|---|
| 呼叫 | call | `rustpbx_call_records`（主 CDR） | bucket 趋势（total/answered/missed/时长/ASR）、挂断原因分布、SIP 码 TopN（CAST TEXT 解码）、主叫 TopN、**忙时热力**（星期×小时，本地时区，hour 桶 Rust 侧聚合） |
| 中继 | trunk | `rustpbx_cdr_daily`（day 桶）/ CDR CASE 折叠（子日桶） | 出中继 `outbound_sip_trunk_id` 在 rollup/查询侧折叠进 trunk 维度，`direction` 区分 in/out；join `rustpbx_sip_trunks` 取名称/carrier；NULL → "(无中继)" |
| 路由 | route | CDR.route_id join `rustpbx_routes` | 每路由量/ASR；route_id NULL → "未命中路由"（配置遗漏探测器） |
| 语音信箱 | voicemail | `rustpbx_voicemail_*`（raw SQL，**commerce-gated**） | 汇总卡（总量/未读/已读率/转写覆盖率）+ 邮箱积压 Top10；表不存在时 enabled=false 优雅空返回；前端按 `features.voicemail`（`cfg!(feature = "addon-voicemail")`）隐藏 tab |
| 注册/终端 | locator | `LocatorStats` 实时 + `rustpbx_runtime_snapshots`（kind=locator） | 实时卡（在线位置/用户/WebRTC/传输分布）+ 趋势（60s 采样、30 天保留、桶均值） |
| 呼叫中心 | cc | P0–P3 既有体系 | 别名挂载 `/reports/v2/cc/queue`、`/cc/agents`；旧 `/cc/reports/v2/*` 保留兼容 |

### 7.1 `rustpbx_runtime_snapshots`（唯一的报表新增迁移）
泛化运行时快照表：`kind`（locator / system_capacity / 未来扩展）+ `num1..4` + `data` JSON + `instance_id`，
60s 采样任务（app.rs 启动），按小时清理 30 天前旧行。未来域的快照需求一律复用本表，不再新增迁移。

### 7.2 已知边界
- locator 实时卡在 SIP server 未启动时 `available=false` 显示 "—"；趋势在采样积累后出现。
- trunk day 桶依赖 rollup 任务（每 5 分钟重算今日/昨日），冷启动首 tick 即执行。
- IVR 节点统计（二期）：需独立结构化落库，见需求文档 gap G3。

## 8. CC 报表：技能组为主（skill-group-first）

`cc_calls.queue_id` 即 ACD 派发的技能组 id。CC 报表已重构为技能组维度：

- **`GET /console/api/reports/v2/cc/groups`**（`include_empty` 默认 true）：每组一行窗口聚合
  （进线/接通/放弃/转接/留言、AWT/AHT、ASR、放弃率、CSAT），**SLA 按各组 `sla_target_secs` 精确计算**
  （LEFT JOIN + COALESCE 目标值，跨组汇总不再用固定 20s 近似）。0 话务的已配置组也会列出。
- **`GET /console/api/reports/v2/cc/groups/series?group_id=`**：单组 bucket 趋势（组目标精确 SLA）。
- **`GET /console/api/reports/v2/cc/agents?group_id=`**：
  - 带 group_id → 按（技能组, 坐席）呼叫归因指标（呼叫/接通/通话/平均处理/CSAT）；
  - 不带 → 全局坐席报表（含来自状态历史的占用率/小休——状态历史无组维度，物理不可拆）。
- `/cc/reports/v2/*` 旧路径保留别名兼容；**CC SPA 的 Reports tab 已下线**（统一报表页为唯一 UI 入口），旧 REST 端点保留。

截图：`docs/tab_cc_groups.png`（组汇总）、`tab_cc_group_selected.png`（单组趋势+组内坐席）、`tab_cc_en.png`（英文 i18n）。

- **`GET /console/api/reports/v2/cc/groups/heat?group_id=`**（可选）：接待/排队热力
  （星期×小时 7×24 网格，本地时区）——接待=接通量（绿色标尺）、排队=平均等待秒（红色标尺）；
  由 hour 桶序列在 Rust 侧聚合，跟随技能组选择器过滤。

## 9. 客户端上报（client telemetry）

restsend-call 客户端通过唯一公开端点 `POST {api_prefix}/report` 上报异常/诊断/升级事件
（契约 = `restsend-call/crates/pc/src/telemetry.rs`，无鉴权，`api_auth_middleware` 与
`csrf_guard` 均对 `POST */report` 豁免；防滥用为按 IP 240/min 限频 + 16 MiB body /
64 MiB 解压上限）。元数据走 query params（`X-Restsend-*` headers 镜像兜底），body 为
gzip 压缩的日志纯文本。

**零迁移存储**：复用 `rustpbx_runtime_snapshots`，一行一事件：

| 列 | 内容 | 用途 |
|---|---|---|
| `kind` | `cr:{event}`（15 种事件，≤23 字符） | 事件分布 / 过滤（真实列） |
| `instance_id` | 客户端 `version` | 版本分布 / 下钻（真实列） |
| `num1..3` | body 字节数 / in_call / safe_mode | 数值维度 |
| `data` JSON | os/arch/**agent_id**/sip_user/registrar/call_id/remote_uri/note/ts/ip/storage_key/body_preview… | 其余维度（`crate::report::json_text_expr` 三方言提取） |

- **日志体不进 DB**：本地目录 `[release].telemetry_dir`（默认 `storage/telemetry`），
  `YYYY-MM-DD/*.gz` 原样保存；明细页按 `storage_key` 读取（路径穿越防护），与快照行
  共享 30 天保留（小时清理任务同时删过期天目录）。
- Uploads return HTTP 5xx when the body cannot be persisted. Desktop clients
  treat any 2xx response as an upload receipt; `stored_body: false` alone is insufficient.
- New uploads persist the stable server identity in `data.received_by`. Body
  queries can use configured peers' AMI-authenticated
  `GET {ami_path}/cluster/client-reports/body?id=` endpoint; this endpoint only
  reads locally owned bodies and never forwards. Upgrade all participating nodes
  and retain stable, unique node identities. Legacy rows without an owner remain
  readable on the receiving node, but cannot be safely claimed by another peer.
- `GET {api_prefix}/release` 更新检查落一行 `cr:update_check`（原 debug log 升级为数据），
  版本分布因此包含"活跃客户端"。
- **异常口径**（ABNORMAL_EVENTS）：login_fail / dial_fail / answer_fail / panic /
  crash_marker / engine_exit / link_lost / call_abnormal / update_fail /
  update_download_fail。
- 统计 API（RBAC：`reports:read` OR `cdr:read`）：
  - `GET /console/api/client-reports/summary` — KPI + 事件/版本/OS/客户端(agent_id) 分布
  - `GET /console/api/client-reports/trend` — 按天趋势（total/abnormal/update_checks）
  - `GET /console/api/client-reports/list` — 明细（event/version/agent 过滤）
  - `GET /console/api/client-reports/body?id=` — 单条日志体（解压后截断返回）
  - `GET /console/api/client-reports/export` — 趋势 CSV
- UI：版本管理页（`/console/releases`）"客户端上报" tab。
- 集群口径：事实行按需 COUNT；`instance_id` 语义此处为"客户端版本"（与 locator/
  system_capacity 的"节点 id"语义隔离，互不干扰——它们的看板按 kind 精确过滤）。
