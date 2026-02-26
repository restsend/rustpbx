# RustPBX Call Application Framework

**版本**: 2.0  
**日期**: 2026-02-26  
**状态**: 部分实现（框架层完成，CallSession 集成待实现）

---

## 📋 概述

本文档描述了 RustPBX 的**统一呼叫应用框架（Call Application Framework）**。该框架为 Voicemail、IVR、Conference、Queue 等呼叫应用提供统一抽象，是对 `proxy_call` 层能力的高层封装。

### 设计目标

1. **统一抽象**：所有呼叫应用使用相同的接口和编程模型
2. **复用底层能力**：充分利用现有的 `proxy_call` 层能力（`MediaPeer`、`Recorder`、`FileTrack`）
3. **易于扩展**：新增功能无需修改核心代码
4. **测试友好**：每个应用可独立测试

---

## 🏗️ 架构分层

```text
┌──────────────────────────────────────────────────────────────┐
│  Application Layer (应用层)                                   │
│  ┌─────────┬──────┬─────────┬───────────┬──────────────────┐ │
│  │Voicemail│ IVR  │Conference│   Queue   │  Custom Apps     │ │
│  └─────────┴──────┴─────────┴───────────┴──────────────────┘ │
│                    ↑ implements CallApp trait                 │
└──────────────────────────────────────────────────────────────┘
                              ▲
                              │
┌──────────────────────────────────────────────────────────────┐
│  Call Application Framework (框架层)                          │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ CallController - 统一的呼叫控制 API                     │  │
│  │  • play_audio()      • collect_dtmf()                  │  │
│  │  • start_recording() • hangup()                        │  │
│  └────────────────────────────────────────────────────────┘  │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ ApplicationContext - 应用上下文                        │  │
│  │  • session_vars      • shared_state                   │  │
│  │  • db                • storage                        │  │
│  └────────────────────────────────────────────────────────┘  │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ AppEventLoop - 应用事件循环                            │  │
│  │  • 事件分发          • 状态管理                       │  │
│  └────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
                              ▲
                              │ sends SessionAction
┌──────────────────────────────────────────────────────────────┐
│  Proxy Call Layer (proxy_call 层)                            │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ CallSession (App Mode) - 单腿会话管理                  │  │
│  │  • Dialog 管理       • AppMediaEventPump              │  │
│  │  • SDP 协商          • FileTrack/RtpTrack             │  │
│  └────────────────────────────────────────────────────────┘  │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ SessionAction 处理器                                   │  │
│  │  • AcceptCall        • PlayPrompt                     │  │
│  │  • Hangup            • StartRecording                 │  │
│  └────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
                              ▲
                              │ operates on
┌──────────────────────────────────────────────────────────────┐
│  SIP/Media Foundation (底层基础设施)                          │
│  • DialogLayer       • MediaPeer (VoiceEnginePeer)           │
│  • AudioSource       • Recorder                              │
│  • FileTrack         • (No MediaBridge in App Mode)          │
└──────────────────────────────────────────────────────────────┘
```

---

## 🎯 核心组件设计

### 1. CallApp Trait - 应用接口

所有呼叫应用都必须实现此 trait：

```rust
#[async_trait]
pub trait CallApp: Send + Sync {
    /// 应用类型标识
    fn app_type(&self) -> CallAppType;
    
    /// 应用名称（用于日志和调试）
    fn name(&self) -> &str;
    
    /// 应用初始化（呼叫被路由到此应用时）
    async fn on_enter(
        &mut self, 
        controller: &mut CallController,
        context: &ApplicationContext
    ) -> Result<AppAction>;
    
    /// 处理 DTMF 输入
    async fn on_dtmf(
        &mut self,
        digit: String,
        controller: &mut CallController,
        context: &ApplicationContext
    ) -> Result<AppAction>;
    
    /// 处理音频播放完成事件
    async fn on_audio_complete(
        &mut self,
        track_id: String,
        controller: &mut CallController,
        context: &ApplicationContext
    ) -> Result<AppAction>;
    
    /// 处理录音完成事件
    async fn on_record_complete(
        &mut self,
        path: String,
        duration: Duration,
        controller: &mut CallController,
        context: &ApplicationContext
    ) -> Result<AppAction>;
    
    /// 处理外部事件（如 HTTP 回调、定时器、会议事件等）
    async fn on_external_event(
        &mut self,
        event: AppEvent,
        controller: &mut CallController,
        context: &ApplicationContext
    ) -> Result<AppAction>;
    
    /// 处理超时事件
    async fn on_timeout(
        &mut self,
        timeout_id: String,
        controller: &mut CallController,
        context: &ApplicationContext
    ) -> Result<AppAction>;
    
    /// 应用退出清理
    async fn on_exit(&mut self, reason: ExitReason) -> Result<()>;
}
```

