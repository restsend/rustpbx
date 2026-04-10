# 呼叫中心（CC）+ Web 坐席需求与技术方案（基于 rustpbx）

更新时间：2026-03-27

## 1. 目标与范围

目标：基于 rustpbx 构建一套可商用的呼叫中心应用，覆盖 Web 坐席、技能组（Skill Group）、转接（含 REFER/3PCC 回退）、IVR 转人工、通后评分评价、主管质检与运营报表能力。AI 接待为可选增强模块，不影响核心链路。

范围：
- 坐席工作台（Web Agent）
- ACD/Queue/Skill Group
- IVR 与人工协同
- 通话控制与转接策略（REFER + 3PCC fallback）
- 质检评分与运营分析
- AI 接待（可选，独立 SIP UA 模式接入）

## 2. 产品需求（PRD 草案）

### 2.1 Web 坐席（Agent Desktop）

1. 账号与状态
- 坐席登录、强制签出、单点登录可扩展。
- 坐席状态：空闲、振铃、通话中、整理（Wrap-up）、离席（Break）。
- 状态切换策略：手动切换 + 超时自动恢复。

2. 来电与通话控制
- 来电弹屏：客户资料、历史通话、工单摘要。
- 基础控制：接听、挂断、保持/恢复、静音。
- 转接控制：盲转、咨询转、转接取消、转接完成。
- 会议与监督协同：支持被监听/耳语/强插场景下的状态提示。

3. 事后处理（Wrap-up）
- 结果码（Disposition）必填/选填策略。
- 通话备注、标签、回访任务。
- 质检入口与申诉标记。

### 2.2 呼叫中心核心（ACD + Skill Group）

1. 技能组管理
- 技能项定义（语言、业务线、级别）。
- 坐席技能等级（0-5 或 1-10）及有效期。
- 队列绑定技能策略（硬匹配/软匹配）。

2. 路由与分配
- 分配策略：最长空闲、最少通话、轮询、技能优先。
- 优先级：VIP、渠道、业务标签。
- 失败重试：坐席未接、拒接、忙线后的重分配策略。

3. 队列生命周期
- 入队、排队等待、分配坐席、接通、回队（Requeue）、超时退出。
- 排队音乐、排队播报、预计等待时长（可选）。
- 溢出策略：跨队列、转 IVR、转语音信箱。

4. SLA 与运营指标
- SLA、接通率、放弃率、ASA、AHT、FCR、满意度。
- 实时看板 + 历史报表。

### 2.3 IVR 转人工 + 通后评价

1. IVR 能力
- 多级菜单与 DTMF 收号。
- 根据按键进入不同技能组/队列。
- 高峰策略：提示等待、回呼、留言。

2. 转人工协同
- IVR 节点转队列（queue.enqueue 或路由到队列目标）。
- 转人工后携带上下文（菜单路径、业务标签）。

3. 评价体系
- 通后自动评价（1-5 分、按键或链接）。
- 差评触发质检工单/主管回呼。
- 评分归因：坐席、技能组、时段、业务类型。

### 2.4 主管与质检

1. 监督能力
- 监听（Listen）、耳语（Whisper）、强插（Barge）。

2. 质检流程
- 抽检规则（随机/低分优先/VIP优先）。
- 质检表单（开场、合规、解决度、服务态度）。
- 复核与申诉闭环。

### 2.5 非功能需求

- 并发与稳定性：高峰排队与转接可靠。
- 一致性：坐席状态、队列状态、通话状态最终一致。
- 安全：RBAC、审计日志、敏感录音访问控制。
- 可观测：转接成功率、回退比例、失败原因分布。

## 3. 技术选型结论：独立 App 还是 Addon

结论：采用“独立 App + Addon 增强”的混合方案。

1. 独立 App（推荐作为主形态）
- 负责：Web 坐席、业务编排、CRM/工单集成、质检评分、BI。
- 通过 rustpbx 的 RWI/REST/Webhook 实现控制与数据采集。
- 优势：迭代快、可独立扩展、业务边界清晰。

2. Addon（用于深度内核扩展）
- 负责：必须贴近 PBX 内核的策略和管理注入。
- 典型场景：复杂队列策略、控制台扩展、租户级能力钩子。
- 优势：低延迟深集成；代价：版本耦合更高。

3. 不建议纯 Addon 全做
- Web 坐席和运营业务若完全作为 addon，产品迭代与跨系统集成会受限。

## 4. 建议架构

### 4.1 分层架构

1. CC 应用层（独立部署）
- Agent Gateway：WebSocket 会话、坐席状态机、订阅推送。
- Routing Orchestrator：技能匹配、排队与溢出决策。
- QA/Survey Service：评分、质检、工单联动。
- BI Service：指标聚合与报表导出。

2. rustpbx 能力层
- SIP/媒体/桥接。
- RWI：实时通话控制（originate/answer/transfer/queue/supervisor）。
- HTTP Router：来话动态路由。
- Call Record/CDR Push：话单与录音事件输出。

3. Addon 层（按需）
- 队列策略增强与特定管理入口。
- 企业权限、租户策略、商业能力包。

### 4.2 控制与事件通道建议

- 强实时控制：RWI（主）。
- 配置与后台管理：REST（console/ami）。
- 异步数据与对账：Webhook/CDR Push。

## 5. REFER 与 3PCC 回退策略（关键）

目标：优先使用 REFER；当对端 UA/Trunk 不支持 REFER 或 Replaces 时，自动回退本地 3PCC，保证业务成功率。

1. 策略原则
- Prefer REFER：能力可用时优先标准 SIP 转接。
- Fallback 3PCC：REFER 失败、超时、能力不兼容时回退。
- Per-peer policy：按 trunk/UA 维度配置兼容策略。

