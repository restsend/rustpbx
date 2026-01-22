use crate::{
    config::{CallRecordConfig, S3Vendor},
    utils::sanitize_id,
};
use anyhow::{Error, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest;
use sea_orm::prelude::DateTimeUtc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, future::Future, path::Path, pin::Pin, sync::Arc, time::Instant};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub mod sipflow;
pub mod storage;
#[cfg(test)]
mod tests;

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
    #[serde(flatten, default)]
    pub details: CallDetails,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub extensions: http::Extensions,
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

impl std::str::FromStr for CallRecordHangupReason {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "caller" => Ok(Self::ByCaller),
            "callee" => Ok(Self::ByCallee),
            "refer" => Ok(Self::ByRefer),
            "system" => Ok(Self::BySystem),
            "autohangup" => Ok(Self::Autohangup),
            "noAnswer" => Ok(Self::NoAnswer),
            "noBalance" => Ok(Self::NoBalance),
            "answerMachine" => Ok(Self::AnswerMachine),
            "serverUnavailable" => Ok(Self::ServerUnavailable),
            "canceled" => Ok(Self::Canceled),
            "rejected" => Ok(Self::Rejected),
            "failed" => Ok(Self::Failed),
            _ => Ok(Self::Other(s.to_string())),
        }
    }
}

impl ToString for CallRecordHangupReason {
    fn to_string(&self) -> String {
        match self {
            Self::ByCaller => "caller".to_string(),
            Self::ByCallee => "callee".to_string(),
            Self::ByRefer => "refer".to_string(),
            Self::BySystem => "system".to_string(),
            Self::Autohangup => "autohangup".to_string(),
            Self::NoAnswer => "noAnswer".to_string(),
            Self::NoBalance => "noBalance".to_string(),
            Self::AnswerMachine => "answerMachine".to_string(),
            Self::ServerUnavailable => "serverUnavailable".to_string(),
            Self::Canceled => "canceled".to_string(),
            Self::Rejected => "rejected".to_string(),
            Self::Failed => "failed".to_string(),
            Self::Other(s) => s.to_string(),
        }
    }
}

#[async_trait::async_trait]
pub trait CallRecordHook: Send + Sync {
    async fn on_record_completed(&self, record: &mut CallRecord) -> anyhow::Result<()>;
}

pub type CallRecordSender = tokio::sync::mpsc::UnboundedSender<CallRecord>;
pub type CallRecordReceiver = tokio::sync::mpsc::UnboundedReceiver<CallRecord>;

pub type FnSaveCallRecord = Arc<
    Box<
        dyn Fn(
                CancellationToken,
                Arc<dyn CallRecordFormatter>,
                Arc<CallRecordConfig>,
                CallRecord,
            ) -> Pin<Box<dyn Future<Output = CallRecordSaveResult> + Send>>
            + Send
            + Sync,
    >,
>;

type CallRecordSaveResult = std::result::Result<(CallRecord, String), (CallRecord, Error)>;

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
    match serde_json::to_string(&event) {
        Ok(line) => {
            file.write_all(format!("{}\n", line).as_bytes()).await.ok();
        }
        Err(_) => {}
    }
}

pub fn default_cdr_file_name(record: &CallRecord) -> String {
    format!("{}.json", sanitize_id(&record.call_id))
}

pub fn default_transcript_file_name(record: &CallRecord) -> String {
    format!("{}.transcript.json", sanitize_id(&record.call_id))
}

pub trait CallRecordFormatter: Send + Sync {
    fn format(&self, record: &CallRecord) -> Result<String> {
        Ok(serde_json::to_string(record)?)
    }
    fn format_file_name(&self, record: &CallRecord) -> String;
    fn format_transcript_path(&self, record: &CallRecord) -> String;
    fn format_media_path(&self, record: &CallRecord, media: &CallRecordMedia) -> String;
}

pub struct DefaultCallRecordFormatter {
    pub root: String,
}

impl Default for DefaultCallRecordFormatter {
    fn default() -> Self {
        Self {
            root: "./config/cdr".to_string(),
        }
    }
}

