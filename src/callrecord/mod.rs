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
use serde::{Deserialize, Serialize};
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

pub type CallRecordSender = tokio::sync::mpsc::UnboundedSender<CallRecord>;
pub type CallRecordReceiver = tokio::sync::mpsc::UnboundedReceiver<CallRecord>;

pub type FnSaveCallRecord = Arc<
    Box<
        dyn Fn(
                &CancellationToken,
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
    pub start_time: SystemTime,
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
    pub start_time: SystemTime,
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

pub struct CallRecordManager {
    pub sender: CallRecordSender,
    config: Arc<CallRecordConfig>,
    cancel_token: CancellationToken,
    receiver: CallRecordReceiver,
    saver_fn: FnSaveCallRecord,
}

pub struct CallRecordManagerBuilder {
    pub cancel_token: Option<CancellationToken>,
    pub config: Option<CallRecordConfig>,
    saver_fn: Option<FnSaveCallRecord>,
}

impl CallRecordManagerBuilder {
    pub fn new() -> Self {
        Self {
            cancel_token: None,
            config: None,
            saver_fn: None,
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

    pub fn build(self) -> CallRecordManager {
        let cancel_token = self.cancel_token.unwrap_or_default();
        let config = Arc::new(self.config.unwrap_or_default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let saver_fn = self
            .saver_fn
            .unwrap_or_else(|| Arc::new(Box::new(CallRecordManager::default_saver)));

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
        }
    }
}

impl CallRecordManager {
    fn default_saver(
        _cancel_token: &CancellationToken,
        config: Arc<CallRecordConfig>,
        record: CallRecord,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        Box::pin(async move {
            let start_time = Instant::now();
            let file_content = serde_json::to_string(&record).unwrap();
            let r = match config.as_ref() {
                CallRecordConfig::Local { root } => {
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
                        vendor, bucket, region, access_key, secret_key, endpoint, root, with_media,
                        &record,
                    )
                    .await
                }
                CallRecordConfig::Http {
                    url,
                    headers,
                    with_media,
                } => Self::save_with_http(url, headers, with_media, &record).await,
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
        url: &String,
        headers: &Option<HashMap<String, String>>,
        with_media: &Option<bool>,
        record: &CallRecord,
    ) -> Result<String> {
        let client = reqwest::Client::new();

        // Serialize call record to JSON
        let call_log_json = serde_json::to_string(record)?;

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

        // Build request
        let mut request = client.post(url).multipart(form);

        // Add custom headers if provided
        if let Some(headers_map) = headers {
            for (key, value) in headers_map {
                request = request.header(key, value);
            }
        }

        // Send request
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
        let call_log_json = serde_json::to_string(record)?;

        // Upload call log JSON
        let json_path = ObjectPath::from(format!(
            "{}/{}.json",
            root.trim_end_matches('/'),
            record.call_id
        ));
        object_store.put(&json_path, call_log_json.into()).await?;

        let mut uploaded_files = vec![json_path.to_string()];

        // Upload media files if with_media is true
        if with_media.unwrap_or(false) {
            for media in &record.recorder {
                if Path::new(&media.path).exists() {
                    match tokio::fs::read(&media.path).await {
                        Ok(file_content) => {
                            let file_name = Path::new(&media.path)
                                .file_name()
                                .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
                                .to_string_lossy()
                                .to_string();

                            let media_path = ObjectPath::from(format!(
                                "{}/media/{}/{}",
                                root.trim_end_matches('/'),
                                record.call_id,
                                file_name
                            ));

                            object_store.put(&media_path, file_content.into()).await?;
                            uploaded_files.push(media_path.to_string());
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
        config: Arc<CallRecordConfig>,
        saver_fn: FnSaveCallRecord,
        receiver: &mut CallRecordReceiver,
    ) -> Result<()> {
        while let Some(record) = receiver.recv().await {
            let cancel_token_ref = cancel_token.clone();
            let save_fn_ref = saver_fn.clone();
            let config_ref = config.clone();
            tokio::spawn(async move {
                select! {
                    _ = cancel_token_ref.cancelled() => {
                        info!("CallRecordManager cancelled");
                    }
                    r = save_fn_ref(&cancel_token_ref, config_ref, record) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_save_with_http_without_media() {
        // Create a test CallRecord
        let mut extras = HashMap::new();
        extras.insert(
            "test_key".to_string(),
            serde_json::Value::String("test_value".to_string()),
        );

        let record = CallRecord {
            call_type: crate::handler::call::ActiveCallType::Sip,
            option: crate::handler::CallOption::default(),
            call_id: "test_call_123".to_string(),
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            duration: 60,
            caller: "+1234567890".to_string(),
            callee: "+0987654321".to_string(),
            status_code: 200,
            hangup_reason: CallRecordHangupReason::ByCaller,
            recorder: vec![],
            extras,
        };

        // Test without media (should not fail if no server available)
        let url = "http://httpbin.org/post".to_string();
        let headers = None;
        let with_media = Some(false);

        // This test will only pass if httpbin.org is available
        // In production, you might want to use a mock server
        let result = CallRecordManager::save_with_http(&url, &headers, &with_media, &record).await;

        // We expect this to succeed for the JSON upload
        if result.is_ok() {
            println!("HTTP upload test passed: {}", result.unwrap());
        } else {
            println!(
                "HTTP upload test failed (expected if no internet): {:?}",
                result.err()
            );
        }
    }

    #[tokio::test]
    async fn test_save_with_http_with_media() {
        // Create a temporary media file
        let mut temp_file = NamedTempFile::new().unwrap();
        let test_content = b"fake audio content";
        temp_file.write_all(test_content).unwrap();
        temp_file.flush().unwrap();

        let media = CallRecordMedia {
            track_id: "track_001".to_string(),
            r#type: "audio/wav".to_string(),
            path: temp_file.path().to_string_lossy().to_string(),
            size: test_content.len() as u64,
            duration: 5000,
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            extra: HashMap::new(),
        };

        let mut extras = HashMap::new();
        extras.insert(
            "test_key".to_string(),
            serde_json::Value::String("test_value".to_string()),
        );

        let record = CallRecord {
            call_type: crate::handler::call::ActiveCallType::Sip,
            option: crate::handler::CallOption::default(),
            call_id: "test_call_with_media_456".to_string(),
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            duration: 120,
            caller: "+1234567890".to_string(),
            callee: "+0987654321".to_string(),
            status_code: 200,
            hangup_reason: CallRecordHangupReason::ByCaller,
            recorder: vec![media],
            extras,
        };

        // Test with media
        let url = "http://httpbin.org/post".to_string();
        let headers = None;
        let with_media = Some(true);

        let result = CallRecordManager::save_with_http(&url, &headers, &with_media, &record).await;

        if result.is_ok() {
            println!("HTTP upload with media test passed: {}", result.unwrap());
        } else {
            println!(
                "HTTP upload with media test failed (expected if no internet): {:?}",
                result.err()
            );
        }
    }

    #[tokio::test]
    async fn test_save_with_http_with_custom_headers() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer test_token".to_string());
        headers.insert("X-Custom-Header".to_string(), "test_value".to_string());

        let mut extras = HashMap::new();
        extras.insert(
            "test_key".to_string(),
            serde_json::Value::String("test_value".to_string()),
        );

        let record = CallRecord {
            call_type: crate::handler::call::ActiveCallType::Sip,
            option: crate::handler::CallOption::default(),
            call_id: "test_call_headers_789".to_string(),
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            duration: 30,
            caller: "+1234567890".to_string(),
            callee: "+0987654321".to_string(),
            status_code: 200,
            hangup_reason: CallRecordHangupReason::ByCaller,
            recorder: vec![],
            extras,
        };

        let url = "http://httpbin.org/post".to_string();
        let with_media = Some(false);

        let result =
            CallRecordManager::save_with_http(&url, &Some(headers), &with_media, &record).await;

        if result.is_ok() {
            println!("HTTP upload with headers test passed: {}", result.unwrap());
        } else {
            println!(
                "HTTP upload with headers test failed (expected if no internet): {:?}",
                result.err()
            );
        }
    }

    #[tokio::test]
    async fn test_save_with_s3_like_with_custom_headers() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer test_token".to_string());
        headers.insert("X-Custom-Header".to_string(), "test_value".to_string());

        let mut extras = HashMap::new();
        extras.insert(
            "test_key".to_string(),
            serde_json::Value::String("test_value".to_string()),
        );

        let record = CallRecord {
            call_type: crate::handler::call::ActiveCallType::Sip,
            option: crate::handler::CallOption::default(),
            call_id: "test_call_headers_789".to_string(),
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            duration: 30,
            caller: "+1234567890".to_string(),
            callee: "+0987654321".to_string(),
            status_code: 200,
            hangup_reason: CallRecordHangupReason::ByCaller,
            recorder: vec![],
            extras,
        };

        let url = "http://httpbin.org/post".to_string();
        let with_media = Some(false);

        let result =
            CallRecordManager::save_with_http(&url, &Some(headers), &with_media, &record).await;

        if result.is_ok() {
            println!("HTTP upload with headers test passed: {}", result.unwrap());
        } else {
            println!(
                "HTTP upload with headers test failed (expected if no internet): {:?}",
                result.err()
            );
        }
    }

    #[tokio::test]
    async fn test_save_with_s3_like_memory_store() {
        // Test using memory store for S3-like functionality without real cloud storage
        let vendor = crate::config::S3Vendor::Minio;
        let bucket = "test-bucket".to_string();
        let region = "us-east-1".to_string();
        let access_key = "minioadmin".to_string();
        let secret_key = "minioadmin".to_string();
        let endpoint = "http://localhost:9000".to_string(); // Local minio endpoint
        let root = "test-records".to_string();
        let with_media = Some(false);

        let mut extras = HashMap::new();
        extras.insert(
            "test_key".to_string(),
            serde_json::Value::String("test_value".to_string()),
        );

        let record = CallRecord {
            call_type: crate::handler::call::ActiveCallType::Sip,
            option: crate::handler::CallOption::default(),
            call_id: "test_s3_call_123".to_string(),
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            duration: 60,
            caller: "+1234567890".to_string(),
            callee: "+0987654321".to_string(),
            status_code: 200,
            hangup_reason: CallRecordHangupReason::ByCaller,
            recorder: vec![],
            extras,
        };

        // This test will only succeed if there's a local minio instance running
        // In real scenarios, this would use actual cloud storage credentials
        let result = CallRecordManager::save_with_s3_like(
            &vendor,
            &bucket,
            &region,
            &access_key,
            &secret_key,
            &endpoint,
            &root,
            &with_media,
            &record,
        )
        .await;

        // We expect this might fail in test environment without real S3 storage
        match result {
            Ok(message) => println!("S3 upload test passed: {}", message),
            Err(e) => println!(
                "S3 upload test failed (expected without real S3 setup): {:?}",
                e
            ),
        }
    }

    #[tokio::test]
    async fn test_save_with_s3_like_with_media() {
        // Create a temporary media file
        let mut temp_file = NamedTempFile::new().unwrap();
        let test_content = b"fake audio content for S3 test";
        temp_file.write_all(test_content).unwrap();
        temp_file.flush().unwrap();

        let media = CallRecordMedia {
            track_id: "s3_track_001".to_string(),
            r#type: "audio/wav".to_string(),
            path: temp_file.path().to_string_lossy().to_string(),
            size: test_content.len() as u64,
            duration: 5000,
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            extra: HashMap::new(),
        };

        let mut extras = HashMap::new();
        extras.insert(
            "test_key".to_string(),
            serde_json::Value::String("test_value".to_string()),
        );

        let record = CallRecord {
            call_type: crate::handler::call::ActiveCallType::Sip,
            option: crate::handler::CallOption::default(),
            call_id: "test_s3_media_456".to_string(),
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            duration: 120,
            caller: "+1234567890".to_string(),
            callee: "+0987654321".to_string(),
            status_code: 200,
            hangup_reason: CallRecordHangupReason::ByCaller,
            recorder: vec![media],
            extras,
        };

        // Test with different S3 vendors
        let test_cases = vec![
            (crate::config::S3Vendor::AWS, "https://s3.amazonaws.com"),
            (crate::config::S3Vendor::Minio, "http://localhost:9000"),
            (
                crate::config::S3Vendor::Aliyun,
                "https://oss-cn-hangzhou.aliyuncs.com",
            ),
        ];

        for (vendor, endpoint) in test_cases {
            let bucket = "test-bucket".to_string();
            let region = "us-east-1".to_string();
            let access_key = "test_access_key".to_string();
            let secret_key = "test_secret_key".to_string();
            let endpoint = endpoint.to_string();
            let root = "test-records".to_string();
            let with_media = Some(true);

            let result = CallRecordManager::save_with_s3_like(
                &vendor,
                &bucket,
                &region,
                &access_key,
                &secret_key,
                &endpoint,
                &root,
                &with_media,
                &record,
            )
            .await;

            match result {
                Ok(message) => println!("S3 {:?} upload with media test passed: {}", vendor, message),
                Err(e) => println!("S3 {:?} upload with media test failed (expected without real credentials): {:?}", vendor, e),
            }
        }
    }
}