#### 应用动作（AppAction）

```rust
pub enum AppAction {
    /// 继续当前应用
    Continue,
    
    /// 退出应用（进入下一个路由阶段）
    Exit,
    
    /// 转移到其他目标
    Transfer(Location),
    
    /// 链接到下一个应用（应用链）
    Chain(Box<dyn CallApp>),
    
    /// 挂断呼叫
    Hangup {
        reason: Option<CallRecordHangupReason>,
    },
    
    /// 等待指定时间后重新调用当前应用
    Sleep(Duration),
}
```

#### 应用类型

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallAppType {
    Voicemail,
    Ivr,
    Conference,
    Queue,
    Custom,
}
```

---

### 2. CallController - 统一的呼叫控制 API

`CallController` 是应用层与 `proxy_call` 层之间的桥梁，提供高层次的呼叫控制原语。

#### 结构定义

```rust
pub struct CallController {
    session_handle: CallSessionHandle,
    session_id: String,
    action_tx: SessionActionSender,
    event_rx: mpsc::UnboundedReceiver<ControllerEvent>,
    playback_state: Arc<RwLock<PlaybackState>>,
    recording_state: Arc<RwLock<RecordingState>>,
    variables: HashMap<String, String>,
}
```

#### API 分类

##### 基础控制

```rust
impl CallController {
    /// 接听呼叫（发送 200 OK）
    pub async fn answer(&mut self) -> Result<()>;
    
    /// 挂断呼叫
    pub async fn hangup(&mut self, reason: Option<CallRecordHangupReason>) -> Result<()>;
    
    /// 获取呼叫信息
    pub fn call_info(&self) -> &CallInfo;
    
    /// 检查呼叫是否仍然活跃
    pub fn is_active(&self) -> bool;
}
```

##### 音频播放

```rust
impl CallController {
    /// 播放音频文件
    /// 
    /// # 参数
    /// - `file`: 音频文件路径（支持 WAV/MP3/HTTP URL）
    /// - `interruptible`: 是否可被 DTMF 中断
    /// 
    /// # 返回
    /// `PlaybackHandle` 用于控制播放（暂停/停止/查询状态）
    pub async fn play_audio(
        &mut self,
        file: String,
        interruptible: bool,
    ) -> Result<PlaybackHandle>;
    
    /// 停止当前播放
    pub async fn stop_audio(&mut self) -> Result<()>;
    
    /// 切换音频源（无缝切换，无需重新协商 SDP）
    pub async fn switch_audio(&mut self, file: String) -> Result<()>;
    
    /// 播放多个音频文件（顺序播放）
    pub async fn play_sequence(
        &mut self,
        files: Vec<String>,
    ) -> Result<PlaybackHandle>;
    
    /// 循环播放音频
    pub async fn play_loop(
        &mut self,
        file: String,
        max_loops: Option<usize>,
    ) -> Result<PlaybackHandle>;
}
```

##### DTMF 收集

```rust
impl CallController {
    /// 收集 DTMF 输入
    /// 
    /// # 配置
    /// - `min_digits`: 最少位数
    /// - `max_digits`: 最多位数
    /// - `timeout`: 超时时间
    /// - `terminator`: 终止符（如 '#'）
    /// - `play_prompt`: 可选的提示音
    /// 
    /// # 返回
    /// 收集到的数字字符串
    pub async fn collect_dtmf(
        &mut self,
        config: DtmfCollectConfig,
    ) -> Result<String>;
    