impl DefaultCallRecordFormatter {
    pub fn new_with_config(config: &CallRecordConfig) -> Self {
        let root = match config {
            CallRecordConfig::Local { root } => root.clone(),
            CallRecordConfig::S3 { root, .. } => root.clone(),
            _ => "./config/cdr".to_string(),
        };
        Self { root }
    }
}

impl CallRecordFormatter for DefaultCallRecordFormatter {
    fn format_file_name(&self, record: &CallRecord) -> String {
        let trimmed_root = self.root.trim_end_matches('/');
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

    fn format_transcript_path(&self, record: &CallRecord) -> String {
        let trimmed_root = self.root.trim_end_matches('/');
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
    fn format_media_path(&self, record: &CallRecord, media: &CallRecordMedia) -> String {
        let file_name = Path::new(&media.path)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
            .to_string_lossy()
            .to_string();

        format!(
            "{}/{}/{}_{}_{}",
            self.root.trim_end_matches('/'),
            record.start_time.format("%Y%m%d"),
            record.call_id,
            media.track_id,
            file_name
        )
    }
}

pub struct CallRecordManager {
    pub max_concurrent: usize,
    pub sender: CallRecordSender,
    pub stats: Arc<CallRecordStats>,
    config: Arc<CallRecordConfig>,
    cancel_token: CancellationToken,
    receiver: CallRecordReceiver,
    saver_fn: FnSaveCallRecord,
    formatter: Arc<dyn CallRecordFormatter>,
    hooks: Arc<Vec<Box<dyn CallRecordHook>>>,
}

pub struct CallRecordManagerBuilder {
    pub cancel_token: Option<CancellationToken>,
    pub config: Option<CallRecordConfig>,
    pub max_concurrent: Option<usize>,
    saver_fn: Option<FnSaveCallRecord>,
    formatter: Option<Arc<dyn CallRecordFormatter>>,
    hooks: Vec<Box<dyn CallRecordHook>>,
}

impl CallRecordManagerBuilder {
    pub fn new() -> Self {
        Self {
            cancel_token: None,
            config: None,
            max_concurrent: None,
            saver_fn: None,
            formatter: None,
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

    pub fn with_saver(mut self, saver: FnSaveCallRecord) -> Self {
        self.saver_fn = Some(saver);
        self
    }

    pub fn with_formatter(mut self, formatter: Arc<dyn CallRecordFormatter>) -> Self {
        self.formatter = Some(formatter);
        self
    }

    pub fn with_hook(mut self, hook: Box<dyn CallRecordHook>) -> Self {
        self.hooks.push(hook);
        self
    }
    pub fn with_max_concurrent(mut self, max_concurrent: usize) -> Self {
        self.max_concurrent = Some(max_concurrent);
        self
    }

    pub fn build(self) -> CallRecordManager {
        let cancel_token = self.cancel_token.unwrap_or_default();
        let config = Arc::new(self.config.unwrap_or_default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let saver_fn = self
            .saver_fn
            .unwrap_or_else(|| Arc::new(Box::new(CallRecordManager::default_saver)));
        let formatter = self
            .formatter
            .unwrap_or_else(|| Arc::new(DefaultCallRecordFormatter::default()));
        let max_concurrent = self.max_concurrent.unwrap_or(64);

        match config.as_ref() {
            CallRecordConfig::Local { root } => {
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
            }
            _ => {}
        }

        CallRecordManager {
            max_concurrent,
            stats: Arc::new(CallRecordStats::new()),
            cancel_token,
            sender,
            receiver,
            config,
            saver_fn,
            formatter,
            hooks: Arc::new(self.hooks),
        }
    }
}

impl CallRecordManager {
    fn default_saver(
        _cancel_token: CancellationToken,
        formatter: Arc<dyn CallRecordFormatter>,
        config: Arc<CallRecordConfig>,
        record: CallRecord,
    ) -> Pin<Box<dyn Future<Output = CallRecordSaveResult> + Send>> {
        Box::pin(async move {
            let mut record = record;
            let start_time = Instant::now();
            let result = match config.as_ref() {
                CallRecordConfig::Local { .. } => {
                    Self::save_local_record(formatter.clone(), &mut record).await
                }
                CallRecordConfig::S3 {
                    vendor,
                    bucket,
                    region,
                    access_key,
                    secret_key,
                    endpoint,
                    with_media,
                    keep_media_copy,
                    ..
                } => {
                    Self::save_with_s3_like(
                        formatter.clone(),
                        vendor,
                        bucket,
                        region,
                        access_key,
                        secret_key,
                        endpoint,
                        with_media,
                        keep_media_copy,
                        &record,
                    )
                    .await
                }
                CallRecordConfig::Http {
                    url,
                    headers,
                    with_media,
                    keep_media_copy,
                } => {
                    Self::save_with_http(
                        formatter.clone(),
                        url,
                        headers,
                        with_media,
                        keep_media_copy,
                        &record,
                    )
                    .await
                }
            };
            let file_name = match result {
                Ok(file_name) => file_name,
                Err(e) => return Err((record, e)),
            };
            let elapsed = start_time.elapsed();
            info!(
                ?elapsed,
                call_id = record.call_id,
                file_name,
                "CallRecordManager saved"
            );
            Ok((record, file_name))
        })
    }

    async fn save_local_record(
        formatter: Arc<dyn CallRecordFormatter>,
        record: &mut CallRecord,
    ) -> Result<String> {
        let file_content = formatter.format(record)?;
        let file_name = formatter.format_file_name(record);

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

    async fn save_with_http(
        formatter: Arc<dyn CallRecordFormatter>,
        url: &String,
        headers: &Option<HashMap<String, String>>,
        with_media: &Option<bool>,
        keep_media_copy: &Option<bool>,
        record: &CallRecord,
    ) -> Result<String> {
        let client = reqwest::Client::new();
        // Serialize call record to JSON
        let call_log_json = formatter.format(record)?;
        // Create multipart form
        let mut form = reqwest::multipart::Form::new().text("calllog.json", call_log_json);

        // Add media files if with_media is true
        if with_media.unwrap_or(false) {
            for media in &record.recorder {
                if std::path::Path::new(&media.path).exists() {
                    match tokio::fs::read(&media.path).await {
                        Ok(file_content) => {
                            let file_name = std::path::Path::new(&media.path)
                                .file_name()
                                .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
                                .to_string_lossy()
                                .to_string();

                            let part = match reqwest::multipart::Part::bytes(file_content)
                                .file_name(file_name.clone())
                                .mime_str("application/octet-stream")
                            {
                                Ok(part) => part,
                                Err(_) => {
                                    // Fallback to default MIME type if parsing fails
                                    reqwest::multipart::Part::bytes(
                                        tokio::fs::read(&media.path).await?,
                                    )
                                    .file_name(file_name)
                                }
                            };

                            form = form.part(format!("media_{}", media.track_id), part);
                        }
                        Err(e) => {
                            warn!("Failed to read media file {}: {}", media.path, e);
                        }
                    }
                }
            }
        }
        let mut request = client.post(url).multipart(form);
        if let Some(headers_map) = headers {
            for (key, value) in headers_map {
                request = request.header(key, value);
            }
        }
        let response = request.send().await?;
        if response.status().is_success() {
            let response_text = response.text().await.unwrap_or_default();

            if keep_media_copy.unwrap_or(false) {
                for media in &record.recorder {
                    let p = Path::new(&media.path);
                    if p.exists() {
                        tokio::fs::remove_file(p).await.ok();
                    }
                }
            }
            Ok(format!("HTTP upload successful: {}", response_text))
        } else {
            Err(anyhow::anyhow!(
                "HTTP upload failed with status: {} - {}",
                response.status(),
                response.text().await.unwrap_or_default()
            ))
        }
    }

    async fn save_with_s3_like(
        formatter: Arc<dyn CallRecordFormatter>,
        vendor: &S3Vendor,
        bucket: &String,
        region: &String,
        access_key: &String,
        secret_key: &String,
        endpoint: &String,
        with_media: &Option<bool>,
        keep_media_copy: &Option<bool>,
        record: &CallRecord,
    ) -> Result<String> {
        let start_time = Instant::now();
        let storage_config = crate::storage::StorageConfig::S3 {
            vendor: vendor.clone(),
            bucket: bucket.clone(),
            region: region.clone(),
            access_key: access_key.clone(),
            secret_key: secret_key.clone(),
            endpoint: Some(endpoint.clone()),
            prefix: None,
        };
        let storage = crate::storage::Storage::new(&storage_config)?;

        // Serialize call record to JSON
        let call_log_json = formatter.format(record)?;
        // Upload call log JSON
        let filename = formatter.format_file_name(record);
        let local_files = vec![filename.clone()];
        let buf_size = call_log_json.len();
        match storage.write(&filename, call_log_json.into()).await {
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
        // Upload media files if with_media is true
        if with_media.unwrap_or(false) {
            let mut media_files = vec![];
            for media in &record.recorder {
                if Path::new(&media.path).exists() {
                    let media_path = formatter.format_media_path(record, media);
                    media_files.push((media.path.clone(), media_path));
                }
            }
            for (path, media_path) in &media_files {
                let start_time = Instant::now();
                let file_content = match tokio::fs::read(path).await {
                    Ok(file_content) => file_content,
                    Err(e) => {
                        warn!("failed to read media file {}: {}", path, e);
                        continue;
                    }
                };
                let buf_size = file_content.len();
                match storage.write(media_path, file_content.into()).await {
                    Ok(_) => {
                        info!(
                            elapsed = start_time.elapsed().as_secs_f64(),
                            media_path, buf_size, "upload media file"
                        );
                    }
                    Err(e) => {
                        warn!(media_path, "failed to upload media file: {}", e);
                    }
                }
            }
        }
        // Optionally delete local media files if keep_media_copy is false
        if !keep_media_copy.unwrap_or(false) {
            for media in &record.recorder {
                let p = Path::new(&media.path);
                if p.exists() {
                    tokio::fs::remove_file(p).await.ok();
                }
            }
            for file_name in &local_files {
                let p = Path::new(file_name);
                if p.exists() {
                    tokio::fs::remove_file(p).await.ok();
                }
            }
        }

        Ok(format!(
            "{}/{}",
            endpoint.trim_end_matches('/'),
            filename.trim_start_matches('/')
        ))
    }

    pub async fn serve(&mut self) {
        let token = self.cancel_token.clone();
        info!("CallRecordManager serving");
        tokio::select! {
            _ = token.cancelled() => {}
            _ = self.recv_loop() => {}
        }
        info!("CallRecordManager served");
    }

    async fn recv_loop(&mut self) -> Result<()> {
        let mut futures = FuturesUnordered::new();
        loop {
            let limit = self.max_concurrent - futures.len();
            if limit == 0 {
                if let Some(_) = futures.next().await {}
                continue;
            }
            let mut buffer = Vec::with_capacity(limit);
            if self.receiver.recv_many(&mut buffer, limit).await == 0 {
                break;
            }

            for record in buffer {
                let cancel_token_ref = self.cancel_token.clone();
                let save_fn_ref = self.saver_fn.clone();
                let config_ref = self.config.clone();
                let formatter_ref = self.formatter.clone();
                let hooks_ref = self.hooks.clone();

                futures.push(async move {
                    let save_outcome =
                        save_fn_ref(cancel_token_ref, formatter_ref, config_ref, record).await;
                    let mut record = match save_outcome {
                        Ok((record, _file_name)) => record,
                        Err((record, err)) => {
                            warn!("Failed to save call record: {}", err);
                            record
                        }
                    };

                    for hook in hooks_ref.iter() {
                        if let Err(e) = hook.on_record_completed(&mut record).await {
                            warn!("CallRecordHook failed: {}", e);
                        }
                    }
                });
            }
            while let Some(_) = futures.next().await {}
        }
        Ok(())
    }
}
