use crate::{
    config::{CallRecordConfig, CallRecordStorageConfig, DEFAULT_CALL_RECORD_MAX_CONCURRENT},
    utils::sanitize_id,
};
use anyhow::Result;
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest;
use sea_orm::{
    ConnectionTrait, DatabaseConnection,
    prelude::DateTimeUtc,
    sea_query::{Alias, ColumnDef, Expr, Query, Table},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    future::{Future, pending},
    path::Path,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    time::sleep,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub mod recording_upload;
pub mod sipflow;
pub mod sipflow_upload;
pub mod storage;
#[cfg(test)]
mod tests;

const CALL_RECORD_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_RECORD_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CallRecordEventType {
    Event,
    Command,
    Sip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordEvent {
    pub r#type: CallRecordEventType,
    pub timestamp: u64,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordStats {
    pub backlog: usize,
    pub processed: u64,
    pub failed: u64,
    pub avg: f64,
}

impl Default for CallRecordStats {
    fn default() -> Self {
        Self::new()
    }
}

impl CallRecordStats {
    pub fn new() -> Self {
        Self {
            backlog: 0,
            processed: 0,
            failed: 0,
            avg: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[serde_with::skip_serializing_none]
pub struct CallDetails {
    pub direction: String,
    pub status: String,
    pub from_number: Option<String>,
    pub to_number: Option<String>,
    pub caller_name: Option<String>,
    pub agent_name: Option<String>,
    pub queue: Option<String>,
    pub department_id: Option<i64>,
    pub extension_id: Option<i64>,
    pub sip_trunk_id: Option<i64>,
    pub route_id: Option<i64>,
    pub sip_gateway: Option<String>,
    pub recording_url: Option<String>,
    pub recording_duration_secs: Option<i32>,
    pub has_transcript: bool,
    pub transcript_status: Option<String>,
    pub transcript_language: Option<String>,
    pub tags: Option<Value>,

    #[serde(default)]
    pub rewrite: CallRecordRewrite,
    pub last_error: Option<CallRecordLastError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordRewrite {
    pub caller_original: String,
    pub caller_final: String,
    pub callee_original: String,
    pub callee_final: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordLastError {
    pub code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CallRecord {
    pub call_id: String,
    pub start_time: DateTimeUtc,
    pub ring_time: Option<DateTimeUtc>,
    pub answer_time: Option<DateTimeUtc>,
    pub end_time: DateTimeUtc,
    pub caller: String,
    pub callee: String,
    pub status_code: u16,
    pub hangup_reason: Option<CallRecordHangupReason>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub hangup_messages: Vec<CallRecordHangupMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub recorder: Vec<CallRecordMedia>,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub sip_leg_roles: HashMap<String, String>,
    #[serde(skip_serializing_if = "LegTimeline::is_empty", default)]
    pub leg_timeline: LegTimeline,
    #[serde(flatten, default)]
    pub details: CallDetails,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub extensions: http::Extensions,
}

impl Clone for CallRecord {
    fn clone(&self) -> Self {
        Self {
            call_id: self.call_id.clone(),
            start_time: self.start_time,
            ring_time: self.ring_time,
            answer_time: self.answer_time,
            end_time: self.end_time,
            caller: self.caller.clone(),
            callee: self.callee.clone(),
            status_code: self.status_code,
            hangup_reason: self.hangup_reason.clone(),
            hangup_messages: self.hangup_messages.clone(),
            recorder: self.recorder.clone(),
            sip_leg_roles: self.sip_leg_roles.clone(),
            leg_timeline: self.leg_timeline.clone(),
            details: self.details.clone(),
            extensions: http::Extensions::new(), // extensions cannot be cloned
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordMedia {
    pub track_id: String,
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum CallRecordHangupReason {
    ByCaller,
    ByCallee,
    ByRefer,
    BySystem,
    Autohangup,
    NoAnswer,
    NoBalance,
    AnswerMachine,
    ServerUnavailable,
    Canceled,
    Rejected,
    Failed,
    RtpTimeout,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordHangupMessage {
    pub code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LegTimelineEventType {
    Added,
    Bridged,
    Unbridged,
    Transferred,
    TransferAccepted,
    TransferFailed,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegTimelineEvent {
    pub timestamp: DateTimeUtc,
    pub leg_id: String,
    pub event_type: LegTimelineEventType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_leg_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LegTimeline {
    pub events: Vec<LegTimelineEvent>,
}

impl LegTimeline {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn add_event(
        &mut self,
        leg_id: String,
        event_type: LegTimelineEventType,
        peer_leg_id: Option<String>,
        details: Option<Value>,
    ) {
        self.events.push(LegTimelineEvent {
            timestamp: chrono::Utc::now(),
            leg_id,
            event_type,
            peer_leg_id,
            details,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl std::str::FromStr for CallRecordHangupReason {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "caller" => Ok(Self::ByCaller),
            "callee" => Ok(Self::ByCallee),
            "refer" => Ok(Self::ByRefer),
            "system" => Ok(Self::BySystem),
            "autohangup" => Ok(Self::Autohangup),
            "noanswer" => Ok(Self::NoAnswer),
            "nobalance" => Ok(Self::NoBalance),
            "answermachine" => Ok(Self::AnswerMachine),
            "serverunavailable" => Ok(Self::ServerUnavailable),
            "canceled" => Ok(Self::Canceled),
            "rejected" => Ok(Self::Rejected),
            "failed" => Ok(Self::Failed),
            _ => Ok(Self::Other(s.to_string())),
        }
    }
}

impl std::fmt::Display for CallRecordHangupReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ByCaller => write!(f, "caller"),
            Self::ByCallee => write!(f, "callee"),
            Self::ByRefer => write!(f, "refer"),
            Self::BySystem => write!(f, "system"),
            Self::Autohangup => write!(f, "autohangup"),
            Self::NoAnswer => write!(f, "noAnswer"),
            Self::NoBalance => write!(f, "noBalance"),
            Self::AnswerMachine => write!(f, "answerMachine"),
            Self::ServerUnavailable => write!(f, "serverUnavailable"),
            Self::Canceled => write!(f, "canceled"),
            Self::Rejected => write!(f, "rejected"),
            Self::Failed => write!(f, "failed"),
            Self::RtpTimeout => write!(f, "rtpTimeout"),
            Self::Other(s) => write!(f, "{}", s),
        }
    }
}

#[async_trait::async_trait]
pub trait CallRecordHook: Send + Sync {
    async fn on_record_completed(&self, record: &mut CallRecord) -> anyhow::Result<()>;
}

/// Channel type used to forward call records to the background saver.
///
/// This used to be an `mpsc::unbounded_channel`, which under saver stalls
/// (e.g. S3 latency) accumulated `CallRecord` objects without limit and
/// could exhaust process memory. It is now a bounded channel: when the
/// saver falls behind, the producer drops new records (logged at warn)
/// instead of buffering indefinitely. Real deployments should never hit
/// the cap under normal load — the bound is a safety net, not a target.
pub const CALL_RECORD_CHANNEL_CAPACITY: usize = 1024;
pub type CallRecordSender = tokio::sync::mpsc::Sender<CallRecord>;
pub type CallRecordReceiver = tokio::sync::mpsc::Receiver<CallRecord>;

#[async_trait::async_trait]
pub trait CallRecordSaver: Send + Sync {
    async fn save(&self, record: &CallRecord) -> Result<String>;
}

pub async fn write_call_record_event<T: Serialize>(
    r#type: CallRecordEventType,
    obj: T,
    file: &mut File,
) {
    let content = match serde_json::to_string(&obj) {
        Ok(s) => s,
        Err(_) => return,
    };
    let event = CallRecordEvent {
        r#type,
        timestamp: crate::media::get_timestamp(),
        content,
    };
    if let Ok(line) = serde_json::to_string(&event) {
        file.write_all(format!("{}\n", line).as_bytes()).await.ok();
    }
}

pub fn default_cdr_file_name(record: &CallRecord) -> String {
    format!("{}.json", sanitize_id(&record.call_id))
}

pub fn default_transcript_file_name(record: &CallRecord) -> String {
    format!("{}.transcript.json", sanitize_id(&record.call_id))
}

pub fn format_call_record(record: &CallRecord) -> Result<String> {
    Ok(serde_json::to_string(record)?)
}

pub fn format_file_name(root: &str, record: &CallRecord) -> String {
    let trimmed_root = root.trim_end_matches('/');
    let file_name = default_cdr_file_name(record);
    if trimmed_root.is_empty() {
        file_name
    } else {
        format!(
            "{}/{}/{}",
            trimmed_root,
            record.start_time.format("%Y%m%d"),
            file_name
        )
    }
}

pub fn format_transcript_path(root: &str, record: &CallRecord) -> String {
    let trimmed_root = root.trim_end_matches('/');
    let file_name = default_transcript_file_name(record);
    if trimmed_root.is_empty() {
        file_name
    } else {
        format!(
            "{}/{}/{}",
            trimmed_root,
            record.start_time.format("%Y%m%d"),
            file_name
        )
    }
}

pub fn format_media_path(root: &str, record: &CallRecord, media: &CallRecordMedia) -> String {
    let trimmed_root = root.trim_end_matches('/');
    let file_name = Path::new(&media.path)
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
        .to_string_lossy()
        .to_string();

    let key = format!(
        "{}/{}_{}_{}",
        record.start_time.format("%Y%m%d"),
        record.call_id,
        media.track_id,
        file_name
    );
    if trimmed_root.is_empty() {
        key
    } else {
        format!("{}/{}", trimmed_root, key)
    }
}

pub fn format_sipflow_media_key(record: &CallRecord) -> String {
    format!(
        "{}/{}.wav",
        record.start_time.format("%Y%m%d"),
        record.call_id
    )
}

pub fn format_sipflow_signaling_key(record: &CallRecord) -> String {
    format!(
        "{}/{}.jsonl",
        record.start_time.format("%Y%m%d"),
        record.call_id
    )
}

pub fn format_sipflow_media_file_name(record: &CallRecord) -> String {
    format!("{}.wav", record.call_id)
}

pub fn format_sipflow_signaling_file_name(record: &CallRecord) -> String {
    format!("{}.jsonl", record.call_id)
}

pub struct CallRecordManager {
    pub max_concurrent: usize,
    pub sender: CallRecordSender,
    pub stats: Arc<CallRecordStats>,
    cancel_token: CancellationToken,
    receiver: CallRecordReceiver,
    saver: Box<dyn CallRecordSaver>,
    hooks: Vec<Box<dyn CallRecordHook>>,
}

pub struct CallRecordManagerBuilder {
    pub cancel_token: Option<CancellationToken>,
    pub config: Option<CallRecordConfig>,
    pub max_concurrent: Option<usize>,
    hooks: Vec<Box<dyn CallRecordHook>>,
}

impl Default for CallRecordManagerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl CallRecordManagerBuilder {
    pub fn new() -> Self {
        Self {
            cancel_token: None,
            config: None,
            max_concurrent: None,
            hooks: Vec::new(),
        }
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn with_config(mut self, config: CallRecordConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_hook(mut self, hook: Box<dyn CallRecordHook>) -> Self {
        self.hooks.push(hook);
        self
    }
    pub fn with_max_concurrent(mut self, max_concurrent: usize) -> Self {
        self.max_concurrent = Some(max_concurrent.max(1));
        self
    }

    pub async fn build(self) -> Result<CallRecordManager> {
        let Self {
            cancel_token,
            config,
            max_concurrent,
            hooks,
        } = self;
        let cancel_token = cancel_token.unwrap_or_default();
        let (sender, receiver) = tokio::sync::mpsc::channel(CALL_RECORD_CHANNEL_CAPACITY);
        let saver: Box<dyn CallRecordSaver> = match config.map(|config| config.storage) {
            None => Box::new(NoopCallRecordSaver),
            Some(CallRecordStorageConfig::Local { root }) => {
                if !Path::new(&root).exists() {
                    match std::fs::create_dir_all(&root) {
                        Ok(_) => {
                            info!("CallRecordManager created directory: {}", root);
                        }
                        Err(e) => {
                            warn!("CallRecordManager failed to create directory: {}", e);
                        }
                    }
                }
                Box::new(LocalCallRecordSaver { root })
            }
            Some(CallRecordStorageConfig::S3 {
                root,
                vendor,
                bucket,
                region,
                access_key,
                secret_key,
                endpoint,
                ..
            }) => {
                let storage = crate::storage::Storage::new(&crate::storage::StorageConfig::S3 {
                    vendor,
                    bucket: bucket.clone(),
                    region,
                    access_key,
                    secret_key,
                    endpoint: Some(endpoint.clone()),
                    prefix: None,
                })
                .map_err(|e| {
                    warn!(
                        bucket,
                        endpoint, "CallRecordManager failed to initialize S3 storage: {}", e
                    );
                    e
                })?;

                Box::new(S3CallRecordSaver {
                    root,
                    bucket,
                    endpoint,
                    storage,
                })
            }
            Some(CallRecordStorageConfig::Http { url, headers, .. }) => {
                Box::new(HttpCallRecordSaver {
                    url,
                    headers,
                    client: reqwest::Client::builder()
                        .timeout(CALL_RECORD_HTTP_TIMEOUT)
                        .connect_timeout(CALL_RECORD_HTTP_CONNECT_TIMEOUT)
                        .build()?,
                })
            }
            Some(CallRecordStorageConfig::Database {
                database_url: callrecord_database_url,
                table_name,
                ..
            }) => {
                let Some(db_url) = callrecord_database_url else {
                    return Err(anyhow::anyhow!(
                        "database call record saver requires database_url"
                    ));
                };
                let db = crate::models::connect_db(&db_url).await.map_err(|e| {
                    warn!(
                        database_url = %db_url,
                        "CallRecordManager failed to initialize database saver: {}", e
                    );
                    e
                })?;
                let create_table = Table::create()
                    .table(Alias::new(table_name.as_str()))
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Alias::new("id"))
                            .integer()
                            .not_null()
                            .primary_key()
                            .auto_increment(),
                    )
                    .col(ColumnDef::new(Alias::new("call_id")).text().not_null())
                    .col(ColumnDef::new(Alias::new("caller")).text().not_null())
                    .col(ColumnDef::new(Alias::new("callee")).text().not_null())
                    .col(ColumnDef::new(Alias::new("start_time")).text().not_null())
                    .col(ColumnDef::new(Alias::new("end_time")).text().not_null())
                    .col(ColumnDef::new(Alias::new("status_code")).integer())
                    .col(ColumnDef::new(Alias::new("hangup_reason")).text())
                    .col(ColumnDef::new(Alias::new("details")).text())
                    .col(
                        ColumnDef::new(Alias::new("created_at"))
                            .timestamp()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned();
                db.execute(db.get_database_backend().build(&create_table))
                    .await?;

                Box::new(DatabaseCallRecordSaver {
                    db,
                    db_url,
                    table_name,
                })
            }
        };
        let max_concurrent = max_concurrent
            .unwrap_or(DEFAULT_CALL_RECORD_MAX_CONCURRENT)
            .max(1);

        Ok(CallRecordManager {
            max_concurrent,
            stats: Arc::new(CallRecordStats::new()),
            cancel_token,
            sender,
            receiver,
            saver,
            hooks,
        })
    }
}

pub struct NoopCallRecordSaver;

#[async_trait::async_trait]
impl CallRecordSaver for NoopCallRecordSaver {
    async fn save(&self, _record: &CallRecord) -> Result<String> {
        Ok(String::new())
    }
}

struct LocalCallRecordSaver {
    root: String,
}

#[async_trait::async_trait]
impl CallRecordSaver for LocalCallRecordSaver {
    async fn save(&self, record: &CallRecord) -> Result<String> {
        let file_content = format_call_record(record)?;
        let file_name = format_file_name(&self.root, record);

        // Ensure parent directory exists
        if let Some(parent) = Path::new(&file_name).parent() {
            fs::create_dir_all(parent).await?;
        }

        let mut file = File::create(&file_name).await.map_err(|e| {
            anyhow::anyhow!("Failed to create call record file {}: {}", file_name, e)
        })?;
        file.write_all(file_content.as_bytes()).await?;
        file.flush().await?;
        Ok(file_name.to_string())
    }
}

struct HttpCallRecordSaver {
    url: String,
    headers: Option<HashMap<String, String>>,
    client: reqwest::Client,
}

#[async_trait::async_trait]
impl CallRecordSaver for HttpCallRecordSaver {
    async fn save(&self, record: &CallRecord) -> Result<String> {
        let call_log_json = format_call_record(record)?;
        let form = reqwest::multipart::Form::new().text("calllog.json", call_log_json);

        let mut request = self.client.post(&self.url).multipart(form);
        if let Some(headers_map) = &self.headers {
            for (key, value) in headers_map {
                request = request.header(key, value);
            }
        }
        let response = request.send().await?;
        if response.status().is_success() {
            let response_text = response.text().await.unwrap_or_default();
            Ok(format!("HTTP upload successful: {}", response_text))
        } else {
            Err(anyhow::anyhow!(
                "HTTP upload failed with status: {} - {}",
                response.status(),
                response.text().await.unwrap_or_default()
            ))
        }
    }
}

struct S3CallRecordSaver {
    root: String,
    bucket: String,
    endpoint: String,
    storage: crate::storage::Storage,
}

#[async_trait::async_trait]
impl CallRecordSaver for S3CallRecordSaver {
    async fn save(&self, record: &CallRecord) -> Result<String> {
        let start_time = Instant::now();
        let call_log_json = format_call_record(record)?;
        let filename = format_file_name(&self.root, record);
        let buf_size = call_log_json.len();
        match self.storage.write(&filename, call_log_json.into()).await {
            Ok(_) => {
                info!(
                    elapsed = start_time.elapsed().as_secs_f64(),
                    filename, buf_size, "upload call record"
                );
            }
            Err(e) => {
                warn!(filename, "failed to upload call record: {}", e);
            }
        }

        Ok(format!(
            "{}/{}/{}",
            self.endpoint.trim_end_matches('/'),
            self.bucket.trim_matches('/'),
            filename.trim_start_matches('/')
        ))
    }
}

struct DatabaseCallRecordSaver {
    db: DatabaseConnection,
    db_url: String,
    table_name: String,
}

#[async_trait::async_trait]
impl CallRecordSaver for DatabaseCallRecordSaver {
    async fn save(&self, record: &CallRecord) -> Result<String> {
        let details_json = serde_json::to_string(&record.details)?;
        let hangup_reason = record.hangup_reason.as_ref().map(|r| r.to_string());
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new(self.table_name.as_str()))
            .columns([
                Alias::new("call_id"),
                Alias::new("caller"),
                Alias::new("callee"),
                Alias::new("start_time"),
                Alias::new("end_time"),
                Alias::new("status_code"),
                Alias::new("hangup_reason"),
                Alias::new("details"),
            ])
            .values_panic([
                record.call_id.clone().into(),
                record.caller.clone().into(),
                record.callee.clone().into(),
                record.start_time.to_rfc3339().into(),
                record.end_time.to_rfc3339().into(),
                (record.status_code as i32).into(),
                hangup_reason.into(),
                details_json.into(),
            ]);
        self.db
            .execute(self.db.get_database_backend().build(&insert))
            .await?;

        Ok(format!("database:{}/{}", self.db_url, record.call_id))
    }
}

impl CallRecordManager {
    pub async fn serve(&mut self) {
        let token = self.cancel_token.clone();
        info!("CallRecordManager serving");
        let max_concurrent = self.max_concurrent.max(1);
        let receiver = &mut self.receiver;
        let saver = self.saver.as_ref();
        let hooks = &self.hooks;
        let mut futures = FuturesUnordered::new();
        let mut receiver_closed = false;
        let mut shutting_down = false;
        let mut shutdown_timeout: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(pending());

        loop {
            if (receiver_closed || shutting_down) && futures.is_empty() {
                break;
            }

            let can_receive = !receiver_closed && !shutting_down && futures.len() < max_concurrent;

            tokio::select! {
                record = receiver.recv(), if can_receive => {
                    let Some(record) = record else {
                        receiver_closed = true;
                        continue;
                    };

                    futures.push(async move {
                        let mut record = record;
                        let start_time = Instant::now();
                        match saver.save(&record).await {
                            Ok(file_name) => {
                                let elapsed = start_time.elapsed();
                                info!(
                                    ?elapsed,
                                    call_id = record.call_id,
                                    file_name,
                                    "CallRecordManager saved"
                                );
                            }
                            Err(err) => {
                                warn!("Failed to save call record: {}", err);
                            }
                        }

                        for hook in hooks.iter() {
                            if let Err(e) = hook.on_record_completed(&mut record).await {
                                warn!("CallRecordHook failed: {}", e);
                            }
                        }
                    });
                }
                Some(()) = futures.next(), if !futures.is_empty() => {}
                _ = token.cancelled(), if !shutting_down => {
                    shutting_down = true;
                    let pending = futures.len();
                    info!(pending, "CallRecordManager received shutdown");
                    shutdown_timeout = Box::pin(sleep(Duration::from_secs(5)));
                }
                _ = &mut shutdown_timeout, if shutting_down => {
                    warn!(
                        pending = futures.len(),
                        "CallRecordManager shutdown timed out before all tasks finished"
                    );
                    break;
                }
            }
        }
        info!(pending = futures.len(), "CallRecordManager exiting");
    }
}