    /// 等待单个 DTMF
    pub async fn wait_dtmf(&mut self, timeout: Duration) -> Result<Option<String>>;
    
    /// 清空 DTMF 缓冲区
    pub fn clear_dtmf_buffer(&mut self);
}

pub struct DtmfCollectConfig {
    pub min_digits: usize,
    pub max_digits: usize,
    pub timeout: Duration,
    pub terminator: Option<char>, // '#' 或 '*'
    pub play_prompt: Option<String>,
    pub inter_digit_timeout: Option<Duration>,
}
```

##### 录音控制

```rust
impl CallController {
    /// 开始录音
    /// 
    /// # 参数
    /// - `path`: 录音文件保存路径
    /// - `max_duration`: 最大录音时长（None 表示无限制）
    /// - `beep`: 是否播放 beep 提示音
    /// 
    /// # 返回
    /// `RecordingHandle` 用于控制录音
    pub async fn start_recording(
        &mut self,
        path: String,
        max_duration: Option<Duration>,
        beep: bool,
    ) -> Result<RecordingHandle>;
    
    /// 停止录音
    pub async fn stop_recording(&mut self) -> Result<RecordingInfo>;
    
    /// 暂停录音
    pub async fn pause_recording(&mut self) -> Result<()>;
    
    /// 恢复录音
    pub async fn resume_recording(&mut self) -> Result<()>;
}

pub struct RecordingInfo {
    pub path: String,
    pub duration: Duration,
    pub size_bytes: u64,
    pub format: RecordingFormat,
}
```

##### 呼叫转移

```rust
impl CallController {
    /// 盲转（Blind Transfer）
    /// 直接将呼叫转移到目标，无需确认
    pub async fn blind_transfer(&mut self, target: Location) -> Result<()>;
    
    /// 咨询转（Attended Transfer）
    /// 先呼叫目标，确认后再转移
    pub async fn attended_transfer(
        &mut self,
        target: Location,
    ) -> Result<TransferSession>;
    
    /// 取消正在进行的咨询转
    pub async fn cancel_transfer(&mut self) -> Result<()>;
}

pub struct TransferSession {
    pub target: Location,
    pub state: TransferState,
}

pub enum TransferState {
    Dialing,
    Ringing,
    Connected,
    Failed(String),
}
```

##### 会议控制

```rust
impl CallController {
    /// 加入会议室
    /// 
    /// # 参数
    /// - `room_id`: 会议室 ID
    /// - `role`: 会议角色（主持人/参与者）
    /// 
    /// # 返回
    /// `ConferenceHandle` 用于会议控制
    pub async fn join_conference(
        &mut self,
        room_id: String,
        role: ConferenceRole,
    ) -> Result<ConferenceHandle>;
    
    /// 离开会议室
    pub async fn leave_conference(&mut self) -> Result<()>;
}

pub enum ConferenceRole {
    /// 主持人（可静音他人、踢人、锁定会议室）
    Moderator,
    /// 普通参与者
    Participant,
    /// 监听者（只听不说）
    Listener,
}

pub struct ConferenceHandle {
    room_id: String,
    role: ConferenceRole,
    // 会议控制方法...
}
```

##### 静音控制

```rust
impl CallController {
    /// 静音呼叫者（阻止音频发送）
    pub async fn mute(&mut self) -> Result<()>;
    
    /// 取消静音
    pub async fn unmute(&mut self) -> Result<()>;
    
    /// 检查当前静音状态
    pub fn is_muted(&self) -> bool;
}
```

##### 变量管理

```rust
impl CallController {
    /// 设置会话变量
    pub fn set_variable(&mut self, key: String, value: String);
    
    /// 获取会话变量
    pub fn get_variable(&self, key: &str) -> Option<&String>;
    
    /// 删除会话变量
    pub fn remove_variable(&mut self, key: &str) -> Option<String>;
    
