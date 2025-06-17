use crate::{
    config::{CallRecordConfig, S3Vendor},
    handler::{call::ActiveCallType, CallOption},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use object_store::{
    aws::AmazonS3Builder, azure::MicrosoftAzureBuilder, gcp::GoogleCloudStorageBuilder,
    path::Path as ObjectPath, ObjectStore,
};
use reqwest;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, future::Future, path::Path, pin::Pin, sync::Arc, time::Instant};
use tokio::{fs::File, io::AsyncWriteExt, select};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    pub call_type: ActiveCallType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option: Option<CallOption>,
    pub call_id: String,
    pub start_time: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring_time: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_time: Option<DateTime<Utc>>,
    pub end_time: DateTime<Utc>,
    pub caller: String,
    pub callee: String,
    pub status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hangup_reason: Option<CallRecordHangupReason>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recorder: Vec<CallRecordMedia>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extras: Option<HashMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dump_event_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecordMedia {
    pub track_id: String,
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallRecordHangupReason {
    ByCaller,
    ByCallee,
    BySystem,
    Autohangup,
    NoAnswer,
    NoBalance,
    AnswerMachine,
    ServerUnavailable,
    Canceled,
    Rejected,
    Failed,
    Other,
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
                            error!("CallRecordManager failed to create directory: {}", e);
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
                    let mut file = File::create(&file_name).await?;
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
                } => {
                    Self::save_with_s3_like(
                        formatter, vendor, bucket, region, access_key, secret_key, endpoint, root,
                        with_media, &record,
                    )
                    .await
                }
                CallRecordConfig::Http {
                    url,
                    headers,
                    with_media,
                } => Self::save_with_http(formatter, url, headers, with_media, &record).await,
            };
            let file_name = match r {
                Ok(file_name) => file_name,
                Err(e) => {
                    error!("Failed to save call record: {}", e);
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
                            error!("Failed to read media file {}: {}", media.path, e);
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
                error!("Failed to upload call record: {}", e);
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
                        error!("Failed to read media file {}: {}", path, e);
                        continue;
                    }
                };
                match object_store.put(media_path, file_content.into()).await {
                    Ok(r) => {
                        info!("Upload media file: {:?}", r);
                    }
                    Err(e) => {
                        error!("Failed to upload media file: {}", e);
                    }
                }
            }
            uploaded_files.extend(media_files);
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
                                error!("Failed to save call record: {}", e);
                            }
                        }
                    }
                }
            });
        }
        Ok(())
    }
}
