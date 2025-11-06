use self::sipflow::SipMessageItem;
use crate::{
    call::{ActiveCallType, CallOption},
    config::{CallRecordConfig, S3Vendor},
    models::{
        bill_template::{Entity as BillTemplateEntity, Model as BillTemplateModel},
        call_record::{self, Column as CallRecordColumn, Entity as CallRecordEntity},
        sip_trunk::Entity as SipTrunkEntity,
    },
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
    collections::HashMap, convert::TryFrom, future::Future, mem, path::Path, pin::Pin,
    str::FromStr, sync::Arc, time::Instant,
};
use tokio::sync::mpsc::error::SendError;
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    select,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub mod sipflow;
pub mod storage;
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
pub struct CallRecordEvent {
    pub r#type: CallRecordEventType,
    pub timestamp: u64,
    pub content: String,
}

impl CallRecordEvent {
    pub async fn write<T: Serialize>(r#type: CallRecordEventType, obj: T, file: &mut File) {
        let content = match serde_json::to_string(&obj) {
            Ok(s) => s,
            Err(_) => return,
        };
        let event = Self {
            r#type,
            timestamp: crate::get_timestamp(),
            content,
        };
        match serde_json::to_string(&event) {
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
    #[serde(default)]
    pub recorder: Vec<CallRecordMedia>,
    pub extras: Option<HashMap<String, serde_json::Value>>,
    pub dump_event_file: Option<String>,
    pub refer_callrecord: Option<Box<CallRecord>>,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub sip_flows: HashMap<String, Vec<SipMessageItem>>,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub sip_leg_roles: HashMap<String, String>,
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

fn sanitize_filename_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if sanitized.is_empty() {
        "leg".to_string()
    } else {
        sanitized
    }
}

pub fn default_cdr_file_name(record: &CallRecord) -> String {
    format!(
        "{}_{}.json",
        record.start_time.format("%Y%m%d-%H%M%S"),
        record.call_id
    )
}

pub fn default_sip_flow_file_name(record: &CallRecord, leg: &str) -> String {
    format!(
        "{}_{}_{}.sipflow.jsonl",
        record.start_time.format("%Y%m%d-%H%M%S"),
        record.call_id,
        sanitize_filename_component(leg)
    )
}

pub fn default_transcript_file_name(record: &CallRecord) -> String {
    format!(
        "{}_{}.transcript.json",
        record.start_time.format("%Y%m%d-%H%M%S"),
        record.call_id
    )
}

pub fn apply_record_file_extras(record: &CallRecord, extras: &mut HashMap<String, Value>) {
    if !record.sip_flows.is_empty() {
        let entries: Vec<Value> = record
            .sip_flows
            .keys()
            .map(|leg| {
                let mut entry = JsonMap::new();
                entry.insert("leg".to_string(), Value::String(leg.clone()));
                entry.insert(
                    "path".to_string(),
                    Value::String(default_sip_flow_file_name(record, leg)),
                );
                if let Some(role) = record.sip_leg_roles.get(leg) {
                    entry.insert("role".to_string(), Value::String(role.clone()));
                }
                Value::Object(entry)
            })
            .collect();
        extras.insert("sip_flow_files".to_string(), Value::Array(entries));
    }

    extras.insert(
        "cdr_file".to_string(),
        Value::String(default_cdr_file_name(record)),
    );
}

pub fn extras_map_to_option(extras: &HashMap<String, Value>) -> Option<HashMap<String, Value>> {
    if extras.is_empty() {
        None
    } else {
        Some(extras.clone())
    }
}

pub fn extras_map_to_metadata(extras: &HashMap<String, Value>) -> Option<Value> {
    if extras.is_empty() {
        return None;
    }

    let mut map = JsonMap::new();
    for (key, value) in extras.iter() {
        map.insert(key.clone(), value.clone());
    }
    Some(Value::Object(map))
}

pub async fn persist_and_dispatch_record(
    db: Option<&DatabaseConnection>,
    sender: Option<&CallRecordSender>,
    record: CallRecord,
    args: CallRecordPersistArgs,
) -> (Option<anyhow::Error>, Option<SendError<CallRecord>>) {
    let persist_error = match db {
        Some(db_conn) => match persist_call_record(db_conn, &record, args).await {
            Ok(_) => None,
            Err(err) => Some(err),
        },
        None => {
            let _ = args;
            None
        }
    };

    let send_error = if let Some(sender) = sender {
        match sender.send(record) {
            Ok(_) => None,
            Err(err) => Some(err),
        }
    } else {
        None
    };

    (persist_error, send_error)
}

pub trait CallRecordFormatter: Send + Sync {
    fn format_file_name(&self, root: &str, record: &CallRecord) -> String {
        let trimmed_root = root.trim_end_matches('/');
        let file_name = default_cdr_file_name(record);
        if trimmed_root.is_empty() {
            file_name
        } else {
            format!("{}/{}", trimmed_root, file_name)
        }
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
    fn format_sip_flow_path(&self, root: &str, record: &CallRecord, leg: &str) -> String {
        let trimmed_root = root.trim_end_matches('/');
        let file_name = default_sip_flow_file_name(record, leg);
        if trimmed_root.is_empty() {
            file_name
        } else {
            format!("{}/{}", trimmed_root, file_name)
        }
    }
    fn format_transcript_path(&self, root: &str, record: &CallRecord) -> String {
        let trimmed_root = root.trim_end_matches('/');
        let file_name = default_transcript_file_name(record);
        if trimmed_root.is_empty() {
            file_name
        } else {
            format!("{}/{}", trimmed_root, file_name)
        }
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

pub fn build_object_store_from_s3(
    vendor: &S3Vendor,
    bucket: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    endpoint: &str,
) -> Result<Arc<dyn ObjectStore>> {
    let store: Arc<dyn ObjectStore> = match vendor {
        S3Vendor::AWS => {
            let builder = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_region(region)
                .with_access_key_id(access_key)
                .with_secret_access_key(secret_key);

            let instance = if !endpoint.is_empty() {
                builder.with_endpoint(endpoint).build()?
            } else {
                builder.build()?
            };
            Arc::new(instance)
        }
        S3Vendor::GCP => {
            let instance = GoogleCloudStorageBuilder::new()
                .with_bucket_name(bucket)
                .with_service_account_key(secret_key)
                .build()?;
            Arc::new(instance)
        }
        S3Vendor::Azure => {
            let instance = MicrosoftAzureBuilder::new()
                .with_container_name(bucket)
                .with_account(access_key)
                .with_access_key(secret_key)
                .build()?;
            Arc::new(instance)
        }
        S3Vendor::Aliyun | S3Vendor::Tencent | S3Vendor::Minio | S3Vendor::DigitalOcean => {
            let instance = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_region(region)
                .with_access_key_id(access_key)
                .with_secret_access_key(secret_key)
                .with_endpoint(endpoint)
                .with_virtual_hosted_style_request(false)
                .build()?;
            Arc::new(instance)
        }
    };

    Ok(store)
}

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
            let mut record = record;
            let sip_flows = mem::take(&mut record.sip_flows);
            let start_time = Instant::now();
            let result = match config.as_ref() {
                CallRecordConfig::Local { root } => {
                    Self::save_local_record(formatter.clone(), root, &mut record, sip_flows).await
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
                        formatter.clone(),
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
                        sip_flows,
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
                        sip_flows,
                    )
                    .await
                }
            };
            let file_name = match result {
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

    async fn save_local_record(
        formatter: Arc<dyn CallRecordFormatter>,
        root: &String,
        record: &mut CallRecord,
        sip_flows: HashMap<String, Vec<SipMessageItem>>,
    ) -> Result<String> {
        let mut recorded_files = Vec::new();

        for (leg, entries) in sip_flows.into_iter() {
            if entries.is_empty() {
                continue;
            }
            let file_path = formatter.format_sip_flow_path(root, record, &leg);
            if let Some(parent) = Path::new(&file_path).parent() {
                fs::create_dir_all(parent).await?;
            }
            let mut file = File::create(&file_path).await.map_err(|e| {
                anyhow::anyhow!("Failed to create SIP flow file {}: {}", file_path, e)
            })?;
            for entry in entries {
                let line = serde_json::to_string(&entry)?;
                file.write_all(line.as_bytes()).await?;
                file.write_all(b"\n").await?;
            }
            file.flush().await?;
            recorded_files.push((leg, file_path));
        }

        if recorded_files.is_empty() {
            Self::remove_sip_flow_metadata(record);
        } else {
            Self::attach_sip_flow_metadata(record, &recorded_files);
        }

        let file_content = formatter.format(record)?;
        let file_name = formatter.format_file_name(root, record);
        let mut file = File::create(&file_name).await.map_err(|e| {
            anyhow::anyhow!("Failed to create call record file {}: {}", file_name, e)
        })?;
        file.write_all(file_content.as_bytes()).await?;
        file.flush().await?;
        Ok(file_name.to_string())
    }

    fn attach_sip_flow_metadata(record: &mut CallRecord, files: &[(String, String)]) {
        let mut extras = record.extras.take().unwrap_or_default();
        let entries: Vec<Value> = files
            .iter()
            .map(|(leg, path)| {
                let mut entry = JsonMap::new();
                entry.insert("leg".to_string(), Value::String(leg.clone()));
                entry.insert("path".to_string(), Value::String(path.clone()));
                if let Some(role) = record.sip_leg_roles.get(leg) {
                    entry.insert("role".to_string(), Value::String(role.clone()));
                }
                Value::Object(entry)
            })
            .collect();
        extras.insert("sip_flow_files".to_string(), Value::Array(entries));
        record.extras = Some(extras);
    }

    fn remove_sip_flow_metadata(record: &mut CallRecord) {
        if let Some(mut extras) = record.extras.take() {
            extras.remove("sip_flow_files");
            if extras.is_empty() {
                record.extras = None;
            } else {
                record.extras = Some(extras);
            }
        }
    }

    async fn save_with_http(
        formatter: Arc<dyn CallRecordFormatter>,
        url: &String,
        headers: &Option<HashMap<String, String>>,
        with_media: &Option<bool>,
        keep_media_copy: &Option<bool>,
        record: &CallRecord,
        sip_flows: HashMap<String, Vec<SipMessageItem>>,
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

        if !sip_flows.is_empty() {
            // Process SIP flows
            for (leg, entries) in sip_flows {
                if entries.is_empty() {
                    continue;
                }

                let mut buffer = Vec::new();
                for entry in entries {
                    let line = serde_json::to_vec(&entry)?;
                    buffer.extend_from_slice(&line);
                    buffer.push(b'\n');
                }

                let file_name = default_sip_flow_file_name(record, &leg);
                let field_name = format!("sip_flow_{}", sanitize_filename_component(&leg));
                let part = reqwest::multipart::Part::bytes(buffer)
                    .file_name(file_name)
                    .mime_str("application/x-ndjson")?;
                form = form.part(field_name, part);
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
        sip_flows: HashMap<String, Vec<SipMessageItem>>,
    ) -> Result<String> {
        let object_store =
            build_object_store_from_s3(vendor, bucket, region, access_key, secret_key, endpoint)?;

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
        if !sip_flows.is_empty() {
            for (leg, entries) in sip_flows {
                if entries.is_empty() {
                    continue;
                }
                let flow_path =
                    ObjectPath::from(formatter.format_sip_flow_path(root, record, &leg));
                let mut buffer = Vec::new();
                for entry in entries {
                    let line = serde_json::to_vec(&entry)?;
                    buffer.extend_from_slice(&line);
                    buffer.push(b'\n');
                }
                match object_store.put(&flow_path, buffer.into()).await {
                    Ok(r) => {
                        info!("Upload sip flow file: {:?}", r);
                    }
                    Err(e) => {
                        warn!(path = %flow_path, "Failed to upload sip flow file: {}", e);
                    }
                }
            }
        }
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
    let recording_url = args
        .recording_url
        .clone()
        .or_else(|| record.recorder.first().map(|media| media.path.clone()));
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
    let billing_context = resolve_billing_context(db, sip_trunk_id).await?;
    let billing_computation = compute_billing(duration_secs, &billing_context);
    let billing_snapshot = billing_context.snapshot.clone();
    let billing_method = billing_computation.method.clone();
    let billing_status = Some(billing_computation.status.clone());
    let billing_currency = billing_computation.currency.clone();
    let billing_billable_secs = billing_computation.billable_secs;
    let billing_rate_per_minute = billing_computation.rate_per_minute;
    let billing_amount_subtotal = billing_computation.amount_subtotal;
    let billing_amount_tax = billing_computation.amount_tax;
    let billing_amount_total = billing_computation.amount_total;
    let billing_result_value = billing_computation.to_json();
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
        active.billing_template_id = Set(billing_context.template_id);
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
        active.billing_snapshot = Set(billing_snapshot.clone());
        active.billing_method = Set(billing_method.clone());
        active.billing_status = Set(billing_status.clone());
        active.billing_currency = Set(billing_currency.clone());
        active.billing_billable_secs = Set(billing_billable_secs);
        active.billing_rate_per_minute = Set(billing_rate_per_minute);
        active.billing_amount_subtotal = Set(billing_amount_subtotal);
        active.billing_amount_tax = Set(billing_amount_tax);
        active.billing_amount_total = Set(billing_amount_total);
        active.billing_result = Set(billing_result_value.clone());
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
        billing_template_id: Set(billing_context.template_id),
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
        billing_snapshot: Set(billing_snapshot.clone()),
        billing_method: Set(billing_method.clone()),
        billing_status: Set(billing_status),
        billing_currency: Set(billing_currency),
        billing_billable_secs: Set(billing_billable_secs),
        billing_rate_per_minute: Set(billing_rate_per_minute),
        billing_amount_subtotal: Set(billing_amount_subtotal),
        billing_amount_tax: Set(billing_amount_tax),
        billing_amount_total: Set(billing_amount_total),
        billing_result: Set(billing_result_value),
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

    if !record.sip_leg_roles.is_empty() {
        for (sip_call_id, role) in record.sip_leg_roles.iter() {
            let leg_record = match role.as_str() {
                "primary" => Some(record),
                "b2bua" => record.refer_callrecord.as_deref(),
                _ => None,
            };

            let payload = match leg_record {
                Some(inner) => call_leg_payload(role, inner, Some(sip_call_id)),
                None => minimal_leg_payload(role, sip_call_id),
            };

            legs.push(payload);
        }
    }

    if legs.is_empty() {
        legs.push(call_leg_payload("primary", record, None));
        if let Some(refer) = record.refer_callrecord.as_ref() {
            legs.push(call_leg_payload("b2bua", refer, None));
        }
    } else if record.refer_callrecord.is_some()
        && !record.sip_leg_roles.values().any(|role| role == "b2bua")
    {
        if let Some(refer) = record.refer_callrecord.as_ref() {
            legs.push(call_leg_payload("b2bua", refer, None));
        }
    }

    if legs.is_empty() {
        None
    } else {
        Some(json!({
            "is_b2bua": record.refer_callrecord.is_some(),
            "legs": legs,
        }))
    }
}

fn call_leg_payload(role: &str, record: &CallRecord, sip_call_id: Option<&String>) -> Value {
    let call_id = sip_call_id
        .cloned()
        .unwrap_or_else(|| record.call_id.clone());

    json!({
        "role": role,
        "call_type": record.call_type.clone(),
        "call_id": call_id,
        "session_id": record.call_id.clone(),
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
    })
}

fn minimal_leg_payload(role: &str, sip_call_id: &str) -> Value {
    json!({
        "role": role,
        "call_id": sip_call_id,
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

const BILLING_METHOD_TEMPLATE: &str = "template";
const BILLING_STATUS_UNRATED: &str = "unrated";
const BILLING_STATUS_ZERO_DURATION: &str = "zero-duration";
const BILLING_STATUS_INCLUDED: &str = "included";
const BILLING_STATUS_CHARGED: &str = "charged";

#[derive(Debug, Clone, Default)]
struct BillingContext {
    template_id: Option<i64>,
    snapshot: Option<Value>,
    parameters: Option<BillingParameters>,
}

#[derive(Debug, Clone)]
struct BillingParameters {
    template_id: Option<i64>,
    template_name: Option<String>,
    display_name: Option<String>,
    currency: String,
    initial_increment_secs: i32,
    billing_increment_secs: i32,
    overage_rate_per_minute: f64,
    tax_percent: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct BillingTemplateSnapshot {
    id: Option<i64>,
    name: Option<String>,
    display_name: Option<String>,
    description: Option<String>,
    currency: Option<String>,
    billing_interval: Option<Value>,
    included_minutes: Option<i32>,
    included_messages: Option<i32>,
    initial_increment_secs: Option<i32>,
    billing_increment_secs: Option<i32>,
    overage_rate_per_minute: Option<f64>,
    setup_fee: Option<f64>,
    tax_percent: Option<f64>,
    metadata: Option<Value>,
    captured_at: Option<Value>,
}

impl BillingParameters {
    fn from_template(template: &BillTemplateModel) -> Self {
        Self {
            template_id: Some(template.id),
            template_name: Some(template.name.clone()),
            display_name: template.display_name.clone(),
            currency: template.currency.clone(),
            initial_increment_secs: template.initial_increment_secs.max(1),
            billing_increment_secs: template.billing_increment_secs.max(1),
            overage_rate_per_minute: template.overage_rate_per_minute.max(0.0),
            tax_percent: template.tax_percent.max(0.0),
        }
    }

    fn from_snapshot(snapshot: &BillingTemplateSnapshot) -> Self {
        Self {
            template_id: snapshot.id,
            template_name: snapshot.name.clone(),
            display_name: snapshot.display_name.clone(),
            currency: snapshot
                .currency
                .clone()
                .unwrap_or_else(|| "USD".to_string()),
            initial_increment_secs: snapshot.initial_increment_secs.unwrap_or(60).max(1),
            billing_increment_secs: snapshot.billing_increment_secs.unwrap_or(60).max(1),
            overage_rate_per_minute: snapshot.overage_rate_per_minute.unwrap_or(0.0).max(0.0),
            tax_percent: snapshot.tax_percent.unwrap_or(0.0).max(0.0),
        }
    }
}

async fn resolve_billing_context(
    db: &DatabaseConnection,
    sip_trunk_id: Option<i64>,
) -> Result<BillingContext> {
    let Some(trunk_id) = sip_trunk_id else {
        return Ok(BillingContext::default());
    };

    let Some(trunk) = SipTrunkEntity::find_by_id(trunk_id).one(db).await? else {
        return Ok(BillingContext::default());
    };

    let mut context = BillingContext {
        template_id: trunk.billing_template_id,
        snapshot: trunk.billing_snapshot.clone(),
        parameters: None,
    };

    if let Some(snapshot) = context.snapshot.clone() {
        if let Some(params) = extract_billing_parameters(&snapshot) {
            if context.template_id.is_none() {
                context.template_id = params.template_id;
            }
            context.parameters = Some(params);
        }
    }

    if context.parameters.is_none() {
        if let Some(template_id) = context.template_id {
            if let Some(template) = BillTemplateEntity::find_by_id(template_id).one(db).await? {
                context.parameters = Some(BillingParameters::from_template(&template));
                if context.snapshot.is_none() {
                    context.snapshot = Some(build_bill_template_snapshot(&template));
                }
            }
        }
    }

    Ok(context)
}

fn build_bill_template_snapshot(template: &BillTemplateModel) -> Value {
    json!({
        "id": template.id,
        "name": template.name,
        "display_name": template.display_name,
        "description": template.description,
        "currency": template.currency,
        "billing_interval": template.billing_interval,
        "included_minutes": template.included_minutes,
        "included_messages": template.included_messages,
        "initial_increment_secs": template.initial_increment_secs,
        "billing_increment_secs": template.billing_increment_secs,
        "overage_rate_per_minute": template.overage_rate_per_minute,
        "setup_fee": template.setup_fee,
        "tax_percent": template.tax_percent,
        "metadata": template.metadata.clone(),
        "captured_at": Utc::now(),
    })
}

fn extract_billing_parameters(snapshot: &Value) -> Option<BillingParameters> {
    serde_json::from_value::<BillingTemplateSnapshot>(snapshot.clone())
        .map(|data| BillingParameters::from_snapshot(&data))
        .ok()
}

#[derive(Debug, Clone)]
struct BillingComputation {
    method: Option<String>,
    status: String,
    currency: Option<String>,
    billable_secs: Option<i32>,
    rate_per_minute: Option<f64>,
    amount_subtotal: Option<f64>,
    amount_tax: Option<f64>,
    amount_total: Option<f64>,
    detail: Option<Value>,
}

impl Default for BillingComputation {
    fn default() -> Self {
        Self {
            method: None,
            status: String::new(),
            currency: None,
            billable_secs: None,
            rate_per_minute: None,
            amount_subtotal: None,
            amount_tax: None,
            amount_total: None,
            detail: None,
        }
    }
}

impl BillingComputation {
    fn to_json(&self) -> Option<Value> {
        if self.status.is_empty()
            && self.method.is_none()
            && self.currency.is_none()
            && self.billable_secs.is_none()
            && self.rate_per_minute.is_none()
            && self.amount_subtotal.is_none()
            && self.amount_tax.is_none()
            && self.amount_total.is_none()
            && self.detail.is_none()
        {
            return None;
        }

        Some(json!({
            "method": self.method.clone(),
            "status": self.status.clone(),
            "currency": self.currency.clone(),
            "billable_secs": self.billable_secs,
            "rate_per_minute": self.rate_per_minute,
            "amount": {
                "subtotal": self.amount_subtotal,
                "tax": self.amount_tax,
                "total": self.amount_total,
            },
            "detail": self.detail.clone(),
        }))
    }
}

fn compute_billing(duration_secs: i32, context: &BillingContext) -> BillingComputation {
    if duration_secs <= 0 {
        return BillingComputation {
            method: context
                .parameters
                .as_ref()
                .map(|_| BILLING_METHOD_TEMPLATE.to_string()),
            status: BILLING_STATUS_ZERO_DURATION.to_string(),
            currency: context
                .parameters
                .as_ref()
                .map(|params| params.currency.clone()),
            billable_secs: Some(0),
            rate_per_minute: context
                .parameters
                .as_ref()
                .map(|params| params.overage_rate_per_minute),
            amount_subtotal: Some(0.0),
            amount_tax: Some(0.0),
            amount_total: Some(0.0),
            detail: Some(json!({
                "reason": "zero_duration",
                "template_id": context.template_id,
                "raw_duration_secs": duration_secs,
            })),
        };
    }

    let Some(params) = context.parameters.as_ref() else {
        return BillingComputation {
            method: None,
            status: BILLING_STATUS_UNRATED.to_string(),
            currency: None,
            billable_secs: Some(0),
            rate_per_minute: None,
            amount_subtotal: None,
            amount_tax: None,
            amount_total: None,
            detail: Some(json!({
                "reason": "missing_billing_parameters",
                "template_id": context.template_id,
                "raw_duration_secs": duration_secs,
            })),
        };
    };

    let billed_secs = apply_billing_increments(
        duration_secs,
        params.initial_increment_secs,
        params.billing_increment_secs,
    );
    let billable_minutes = billed_secs as f64 / 60.0;
    let rate = params.overage_rate_per_minute.max(0.0);
    let subtotal = round_currency((billable_minutes * rate).max(0.0));
    let tax = round_currency((subtotal * params.tax_percent / 100.0).max(0.0));
    let total = round_currency((subtotal + tax).max(0.0));

    let status = if total <= 0.0 {
        BILLING_STATUS_INCLUDED.to_string()
    } else {
        BILLING_STATUS_CHARGED.to_string()
    };

    BillingComputation {
        method: Some(BILLING_METHOD_TEMPLATE.to_string()),
        status,
        currency: Some(params.currency.clone()),
        billable_secs: Some(billed_secs),
        rate_per_minute: Some(rate),
        amount_subtotal: Some(subtotal),
        amount_tax: Some(tax),
        amount_total: Some(total),
        detail: Some(json!({
            "template": {
                "id": params.template_id,
                "name": params.template_name,
                "display_name": params.display_name,
            },
            "raw_duration_secs": duration_secs,
            "billable_secs": billed_secs,
            "billable_minutes": (billable_minutes * 10_000.0).round() / 10_000.0,
            "rounding_delta_secs": billed_secs.saturating_sub(duration_secs.max(0)),
            "increments": {
                "initial_secs": params.initial_increment_secs,
                "billing_secs": params.billing_increment_secs,
            },
            "computed_at": Utc::now(),
        })),
    }
}

fn apply_billing_increments(
    duration_secs: i32,
    initial_increment_secs: i32,
    billing_increment_secs: i32,
) -> i32 {
    if duration_secs <= 0 {
        return 0;
    }

    let initial = initial_increment_secs.max(1);
    let increment = billing_increment_secs.max(1);

    if duration_secs <= initial {
        initial
    } else {
        let remaining = duration_secs - initial;
        let increments = (remaining + increment - 1) / increment;
        initial + increments * increment
    }
}

fn round_currency(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
