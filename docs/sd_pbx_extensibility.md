# RustPBX 可扩展性指南：构建 SD-PBX

RustPBX 旨在成为下一代 **SD-PBX (Software Defined PBX)**，其核心设计理念是**开发者友好**和**无需编写插件代码**。通过丰富且标准化的 Webhook、REST API 和事件机制，开发者可以仅通过 HTTP 协议就与现有业务系统（如 CRM、ERP、客服系统、工单系统）实现深度集成。

本文档详细梳理了现有的可扩展能力，并规划了未来实现完全 SD-PBX 所需的扩展点。

---

## 📚 1. 现有可扩展能力 (Current Capabilities)

目前 RustPBX 已经内置了多个关键的 HTTP 扩展点，覆盖了呼叫路由、用户认证、CDR 推送等核心场景。

### 1.1 呼叫路由控制 (Call Routing)
**场景**：当电话呼入时，由外部业务系统决定如何处理（是转给分机、进入排队、还是拒绝）。

*   **机制**：`http_router`
*   **触发时机**：收到 SIP INVITE 请求时。
*   **调用方式**：同步 HTTP POST
*   **配置**：
    ```toml
    [proxy.http_router]
    url = "https://your-api.com/route"
    fallback_to_static = true
    timeout_ms = 5000
    ```
*   **能力**：
    *   接收：主叫/被叫号码、SIP 头域、源 IP 等。
    *   控制：**转发** (Sequential/Parallel)、**拒绝**、**添加 SIP 头**、**开启录音**、**设置超时**。

### 1.2 用户认证与鉴权 (Authentication)
**场景**：使用现有的用户中心（如 OAuth、LDAP、数据库）验证 SIP 注册或呼叫请求，无需在 PBX 维护密码。

*   **机制**：`UserBackend::Http`
*   **触发时机**：SIP REGISTER 或 INVITE 需要鉴权时。
*   **调用方式**：同步 HTTP POST/GET
*   **配置**：
    ```toml
    [[proxy.user_backends]]
    type = "http"
    url = "https://your-api.com/auth"
    ```
*   **能力**：
    *   接收：用户名、Realm、SIP Payload。
    *   控制：返回哈希后的密码或明文密码、ACL 权限、账户状态（禁用/欠费）。

### 1.3 状态与位置同步 (Locator Webhook)
**场景**：当分机注册/注销时，通知业务系统更新状态（如“客服上线”、“话机离线”）。

*   **机制**：`locator_webhook`
*   **触发时机**：SIP 注册成功、注销、失效（Expire）时。
*   **调用方式**：异步 HTTP POST (Fire-and-forget)
*   **配置**：
    ```toml
    [proxy.locator_webhook]
    url = "https://your-api.com/sip-events"
    events = ["registered", "unregistered", "offline"]
    ```
*   **能力**：推送 AOR、IP 地址、User-Agent、过期时间等。

### 1.4 CDR 与录音推送 (Data Push)
**场景**：通话结束后，将话单（CDR）和录音文件推送到归档系统或计费系统。

*   **机制**：`callrecord`
*   **触发时机**：通话结束且处理完录音后。
*   **调用方式**：异步 HTTP Multipart POST 或 S3 Upload
*   **配置**：
    ```toml
    [callrecord]
    type = "http"
    url = "https://your-api.com/cdr"
    with_media = true
    ```
*   **能力**：推送完整话单 JSON（包含时间、状态、挂断原因、SIP 路由信息）及录音文件（WAV/MP3）。

### 1.5 实时呼叫控制 (Active Call Control API)
**场景**：在通话进行中，由业务通过 API 干预通话（如点击挂断、强插、强拆、静音）。

*   **机制**：REST API `/console/calls/active/{id}/commands`
*   **能力**：
    *   `hangup`: 挂断指定通话
    *   `transfer`: 盲转到特定号码
    *   `mute/unmute`: 静音/解除静音
    *   `accept`: 强制接听（配合自动应答）

