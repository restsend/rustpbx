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

`rustpbx_call_records` 表新增 `session_id` 列(索引,非唯一):

- `call_id` 保持唯一索引(每腿一条记录)
- `session_id` 用于按逻辑呼叫聚合查询,替代已退役的 `root_call_id` 机制
- 录音/sipflow 制品命名优先使用根 `session_id`(全通 `{session_id}.wav`,
  片段 `{session_id}_{timestamp}_{type}_{id}.wav`; 信令旁路
  `{session_id}.jsonl` / `{session_id}_{call_id}.jsonl`),腿级 `call_id`
  仍写入 CDR/`extra` 以防碰撞
- Console/API:`GET /call-records/by-session/{session_id}/artifacts` 按
  `session_id` 聚合各腿录音片段与 sipflow jsonl

## 迁移说明

- 旧 `X-CC-Call-Id` / `X-CC-Queue-Id` / `X-CC-Queue-Name` 头已由 UUI 替代;
  cc-phone 前端保留 X-* 读取作为旧版本 proxy 的兼容回退
- 事件 typed 字段 `CallIncoming.root_call_id` / `RecordStopped.root_call_id`
  已删除;跨腿关联一律使用 enrichment 注入的顶层 `session_id`
- 嵌套块 `root`(`RootCallInfo`)仍可携带主被叫等展示信息,其 `call_id`
  表示该腿自身上下文, **不是** 逻辑呼叫关联键;关联键只用 `session_id`
- CC OpenAPI:`CallRecord` / `ActiveCall` / `CdrWebhookPayload` /
  `RwiEventCallContext` 已文档化 `session_id`(见 `cc-agent-cti.yaml`)
