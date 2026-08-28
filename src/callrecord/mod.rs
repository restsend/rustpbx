use crate::{
    config::{
        CallRecordConfig, CallRecordStorageConfig, DEFAULT_CALL_RECORD_BATCH_SIZE,
        DEFAULT_CALL_RECORD_CHANNEL_CAPACITY,
    },
    utils::sanitize_id,
};
use anyhow::Result;
use chrono::Utc;
use futures::future::try_join_all;
use reqwest;
use rustpbx_models::DatabasePoolConfig;
use sea_orm::{
    ConnectionTrait, DatabaseConnection,
    prelude::DateTimeUtc,
    sea_query::{Alias, Query},
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

pub(crate) mod database;
pub mod recording_artifacts;
pub mod recording_upload;
pub mod sipflow;
pub mod sipflow_remote_upload;
pub mod sipflow_upload;
pub mod storage;
#[cfg(test)]
mod tests;

pub use recording_artifacts::{
    ActiveRecording, RecordingSegment, RecordingSubdir, UploadFailedMarker, local_archive_path,
    segment_wav_path, upload_failed_marker_path, write_upload_failed_marker,
};

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
    pub outbound_sip_trunk_id: Option<i64>,
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
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    /// Transient (never serialized into the CDR JSON): the storage path
    /// returned by the saver. Threaded into the DB hook so the persisted
    /// record remembers where its CDR file lives, surviving later storage
    /// root changes (issue #237).
    #[serde(skip)]
    pub cdr_file_path: Option<String>,
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
    /// Root session id of the whole logical call (first INVITE's Call-ID,
    /// inherited by every child leg — queue dispatch, REFER transfer,
    /// consultation). Equal to `call_id` for root sessions. This replaces
    /// the former `root_call_id` correlation mechanism.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
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
            session_id: self.session_id.clone(),
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

/// Transient carrier for the size of a SipFlow-uploaded recording. Stashed on
/// `CallRecord.extensions` by `SipFlowUploadHook` / `SipFlowRemoteUploadHook`
/// so `RecordingUploadHook` can emit an accurate
/// `recording_metadata_available.file_size` when there is no local WAV
/// recorder file.
#[derive(Debug, Clone, Copy)]
pub struct RecordingFileSize(pub u64);

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
    /// Caller hung up while waiting in a queue (before any agent connected).
    /// Distinct from `Canceled` (generic SIP 487) and `ByCaller` (normal caller
    /// hangup on a connected call).
    Abandoned,
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
            "abandoned" => Ok(Self::Abandoned),
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
            Self::Abandoned => write!(f, "abandoned"),
            Self::Other(s) => write!(f, "{}", s),
        }
    }
}

impl CallRecordHangupReason {
    /// Normalized hangup initiator: who ended the call.
    ///
    /// This is the single source of truth used by both the core `call_hangup`
    /// event and the CC-layer `cc_hangup` event so the two stay consistent.
    ///
    /// - `"agent"`   — the contact-center agent (callee leg) hung up.
    /// - `"caller"`  — the calling party hung up.
    /// - `"system"`  — the server/auto-hangup ended the call.
    /// - `"transfer"`— the call ended due to a transfer/REFER.
    /// - `"unknown"` — initiator cannot be determined.
    pub fn initiator(&self) -> &'static str {
        match self {
            Self::ByCaller | Self::Abandoned => "caller",
            Self::ByCallee => "agent",
            Self::BySystem | Self::Autohangup => "system",
            Self::ByRefer => "transfer",
            _ => "unknown",
        }
    }
}

#[async_trait::async_trait]
pub trait CallRecordHook: Send + Sync {
    /// Enrichment phase: runs in registration order **before** the batch is
    /// saved and **before** any side-effect (`on_record_completed`) hook.
    ///
    /// Addons use this to fill generic core fields (e.g. `agent_id`) from the
    /// typed session-extensions bag, so that subsequent side-effect hooks
    /// (upload, billing, CSAT) and the saver see the completed record. This
    /// keeps the core free of addon-specific knowledge: core only carries the
    /// generic fields, the addon populates them.
    async fn on_record_enrich(&self, _records: &mut [CallRecord]) -> anyhow::Result<()> {
        Ok(())
    }

