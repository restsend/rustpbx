use crate::{
    call::{ActiveCallType, CallOption},
    config::{CallRecordConfig, S3Vendor},
    models::call_record::{self, Column as CallRecordColumn, Entity as CallRecordEntity},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use object_store::{
    ObjectStore, aws::AmazonS3Builder, azure::MicrosoftAzureBuilder,
    gcp::GoogleCloudStorageBuilder, path::Path as ObjectPath,
};
use reqwest;
use rsip::Uri;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value, json};
use serde_with::skip_serializing_none;
use std::{
    collections::HashMap, convert::TryFrom, future::Future, path::Path, pin::Pin, str::FromStr,
    sync::Arc, time::Instant,
};
use tokio::{fs::File, io::AsyncWriteExt, select};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[cfg(test)]
mod tests;

pub type CallRecordSender = tokio::sync::mpsc::UnboundedSender<CallRecord>;
pub type CallRecordReceiver = tokio::sync::mpsc::UnboundedReceiver<CallRecord>;

pub type FnSaveCallRecord = Arc<
    Box<
        dyn Fn(
                CancellationToken,
                Arc<dyn CallRecordFormatter>,
                Arc<CallRecordConfig>,
                CallRecord,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
            + Send
            + Sync,
    >,
>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CallRecordEventType {
    Event,
    Command,
    Sip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecordEvent<'a> {
    pub r#type: CallRecordEventType,
    pub timestamp: u64,
    pub content: &'a str,
}

impl<'a> CallRecordEvent<'a> {
    pub fn new(r#type: CallRecordEventType, content: &'a str) -> Self {
        Self {
            r#type,
            timestamp: crate::get_timestamp(),
            content,
        }
    }
    pub async fn write_to_file(&self, file: &mut File) {
        match serde_json::to_string(self) {
            Ok(line) => {
                file.write_all(format!("{}\n", line).as_bytes()).await.ok();
            }
            Err(_) => {}
        }
    }
}
#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CallRecord {
    pub call_type: ActiveCallType,
    pub option: Option<CallOption>,
    pub call_id: String,
    pub start_time: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub end_time: DateTime<Utc>,
    pub caller: String,
    pub callee: String,
    pub status_code: u16,
    pub offer: Option<String>,
    pub answer: Option<String>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recorder: Vec<CallRecordMedia>,
    pub extras: Option<HashMap<String, serde_json::Value>>,
    pub dump_event_file: Option<String>,
    pub refer_callrecord: Option<Box<CallRecord>>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl FromStr for CallRecordHangupReason {
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
pub trait CallRecordFormatter: Send + Sync {
    fn format_file_name(&self, root: &str, record: &CallRecord) -> String {
        format!(
            "{}/{}_{}.json",
            root.trim_end_matches('/'),
            record.start_time.format("%Y%m%d-%H%M%S"),
            record.call_id
        )
    }

    fn format(&self, record: &CallRecord) -> Result<String> {
        Ok(serde_json::to_string(record)?)
    }

    fn format_dump_events_path(&self, root: &str, record: &CallRecord) -> String {
        format!(
            "{}/{}_{}.events.jsonl",
            root.trim_end_matches('/'),
            record.start_time.format("%Y%m%d"),
            record.call_id
        )
    }
    fn format_media_path(
        &self,
        root: &str,
        record: &CallRecord,
        media: &CallRecordMedia,
    ) -> String {
        let file_name = Path::new(&media.path)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
            .to_string_lossy()
            .to_string();

        format!(
            "{}/{}_{}_{}",
            root.trim_end_matches('/'),
            record.call_id,
            media.track_id,
            file_name
        )
    }
}

pub struct DefaultCallRecordFormatter;
impl CallRecordFormatter for DefaultCallRecordFormatter {}

pub struct CallRecordManager {
    pub sender: CallRecordSender,
    config: Arc<CallRecordConfig>,
    cancel_token: CancellationToken,
    receiver: CallRecordReceiver,
    saver_fn: FnSaveCallRecord,
    formatter: Arc<dyn CallRecordFormatter>,
}

pub struct CallRecordManagerBuilder {
    pub cancel_token: Option<CancellationToken>,
    pub config: Option<CallRecordConfig>,
    saver_fn: Option<FnSaveCallRecord>,
    formatter: Option<Arc<dyn CallRecordFormatter>>,
}

impl CallRecordManagerBuilder {
    pub fn new() -> Self {
        Self {
            cancel_token: None,
            config: None,
            saver_fn: None,
            formatter: None,
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

    pub fn build(self) -> CallRecordManager {
        let cancel_token = self.cancel_token.unwrap_or_default();
        let config = Arc::new(self.config.unwrap_or_default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let saver_fn = self
            .saver_fn
            .unwrap_or_else(|| Arc::new(Box::new(CallRecordManager::default_saver)));
        let formatter = self
            .formatter
            .unwrap_or_else(|| Arc::new(DefaultCallRecordFormatter));

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
            cancel_token,
            sender,
            receiver,
            config,
            saver_fn,
            formatter,
        }
    }
}

impl CallRecordManager {
    fn default_saver(
        _cancel_token: CancellationToken,
        formatter: Arc<dyn CallRecordFormatter>,
        config: Arc<CallRecordConfig>,
        record: CallRecord,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(async move {
            let start_time = Instant::now();
            let r = match config.as_ref() {
                CallRecordConfig::Local { root } => {
                    let file_content = formatter.format(&record)?;
                    let file_name = formatter.format_file_name(root, &record);
                    let mut file = File::create(&file_name).await.map_err(|e| {
                        anyhow::anyhow!("Failed to create call record file {}: {}", file_name, e)
                    })?;
                    file.write_all(file_content.as_bytes()).await?;
                    file.flush().await?;
                    Ok(file_name.to_string())
                }
                CallRecordConfig::S3 {
                    vendor,
                    bucket,
                    region,
                    access_key,
                    secret_key,
                    endpoint,
                    root,
                    with_media,
                    keep_media_copy,
                } => {
                    Self::save_with_s3_like(
                        formatter,
                        vendor,
                        bucket,
                        region,
                        access_key,
                        secret_key,
                        endpoint,
                        root,
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
                        formatter,
                        url,
                        headers,
                        with_media,
                        keep_media_copy,
                        &record,
                    )
                    .await
                }
            };
            let file_name = match r {
                Ok(file_name) => file_name,
                Err(e) => {
                    warn!("Failed to save call record: {}", e);
                    return Err(e);
                }
            };
            let elapsed = start_time.elapsed();
            info!(
                ?elapsed,
                call_id = record.call_id,
                file_name,
                "CallRecordManager saved"
            );
            Ok(())
        })
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
            if let Some(dump_events_file) = &record.dump_event_file {
                if Path::new(&dump_events_file).exists() {
                    let file_name = Path::new(&dump_events_file)
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
                        .to_string_lossy()
                        .to_string();
                    match reqwest::multipart::Part::bytes(tokio::fs::read(&dump_events_file).await?)
                        .file_name(file_name.clone())
                        .mime_str("application/octet-stream")
                    {
                        Ok(part) => {
                            form = form.part(format!("dump_events_{}", file_name), part);
                        }
                        Err(_) => {}
                    };
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
        root: &String,
        with_media: &Option<bool>,
        keep_media_copy: &Option<bool>,
        record: &CallRecord,
    ) -> Result<String> {
        // Create object store based on vendor
        let object_store: Arc<dyn ObjectStore> = match vendor {
            S3Vendor::AWS => {
                let builder = AmazonS3Builder::new()
                    .with_bucket_name(bucket)
                    .with_region(region)
                    .with_access_key_id(access_key)
                    .with_secret_access_key(secret_key);

                let store = if !endpoint.is_empty() {
                    builder.with_endpoint(endpoint).build()?
                } else {
                    builder.build()?
                };
                Arc::new(store)
            }
            S3Vendor::GCP => {
                let store = GoogleCloudStorageBuilder::new()
                    .with_bucket_name(bucket)
                    .with_service_account_key(secret_key) // For GCP, secret_key is the service account key
                    .build()?;
                Arc::new(store)
            }
            S3Vendor::Azure => {
                let store = MicrosoftAzureBuilder::new()
                    .with_container_name(bucket)
                    .with_account(access_key)
                    .with_access_key(secret_key)
                    .build()?;
                Arc::new(store)
            }
            S3Vendor::Aliyun | S3Vendor::Tencent | S3Vendor::Minio | S3Vendor::DigitalOcean => {
                // These vendors are S3-compatible, use AmazonS3Builder with custom endpoint
                let store = AmazonS3Builder::new()
                    .with_bucket_name(bucket)
                    .with_region(region)
                    .with_access_key_id(access_key)
                    .with_secret_access_key(secret_key)
                    .with_endpoint(endpoint)
                    .with_virtual_hosted_style_request(false) // Use path-style for compatibility
                    .build()?;
                Arc::new(store)
            }
        };

        // Serialize call record to JSON
        let call_log_json = formatter.format(record)?;

        // Upload call log JSON
        let json_path = ObjectPath::from(formatter.format_file_name(root, record));
        match object_store.put(&json_path, call_log_json.into()).await {
            Ok(r) => {
                info!("Upload call record: {:?}", r);
            }
            Err(e) => {
                warn!("Failed to upload call record: {}", e);
            }
        }
        let mut uploaded_files = vec![(json_path.to_string(), json_path.clone())];
        // Upload media files if with_media is true
        if with_media.unwrap_or(false) {
            let mut media_files = vec![];
            for media in &record.recorder {
                if Path::new(&media.path).exists() {
                    let media_path =
                        ObjectPath::from(formatter.format_media_path(root, record, media));
                    media_files.push((media.path.clone(), media_path));
                }
            }
            if let Some(dump_events_file) = &record.dump_event_file {
                if Path::new(&dump_events_file).exists() {
                    let dump_events_path =
                        ObjectPath::from(formatter.format_dump_events_path(root, record));
                    media_files.push((dump_events_file.clone(), dump_events_path));
                }
            }
            for (path, media_path) in &media_files {
                let file_content = match tokio::fs::read(path).await {
                    Ok(file_content) => file_content,
                    Err(e) => {
                        warn!("Failed to read media file {}: {}", path, e);
                        continue;
                    }
                };
                match object_store.put(media_path, file_content.into()).await {
                    Ok(r) => {
                        info!("Upload media file: {:?}", r);
                    }
                    Err(e) => {
                        warn!("Failed to upload media file: {}", e);
                    }
                }
            }
            uploaded_files.extend(media_files);
        }
        // Optionally delete local media files if keep_media_copy is false
        if keep_media_copy.unwrap_or(false) {
            for media in &record.recorder {
                let p = Path::new(&media.path);
                if p.exists() {
                    tokio::fs::remove_file(p).await.ok();
                }
            }
        }

        Ok(format!(
            "{}/{}",
            endpoint.trim_end_matches('/'),
            json_path.to_string().trim_start_matches('/')
        ))
    }

    pub async fn serve(&mut self) {
        let token = self.cancel_token.clone();
        info!("CallRecordManager serving");
        select! {
            _ = self.cancel_token.cancelled() => {
                info!("CallRecordManager cancelled");
            }
            _ = Self::recv_loop(
                token,
                self.formatter.clone(),
                self.config.clone(),
                self.saver_fn.clone(),
                &mut self.receiver,
            ) => {
                info!("CallRecordManager received done");
            }
        }
        info!("CallRecordManager served");
    }

    async fn recv_loop(
        cancel_token: CancellationToken,
        formatter: Arc<dyn CallRecordFormatter>,
        config: Arc<CallRecordConfig>,
        saver_fn: FnSaveCallRecord,
        receiver: &mut CallRecordReceiver,
    ) -> Result<()> {
        while let Some(record) = receiver.recv().await {
            let cancel_token_ref = cancel_token.clone();
            let save_fn_ref = saver_fn.clone();
            let config_ref = config.clone();
            let formatter_ref = formatter.clone();
            tokio::spawn(async move {
                select! {
                    _ = cancel_token_ref.cancelled() => {
                        info!("CallRecordManager cancelled");
                    }
                    r = save_fn_ref(cancel_token_ref.clone(), formatter_ref, config_ref, record) => {
                        match r {
                            Ok(_) => {
                            }
                            Err(e) => {
                                warn!("Failed to save call record: {}", e);
                            }
                        }
                    }
                }
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CallRecordPersistArgs {
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
    pub transcript_status: String,
    pub transcript_language: Option<String>,
    pub tags: Option<Value>,
    pub quality_mos: Option<f64>,
    pub quality_latency_ms: Option<f64>,
    pub quality_jitter_ms: Option<f64>,
    pub quality_packet_loss_percent: Option<f64>,
    pub analytics: Option<Value>,
    pub metadata: Option<Value>,
}

impl Default for CallRecordPersistArgs {
    fn default() -> Self {
        Self {
            direction: "unknown".to_string(),
            status: "failed".to_string(),
            from_number: None,
            to_number: None,
            caller_name: None,
            agent_name: None,
            queue: None,
            department_id: None,
            extension_id: None,
            sip_trunk_id: None,
            route_id: None,
            sip_gateway: None,
            recording_url: None,
            recording_duration_secs: None,
            has_transcript: false,
            transcript_status: "pending".to_string(),
            transcript_language: None,
            tags: None,
            quality_mos: None,
            quality_latency_ms: None,
            quality_jitter_ms: None,
            quality_packet_loss_percent: None,
            analytics: None,
            metadata: None,
        }
    }
}

pub async fn persist_call_record(
    db: &DatabaseConnection,
    record: &CallRecord,
    args: CallRecordPersistArgs,
) -> Result<()> {
    let direction = args.direction.trim().to_ascii_lowercase();
    let status = args.status.trim().to_ascii_lowercase();
    let from_number = args.from_number.clone();
    let to_number = args.to_number.clone();
    let caller_name = args.caller_name.clone();
    let agent_name = args.agent_name.clone();
    let queue = args.queue.clone();
    let department_id = args.department_id;
    let extension_id = args.extension_id;
    let sip_trunk_id = args.sip_trunk_id;
    let route_id = args.route_id;
    let sip_gateway = args.sip_gateway.clone();
    let recording_url = args.recording_url.clone();
    let recording_duration_secs = args.recording_duration_secs;
    let tags = args.tags.clone();
    let analytics = args.analytics.clone();
    let metadata_value = merge_metadata(record, args.metadata.clone());
    let signaling_value = build_signaling_payload(record);
    let duration_secs = (record.end_time - record.start_time).num_seconds().max(0) as i32;
    let display_id = record
        .option
        .as_ref()
        .and_then(|opt| opt.extra.as_ref())
        .and_then(|extra| extra.get("display_id").cloned());
    let caller_uri = normalize_endpoint_uri(&record.caller);
    let callee_uri = normalize_endpoint_uri(&record.callee);
    if let Some(model) = CallRecordEntity::find()
        .filter(CallRecordColumn::CallId.eq(record.call_id.clone()))
        .one(db)
        .await?
    {
        let mut active: call_record::ActiveModel = model.into();
        active.display_id = Set(display_id.clone());
        active.direction = Set(direction.clone());
        active.status = Set(status.clone());
        active.started_at = Set(record.start_time);
        active.ended_at = Set(Some(record.end_time));
        active.duration_secs = Set(duration_secs);
        active.from_number = Set(from_number.clone());
        active.to_number = Set(to_number.clone());
        active.caller_name = Set(caller_name.clone());
        active.agent_name = Set(agent_name.clone());
        active.queue = Set(queue.clone());
        active.department_id = Set(department_id);
        active.extension_id = Set(extension_id);
        active.sip_trunk_id = Set(sip_trunk_id);
        active.route_id = Set(route_id);
        active.sip_gateway = Set(sip_gateway.clone());
        active.caller_uri = Set(caller_uri.clone());
        active.callee_uri = Set(callee_uri.clone());
        active.recording_url = Set(recording_url.clone());
        active.recording_duration_secs = Set(recording_duration_secs);
        active.has_transcript = Set(args.has_transcript);
        active.transcript_status = Set(args.transcript_status.clone());
        active.transcript_language = Set(args.transcript_language.clone());
        active.tags = Set(tags.clone());
        active.quality_mos = Set(args.quality_mos);
        active.quality_latency_ms = Set(args.quality_latency_ms);
        active.quality_jitter_ms = Set(args.quality_jitter_ms);
        active.quality_packet_loss_percent = Set(args.quality_packet_loss_percent);
        active.analytics = Set(analytics.clone());
        active.metadata = Set(metadata_value.clone());
        active.signaling = Set(signaling_value.clone());
        active.updated_at = Set(record.end_time);
        active.update(db).await?;
        return Ok(());
    }

    let active = call_record::ActiveModel {
        call_id: Set(record.call_id.clone()),
        display_id: Set(display_id.clone()),
        direction: Set(direction.clone()),
        status: Set(status.clone()),
        started_at: Set(record.start_time),
        ended_at: Set(Some(record.end_time)),
        duration_secs: Set(duration_secs),
        from_number: Set(from_number.clone()),
        to_number: Set(to_number.clone()),
        caller_name: Set(caller_name.clone()),
        agent_name: Set(agent_name.clone()),
        queue: Set(queue.clone()),
        department_id: Set(department_id),
        extension_id: Set(extension_id),
        sip_trunk_id: Set(sip_trunk_id),
        route_id: Set(route_id),
        sip_gateway: Set(sip_gateway.clone()),
        caller_uri: Set(caller_uri.clone()),
        callee_uri: Set(callee_uri.clone()),
        recording_url: Set(recording_url.clone()),
        recording_duration_secs: Set(recording_duration_secs),
        has_transcript: Set(args.has_transcript),
        transcript_status: Set(args.transcript_status.clone()),
        transcript_language: Set(args.transcript_language.clone()),
        tags: Set(tags.clone()),
        quality_mos: Set(args.quality_mos),
        quality_latency_ms: Set(args.quality_latency_ms),
        quality_jitter_ms: Set(args.quality_jitter_ms),
        quality_packet_loss_percent: Set(args.quality_packet_loss_percent),
        analytics: Set(analytics.clone()),
        metadata: Set(metadata_value.clone()),
        signaling: Set(signaling_value.clone()),
        created_at: Set(record.start_time),
        updated_at: Set(record.end_time),
        archived_at: Set(None),
        ..Default::default()
    };

    active.insert(db).await?;
    Ok(())
}

pub fn extract_sip_username(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(uri) = Uri::try_from(trimmed) {
        if let Some(user) = uri.user() {
            let value = user.to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }

        if trimmed.starts_with("sip:") || trimmed.starts_with("sips:") {
            let without_scheme = trimmed
                .split_once(':')
                .map(|(_, rest)| rest)
                .unwrap_or(trimmed);
            let candidate = without_scheme.split('@').next().unwrap_or_default().trim();
            if !candidate.is_empty() {
                return Some(candidate.to_string());
            }
        }
    }

    let mut candidate = trimmed.split('@').next().unwrap_or(trimmed).trim();
    if let Some(stripped) = candidate.strip_prefix("tel:") {
        candidate = stripped;
    }
    if let Some(stripped) = candidate.strip_prefix("sip:") {
        candidate = stripped;
    }
    if let Some(stripped) = candidate.strip_prefix("sips:") {
        candidate = stripped;
    }
    if candidate.is_empty() {
        None
    } else {
        Some(candidate.to_string())
    }
}

fn normalize_endpoint_uri(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn build_signaling_payload(record: &CallRecord) -> Option<Value> {
    let mut legs = Vec::new();
    legs.push(call_leg_payload("primary", record));

    if let Some(refer) = record.refer_callrecord.as_ref() {
        legs.push(call_leg_payload("b2bua", refer));
    }

    Some(json!({
        "is_b2bua": record.refer_callrecord.is_some(),
        "legs": legs,
    }))
}

fn call_leg_payload(role: &str, record: &CallRecord) -> Value {
    json!({
        "role": role,
        "call_type": record.call_type.clone(),
        "call_id": record.call_id.clone(),
        "caller": record.caller.clone(),
        "callee": record.callee.clone(),
        "status_code": record.status_code,
        "hangup_reason": record
            .hangup_reason
            .as_ref()
            .map(|reason| reason.to_string()),
    "start_time": record.start_time.clone(),
    "ring_time": record.ring_time.clone(),
    "answer_time": record.answer_time.clone(),
    "end_time": record.end_time.clone(),
        "offer": record.offer.clone(),
        "answer": record.answer.clone(),
    })
}

fn merge_metadata(record: &CallRecord, extra_metadata: Option<Value>) -> Option<Value> {
    let mut map = JsonMap::new();
    map.insert(
        "status_code".to_string(),
        Value::Number(JsonNumber::from(record.status_code)),
    );
    if let Some(reason) = record
        .hangup_reason
        .as_ref()
        .map(|reason| reason.to_string())
    {
        map.insert("hangup_reason".to_string(), Value::String(reason));
    }

    if let Some(option) = &record.option {
        if let Some(extra) = &option.extra {
            for (key, value) in extra {
                map.entry(key.clone())
                    .or_insert_with(|| Value::String(value.clone()));
            }
        }
    }

    if let Some(extras) = &record.extras {
        for (key, value) in extras {
            map.insert(key.clone(), value.clone());
        }
    }

    if let Some(Value::Object(values)) = extra_metadata {
        for (key, value) in values {
            map.insert(key, value);
        }
    } else if let Some(value) = extra_metadata {
        map.insert("extra_metadata".to_string(), value);
    }

    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}