    /// 获取所有变量
    pub fn variables(&self) -> &HashMap<String, String>;
}
```

##### 事件监听

```rust
impl CallController {
    /// 等待下一个事件
    pub async fn wait_event(&mut self) -> Option<ControllerEvent>;
    
    /// 带超时等待事件
    pub async fn wait_event_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ControllerEvent>>;
}
```

#### 控制器事件

```rust
pub enum ControllerEvent {
    /// 收到 DTMF
    DtmfReceived(String),
    
    /// 音频播放完成
    AudioComplete {
        track_id: String,
        interrupted: bool,
    },
    
    /// 录音完成
    RecordingComplete(RecordingInfo),
    
    /// 加入会议室
    ConferenceJoined {
        room_id: String,
        participant_count: usize,
    },
    
    /// 离开会议室
    ConferenceLeft {
        room_id: String,
        reason: String,
    },
    
    /// 转移状态变化
    TransferStateChanged(TransferState),
    
    /// 呼叫被挂断
    Hangup(CallRecordHangupReason),
    
    /// 自定义事件
    Custom(String, serde_json::Value),
}
```

---

### 3. ApplicationContext - 应用上下文

应用运行时的共享上下文，提供对外部资源的访问。

```rust
pub struct ApplicationContext {
    /// 会话级变量（跨应用共享）
    pub session_vars: Arc<RwLock<HashMap<String, String>>>,
    
    /// 全局共享状态（如会议室管理器、队列管理器等）
    pub shared_state: Arc<AppSharedState>,
    
    /// 数据库连接
    pub db: DatabaseConnection,
    
    /// 事件总线（用于应用间通信）
    pub event_bus: Arc<EventBus>,
    
    /// 存储服务（录音、留言等）
    pub storage: Arc<dyn StorageBackend>,
    
    /// HTTP 客户端
    pub http_client: reqwest::Client,
    
    /// 呼叫元信息
    pub call_info: CallInfo,
    
    /// 配置引用
    pub config: Arc<ProxyConfig>,
}

pub struct CallInfo {
    pub session_id: String,
    pub caller: String,
    pub callee: String,
    pub direction: DialDirection,
    pub started_at: DateTime<Utc>,
    pub caller_ip: Option<String>,
    pub callee_ip: Option<String>,
}

pub struct AppSharedState {
    /// 会议室管理器
    pub conference_manager: Arc<ConferenceManager>,
    
    /// 队列管理器
    pub queue_manager: Arc<QueueManager>,
    
    /// 自定义共享数据
    pub custom_data: Arc<RwLock<HashMap<String, Box<dyn Any + Send + Sync>>>>,
}
```

---

### 4. AppEventLoop - 应用事件循环

负责驱动应用的执行和事件分发。

```rust
pub struct AppEventLoop {
    app: Box<dyn CallApp>,
    controller: CallController,
    context: ApplicationContext,
    cancel_token: CancellationToken,
}

impl AppEventLoop {
    pub async fn run(mut self) -> Result<()> {
        // 1. 调用 on_enter
        let mut action = self.app.on_enter(&mut self.controller, &self.context).await?;
        
        // 2. 主事件循环
        loop {
            match action {
                AppAction::Continue => {
                    // 等待下一个事件
                    action = self.handle_next_event().await?;
                }
                AppAction::Exit => {
                    self.app.on_exit(ExitReason::Normal).await?;
                    break;
                }
                AppAction::Hangup { reason } => {
                    self.controller.hangup(reason).await?;
                    self.app.on_exit(ExitReason::Hangup).await?;
                    break;
                }
                AppAction::Transfer(location) => {
                    self.controller.blind_transfer(location).await?;
                    self.app.on_exit(ExitReason::Transferred).await?;
                    break;
                }
                AppAction::Chain(next_app) => {
                    self.app.on_exit(ExitReason::Chained).await?;
                    self.app = next_app;
                    action = self.app.on_enter(&mut self.controller, &self.context).await?;
                }
                AppAction::Sleep(duration) => {
                    tokio::time::sleep(duration).await;
                    action = self.app.on_enter(&mut self.controller, &self.context).await?;
                }
            }
        }
        
        Ok(())
    }
    