### 1.6 管理与运维 API (Management API)
**场景**：通过代码自动化管理分机、中继、路由规则。

*   **机制**：REST API `/console/*`
*   **能力**：
    *   CRUD 分机、中继、路由规则
    *   查询/下载历史录音与 CDR
    *   热重载配置
    *   查询系统状态与并发数

---

## 🚀 2. SD-PBX 演进路线：缺失的扩展点 (Missing Capabilities)

要实现完全的 **SD-PBX**（即开发者可以将 RustPBX 仅仅作为一个无状态的“电话能力网关”，通过 HTTP 编排所有业务逻辑），目前还需要补充以下核心能力。这些将是后续开发的重点。

### 2.1 呼叫应用控制协议 (Call Control Protocol / NCCO)
目前 `http_router` 只能决定“转给谁”，无法控制通话过程中的行为（如放音、收号、ASR、TTS）。我们需要引入类似 **NCCO (Nexmo Call Control Object)** 或 **TwiML** 的响应格式。

*   **目标**：当 `http_router` 返回时，允许返回一组**执行指令**，而不仅仅是路由目标。
*   **示例响应**：
    ```json
    {
      "action": "answer",
      "steps": [
        { "action": "play", "url": "https://voice.com/welcome.mp3" },
        { "action": "input", "maxDigits": 4, "timeout": 10, "eventUrl": "/dtmf" },
        { "action": "connect", "endpoint": { "type": "sip", "uri": "sip:101@domain" } }
      ]
    }
    ```
*   **涵盖能力**：
    *   `play`: 播放音频（支持 URL 缓存）
    *   `say`: TTS 文本转语音（集成云厂商 API）
    *   `input`: 采集 DTMF 按键
    *   `record`: 开始/停止实时录音
    *   `conference`: 加入会议室

### 2.2 实时事件流 (Real-time Event Stream)
目前的 Webhook 大多是单次请求/响应。对于复杂的交互（如 AI 语音助手、实时质检），需要全双工的实时事件流。

*   **建议方案**：基于 WebSocket 的 **Event Socket** (参考 FreeSWITCH ESL)。
*   **功能**：
    *   订阅特定的事件类别（如 `CHANNEL_CREATE`, `DTMF`, `MEDIA_START`）。
    *   异步接收事件推送。
    *   通过 Socket 发送异步命令控制通话。

### 2.3 媒体流注入与导出 (Media Streaming / Audio Forking)
为了支持实时 AI（语音机器人、实时翻译），需要将通话后的音频实时流式传输给外部服务。

*   **建议方案**：基于 WebSocket 的音频流接口 (类似 Twilio Media Streams)。
*   **功能**：
    *   将通话双方的音频（L16/8000Hz）通过 WebSocket 发送到指定 URL。
    *   允许外部服务将音频流注入回通话中（实现 AI 对话）。

### 2.4 主动外呼 API (Outbound Call API)
目前可以通过路由呼出，但缺乏一个面向应用的“点击拨号”或“通知外呼”的标准 API。

*   **建议方案**：`POST /api/v1/calls`
*   **参数**：
    *   `to`: 目标号码
    *   `from`: 主叫显号
    *   `answer_url`: 对方接通后回调的 webhook 每一跳控制流程。

### 2.5 状态机 (Web-based IVR)
允许开发者配置一个 Web 页面作为 IVR 的状态机，每一跳状态变化都回调该页面，实现无限状态的业务逻辑。

---

## 🛠 3. 总结

RustPBX 已经具备了作为**高性能 SIP 网关**的通过 HTTP 扩展的基础能力。要进化为完全的 **SD-PBX**，接下来的核心工作是实现 **Call Application Framework**（即上述的 2.1 和 2.4），将内部的 SIP 状态机解耦，通过标准 JSON 指令集暴露给 HTTP 开发者。

这将彻底消除开发者编写 Rust 插件的需求，使 RustPBX 成为真正通用的通信基础设施。
