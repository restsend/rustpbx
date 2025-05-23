use crate::{
    config::{CallRecordConfig, S3Vendor},
    handler::{call::ActiveCallType, CallOption},
};
use anyhow::Result;
use object_store::{
    aws::AmazonS3Builder, azure::MicrosoftAzureBuilder, gcp::GoogleCloudStorageBuilder,
    path::Path as ObjectPath, ObjectStore,
};
use reqwest;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    collections::HashMap,
    future::Future,
    path::Path,
    pin::Pin,
    sync::Arc,
    time::{Instant, SystemTime},
};
use tokio::{fs::File, io::AsyncWriteExt, select};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

#[cfg(test)]
mod tests;
/// Custom serialization module for SystemTime to ISO 8601 format
///
/// This module provides custom serialization and deserialization for `SystemTime`
/// to ensure it's formatted as ISO 8601 (RFC 3339) compliant strings in JSON.
///
/// ## Usage
///
/// Add the `#[serde(with = "iso8601_systemtime")]` attribute to any `SystemTime` field:
///
/// ```rust
/// use std::time::SystemTime;
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct MyStruct {
///     #[serde(with = "iso8601_systemtime")]
///     created_at: SystemTime,
/// }
/// ```
///
/// ## Output format
///
/// The serialized format follows ISO 8601 / RFC 3339:
/// - `"2024-01-01T00:00:00+00:00"` (with timezone)
/// - `"2024-01-01T12:30:45.123456789Z"` (with nanoseconds if present)
mod iso8601_systemtime {
    use super::*;
    use chrono::{DateTime, Utc};

    /// Serialize SystemTime to ISO 8601 / RFC 3339 format
    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let datetime: DateTime<Utc> = (*time).into();
        serializer.serialize_str(&datetime.to_rfc3339())
    }

    /// Deserialize ISO 8601 / RFC 3339 format to SystemTime
    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let datetime = DateTime::parse_from_rfc3339(&s).map_err(serde::de::Error::custom)?;
        Ok(datetime.with_timezone(&Utc).into())
    }
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
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>
            + Send
            + Sync,
    >,
>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    pub call_type: ActiveCallType,
    pub option: CallOption,
    pub call_id: String,
    #[serde(with = "iso8601_systemtime")]
    pub start_time: SystemTime,
    #[serde(with = "iso8601_systemtime")]
    pub end_time: SystemTime,
    pub duration: u64,
    pub caller: String,
    pub callee: String,
    pub status_code: u16,
    pub hangup_reason: CallRecordHangupReason,
    pub recorder: Vec<CallRecordMedia>,
    pub extras: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecordMedia {
    pub track_id: String,
    pub r#type: String,
    pub path: String,
    pub size: u64,
    pub duration: u64,
    #[serde(with = "iso8601_systemtime")]
    pub start_time: SystemTime,
    #[serde(with = "iso8601_systemtime")]
    pub end_time: SystemTime,
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallRecordHangupReason {
    ByCaller,
    ByCallee,
    BySystem,
    NoAnswer,
    NoBalance,
    AnswerMachine,
    ServerUnavailable,
    Canceled,
    Rejected(u16, String),
    Failed(u16, String),
    Other(String),
}
pub trait CallRecordFormatter: Send + Sync {
    fn format(&self, record: &CallRecord) -> Result<String>;
    fn format_media_path(&self, record: &CallRecord, media: &CallRecordMedia) -> Result<String>;
}

pub struct DefaultCallRecordFormatter;
impl CallRecordFormatter for DefaultCallRecordFormatter {
    fn format(&self, record: &CallRecord) -> Result<String> {
        Ok(serde_json::to_string(record)?)
    }
    fn format_media_path(&self, record: &CallRecord, media: &CallRecordMedia) -> Result<String> {
        let file_name = Path::new(&media.path)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
            .to_string_lossy()
            .to_string();

        Ok(format!(
            "{}/media/{}/{}",
            record.call_id, media.track_id, file_name
        ))
    }
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
        cancel_token: CancellationToken,
        formatter: Arc<dyn CallRecordFormatter>,
        config: Arc<CallRecordConfig>,
        record: CallRecord,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(async move {
            let start_time = Instant::now();
            let r = match config.as_ref() {
                CallRecordConfig::Local { root } => {
                    let file_content = formatter.format(&record)?;
                    let file_name = Path::new(&root).join(format!("{}.json", record.call_id));
                    let mut file = File::create(&file_name).await?;
                    file.write_all(file_content.as_bytes()).await?;
                    file.flush().await?;
                    Ok(file_name.to_string_lossy().to_string())
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
                        cancel_token.child_token(),
                        formatter,
                        vendor,
                        bucket,
                        region,
                        access_key,
                        secret_key,
                        endpoint,
                        root,
                        with_media,
                        &record,
                    )
                    .await
                }
                CallRecordConfig::Http {
                    url,
                    headers,
                    with_media,
                } => {
                    Self::save_with_http(
                        cancel_token.child_token(),
                        formatter,
                        url,
                        headers,
                        with_media,
                        &record,
                    )
                    .await
                }
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
        cancel_token: CancellationToken,
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
                                .mime_str(&media.r#type)
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
        }

        let mut request = client.post(url).multipart(form);
        if let Some(headers_map) = headers {
            for (key, value) in headers_map {
                request = request.header(key, value);
            }
        }

        let response = select! {
            _ = cancel_token.cancelled() => {
                return Err(anyhow::anyhow!("CallRecordManager cancelled"));
            }
            response= request.send() => {
                response?
            }
        };

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
        cancel_token: CancellationToken,
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
        let json_path = ObjectPath::from(format!(
            "{}/{}.json",
            root.trim_end_matches('/'),
            record.call_id
        ));

        select! {
            _ = cancel_token.cancelled() => {}
            _ = object_store.put(&json_path, call_log_json.into()) =>{}
        };

        let mut uploaded_files = vec![json_path.to_string()];

        // Upload media files if with_media is true
        if with_media.unwrap_or(false) {
            for media in &record.recorder {
                if Path::new(&media.path).exists() {
                    match tokio::fs::read(&media.path).await {
                        Ok(file_content) => {
                            let media_path =
                                ObjectPath::from(formatter.format_media_path(record, media)?);
                            select! {
                                _ = cancel_token.cancelled() => {}
                                _ = object_store.put(&media_path, file_content.into()) => {
                                    uploaded_files.push(media_path.to_string());
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to read media file {}: {}", media.path, e);
                        }
                    }
                }
            }
        }

        Ok(format!(
            "S3 upload successful: {} files uploaded to {}/{}",
            uploaded_files.len(),
            bucket,
            root
        ))
    }

    pub async fn serve(&mut self) {
        let token = self.cancel_token.clone();

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
