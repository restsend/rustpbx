# session_id 关联模型(call session correlation)

## 概述

rustpbx 使用两级 ID 模型关联一通逻辑呼叫的所有通话腿(legs):

| ID | 定义 | 生命周期 |
|---|---|---|
| `call_id` | 每个 SIP 通话(leg/session)的唯一标识 | 每次新 INVITE(含转接产生)各不相同 |
| `session_id` | 整通逻辑呼叫的根会话 ID | 首 INVITE 的 Call-ID;跨所有子通话腿不变 |

子通话腿包括:IVR/queue 派发的 agent 腿、REFER 盲转/attended 转接产生的新通话、
consult 咨询腿、外转外部网络后回落的通话。

此模型与业界一致(FreeSWITCH `call_uuid`、Asterisk `linkedid`、IMS `ICID`)。

## 定义与产生规则

- **入呼**:`session_id` = 首 INVITE 的 Call-ID(即 proxy session id)
- **外呼(originate)**:`session_id` = 生成的根 Call-ID(外呼即根)
- **子通话**:创建时从父通话的 CallMeta 继承 `session_id`(服务端内部继承为权威,
  不依赖信令头)
- **`session_id == call_id`** 表示该通话是根

## User-to-User(RFC 7433)信令载体

CC 场景(IVR / queue / transfer)下,`session_id` 通过 `User-to-User` 头跨网络传递:

```
User-to-User: <session_id>;encoding=hex;purpose=call-center;queue=<queue_id>;qn=<urlencoded_name>;skill=<group>
```

- `purpose=call-center`:RFC 7433 注册的标准用途值;非该 purpose 的 UUI 被忽略
- `queue` / `qn` / `skill`:CC 上下文(队列 canonical key、可读名、技能组)
- 知名头,跨运营商/SBC 存活率远高于未知 `X-` 头
- **普通 p2p 呼叫和 wholesale 呼叫不注入 UUI**

### 注入点

| 场景 | 位置 |
|---|---|
| queue 派发 agent | `CcQueueLocationEnricher`(替代旧 `X-CC-Call-Id` / `X-CC-Queue-*`) |
| 入向 REFER 转接 | `execute_inbound_refer_transfer` 新 INVITE |
| originate | RWI originate INVITE |

### 入呼解析

入呼 INVITE 携带 `purpose=call-center` 的 UUI 时,`session_id` 取自 UUI
(外部网络转回的腿重新挂回根会话);UUI 丢失则该通话成为新根(断链,不降级)。

## 事件契约(e2e 保证)

所有 cc_* / queue_* / skill_group_* / transfer 事件:

- `call_id` = 当前腿(leg)的 call id
- `session_id` = 整通逻辑呼叫的根 session id(由 `EventCallContext.session_id`
  通过 RWI gateway broadcast enrichment 自动附加到每个事件 payload)

内部继承链(CallMeta/meta_store)是权威数据源,UUI 只是网络边界上的载体 ——
UUI 丢失不影响事件链路的 session_id 关联。

## CDR

`rustpbx_call_records` 表有 `session_id` 列(VARCHAR(120),索引
`idx_rustpbx_call_records_session_id`,非唯一;迁移
`call_record_session_id_column`):

- `call_id` 保持唯一索引(每腿一条记录)
- `session_id` 用于按逻辑呼叫聚合查询(`GROUP BY session_id`),替代已退役的
  `root_call_id` 机制;它是唯一事实源——**不再写入 `metadata` JSON**
- **主/子 CDR 派生**:根会话的记录(`session_id` 为 NULL 的旧行,或
  `session_id == call_id`)为 primary;派生腿(queue 派发、转接跳转、
  集群跨节点腿)为 child
- `leg_timeline` 列记录腿生命周期事件(added / bridged / unbridged /
  transferred / removed,上限 128 条),盲转、queue 派发、桥接均会写入
- `metadata.transferred = true` 标记本腿被转接过(CSAT 抑制同样依赖
  `CallMeta.transferred`)
- 录音/sipflow 制品命名优先使用根 `session_id`(全通 `{session_id}.wav`,
  片段 `{session_id}_{timestamp}_{type}_{id}.wav`; 信令旁路
  `{session_id}.jsonl` / `{session_id}_{call_id}.jsonl`),腿级 `call_id`
  仍写入 CDR/`extra` 以防碰撞

### 查询与消费

- Console 呼叫记录列表**默认只显示 primary**(每通一条);"All call legs"
  过滤器(`filters.allLegs`)展开全部腿
- Console 详情页展示同一逻辑呼叫的子腿列表(`child_legs`)
- `GET /call-records/by-session/{session_id}/artifacts` 按 `session_id`
  列(回退 `call_id`)聚合各腿录音片段与 sipflow jsonl,legs 带
  `leg_role`(primary/child)
- CC CDR webhook / OpenAPI 的 `CdrWebhookPayload` 带 `legRole` 字段
- SQL 直查:主 CDR 过滤条件
  `session_id IS NULL OR session_id = call_id`;聚合键
  `COALESCE(session_id, call_id)`

### 自定义表 / 日轮转

raw-SQL saver(自定义表名、SQLite 日轮转)同步写入 `session_id` 列并在建表
DDL 中包含该列;对升级前已存在的表,启动时 `ensure_session_id_column` 会
尽力补列(幂等)。外部自建表需自行保证列存在。CSV 导出会自动多出
`session_id` 列。

## 迁移说明

- 旧 `X-CC-Call-Id` / `X-CC-Queue-Id` / `X-CC-Queue-Name` 头已由 UUI 替代;
  cc-phone 前端保留 X-* 读取作为旧版本 proxy 的兼容回退
- 事件 typed 字段 `CallCreated.root_call_id`（原 `CallIncoming`，事件已更名为
  `call_created`）/ `RecordStopped.root_call_id`
  已删除;跨腿关联一律使用 enrichment 注入的顶层 `session_id`
- 嵌套块 `root`(`RootCallInfo`)仍可携带主被叫等展示信息,其 `call_id`
  表示该腿自身上下文, **不是** 逻辑呼叫关联键;关联键只用 `session_id`
- CC OpenAPI:`CallRecord` / `ActiveCall` / `CdrWebhookPayload` /
  `RwiEventCallContext` 已文档化 `session_id`(见 `cc-agent-cti.yaml`)
