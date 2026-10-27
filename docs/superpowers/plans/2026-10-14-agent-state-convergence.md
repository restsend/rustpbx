# 多节点坐席状态一致性实施计划

> 用户已授权完整设计和修复，并明确要求 root 和 CC 子仓库分别使用隔离 worktree。采用 TDD；开发和测试保持隔离。用户现已明确授权将 Root 与 CC 的改动本地合入 lf_dev；不推送、不部署或重启原服务。

**目标：** 修复转接后结束事后处理回跳，并验证相关状态切换。

**设计：** [设计文档](../specs/2026-10-14-agent-state-convergence-design.md)。现有坐席行事务锁下的共享状态与历史提交是状态提交点，SIP 是已提交状态的投影。

**技术：** Rust 2024、Tokio、SeaORM、Axum、现有 CC addon 和 AMI hub。

## 约束

- 保留厂商迁移及正常状态机，不修改无关代码或格式。
- 不自动执行数据库 DDL；保持所有服务 `--skip-migrate`。
- 所有新增数据为中性测试数据；文档使用中文。
- 每个源码版本只进行一次兼容测试批次，使用 `RUST_MIN_STACK=8388608 cargo test-fast`。

## 任务 1：真实入口回归与 RED

文件：`src/addons/cc/tests/agent_state_convergence_tests.rs` 和已有状态测试。

- [x] 建立两 registry 共享数据库的转接离席、手动空闲、延迟旧挂机回归；断言最后状态为 `idle`、并发为 0、没有第二次 wrapup 历史。
- [x] 真实 Axum 状态入口和 hub 复现远端无版本 Presence 污染、重复空闲请求不修正 SIP 投影。
- [x] 覆盖旧 wrapup 定时器、新通话绑定、同秒竞争及数据库写失败。
- [x] 运行 `RUST_MIN_STACK=8388608 cargo test-fast agent_state_convergence`，确认实际执行并失败于目标行为。

## 任务 2：持久化和条件写入

文件：`src/addons/cc/models/cc_agent_status_history.rs`、`stats_writer.rs`、`agent.rs`。

- [x] 使用现有 `cc_agents` 行的无变化 UPDATE 取得事务写锁，保持全部数据库字段和迁移不变。
- [x] 状态机写入前加载最新共享状态和绑定；在事务锁下更新共享状态与历史，在发布前完成持久化。错误时不提交候选本地状态。
- [x] 预留与呼叫数更新复用真实提交结果，旧呼叫自动写入在最新绑定上校验。
- [x] 共享快照加载、恢复和集群同步使用已提交历史顺序与真实绑定；不通过旧永久本地所有权拒绝有效最新状态。

## 任务 3：SIP 投影与 GREEN

文件：`src/proxy/cluster_event.rs`、`src/addons/cc/registrar_bridge.rs`、`src/addons/cc/mod.rs`、`agent.rs`。

- [x] 增加默认允许的远端 Presence 入站校验，CC 管理身份由权威快照投影，普通 SIP 保持原行为。
- [x] 本地提交、有效集群状态及重复手动请求在 transition lock 下投影；删除异步 RWI 旧事件再投影路径。
- [x] 运行同一 `agent_state_convergence` 测试批次，确认 GREEN。

## 任务 4：回归、发布依据与运行验证

- [x] 检查所有 touched diff；运行完整 CC 状态相关与 core presence/cluster 回归，检查实际数量、失败与忽略项。
- [x] 核对没有 DDL 或迁移变更，说明现有表的事务锁权限与协调升级顺序，区分已验证后端。
- [x] 记录原服务和隔离服务运行状态；原服务保持运行，仅在隔离目录通过 `service-call/build.sh` 构建。
- [x] 成功后立即在持久 PTY 中 `service-call/output/control.sh start`，验证进程、HTTP/SIP 监听和无致命启动错误；隔离服务使用独立端口、日志和数据库，避免影响原服务。
- [x] 汇报实际修复、验证和部署边界，完成 goal。

## 验证记录

- root 基线：81f023fc3c1862978b847281cea207b7804d4df1；CC 基线：9328753b535b479c7460a59261bea916ee99111d。其余递归子模块与主仓库记录引用一致，未改动。
- 最初六项核心回归 RED 全部失败，后续针对被实际发现的边界补充 RED/GREEN；最终 18 项核心回归全部通过。
- 最终源码：CC 1,083 项通过、2 项忽略；core presence/cluster 110 项通过、0 项失败；合计 1,193 项通过、2 项忽略。
- SQLite 实际独立连接、--skip-migrate 启动以及修改前后 schema signature 比较通过。
- MySQL 8.0.46：真实两个独立连接并发抢占、状态历史、运行态及旧定时器保护通过；测试前后列定义完全相同。
- PostgreSQL 16：基线迁移尝试删除缺失旧表会使事务进入 aborted；准备旧表后，仍在既有 cc_agents.created_at 的 TIMESTAMP/TIMESTAMPTZ 解码失败。未更改厂商迁移或字段类型，因此本次不宣称 PostgreSQL 通过，也不要求 MySQL 部署做 DDL。
- 最终日志：/tmp/agent-state-convergence-cc-green-final.log、/tmp/agent-state-convergence-core-final.log、/tmp/agent-state-convergence-mysql-final.log。
- 正式 service-call 构建成功（8 分 34 秒）。隔离服务在持久 PTY 中启动，CC 初始化成功；HTTP 19088 返回 200、SIP UDP 19062 监听正常，进程持续存活，启动日志无致命错误。启动前后的 87 个 schema 对象完全相同，--skip-migrate 生效。验证后关闭隔离测试服务；原仓库与原运行服务保持不变。

## 本地合并

按用户明确授权，将 CC 源码与测试提交到 CC 修复分支后快进合入 CC 的 lf_dev，再将 Root 源码、设计记录和新的 CC 引用提交并快进合入 Root 的 lf_dev。合并前重新执行同一版本的 1,193 项相关回归；合并后核对源文件逐字节一致、Root 子模块引用与 CC 提交一致，并复跑核心状态回归。没有执行 RDS 写入、DDL、推送或部署。