    /// Side-effect phase: runs **after** the batch has been saved. Used for
    /// uploads, webhook emission, billing, CSAT linkage, metrics, etc.
    async fn on_record_completed(&self, _records: &mut [CallRecord]) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Channel type used to forward call records to the background saver.
///
/// This used to be an `mpsc::unbounded_channel`, which under saver stalls
/// (e.g. S3 latency) accumulated `CallRecord` objects without limit and
/// could exhaust process memory. It is now a bounded channel: when the
/// saver falls behind, the producer drops new records (logged at warn)
/// instead of buffering indefinitely. Real deployments should never hit
/// the configured cap under normal load — the bound is a safety net, not a
/// target.
pub const CALL_RECORD_CHANNEL_CAPACITY: usize = DEFAULT_CALL_RECORD_CHANNEL_CAPACITY;
pub type CallRecordSender = tokio::sync::mpsc::Sender<CallRecord>;
pub type CallRecordReceiver = tokio::sync::mpsc::Receiver<CallRecord>;

#[async_trait::async_trait]
pub trait CallRecordSaver: Send + Sync {
    /// Persist a batch and return one storage path per input record, in order.
    async fn save(&self, records: &[CallRecord]) -> Result<Vec<String>>;
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

pub fn sipflow_media_key_for(call_id: &str, start_utc: DateTimeUtc) -> String {
    format!("{}/{}.wav", start_utc.format("%Y%m%d"), call_id)
}

pub fn sipflow_signaling_key_for(call_id: &str, start_utc: DateTimeUtc) -> String {
    format!("{}/{}.jsonl", start_utc.format("%Y%m%d"), call_id)
}

pub fn sipflow_signaling_file_name_for(call_id: &str) -> String {
    format!("{}.jsonl", call_id)
}

pub fn format_sipflow_media_key(record: &CallRecord) -> String {
    sipflow_media_key_for(&record.call_id, record.start_time)
}

pub fn format_sipflow_signaling_key(record: &CallRecord) -> String {
    sipflow_signaling_key_for(&record.call_id, record.start_time)
}

pub fn format_sipflow_media_file_name(record: &CallRecord) -> String {
    format!("{}.wav", record.call_id)
}

pub fn format_sipflow_signaling_file_name(record: &CallRecord) -> String {
    sipflow_signaling_file_name_for(&record.call_id)
}

pub struct CallRecordManager {
    pub batch_size: usize,
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
    pub channel_capacity: Option<usize>,
    pub batch_size: Option<usize>,
    hooks: Vec<Box<dyn CallRecordHook>>,
    main_db: Option<DatabaseConnection>,
    pool_config: Option<DatabasePoolConfig>,
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
            channel_capacity: None,
            batch_size: None,
            hooks: Vec::new(),
            main_db: None,
            pool_config: None,
        }
    }