2. 最小状态机（建议）
- init -> refer_sent -> notify_progress -> accepted/completed
- 任意失败分支 -> local_3pcc_fallback -> completed/failed

3. 关键事件
- call.transfer.accepted
- call.transfer.failed（带 sip_status, reason, mode）
- call.transferred

4. 风险控制
- 每通话串行化执行转接命令，避免 REFER/NOTIFY/BYE 竞态。
- 转接失败后显式恢复原桥接与媒体状态。

## 6. 数据模型建议（最小集合）

- agent（坐席）
- skill, agent_skill（技能与等级）
- queue, queue_policy（队列与策略）
- queue_member（队列成员）
- interaction（一次服务会话）
- interaction_leg（多段通话）
- transfer_transaction（转接事务）
- wrapup（通后结果）
- survey_result（评价结果）
- qa_review（质检记录）

## 7. 版本规划（12 周示例）

1. 第 1-2 周：需求冻结与架构设计
- 确认能力边界、数据模型、接口契约。

2. 第 3-5 周：Web 坐席 MVP
- 接听/挂断/保持/转接/通后处理。

3. 第 6-8 周：ACD + Skill Group + IVR 转人工
- 队列策略、技能匹配、溢出规则。

4. 第 9-10 周：评价与质检
- 满意度、抽检、复核流程。

5. 第 11-12 周：稳定性与灰度
- REFER fallback 完善、压测、可观测、灰度发布。

## 8. 验收标准（DoD）

- 盲转、咨询转、取消转接均通过 E2E。
- REFER 不兼容场景自动 3PCC 回退成功。
- IVR -> 技能组 -> 坐席接通链路稳定。
- 通后评价与质检数据可追溯到会话维度。
- 核心指标（SLA/接通率/放弃率）可实时与离线查看。

## 9. 最终建议

- 主体：独立 CC App（业务与前端）。
- 底座：rustpbx（SIP/媒体/实时控制）。
- 补强：按需 Addon（内核深集成能力）。
- AI 接待：可选，以独立 SIP UA 形式接入，不影响核心链路（详见第 11 节）。

这条路线在工程效率、业务迭代速度、以及与 rustpbx 现有能力匹配度上最优。

## 10. 直连模式落地（Web 坐席直连 rustpbx，CC App 管理 rustpbx）

结论：该模式可行，且建议作为默认实施方案。

### 10.1 连接拓扑

1. Web 坐席（浏览器）
- SIP over WebSocket 直连 rustpbx。
- 浏览器与 rustpbx 建立 WebRTC 媒体链路。

2. CC App（业务后台）
- 通过 RWI/REST/Webhook 管理 rustpbx。
- 不直接承载媒体，仅承载业务编排与状态聚合。

3. rustpbx（通信底座）
- 承载 SIP 信令、媒体桥接、队列执行与录音话单。

### 10.2 职责边界（必须明确）

1. 坐席前端负责
- 设备权限、软电话注册、振铃提示、基础通话操作。
- 坐席 UI 的本地状态呈现。

2. CC App 负责
- 坐席就绪态管理（Ready/Not Ready/Break）。
- 技能组与队列策略、IVR 转人工规则、超时溢出。
- 通后处理、评价、质检、运营报表。

3. rustpbx 负责
- 呼叫建立与媒体稳定性。
- 转接执行（REFER 优先 + 3PCC 回退）。
- 事件输出与录音/CDR 归档。

### 10.3 控制冲突治理（关键）

为避免“前端和 CC 后台同时控制同一通话”导致状态错乱，必须实施以下约束：

1. 单通话单控制者
- 同一时刻仅允许一个控制源写操作（agent_ui 或 cc_orchestrator）。
- 控制权切换必须显式记录 owner 与过期时间。

2. 命令幂等
- 所有控制命令携带 action_id/request_id。
- 重试必须可去重，重复命令返回同一结果。

3. 状态机前置校验
- 所有操作先校验当前状态再执行。
- 例如：仅在 bridging/connected 状态允许 transfer；仅在 ringing 状态允许 answer。

4. 事件回放与纠偏
- CC App 保留最近事件游标（offset/sequence）。
- 断线恢复后按序回放，必要时触发全量对账（list_calls + active calls）。

补充结论（执行口径）：
- Web 坐席与 rustpbx 直连主要用于 SIP 信令与媒体承载。
- 涉及业务编排的高级控制（咨询转、三方加入、监督、批量策略）应走 Web 坐席 -> CC App -> RWI。
- 浏览器侧只表达“业务意图”，不直接决定 REFER/3PCC/会议桥分支，避免前后端双写同一通话状态。

### 10.4 最小接口清单（建议）

1. Web 坐席 <-> rustpbx（SIP/WS）
- register / unregister
- invite / answer / bye
- hold / unhold / dtmf

2. CC App <-> rustpbx（RWI）
- session.subscribe / session.list_calls
- call.originate / call.answer / call.hangup
- call.transfer / call.transfer.attended / call.transfer.complete / call.transfer.cancel
- queue.enqueue / queue.dequeue / queue.hold / queue.unhold

3. rustpbx -> CC App（Webhook/事件）
- 通话生命周期事件（incoming, answered, bridged, hangup）
- 转接结果事件（accepted, failed, transferred）
- CDR 与录音回调

### 10.5 REFER 回退策略在直连模式中的位置

1. 策略执行层
- 策略在 rustpbx 执行，不放在浏览器侧。
- 浏览器只发起“转接意图”，不自行决定 REFER/3PCC 分支。

