# Rolling Handover — 滚动发布操作手册

无中断滚动升级：新容器（successor）先起并等待端口，`docker stop` 老容器
（predecessor）触发交接而非普通排水。存量通话在老进程跑完自然结束；期间
UDP in-dialog 信令经 loopback HTTP 在两代之间双向摆渡；WS 客户端被限速
驱逐后秒级重连落到新代。

## 镜像默认支持（cc / commerce / wholesale / community）

自 core `489095f8` 起，**所有镜像默认启用滚动交接**（`[rolling]` 缺省
`enabled = true`），零配置即得：

- **单实例部署**：启动时自识别（hello 回包带自身 PID）——SIGTERM 走
  普通排水，**行为与非滚动部署完全一致**，零额外开销。
- **滚动对部署**：先起新容器再 `docker stop` 老容器 → 交接自动发生。
- 显式退出：config 加 `[rolling] enabled = false`。
- 三个产品 example 配置（`config/{cc,wholesale,commerce}.example.toml`）
  已带说明段。

唯一行为差异提醒（默认启用后）：HTTPS/WSS 走手工 TLS accept 循环（为
关监听保留存量连接所必需），证书文件更新后需重启进程生效（滚动发布本身
即自然的证书更新点）；依赖 ACME 热加载的部署可 `[rolling] enabled=false`
回到 axum_server 路径。

## 操作时序

```
1. 发布新镜像（配置变更点见下节）
2. 启动新容器：
     docker run ... -e RUSTPBX_CONF=/app/config.toml rustpbx:<new>
   - `[rolling] enabled = true`（新旧镜像共用同一 config.toml）
   - 新容器公共端口绑定失败 → 200ms 重试，无限等待（hello 心跳维持
     前驱的就绪判定），日志 `rolling: waiting for predecessor ...`
3. 停老容器：
     docker stop -t 3700 <old>
   - SIGTERM 在 rolling 模式下转义为交接（不再走普通排水）
   - `docker stop -t` 必须 > `[graceful_shutdown].drain_timeout_secs`
     （默认 3600s），否则 SIGKILL 会把正在排水的老进程打死、排水保证
     全部作废
4. 观察日志：
     - 旧：`rolling: fresh successor heartbeat — starting handover`
     - 旧：`rolling: UDP transport switched to loopback relay`
     - 旧：`rolling: public listeners closed (Xms)`
     - 新：`rolling: successor owns the public listeners — takeover complete`
     - 旧：`rolling: handoff manifest accepted by successor`
     - 旧：`rolling: handover window complete (Xs)` → 容器退出
5. 移除旧容器（docker 自动），发布完成
```

## 配置（[rolling]）

**缺省即启用**（见上节）——本节仅调优。显式配置示例（cc 镜像的
`config/cc.example.toml`，wholesale/commerce 同形）：

| 字段 | 默认 | 说明 |
|---|---|---|
| `enabled` | false | 滚动交接总开关（新旧共用同一 config） |
| `predecessor_port` | 18181 | 交接窗口期老进程的 loopback 管理监听口。**必须**与同机其它节点/容器的端口不冲突（含集群另一节点的 AMI 口） |
| `bind_wait_secs` | 60 | 交接宣布后 successor 最终绑定的重试预算（之前的空闲等待无上限） |
| `handoff_registrations` | false | 注册表交接（memory locator 单机部署开；DB locator 部署无需） |
| `hello_interval_secs` | 2 | successor → predecessor 心跳间隔；新鲜窗口 = 3× |

## 窗口期行为（什么会变、什么不会）

**不受影响**：
- 存量通话：信令与 RTP 全程在老进程，BYE/re-INVITE 正常（UDP 经中继），
  媒体端口不动
- 新呼入/新外呼：交接完成后立即由新进程承接（亚秒级端口缝隙由 SIP
  强制重传吸收）
- AMI：`/ami/v1` 常驻；对老代资源的操作（hangup、cluster session op、
  userdata/var）由新进程自动代理到老进程
- 队列（ACD）：老代队列的等待者继续被老代坐席服务；**队列有等待者期间
  坐席连接不被驱逐**（`active_call_registry` 的 queue 上下文门控）

**有界的临时变化**（窗口结束即消失）：
- WS/WSS 坐席连接被限速驱逐（每 2s 一轮、轮内 ≤20 连接、连接间 50–200ms
  抖动）→ 坐席重连落新进程并重新 REGISTER；正在通话中的坐席连接保留到
  通话结束
- SIP-over-TCP 话机**不会被驱逐**（rsipstack 无 per-conn API）：其注册
  保留在老代，窗口期新呼入可能路由不到它们；老进程退出后连接断开，
  话机重连落新进程
- 窗口期老进程自身发起的外呼（trunk 方向）经新进程 socket 发出，源
  ip:port 不变，运营商无感知
- 队列清空后才开始坐席迁移：新呼入在新代排队等待首批坐席迁入

**已知限制**：
- 不支持窗口重叠的级联交接（gen2 排水中收到新 SIGTERM → 安全降级为
  join drain）
- 列表类 AMI 查询（如活跃通话列表）不合并两代结果；需要全量时直接查
  `predecessor_port` 上的老进程
- 窗口期任一进程崩溃：等价于今天的容器重启行为（successor 依赖 docker
  restart policy 拉起重绑端口）

## 回滚

发布后发现新版本问题：
1. 新容器有问题但老容器还在排水窗口内：`docker restart <new>`（restart
   policy 拉起后重新绑定端口，窗口继续）
2. 需要退回旧版本：对**旧镜像再走一次滚动**（新容器用旧镜像 + 同一
   `[rolling]` 配置）；`drain_timeout_secs` 内老进程（当前一代）的通话
   同样受保护

## 前置条件与红线

- **同宿主机 host 网络**：rolling 控制通道是 loopback HTTP（127.0.0.1）
- `predecessor_port` 默认 18181：**确认**不与集群其它节点的 SIP/AMI/RTP
  端口冲突（例：call 节点 AMI=18081 / agent 节点 AMI=18082 的双机拓扑，
  18181 安全）
- `docker stop -t` > `drain_timeout_secs`（+60s 余量），默认配置下
  `-t 3700`
- WS 证书（WSS）新旧容器挂载同一证书文件
- RTP 端口段两代**必须错开**（config `rtp_start_port/rtp_end_port`，
  例：老 22000-22999 / 新 23000-23999），否则媒体端口冲突
- 发布节奏：一次一个窗口；上一窗口（老进程退出）完成前不要触发下一次

## 排障速查

| 症状 | 原因 | 处置 |
|---|---|---|
| SIGTERM 后走普通排水（`falling back to plain drain`） | successor 未启动或心跳过期 | 先起新容器再 stop 老容器 |
| 新容器一直 `waiting for predecessor` | 老容器未收到 stop / 端口被第三方占用 | 检查 docker ps 与端口占用 |
| `egress POST failed after retry` | successor 未起或已退出 | restart policy / 检查 successor |
| BYE 挂不断、`relay_ingress` 200 但无响应 | manifest 未被 successor 接收（老版本 bug 已修，带 `Connection: close`） | 升级到含本修复的版本 |
| AMI hangup 404 空 body | 转发 URL 缺少 `/ami/v1` 前缀（旧版本 bug 已修） | 同上 |