    pub fn with_main_db(mut self, db: DatabaseConnection) -> Self {
        self.main_db = Some(db);
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn with_config(mut self, config: CallRecordConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_pool_config(mut self, pool_config: DatabasePoolConfig) -> Self {
        self.pool_config = Some(pool_config);
        self
    }

    pub fn with_hook(mut self, hook: Box<dyn CallRecordHook>) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn with_channel_capacity(mut self, channel_capacity: usize) -> Self {
        self.channel_capacity = Some(channel_capacity.max(1));
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = Some(batch_size.max(1));
        self
    }

    pub async fn build(self) -> Result<CallRecordManager> {
        let Self {
            cancel_token,
            config,
            channel_capacity,
            batch_size,
            hooks,
            main_db,
            pool_config,
        } = self;
        let cancel_token = cancel_token.unwrap_or_default();
        let channel_capacity = channel_capacity
            .or_else(|| config.as_ref().map(|c| c.channel_capacity))
            .unwrap_or(DEFAULT_CALL_RECORD_CHANNEL_CAPACITY)
            .max(1);
        let batch_size = batch_size
            .or_else(|| config.as_ref().map(|c| c.batch_size))
            .unwrap_or(DEFAULT_CALL_RECORD_BATCH_SIZE)
            .max(1);
        let (sender, receiver) = tokio::sync::mpsc::channel(channel_capacity);
        let saver: Box<dyn CallRecordSaver> = match config.map(|c| c.storage) {
            // No [callrecord] section → default: database → rustpbx_call_records
            None => {
                let db = main_db.ok_or_else(|| {
                    anyhow::anyhow!("main_db is required when [callrecord] is not configured")
                })?;
                Box::new(BuiltinDatabaseSaver { db })
            }
            // [callrecord] type = "database" with default settings → SeaORM path
            Some(CallRecordStorageConfig::Database {
                database_url: None,
                table_name,
                skip_create_table: _,
                rotate: crate::config::RotationMode::None,
            }) if table_name == "rustpbx_call_records" => {
                let db = main_db.ok_or_else(|| {
                    anyhow::anyhow!("main_db is required for default call record storage")
                })?;
                Box::new(BuiltinDatabaseSaver { db })
            }
            // Database variant with custom settings → raw SQL
            Some(CallRecordStorageConfig::Database {
                database_url,
                table_name,
                skip_create_table,
                rotate,
            }) => {
                let (db, _db_url) = match &database_url {
                    Some(url) => (
                        crate::models::connect_db(url, pool_config.as_ref()).await?,
                        url.clone(),
                    ),
                    None => (
                        main_db.clone().ok_or_else(|| {
                            anyhow::anyhow!("either database_url or main_db is required")
                        })?,
                        "main".to_string(),
                    ),
                };
                let is_sqlite = database_url
                    .as_deref()
                    .map_or(true, |u| u.starts_with("sqlite:"))
                    || db.get_database_backend() == sea_orm::DatabaseBackend::Sqlite;

                if rotate == crate::config::RotationMode::Daily && is_sqlite {
                    // SQLite daily file rotation
                    if !skip_create_table {
                        create_call_record_table(&db, &table_name).await?;
                    }
                    let base_url = database_url.unwrap_or_else(|| "sqlite:memory:".to_string());
                    Box::new(RotatingSqliteSaver {
                        base_url,
                        table_name,
                        skip_create_table,
                        state: std::sync::Arc::new(tokio::sync::Mutex::new(RotateState {
                            current_date: today_string(),
                            db,
                        })),
                    })
                } else {
                    if rotate == crate::config::RotationMode::Daily {
                        warn!(
                            "Daily rotation is only supported for SQLite; writing without rotation"
                        );
                    }
                    if !skip_create_table {
                        create_call_record_table(&db, &table_name).await?;
                    }
                    Box::new(CustomDatabaseSaver { db, table_name })
                }
            }
            // Local, S3, Http → existing savers
            Some(CallRecordStorageConfig::Local { root }) => {
                if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
                    match tokio::fs::create_dir_all(&root).await {
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
                    endpoint: endpoint.clone(),
                    prefix: None,
                })
                .map_err(|e| {
                    warn!(
                        bucket,
                        ?endpoint,
                        "CallRecordManager failed to initialize S3 storage: {}",
                        e
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
                    client: crate::http_util::build_keepalive_client(
                        Some(CALL_RECORD_HTTP_TIMEOUT),
                        Some(CALL_RECORD_HTTP_CONNECT_TIMEOUT),
                    )?,
                })
            }
        };
        Ok(CallRecordManager {
            batch_size,
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
    async fn save(&self, records: &[CallRecord]) -> Result<Vec<String>> {
        Ok(vec![String::new(); records.len()])
    }
}

struct LocalCallRecordSaver {
    root: String,
}

#[async_trait::async_trait]
impl CallRecordSaver for LocalCallRecordSaver {
    async fn save(&self, records: &[CallRecord]) -> Result<Vec<String>> {
        try_join_all(records.iter().map(|record| async move {
            let start = std::time::Instant::now();
            let file_content = format_call_record(record)?;
            let file_name = format_file_name(&self.root, record);

            if let Some(parent) = Path::new(&file_name).parent() {
                fs::create_dir_all(parent).await?;
            }

            let mut file = File::create(&file_name).await.map_err(|e| {
                anyhow::anyhow!("Failed to create call record file {}: {}", file_name, e)
            })?;
            file.write_all(file_content.as_bytes()).await?;
            file.flush().await?;
            let elapsed = start.elapsed().as_secs_f64();
            crate::metrics::db::query_latency_seconds("local_cdr_save", elapsed);
            if elapsed > 0.1 {
                crate::metrics::db::slow_query_total("local_cdr_save", 100);
            }
            Ok(file_name)
        }))
        .await
    }
}

struct HttpCallRecordSaver {
    url: String,
    headers: Option<HashMap<String, String>>,
    client: reqwest::Client,
}

#[async_trait::async_trait]
impl CallRecordSaver for HttpCallRecordSaver {
    async fn save(&self, records: &[CallRecord]) -> Result<Vec<String>> {
        try_join_all(records.iter().map(|record| async move {
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
        }))
        .await
    }
}

struct S3CallRecordSaver {
    root: String,
    bucket: String,
    endpoint: Option<String>,
    storage: crate::storage::Storage,
}

#[async_trait::async_trait]
impl CallRecordSaver for S3CallRecordSaver {
    async fn save(&self, records: &[CallRecord]) -> Result<Vec<String>> {
        try_join_all(records.iter().map(|record| async move {
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

            let path = format!(
                "{}/{}",
                self.bucket.trim_matches('/'),
                filename.trim_start_matches('/')
            );
            Ok(match self.endpoint.as_ref() {
                Some(ep) if !ep.is_empty() => format!("{}/{}", ep.trim_end_matches('/'), path),
                _ => path,
            })
        }))
        .await
    }
}

// ── Unified Call Record Savers ─────────────────────────────────────────────────

/// Shared extraction of typed call-record values used by both the
/// Column order shared by the SeaORM persistence path and raw-SQL savers.
pub(crate) struct CallRecordRow {
    pub call_id: String,
    pub direction: String,
    pub status: String,
    pub started_at: DateTimeUtc,
    pub ended_at: Option<DateTimeUtc>,
    pub duration_secs: i32,
    pub from_number: Option<String>,
    pub to_number: Option<String>,
    pub caller_name: Option<String>,
    pub agent_name: Option<String>,
    pub queue: Option<String>,
    pub department_id: Option<i64>,
    pub extension_id: Option<i64>,
    pub sip_trunk_id: Option<i64>,
    pub outbound_sip_trunk_id: Option<i64>,
    pub route_id: Option<i64>,
    pub sip_gateway: Option<String>,
    pub rewrite_original_from: Option<String>,
    pub rewrite_original_to: Option<String>,
    pub caller_uri: Option<String>,
    pub callee_uri: Option<String>,
    pub recording_url: Option<String>,
    pub recording_duration_secs: Option<i32>,
    pub has_transcript: bool,
    pub transcript_status: String,
    pub transcript_language: Option<String>,
    pub tags: Option<Value>,
    pub leg_timeline: Option<Value>,
    pub metadata: Option<Value>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl CallRecordRow {
    pub fn from_record(record: &CallRecord) -> Self {
        let details = &record.details;
        let duration_secs = record
            .answer_time
            .map(|answer_time| (record.end_time - answer_time).num_seconds().max(0) as i32)
            .unwrap_or(0);
        let direction = details.direction.trim().to_ascii_lowercase();
        let status = details.status.trim().to_ascii_lowercase();

        let rewrite_original_from = if !details.rewrite.caller_original.is_empty() {
            Some(details.rewrite.caller_original.clone())
        } else {
            None
        };
        let rewrite_original_to = if !details.rewrite.callee_original.is_empty() {
            Some(details.rewrite.callee_original.clone())
        } else {
            None
        };

        let recording_url = details
            .recording_url
            .clone()
            .or_else(|| record.recorder.first().map(|media| media.path.clone()));
        let transcript_status = details
            .transcript_status
            .clone()
            .unwrap_or_else(|| "none".to_string());

        let leg_timeline = if record.leg_timeline.is_empty() {
            None
        } else {
            serde_json::to_value(&record.leg_timeline).ok()
        };

        let mut metadata_map = details.metadata.clone().unwrap_or_default();
        if !record.sip_leg_roles.is_empty() {
            let json = serde_json::to_string(&record.sip_leg_roles).unwrap_or_default();
            metadata_map.insert("sip_leg_roles".to_string(), serde_json::Value::String(json));
        }
        if let Some(ring_time) = record.ring_time {
            metadata_map.insert(
                "ring_time".to_string(),
                serde_json::Value::String(ring_time.to_rfc3339()),
            );
        }
        if let Some(answer_time) = record.answer_time {
            metadata_map.insert(
                "answer_time".to_string(),
                serde_json::Value::String(answer_time.to_rfc3339()),
            );
        }
        let metadata = serde_json::to_value(&metadata_map).ok();

        let caller_uri = crate::models::call_record::normalize_endpoint_uri(&record.caller);
        let callee_uri = crate::models::call_record::normalize_endpoint_uri(&record.callee);

        Self {
            call_id: record.call_id.clone(),
            direction,
            status,
            started_at: record.start_time,
            ended_at: Some(record.end_time),
            duration_secs,
            from_number: details.from_number.clone(),
            to_number: details.to_number.clone(),
            caller_name: details.caller_name.clone(),
            agent_name: details.agent_name.clone(),
            queue: details.queue.clone(),
            department_id: details.department_id,
            extension_id: details.extension_id,
            sip_trunk_id: details.sip_trunk_id,
            outbound_sip_trunk_id: details.outbound_sip_trunk_id,
            route_id: details.route_id,
            sip_gateway: details.sip_gateway.clone(),
            rewrite_original_from,
            rewrite_original_to,
            caller_uri,
            callee_uri,
            recording_url,
            recording_duration_secs: details.recording_duration_secs,
            has_transcript: details.has_transcript,
            transcript_status,
            transcript_language: details.transcript_language.clone(),
            tags: details.tags.clone(),
            leg_timeline,
            metadata,
            created_at: record.start_time,
            updated_at: record.end_time,
        }
    }
}

/// Default database saver: writes to `rustpbx_call_records` via
/// SeaORM-based saver for the built-in `rustpbx_call_records` table.
pub(crate) struct BuiltinDatabaseSaver {
    pub db: DatabaseConnection,
}

#[async_trait::async_trait]
impl CallRecordSaver for BuiltinDatabaseSaver {
    async fn save(&self, records: &[CallRecord]) -> Result<Vec<String>> {
        database::persist_call_records(&self.db, records).await?;
        Ok(records
            .iter()
            .map(|record| format!("rustpbx_call_records/{}", record.call_id))
            .collect())
    }
}

/// Raw-SQL database saver: writes the full 34-column schema to a
/// configurable table (not necessarily `rustpbx_call_records`).
pub(crate) struct CustomDatabaseSaver {
    pub db: DatabaseConnection,
    pub table_name: String,
}

#[async_trait::async_trait]
impl CallRecordSaver for CustomDatabaseSaver {
    async fn save(&self, records: &[CallRecord]) -> Result<Vec<String>> {
        if !records.is_empty() {
            let mut insert = Query::insert();
            insert
                .into_table(Alias::new(&self.table_name))
                .columns(call_record_columns());
            for record in records {
                let row = CallRecordRow::from_record(record);
                insert.values_panic(build_call_record_values(&row));
            }
            self.db
                .execute_raw(self.db.get_database_backend().build(&insert))
                .await?;
        }
        Ok(records
            .iter()
            .map(|record| format!("{}/{}", self.table_name, record.call_id))
            .collect())
    }
}

/// State for the rotating SQLite saver.
pub(crate) struct RotateState {
    pub current_date: String,
    pub db: DatabaseConnection,
}

/// SQLite daily-rotation saver: writes to `{base}-{YYYYMMDD}.db` files,
/// switching the connection at day boundaries.
pub(crate) struct RotatingSqliteSaver {
    pub base_url: String,
    pub table_name: String,
    pub skip_create_table: bool,
    pub state: Arc<tokio::sync::Mutex<RotateState>>,
}

#[async_trait::async_trait]
impl CallRecordSaver for RotatingSqliteSaver {
    async fn save(&self, records: &[CallRecord]) -> Result<Vec<String>> {
        let today = today_string();
        let db = {
            let mut st = self.state.lock().await;
            if st.current_date != today {
                let url = derive_daily_url(&self.base_url, &today);
                let new_db = crate::models::connect_db(&url, None).await?;
                if !self.skip_create_table {
                    create_call_record_table(&new_db, &self.table_name).await?;
                }
                *st = RotateState {
                    current_date: today,
                    db: new_db,
                };
            }
            st.db.clone()
        };
        if !records.is_empty() {
            let mut insert = Query::insert();
            insert
                .into_table(Alias::new(&self.table_name))
                .columns(call_record_columns());
            for record in records {
                let row = CallRecordRow::from_record(record);
                insert.values_panic(build_call_record_values(&row));
            }
            db.execute_raw(db.get_database_backend().build(&insert))
                .await?;
        }
        Ok(records
            .iter()
            .map(|record| format!("{}/{}/{}", self.base_url, self.table_name, record.call_id))
            .collect())
    }
}

// ── Helper: column / value builders ────────────────────────────────────────────

fn call_record_columns() -> Vec<Alias> {
    vec![
        Alias::new("call_id"),
        Alias::new("display_id"),
        Alias::new("direction"),
        Alias::new("status"),
        Alias::new("started_at"),
        Alias::new("ended_at"),
        Alias::new("duration_secs"),
        Alias::new("from_number"),
        Alias::new("to_number"),
        Alias::new("caller_name"),
        Alias::new("agent_name"),
        Alias::new("queue"),
        Alias::new("department_id"),
        Alias::new("extension_id"),
        Alias::new("sip_trunk_id"),
        Alias::new("outbound_sip_trunk_id"),
        Alias::new("route_id"),
        Alias::new("sip_gateway"),
        Alias::new("rewrite_original_from"),
        Alias::new("rewrite_original_to"),
        Alias::new("caller_uri"),
        Alias::new("callee_uri"),
        Alias::new("recording_url"),
        Alias::new("recording_duration_secs"),
        Alias::new("has_transcript"),
        Alias::new("transcript_status"),
        Alias::new("transcript_language"),
        Alias::new("tags"),
        Alias::new("leg_timeline"),
        Alias::new("metadata"),
        Alias::new("created_at"),
        Alias::new("updated_at"),
        Alias::new("archived_at"),
    ]
}

fn build_call_record_values(row: &CallRecordRow) -> Vec<sea_orm::sea_query::SimpleExpr> {
    use sea_orm::sea_query::SimpleExpr;
    fn opt_str(v: &Option<Value>) -> Option<String> {
        v.as_ref().map(|v| v.to_string())
    }
    let ended_at: Option<String> = row.ended_at.map(|dt| dt.to_rfc3339());
    vec![
        row.call_id.clone().into(),
        None::<String>.into(),
        row.direction.clone().into(),
        row.status.clone().into(),
        row.started_at.to_rfc3339().into(),
        ended_at.into(),
        row.duration_secs.into(),
        row.from_number.clone().into(),
        row.to_number.clone().into(),
        row.caller_name.clone().into(),
        row.agent_name.clone().into(),
        row.queue.clone().into(),
        SimpleExpr::from(row.department_id),
        SimpleExpr::from(row.extension_id),
        SimpleExpr::from(row.sip_trunk_id),
        SimpleExpr::from(row.outbound_sip_trunk_id),
        SimpleExpr::from(row.route_id),
        row.sip_gateway.clone().into(),
        row.rewrite_original_from.clone().into(),
        row.rewrite_original_to.clone().into(),
        row.caller_uri.clone().into(),
        row.callee_uri.clone().into(),
        row.recording_url.clone().into(),
        SimpleExpr::from(row.recording_duration_secs),
        SimpleExpr::from(row.has_transcript),
        row.transcript_status.clone().into(),
        row.transcript_language.clone().into(),
        opt_str(&row.tags).into(),
        opt_str(&row.leg_timeline).into(),
        opt_str(&row.metadata).into(),
        row.created_at.to_rfc3339().into(),
        row.updated_at.to_rfc3339().into(),
        None::<String>.into(),
    ]
}

/// Create the full 34-column call record table (plus auto-increment `id` PK
/// and basic indexes) if it does not already exist.
pub(crate) async fn create_call_record_table(
    db: &DatabaseConnection,
    table_name: &str,
) -> Result<()> {
    use sea_orm::sea_query::{ColumnDef, Index, Table};
    use sea_orm_migration::schema::*;

    let create = Table::create()
        .table(Alias::new(table_name))
        .if_not_exists()
        .col(
            ColumnDef::new(Alias::new("id"))
                .big_integer()
                .not_null()
                .primary_key()
                .auto_increment(),
        )
        .col(string_len(Alias::new("call_id"), 255).not_null())
        .col(string_len_null(Alias::new("display_id"), 255))
        .col(string_len(Alias::new("direction"), 32).not_null())
        .col(string_len(Alias::new("status"), 32).not_null())
        .col(timestamp(Alias::new("started_at")).not_null())
        .col(timestamp_null(Alias::new("ended_at")))
        .col(integer(Alias::new("duration_secs")).not_null())
        .col(string_len_null(Alias::new("from_number"), 128))
        .col(string_len_null(Alias::new("to_number"), 128))
        .col(string_len_null(Alias::new("caller_name"), 255))
        .col(string_len_null(Alias::new("agent_name"), 255))
        .col(string_len_null(Alias::new("queue"), 255))
        .col(big_integer_null(Alias::new("department_id")))
        .col(big_integer_null(Alias::new("extension_id")))
        .col(big_integer_null(Alias::new("sip_trunk_id")))
        .col(big_integer_null(Alias::new("outbound_sip_trunk_id")))
        .col(big_integer_null(Alias::new("route_id")))
        .col(string_len_null(Alias::new("sip_gateway"), 255))
        .col(string_len_null(Alias::new("rewrite_original_from"), 255))
        .col(string_len_null(Alias::new("rewrite_original_to"), 255))
        .col(string_len_null(Alias::new("caller_uri"), 255))
        .col(string_len_null(Alias::new("callee_uri"), 255))
        .col(string_len_null(Alias::new("recording_url"), 1024))
        .col(integer_null(Alias::new("recording_duration_secs")))
        .col(boolean(Alias::new("has_transcript")).not_null())
        .col(string_len(Alias::new("transcript_status"), 64).not_null())
        .col(string_len_null(Alias::new("transcript_language"), 64))
        .col(json_null(Alias::new("tags")))
        .col(json_null(Alias::new("leg_timeline")))
        .col(json_null(Alias::new("metadata")))
        .col(timestamp(Alias::new("created_at")).not_null())
        .col(timestamp(Alias::new("updated_at")).not_null())
        .col(timestamp_null(Alias::new("archived_at")))
        .to_owned();

    db.execute_raw(db.get_database_backend().build(&create))
        .await?;

    // Add unique index on call_id + basic query indexes (must run after CREATE TABLE)
    let indexes = [
        Index::create()
            .table(Alias::new(table_name))
            .name(format!("idx_{}_call_id", table_name))
            .col(Alias::new("call_id"))
            .unique()
            .to_owned(),
        Index::create()
            .table(Alias::new(table_name))
            .name(format!("idx_{}_started_at", table_name))
            .col(Alias::new("started_at"))
            .to_owned(),
        Index::create()
            .table(Alias::new(table_name))
            .name(format!("idx_{}_status", table_name))
            .col(Alias::new("status"))
            .to_owned(),
    ];
    for index in indexes {
        let stmt = db.get_database_backend().build(&index);
        if let Err(e) = db.execute_raw(stmt).await {
            warn!("failed to create index on {}: {}", table_name, e);
        }
    }
    info!(table = %table_name, "call record table created");
    Ok(())
}

/// Current local date as `YYYYMMDD` string.
pub fn today_string() -> String {
    Utc::now().format("%Y%m%d").to_string()
}

/// Derive a daily SQLite file URL from a base URL.
/// Handles both `sqlite:///path/to/db` and `sqlite:/path/to/db` formats.
/// `sqlite:///config/cdr/cdr.db` → `sqlite:///config/cdr/cdr-{date}.db`
pub fn derive_daily_url(base_url: &str, date: &str) -> String {
    // Try sqlite:// first, then sqlite: (single slash)
    let remaining = base_url
        .strip_prefix("sqlite://")
        .or_else(|| base_url.strip_prefix("sqlite:"));
    let Some(path) = remaining else {
        return base_url.to_string();
    };
    let prefix = if base_url.starts_with("sqlite://") {
        "sqlite://"
    } else {
        "sqlite:"
    };
    let p = std::path::Path::new(path);
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let ext = p.extension().unwrap_or_default().to_string_lossy();
    let parent = p.parent().unwrap_or_else(|| std::path::Path::new("."));
    let filename = if ext.is_empty() {
        format!("{}-{}", stem, date)
    } else {
        format!("{}-{}.{}", stem, date, ext)
    };
    let full = parent.join(&filename);
    format!("{}{}", prefix, full.display())
}

impl CallRecordManager {
    pub async fn serve(&mut self) {
        let token = self.cancel_token.clone();
        let batch_size = self.batch_size.max(1);
        info!(batch_size, "CallRecordManager serving");
        let receiver = &mut self.receiver;
        let saver = self.saver.as_ref();
        let hooks = &self.hooks;
        let mut records = Vec::with_capacity(batch_size);
        let mut receiver_closed = false;
        let mut shutting_down = false;
        let mut shutdown_timeout: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(pending());

        loop {
            if receiver_closed {
                break;
            }

            records.clear();
            tokio::select! {
                received = receiver.recv_many(&mut records, batch_size) => {
                    if received == 0 {
                        receiver_closed = true;
                        continue;
                    }

                    let started_at = Instant::now();
                    for hook in hooks {
                        if let Err(e) = hook.on_record_enrich(&mut records).await {
                            warn!(batch_size = records.len(), "CallRecordHook enrich failed: {}", e);
                        }
                    }

                    match saver.save(&records).await {
                        Ok(file_names) => {
                            if file_names.len() != records.len() {
                                warn!(
                                    batch_size = records.len(),
                                    paths = file_names.len(),
                                    "CallRecordSaver returned an unexpected number of paths"
                                );
                            }
                            for (record, file_name) in records.iter_mut().zip(file_names) {
                                record.details.cdr_file_path = Some(file_name);
                            }
                            info!(
                                elapsed = ?started_at.elapsed(),
                                batch_size = records.len(),
                                "CallRecordManager saved batch"
                            );
                        }
                        Err(err) => {
                            warn!(batch_size = records.len(), "Failed to save call record batch: {}", err);
                        }
                    }

                    for hook in hooks {
                        if let Err(e) = hook.on_record_completed(&mut records).await {
                            warn!(batch_size = records.len(), "CallRecordHook failed: {}", e);
                        }
                    }
                }
                _ = token.cancelled(), if !shutting_down => {
                    shutting_down = true;
                    info!(pending = receiver.len(), "CallRecordManager received shutdown");
                    shutdown_timeout = Box::pin(sleep(Duration::from_secs(5)));
                }
                _ = &mut shutdown_timeout, if shutting_down => {
                    warn!(
                        pending = receiver.len(),
                        "CallRecordManager shutdown timed out before the channel drained"
                    );
                    break;
                }
            }
        }
        info!(pending = receiver.len(), "CallRecordManager exiting");
    }
}

// ── Model conversion ──────────────────────────────────────────────────

impl From<rustpbx_models::call_record::Model> for CallRecord {
    fn from(val: rustpbx_models::call_record::Model) -> Self {
        let details = CallDetails {
            direction: val.direction,
            status: val.status,
            from_number: val.from_number.clone(),
            to_number: val.to_number.clone(),
            caller_name: val.caller_name,
            agent_name: val.agent_name,
            queue: val.queue,
            department_id: val.department_id,
            extension_id: val.extension_id,
            sip_trunk_id: val.sip_trunk_id,
            outbound_sip_trunk_id: val.outbound_sip_trunk_id,
            route_id: val.route_id,
            sip_gateway: val.sip_gateway,
            recording_url: val.recording_url,
            recording_duration_secs: val.recording_duration_secs,
            has_transcript: val.has_transcript,
            transcript_status: Some(val.transcript_status),
            transcript_language: val.transcript_language,
            tags: val.tags,
            metadata: val.metadata.and_then(|v| serde_json::from_value(v).ok()),
            cdr_file_path: None,
            rewrite: CallRecordRewrite {
                caller_original: val.rewrite_original_from.unwrap_or_default(),
                caller_final: String::new(),
                callee_original: val.rewrite_original_to.unwrap_or_default(),
                callee_final: String::new(),
                contact: None,
                destination: None,
            },
            last_error: None,
        };

        let leg_timeline = val
            .leg_timeline
            .and_then(|json| serde_json::from_value(json).ok())
            .unwrap_or_default();

        CallRecord {
            call_id: val.call_id,
            session_id: None,
            start_time: val.started_at,
            ring_time: None,
            answer_time: None,
            end_time: val.ended_at.unwrap_or(val.started_at),
            caller: val
                .caller_uri
                .unwrap_or_else(|| val.from_number.unwrap_or_default()),
            callee: val
                .callee_uri
                .unwrap_or_else(|| val.to_number.unwrap_or_default()),
            status_code: 0,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            recorder: Vec::new(),
            sip_leg_roles: std::collections::HashMap::new(),
            leg_timeline,
            details,
            extensions: http::Extensions::new(),
        }
    }
}