2. 回退触发条件（建议）
- 对端 405/420/501 或能力表标记不支持 REFER。
- attended 场景中 Replaces 不兼容或超时。

3. 回退结果透出
- RWI 与 Webhook 输出 mode=sip_refer 或 mode=local_3pcc_fallback。
- 便于统计回退率与兼容性问题。

边界补充：
- REFER/3PCC 回退主要用于“转接”事务（盲转/咨询转完成）。
- “把第三人加入当前通话并三方同时在线”优先定义为会议合并（conference merge），不以 REFER 作为主路径。

### 10.6 实施顺序（增量上线）

1. 阶段 A：先打通直连与基础控制
- Web 坐席注册、接听、挂断、保持。

2. 阶段 B：接入 CC 编排
- Ready/Not Ready、队列分配、IVR 转人工、通后处理。

3. 阶段 C：转接与回退闭环
- REFER 优先、失败自动 3PCC、事件可观测。

4. 阶段 D：运营与质检
- 评分、抽检、报表、告警与容量压测。

### 10.7 验收补充（针对直连模式）

- 坐席浏览器异常断开后 30 秒内可自动恢复并同步状态。
- 同一 call_id 不出现双控制源并发写冲突。
- REFER 失败回退 3PCC 后，坐席端仅看到一次业务级“转接成功/失败”终态。
- 回退链路的失败原因可在日志与指标中追踪到 trunk/UA 维度。

## 10.8 三方加入会话（坐席拉第三方）推荐流程

目标：坐席在与客户通话中，呼入第三方并在接通后加入当前会话，三方同时在线。

1. 总体原则
- 主流程采用服务端会议桥（conference bridge / merge）。
- 控制路径采用 Web 坐席 -> CC App -> RWI。
- 媒体路径仍可保持浏览器与 rustpbx 直连，不要求 CC App 承载媒体。

2. 建议时序
- Step 1：坐席与客户已通话（Leg-A=connected）。
- Step 2：坐席在前端发起“咨询第三方”意图，CC App 通过 RWI 置客户保持并发起 Leg-B。
- Step 3：第三方接通后，坐席先与第三方私聊确认（consult_connected）。
- Step 4：坐席点击“加入会话”，CC App 通过 RWI 执行 merge，将 Leg-A、Leg-B、坐席合并到同一会议桥。
- Step 5：合并成功后进入 merged；若失败则回滚到客户通话（back_to_customer），并保留失败原因。

3. 状态机（最小集合）
- connected -> consult_dialing -> consult_connected -> merge_requested -> merged
- 任意失败分支 -> merge_failed -> back_to_customer

4. 事件命名建议（与现有 transfer 事件风格一致）
- call.conference.consult_dialing
- call.conference.consult_connected
- call.conference.merge_requested
- call.conference.merged
- call.conference.merge_failed（含 reason/mode）

5. 与转接策略关系
- 若业务动作是”把客户交给第三方并退出”，使用 transfer（REFER 优先 + 3PCC fallback）。
- 若业务动作是”三方同时在线继续沟通”，使用 conference merge。

## 11. AI 接待模块（可选）

### 11.1 为什么 AI 不应内嵌于 rustpbx

1. 接口不兼容
   - rustpbx 的 `AudioSource` trait 使用同步 `read_samples(&mut self, buffer: &mut [i16]) -> usize` 接口。
   - TTS 流式输出天然异步（HTTP chunked / WebSocket streaming），无法在不阻塞线程的前提下喂入同步 pull 模型。

2. 延迟与稳定性隔离
   - LLM 推理延迟（100ms~数秒）会直接影响 rustpbx 媒体线程调度，造成抖动与丢包。
   - AI 服务崩溃或超时不应波及核心 SIP/媒体稳定性。