    async fn handle_next_event(&mut self) -> Result<AppAction> {
        tokio::select! {
            event = self.controller.wait_event() => {
                match event {
                    Some(ControllerEvent::DtmfReceived(digit)) => {
                        self.app.on_dtmf(digit, &mut self.controller, &self.context).await
                    }
                    Some(ControllerEvent::AudioComplete { track_id, .. }) => {
                        self.app.on_audio_complete(track_id, &mut self.controller, &self.context).await
                    }
                    Some(ControllerEvent::RecordingComplete(info)) => {
                        self.app.on_record_complete(info.path, info.duration, &mut self.controller, &self.context).await
                    }
                    Some(ControllerEvent::Hangup(reason)) => {
                        self.app.on_exit(ExitReason::RemoteHangup(reason)).await?;
                        Ok(AppAction::Exit)
                    }
                    Some(ControllerEvent::Custom(name, data)) => {
                        self.app.on_external_event(AppEvent::Custom(name, data), &mut self.controller, &self.context).await
                    }
                    _ => Ok(AppAction::Continue)
                }
            }
            _ = self.cancel_token.cancelled() => {
                self.app.on_exit(ExitReason::Cancelled).await?;
                Ok(AppAction::Exit)
            }
        }
    }
}

pub enum ExitReason {
    Normal,
    Hangup,
    RemoteHangup(CallRecordHangupReason),
    Transferred,
    Chained,
    Cancelled,
    Error(String),
}
```

---

## 🔗 与 proxy_call 的关系 (App Mode)

在传统的 B2BUA 模式下，`CallSession` 负责桥接两个 `MediaPeer` (Caller 和 Callee)。
在 **CallApp 模式** 下，`CallSession` 作为一个 **单腿 (Single-Leg)** 终结点运行。

### 1. 媒体处理 (Media Processing)

*   **无 MediaBridge**: App Mode 下不创建 `MediaBridge`。
*   **播放 (Playback)**: 当应用调用 `play_audio` 时，`CallSession` 接收到 `SessionAction::PlayPrompt`，它会创建一个 `FileTrack` 并通过 `caller_peer.update_track()` 挂载到 Caller 的媒体流上。
*   **录音 (Recording)**: 当应用调用 `start_recording` 时，`CallSession` 接收到 `SessionAction::StartRecording`，它会配置 `RecorderOption` 并挂载到 Caller 的接收流上，直接将 RTP payload 写入磁盘。
*   **DTMF**: 底层 RTP 栈检测到 RFC 2833/4733 DTMF 包后，通过事件通道发送给 `CallSession`，再由 `CallSession` 转发给 `AppEventLoop`。

### 2. 路由与初始化 (Routing & Initialization)

1.  **Dialplan 扩展**: `Dialplan` 结构体新增 `call_app: Option<Box<dyn CallApp>>` 字段。
2.  **Feature Code 拦截**: `SipServer` 在处理 INVITE 时，通过 `FeatureCodeRegistry` 或 `DialplanInspector` 检查目标号码（如 `*97`）。
3.  **注入 App**: 如果匹配到 Feature Code，则将对应的 `CallApp` 实例注入到 `Dialplan` 中。
4.  **进入 App Mode**: `CallSession::serve` 启动时，检查 `context.dialplan.call_app`。如果存在，则不执行 B2BUA 逻辑，而是调用 `serve_app_mode()`。

### 3. serve_app_mode 流程

```rust
// 伪代码
async fn serve_app_mode(&mut self, app: Box<dyn CallApp>) -> Result<()> {
    // 1. 建立单腿媒体 (VoiceEnginePeer)
    // 2. 创建 CallController 和 ApplicationContext
    // 3. 启动 AppEventLoop (在独立任务中)
    // 4. 启动 AppMediaEventPump (桥接底层媒体事件到 ControllerEvent)
    // 5. 监听 SIP 事务 (BYE, re-INVITE) 和 SessionAction (来自 App)
}
```