3. 官方定位已明确
   - rustpbx 官方已将 Voice Agent 功能迁移至独立仓库 [Active Call](https://github.com/restsend/active-call)，明确不在 rustpbx 内做 AI pipeline。

4. 迭代速度
   - AI 模型、Prompt、ASR/TTS 供应商选型变化频繁，独立进程可独立发布，不需要重编译/重部署 rustpbx。

### 11.2 推荐架构：AI Bot 以独立 SIP UA 接入

```
来电
  │
  ▼
rustpbx（SIP B2BUA）
  │  HTTP Router Webhook (每次 INVITE 触发)
  ▼
CC App（路由决策）
  ├─── [无 AI / 高峰 / 降级] ──► IVR → Queue → 人工坐席
  └─── [正常流量 / AI 可用]  ──► AI Bot SIP Extension
                                      │
                                      │ SIP INVITE + RTP
                                      ▼
                              AI Bot 进程（独立部署）
                              ┌─────────────────────┐
                              │ SIP UA（接收来电）    │
                              │ ASR（语音转文字）     │
                              │ LLM（意图理解/对话）  │
                              │ TTS（文字转语音）     │
                              └─────────────────────┘
                                      │
                                      │ 完成接待后
                                      │ POST /console/calls/active/{id}/commands
                                      │ { “action”: “transfer”, “target”: “queue:support” }
                                      ▼
                              rustpbx 执行转接 → 人工坐席
```

### 11.3 AI Bot 与 rustpbx 的接口契约

1. AI Bot 注册（SIP REGISTER）
   - AI Bot 以普通 SIP 分机身份注册到 rustpbx。
   - 支持多实例注册同一账号（并发接待）或多账号（容量水平扩展）。

2. 来电接入（HTTP Router Webhook → CC App → 路由到 AI Bot）
   - CC App 在路由决策中将 target 指向 AI Bot 分机号（如 `ai-bot-01`）。
   - rustpbx 向该分机发 INVITE，AI Bot 接听并开始 ASR+LLM+TTS 流水线。

3. 上下文传递
   - HTTP Router Webhook 响应可携带 `headers` / `extensions` 字段，rustpbx 将其附加到 INVITE。
   - AI Bot 从 SIP INVITE 的自定义头（`X-CC-Context`）或 INFO 消息中读取业务上下文（来源渠道、IVR 路径、客户标签）。

4. 转人工（AI Bot → rustpbx REST）
   - AI Bot 判断需要转人工时，调用：
     ```
     POST /console/calls/active/{session_id}/commands
     Content-Type: application/json
     { “action”: “transfer”, “target”: “queue:skill_group_name” }
     ```
   - rustpbx 执行转接（B2BUA 模式），AI Bot 的腿自动挂断。
   - CC App 通过 CDR Webhook 收到转接结果，将 AI 对话摘要写入 interaction 记录。

5. AI Bot 主动挂断
   - 意图明确无需人工时（已完成服务），调用：
     ```
     POST /console/calls/active/{session_id}/commands
     { “action”: “hangup” }
     ```

### 11.4 AI 可选性保障（不影响核心链路）

1. 路由决策分支
   - CC App 的 HTTP Router Webhook 处理函数中，AI 是一个可配置的路由分支。
   - 当 AI Bot 全部繁忙、服务不可用或配置为关闭时，CC App 直接返回 IVR/Queue 路由，不经过 AI。

2. 降级策略（建议）
   - AI Bot 心跳检测：CC App 定期探测 AI Bot 注册状态（via `GET /console/calls/active` 或 SIP OPTIONS）。
   - 自动降级：AI Bot 不可用时，CC App 路由决策自动切换为 IVR → Queue。
   - 手动降级：运营后台一键关闭 AI 接待，实时生效（仅改变 CC App 路由配置，无需重启 rustpbx）。

3. 并发容量
   - AI Bot 实例数决定 AI 接待并发上限。
   - 超出容量的来电自动走非 AI 路径（CC App 路由层控制，rustpbx 无感知）。

### 11.5 rustpbx 侧最小变更需求

AI Bot 独立 SIP UA 方案对 rustpbx 的改动极小：

| 需求 | 现状 | 变更量 |
|------|------|--------|
| SIP 分机注册与来电 | 已支持 | 无需改动 |
| HTTP Router Webhook 路由 | 已支持 | 无需改动 |
| REST 转接 API | 已支持（`/console/calls/active/{id}/commands`） | 无需改动 |
| CDR/录音 Webhook | 已支持 | 无需改动 |
| INVITE 自定义头透传 | Webhook 响应 `headers` 字段已支持 | 验证透传到 AI Bot 即可 |
| 实时对话事件推送 | 暂无（当前为事后 CDR） | 可选：增加实时 Webhook 事件推送 |

结论：核心链路零改动，AI 接待作为纯外挂能力接入。

### 11.6 实施阶段（与第 10.6 节对应）

| 阶段 | 内容 | AI 依赖 |
|------|------|---------|
| 阶段 A | Web 坐席注册、接听、挂断、保持 | 无 |
| 阶段 B | 队列分配、IVR 转人工、通后处理 | 无 |
| 阶段 C | REFER 转接与 3PCC 回退闭环 | 无 |
| 阶段 D | 运营报表、质检、告警、压测 | 无 |
| 阶段 E（可选） | AI Bot 接入、降级策略、AI 对话摘要写入 interaction | AI Bot 独立项目 |

阶段 A-D 交付完整可商用的 CC 系统，阶段 E 可按需叠加，互不阻塞。

## 12. Queue 与 IVR 现状差距分析与改造清单

### 12.1 现有实现盘点

rustpbx 当前有三套 Queue 相关模块，各自独立、尚未打通：

| 组件 | 位置 | 职责 | 状态 |
|------|------|------|------|
| `QueueApp` (CallApp) | `call/app/queue.rs` | 作为 Dialplan 中的 CallApp，直接振铃静态 agent 列表 | 可运行，但仅支持静态 Sequential/Parallel |
| `QueueManager` | `call/runtime/queue_manager.rs` | 通用优先级排队数据结构 | 已实现优先级排序、位置追踪，但未被 QueueApp 使用 |
| `RwiCommandProcessor` | `rwi/processor.rs` | RWI 层 queue 操作（enqueue/dequeue/requeue） | 已实现内存状态管理，但不触发实际振铃/媒体动作 |
| `IvrApp` (CallApp) | `call/app/ivr.rs` | 完整 IVR 状态机 | 978行，功能完善 |
| `IvrDefinition` | `call/app/ivr_config.rs` | IVR TOML 配置 | 支持多级菜单、Webhook、营业时间、收号 |

**核心断裂点**：

1. `QueueApp` 走静态 agent 列表振铃，不经过 `QueueManager`。
2. `RwiCommandProcessor::queue_enqueue()` 只改内存 HashMap，不触发 originate。
3. `QueueManager` 有优先级排队能力，但无人调用。
4. IVR `EntryAction::Queue` 只生成 `AppAction::Transfer("queue:sales")`，不携带上下文。

### 12.2 P0：链路闭环（最小可行）

以下三项是 IVR → Queue → Agent 链路跑通的最小集合，缺一不可。

#### 12.2.1 统一 Queue 架构

将 `QueueApp`、`QueueManager`、`RwiCommandProcessor` 三者打通为一个统一的调度流程：

```
入口（IVR / HTTP Router / RWI enqueue）
  │
  ▼
QueueManager（统一排队）
  ├── 入队 + 优先级排序
  ├── 位置追踪
  └── 等待超时检测
        │
        ▼
QueueDispatchEngine（新增）
  ├── 技能/状态匹配 → 选坐席
  ├── originate → 振铃坐席
  ├── 超时 → 重新分配
  └── 溢出 → 跨队列 / IVR / 语音信箱
```

**具体改动**：

1. `QueueApp::on_enter()` 不再直接振铃，改为通过 `QueueManager` 入队等待。
2. `RwiCommandProcessor::queue_enqueue()` 入队后立即尝试分配坐席并 originate。
3. `QueueApp` 增加等待状态 `Waiting { enqueued_at, last_announcement }` 和振铃状态 `RingingAgent { agent_uri, ring_started_at }`。
4. `CallApp` trait 增加 `on_timer()` 方法，支持定时播报和超时检测。

#### 12.2.2 IVR → Queue 上下文透传

IVR `EntryAction::Queue` 增强为携带完整上下文：

```toml
[[ivr.root.entries]]
key = "2"
label = "Billing Support"
action = { type = "queue", target = "billing_queue", skills = ["billing", "english"], priority = 3 }
```

透传内容：

| 字段 | 来源 | 用途 |
|------|------|------|
| `ivr_name` | IVR 定义 | 标记来电来源 |
| `menu_path` | 如 `root → support → billing` | 坐席弹屏显示客户导航路径 |
| `variables` | Collect 收集的变量 | 携带 account_id 等 |
| `skills` | IVR 配置或 Webhook 返回 | 队列技能匹配依据 |
| `priority` | IVR 配置或 Webhook 返回 | 排队优先级 |
| `caller_entered_digits` | 用户按过的键 | 辅助坐席理解意图 |

`AppAction::Transfer` 需扩展为 `AppAction::TransferWithContext { target, context }`。

#### 12.2.3 通用事件实时推送 Webhook

当前 rustpbx 仅有 `LocatorWebhook`（注册事件）和 `CallRecordHook`（事后 CDR），缺少通话实时事件推送。CC App 必须实时感知以下事件才能做调度决策和坐席弹屏。

**新增 Event Webhook 配置**：

```toml
[event_webhook]
url = "https://cc-app.example.com/api/events"
headers = { Authorization = "Bearer xxx" }
events = [
    "call.incoming",
    "call.answered",
    "call.hangup",
    "call.bridged",
    "call.unbridged",
    "call.transfer.accepted",
    "call.transfer.failed",
    "call.transfer.completed",
    "queue.joined",
    "queue.position_changed",
    "queue.agent_offered",
    "queue.agent_connected",
    "queue.left",
    "queue.overflowed",
    "queue.wait_timeout",
]
timeout_ms = 5000
retry_count = 2
```

**推送格式**：

```json
{
    "event": "queue.agent_connected",
    "timestamp": "2026-04-05T10:30:00Z",
    "call_id": "abc-123",
    "session_id": "sess-456",
    "data": {
        "queue_id": "support",
        "agent_id": "agent-001",
        "wait_time_secs": 45
    }
}
```

### 12.3 P1：排队体验与调度增强

#### 12.3.1 坐席动态登录/登出队列

当前 agent 列表是静态配置在 TOML 中的。CC 场景需要坐席动态加入/退出队列。

**新增 RWI 命令**：

| 命令 | 说明 | 对应事件 |
|------|------|---------|
| `queue.agent_login` | 坐席登录到指定队列 | `QueueAgentLoggedIn` |
| `queue.agent_logout` | 坐席登出指定队列 | `QueueAgentLoggedOut` |
| `queue.agent_pause` | 暂停接听（WrapUp / Break） | `QueueAgentPaused` |
| `queue.agent_resume` | 恢复接听 | `QueueAgentResumed` |

**数据模型**（CC App 侧持久化，rustpbx 通过 RWI 同步）：

```
queue_member: queue_id, agent_id, skills, priority, penalty, paused, paused_reason
```

#### 12.3.2 等待体验：位置播报 + 语音拼接

`QueueEntry.position_announcements_enabled` 和 `CallQueue.announcement_interval` 已定义但未实现播报。

**语音文件目录结构**：

```
sounds/queue/
├── position/
│   ├── your_position_is.wav     # "您的排队位置是"
│   ├── 1.wav ~ 99.wav           # 数字语音文件
│   └── please_wait.wav          # "请耐心等待"
├── estimated_wait/
│   ├── estimated_wait.wav       # "预计等待"
│   └── minutes.wav              # "分钟"
└── hold_music.wav               # 保持音乐（已有）
```

**播报流程**：停止保持音乐 → 播 "您的位置是第 N 位" → 播 "请耐心等待" → 恢复保持音乐。

**CallApp 定时器支持**：给 `CallApp` trait 增加 `on_timer(timer_id, ctrl, ctx)` 回调，由 `CallController` 管理定时器注册。

#### 12.3.3 排队中 DTMF 交互

客户在排队等待时按 DTMF 键应有响应：

| 按键 | 动作 |
|------|------|
| `0` | 转人工（提升优先级） |
| `1` | 继续等待（重新播报位置） |
| `2` | 留言（转语音信箱） |
| `3` | 预约回呼 |
| `*` | 退出排队并挂断 |

`QueueApp` 在 `Waiting` 状态下监听 DTMF 并分发动作。

#### 12.3.4 Whisper 公告（坐席接听时单腿播报）

坐席接听后、正式通话前，对坐席腿播放来电信息（客户听不到）：

```
坐席接听
  → Queue 检测到 agent_connected
  → 对坐席腿单播："这是来自技术支持队列的来电，客户已等待 2 分钟"
  → 公告完成 → 正式双向桥接
```

实现依赖已有的 `CallCommand::Play { leg_id: Some(agent_leg) }` 单腿播放能力，需在 Queue 调度流程中串联。

**Whisper 公告配置**：

```toml
[queue.support]
whisper_prompt = "sounds/queue/whisper_support.wav"   # 静态播报
# 或
whisper_webhook = "https://cc-app/api/whisper"        # 动态生成（根据 caller 信息）
```

#### 12.3.5 回呼功能（Callback）

高峰时客户可选"预约回呼"而不在线等待。

**IVR 新增 EntryAction**：

```rust
EntryAction::Callback {
    queue: String,                    // 目标队列
    prompt: Option<String>,           // "我们会在X分钟内回呼您"
    estimated_delay_secs: Option<u64>,
}
```

**执行流程**：

1. IVR 收集回呼意图 → 挂断。
2. CC App 通过 Webhook 收到 callback 事件。
3. CC App 在队列空闲时通过 RWI originate 发起回呼。
4. 客户接听后走正常 Queue 流程分配坐席。

回呼引擎放在 CC App 侧，rustpbx 只负责收集意图和挂断。

#### 12.3.6 通后评价 IVR

通话结束后自动触发评价流程（1-5 分按键收集）。

**触发方式**：CC App 检测到坐席挂断后，通过 RWI originate 对客户腿发起评价 IVR。

**评价 IVR 配置模板**（复用现有 IvrApp）：

```toml
[ivr]
name = "survey"

[ivr.root]
greeting = "sounds/survey/prompt.wav"
timeout_ms = 5000
max_retries = 2
unknown_key_action = { type = "hangup" }

[[ivr.root.entries]]
key = "1"
action = { type = "webhook", url = "https://cc-app/api/survey", variables = "score,session_id" }

[[ivr.root.entries]]
key = "2"
action = { type = "webhook", url = "https://cc-app/api/survey", variables = "score,session_id" }

# ... 3-5 类似
```

差评（1-2 分）自动触发质检工单。

#### 12.3.7 IVR 高峰前置判断

IVR 在执行 `EntryAction::Queue` 前查询队列状态，根据深度做分流。

**方案 A（推荐）**：通过 Webhook 查询 CC App：

```toml
[[ivr.root.entries]]
key = "2"
label = "Support"
action = { type = "queue", target = "support", 
           pre_check_url = "https://cc-app/api/queue/check?queue=support",
           overflow_menu = "queue_busy_options" }
```

Webhook 返回 `{ "action": "queue" }` 或 `{ "action": "menu", "menu": "queue_busy_options" }`。

**方案 B**：在 `QueueApp` 入口自动检查溢出条件并路由到备选菜单。

### 12.4 P2：高级能力

#### 12.4.1 IVR 条件路由（VIP 跳 IVR 直进高优队列）

在 IVR 入口增加 `pre_route` 阶段，通过 Webhook 查询 caller 身份：

```toml
[ivr.root.pre_route]
type = "webhook"
url = "https://cc-app/api/caller-lookup"
timeout = 3
# 返回 { "action": "queue", "target": "vip", "priority": 1 }
# 或 { "action": "continue" } 走正常 IVR
```

VIP 客户跳过全部 IVR 菜单直接进高优先级队列。

#### 12.4.2 队列优先级动态调整

当前 `QueueEntry.priority` 只在入队时设置一次。

增强为：

| 触发 | 调整 |
|------|------|
| VIP 客户入队 | priority = 1（最高） |
| 等待超过 3 分钟 | priority 自动降 2（更优先） |
| 等待超过 5 分钟 | priority 自动降 4 |
| CC App 通过 RWI `queue.set_priority` | 手动调整 |

通过 `on_timer` 定期检查等待时间并调整。

#### 12.4.3 队列统计持久化与 SLA

**QueueRuntimeStats**（扩展 `QueueStats`）：

```rust
pub struct QueueRuntimeStats {
    pub total_entered: u64,
    pub total_answered: u64,
    pub total_abandoned: u64,        // 排队中挂断
    pub total_overflowed: u64,
    pub total_timeout: u64,          // 等待超时
    pub total_requeued: u64,
    pub answer_time_sum: Duration,   // ASA = answer_time_sum / total_answered
    pub talk_time_sum: Duration,     // AHT = talk_time_sum / total_answered
    pub sla_threshold_secs: u64,     // 如 20 秒
    pub sla_target_pct: f32,         // 如 80.0%
    pub answered_within_sla: u64,
}
```

统计数据定期持久化到 CC App（通过 Event Webhook 或 CDR Hook）。

#### 12.4.4 多级溢出链

队列配置增加多级溢出规则：

```toml
[[overflow]]
condition = { wait_time_secs = 120 }
action = { redirect_queue = "support-tier2" }

[[overflow]]
condition = { wait_time_secs = 300 }
action = { redirect_queue = "support-escalation" }

[[overflow]]
condition = { wait_time_secs = 600 }
action = { voicemail = true }

[[overflow]]
condition = { queue_size = 50 }
action = { redirect_queue = "overflow-pool" }
```

防溢出循环：`QueueEntry` 记录 `visited_queues` 和 `overflow_count`，超过 5 次强制走最终处理（语音信箱或挂断）。

#### 12.4.5 分配策略扩展

当前仅有 Sequential/Parallel，需增加 CC 场景常用策略：

| 策略 | 说明 | 实现要点 |
|------|------|---------|
| LongestIdle | 最长空闲的坐席优先 | 跟踪每个坐席的最后通话结束时间 |
| RoundRobin | 轮询（带持久化游标） | queue 级别记录 last_assigned_agent |
| LeastCalls | 今日通话数最少优先 | 需要统计坐席当日已完成通话数 |
| SkillFirst | 技能匹配度最高的优先 | 按 `skill_score(agent, required)` 排序 |
| WeightedRandom | 加权随机（技能权重 + 负载权重） | 随机但倾向低负载高技能坐席 |

策略选择在 CC App 的 Routing Orchestrator 中执行，通过 RWI originate 指令让 rustpbx 执行振铃。

### 12.5 改造优先级与依赖关系

```
P0（链路闭环）
  ├── 12.2.1 统一 Queue 架构 ──────────────┐
  ├── 12.2.2 IVR → Queue 上下文透传 ────────┤  依赖 12.2.1
  └── 12.2.3 通用事件推送 Webhook ──────────┘  独立

P1（体验与调度）
  ├── 12.3.1 坐席动态登录/登出队列 ──────── 依赖 12.2.1
  ├── 12.3.2 位置播报 ──────────────────── 依赖 12.2.1 + on_timer
  ├── 12.3.3 排队中 DTMF 交互 ───────────── 依赖 12.2.1
  ├── 12.3.4 Whisper 公告 ──────────────── 依赖 12.2.1 + 单腿播放
  ├── 12.3.5 回呼功能 ──────────────────── 依赖 12.2.3 + CC App
  ├── 12.3.6 通后评价 IVR ──────────────── 依赖 12.2.3 + CC App
  └── 12.3.7 IVR 高峰前置判断 ──────────── 依赖 12.2.2 + Webhook

P2（高级能力）
  ├── 12.4.1 VIP 条件路由 ──────────────── 依赖 12.3.7
  ├── 12.4.2 优先级动态调整 ─────────────── 依赖 12.2.1 + on_timer
  ├── 12.4.3 统计持久化与 SLA ──────────── 依赖 12.2.3
  ├── 12.4.4 多级溢出链 ────────────────── 依赖 12.2.1
  └── 12.4.5 分配策略扩展 ──────────────── 依赖 12.3.1
```

### 12.6 实施建议

**阶段一（1 周）：P0 链路闭环**

1. 统一三套 Queue 为一套调度流程。
2. `EntryAction::Queue` 携带上下文透传到 QueueManager。
3. 新增 Event Webhook 配置和推送逻辑。
4. 验收：IVR 按键 → 入队（带上下文）→ 匹配坐席 → originate → 接通，全程事件可追踪。

**阶段二（1-2 周）：P1 排队体验**

1. 坐席动态登录/登出。
2. 位置播报 + DTMF 交互。
3. Whisper 公告。
4. 回呼收集 + 评价 IVR。
5. 验收：客户排队时可听到位置、按 DTMF 交互；坐席接听时听到 Whisper；挂断后评价流程触发。

**阶段三（1 周）：P2 高级能力**

1. VIP 条件路由。
2. 多级溢出链。
3. 分配策略扩展（LongestIdle / RoundRobin / LeastCalls）。
4. 统计与 SLA。
5. 验收：SLA 指标可实时查看；VIP 客户跳过 IVR；溢出链跨 3 个队列可追踪。

## 13. 新增方案：CC Addon + CC App(Go) + CC Phone(Vue/JS)

本节将“cc addon + cc app + cc phone”的构想整理为可落地需求与 workflow，作为第 12 节之后的实施蓝图。

### 13.1 目标与分层

目标：在保留 rustpbx 作为通信底座的前提下，新增三层协作架构。

1. CC Addon（Rust，运行于 rustpbx 侧）
- 负责与 CC App 的调度桥接。
- 负责 IVR/Queue/坐席状态/Skill Group 的内核侧执行与同步。
- 负责 RWI 与媒体侧能力封装（含 PCM 流输出）。

2. CC App（Go，独立部署）
- 负责 phone/queue/ivr/presence/skill group 配置管理。
- 负责坐席管理、监控、调度策略、联调测试与验证闭环。
- 负责接收 PCM 流并调用 ASR，再向 CC Phone 推送 in-dialog message。

3. CC Phone（Vue + JS，坐席前端）
- 负责坐席软电话 UI 与会话交互。
- 以 in-dialog message 为核心扩展通道，发起业务动作（如 3PCC 请求）。
- 与 PBX/CC Addon 协同完成通话期内功能编排。

### 13.2 角色职责与边界

1. PBX + CC Addon
- 继续作为 SIP/媒体执行面。
- 保证呼叫控制原子性与状态一致性。
- 提供队列调度执行、通话监听、接管、RWI 事件输出。

2. CC App（控制面）
- 统一配置、策略、监控、审计。
- 统一调度意图与策略决策（IVR 分流、Queue 选路、Skill 匹配、溢出策略）。
- 统一 AI/ASR 接入编排，不侵入 PBX 媒体主线程。

3. CC Phone（交互面）
- 表达坐席业务意图，不直接持有底层 SIP 策略细节。
- 接收 in-dialog message 渲染弹屏、ASR 文本、调度提示。
- 发起带 request_id 的控制命令，支持幂等重试。

### 13.3 功能需求（FR）

FR-1 进线主链路
- 支持 inbound -> IVR -> Queue -> Agent 完整链路。
- 需携带 IVR 上下文（menu_path/skills/priority/variables）进入 Queue。

FR-2 ASR 辅助与 in-dialog message 下发
- PBX/CC Addon 通过 RWI 向 CC App 输出指定 call leg 的 PCM 音频流。
- CC App 调用 ASR 服务得到增量/最终识别文本。
- CC App 将识别结果通过 in-dialog message 下发到 agent leg（弹屏/建议话术/标签）。

FR-3 坐席发起 3PCC
- 坐席通过 CC Phone 的 in-dialog message 发起 3PCC 请求。
- 由 CC App 执行权限校验与状态机校验后，调用 Addon/RWI 执行 3PCC。
- 支持失败回滚、取消、重试及统一终态回传。

FR-4 监听与接管
- CC App 支持主管监听（listen）、耳语（whisper）、强插（barge）与接管（takeover）。
- 接管后控制权 owner 必须从 agent_ui 切换为 supervisor_orchestrator，并记录审计。

FR-5 队列调度与坐席状态
- 支持 Ready/NotReady/Break/WrapUp 等 presence 状态与队列联动。
- 支持 Skill Group 匹配、优先级、重分配、超时溢出。
- 支持 CC App 动态调整队列成员与策略，并实时同步到 Addon。

FR-6 配置、监控、测试验证
- CC App 提供 phone/queue/ivr/presence/skill group 管理界面与 API。
- 提供全链路测试入口：模拟来电、IVR 选择、排队、坐席应答、转接、监听、接管。
- 提供验证报告：成功率、失败原因、时延、事件完整性。

### 13.4 非功能需求（NFR）

1. 一致性
- 单 call_id 任一时刻仅允许一个控制 owner 写操作。
- 所有命令必须包含 request_id/action_id，保证幂等。

2. 实时性
- ASR 增量文本端到端延迟目标：P95 <= 800ms（可按网络环境调优）。
- in-dialog message 下发与前端呈现需支持乱序纠正（sequence）。

3. 稳定性
- ASR 或 CC App 异常不得影响 PBX 基础通话。
- 音频流传输故障时自动降级为“无 ASR”模式，主通话不受阻。

4. 可观测性
- 指标：队列等待、转接成功率、3PCC 失败率、监听接管次数、ASR 时延与错误率。
- 日志：命令链路日志 + 事件日志 + 审计日志可关联同一 call_id/session_id。

5. 安全性
- in-dialog message 与控制 API 必须鉴权与签名校验。
- 接管、强插、导出录音等高风险操作必须 RBAC + 审计。

### 13.5 关键 Workflow

#### Workflow A：进线 -> IVR -> Queue -> Agent + ASR 提示

1. 来电进入 PBX，CC Addon 根据策略将呼叫送入 IVR。
2. IVR 收号后将目标队列与上下文传给 Queue 调度。
3. Queue 选中坐席并发起振铃，坐席应答后建立通话。
4. CC Addon 将 agent/customer leg PCM 流（可配置单向或双向）通过 RWI 推送给 CC App。
5. CC App 调用 ASR 并将识别结果（partial/final）以 in-dialog message 下发给 CC Phone。
6. CC Phone 展示实时字幕/提示，辅助坐席沟通。

#### Workflow B：坐席通过 in-dialog message 发起 3PCC

1. 坐席在 CC Phone 点击“咨询第三方/三方通话”，发送 in-dialog message（含 call_id、target、request_id）。
2. CC App 校验权限、状态与并发锁（owner）。
3. CC App 调用 Addon/RWI 执行 3PCC（originate -> consult -> merge）。
4. 执行结果事件回传至 CC App，再推送给 CC Phone 统一终态（success/failed/cancelled）。
5. 失败时执行回滚（恢复原桥接）并附 reason_code。

#### Workflow C：主管监听/接管与队列调度

1. 主管在 CC App 选择目标通话执行 listen/whisper/barge/takeover。
2. CC App 调用 Addon/RWI 下发指令并记录审计。
3. takeover 成功后，控制 owner 切换并广播给 CC Phone。
4. CC App 可实时调整队列策略或坐席状态，Addon 执行并回传事件。

### 13.6 关键接口建议（最小集合）

1. Addon <-> CC App（控制面）
- `queue.enqueue/dequeue/requeue`
- `agent.presence.update`
- `skill_group.sync`
- `call.listen/whisper/barge/takeover`
- `call.3pcc.start/cancel/complete`

2. Addon -> CC App（媒体与事件）
- `media.pcm.stream.start`
- `media.pcm.stream.chunk`
- `media.pcm.stream.stop`
- `call.lifecycle.*`
- `queue.*`

3. CC App -> CC Phone（in-dialog message）
- `asr.partial`
- `asr.final`
- `dispatch.suggestion`
- `call.control.result`
- `supervisor.notice`

### 13.7 实施顺序（建议）

1. M1：基础链路
- 打通 inbound -> IVR -> Queue -> Agent。
- 打通 Addon 与 CC App 的事件同步。

2. M2：ASR + in-dialog message
- 打通 PCM 流到 CC App。
- 实现 ASR partial/final 消息推送到 CC Phone。

3. M3：3PCC 与失败回滚
- 坐席发起 3PCC。
- 完成状态机、幂等、回滚与可观测。

4. M4：监听/接管 + 调度增强
- 主管操作闭环。
- 动态队列策略与 skill group 调度。

5. M5：测试与验收
- 建立自动化 E2E 场景：进线、排队、ASR、3PCC、监听、接管。
- 输出验收报告（成功率、时延、失败归因）。

### 13.8 验收标准（DoD）

1. 进线到坐席接通链路可稳定复现并可观测。
2. ASR 文本可实时送达 CC Phone，异常时可自动降级。
3. 坐席可通过 in-dialog message 成功发起 3PCC，失败可回滚。
4. CC App 可完成监听/接管与队列调度，审计链路完整。
5. 全链路压测下，主通话稳定性不因 ASR/调度波动而显著下降。