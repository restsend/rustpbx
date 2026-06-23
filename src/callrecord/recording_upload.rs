use std::{collections::HashMap, path::Path};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::multipart::{Form, Part};
use serde_json::json;
use tracing::{info, warn};

use crate::{
    callrecord::{
        CALL_RECORD_HTTP_CONNECT_TIMEOUT, CALL_RECORD_HTTP_TIMEOUT, CallRecord, CallRecordHook,
    },
    config::{RecordingPolicy, RecordingType},
    models::call_record::extract_sip_username,
    rwi::RwiGatewayRef,
    storage::{Storage, StorageConfig},
};

pub struct RecordingUploadHook {
    policy: RecordingPolicy,
    rwi_gateway: Option<RwiGatewayRef>,
    client: reqwest::Client,
    s3_storage: Option<Storage>,
}

impl RecordingUploadHook {
    pub fn new(policy: RecordingPolicy) -> Result<Self> {
        let recording_type = policy.recording_type.unwrap_or_default();
        let s3_storage = if recording_type == RecordingType::S3 {
            let bucket = Self::required(&policy.bucket, "bucket")?;
            let region = Self::required(&policy.region, "region")?;
            let access_key = Self::required(&policy.access_key, "access_key")?;
            let secret_key = Self::required(&policy.secret_key, "secret_key")?;
            let endpoint = policy
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|endpoint| !endpoint.is_empty())
                .map(str::to_string);
            let vendor = policy.vendor.clone().unwrap_or_default();
            let storage = Storage::new(&StorageConfig::S3 {
                vendor,
                bucket: bucket.clone(),
                region,
                access_key,
                secret_key,
                endpoint: endpoint.clone(),
                prefix: None,
            })?;
            Some(storage)
        } else {
            None
        };

        Ok(Self {
            policy,
            rwi_gateway: None,
            client: reqwest::Client::builder()
                .timeout(CALL_RECORD_HTTP_TIMEOUT)
                .connect_timeout(CALL_RECORD_HTTP_CONNECT_TIMEOUT)
                .build()?,
            s3_storage,
        })
    }

    pub fn with_rwi_gateway(mut self, gw: RwiGatewayRef) -> Self {
        self.rwi_gateway = Some(gw);
        self
    }

    fn required(value: &Option<String>, name: &str) -> Result<String> {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("recording.{name} is required"))
    }

    fn storage_key(&self, record: &CallRecord, _track_id: &str, media_path: &str) -> String {
        let file_name = Path::new(media_path)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("recording.wav"))
            .to_string_lossy();
        let key = format!("{}/{}", record.start_time.format("%Y%m%d"), file_name);
        match self
            .policy
            .root
            .as_deref()
            .map(str::trim)
            .filter(|root| !root.is_empty())
        {
            Some(root) => format!("{}/{}", root.trim_end_matches('/'), key),
            None => key,
        }
    }

    fn s3_url(endpoint: Option<&str>, bucket: &str, key: &str) -> String {
        match endpoint
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
        {
            Some(endpoint) => format!(
                "{}/{}/{}",
                endpoint.trim_end_matches('/'),
                bucket.trim_matches('/'),
                key.trim_start_matches('/')
            ),
            None => format!("s3://{}/{}", bucket.trim_matches('/'), key),
        }
    }

    async fn upload_s3(&self, key: &str, data: Vec<u8>) -> Result<String> {
        let storage = self
            .s3_storage
            .as_ref()
            .ok_or_else(|| anyhow!("recording S3 storage is not initialized"))?;
        let bucket = Self::required(&self.policy.bucket, "bucket")?;
        storage.write(key, Bytes::from(data)).await?;
        Ok(Self::s3_url(self.policy.endpoint.as_deref(), &bucket, key))
    }

    async fn upload_http(
        &self,
        record: &CallRecord,
        track_id: &str,
        media_path: &str,
        data: Vec<u8>,
    ) -> Result<String> {
        let url = Self::required(&self.policy.url, "url")?;
        let file_name = Path::new(media_path)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("recording.wav"))
            .to_string_lossy()
            .to_string();
        let part = Part::bytes(data)
            .file_name(file_name)
            .mime_str("audio/wav")?;
        let form = Form::new()
            .text("call_id", record.call_id.clone())
            .text("track_id", track_id.to_string())
            .part("recording", part);
        let mut request = self.client.post(&url).multipart(form);
        if let Some(headers) = self.policy.headers.as_ref() {
            for (key, value) in headers {
                request = request.header(key.as_str(), value.as_str());
            }
        }
        let response = request.send().await?;
        if response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            let trimmed = body.trim();
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                Ok(trimmed.to_string())
            } else {
                Ok(url)
            }
        } else {
            Err(anyhow!(
                "HTTP upload failed: {} - {}",
                response.status(),
                response.text().await.unwrap_or_default()
            ))
        }
    }
}

#[async_trait]
impl CallRecordHook for RecordingUploadHook {
    async fn on_record_completed(&self, record: &mut CallRecord) -> anyhow::Result<()> {
        if record.answer_time.is_none() {
            return Ok(());
        }

        let mut first_uploaded_url = None;
        for index in 0..record.recorder.len() {
            let (track_id, path) = {
                let media = &record.recorder[index];
                (media.track_id.clone(), media.path.clone())
            };
            if !Path::new(&path).exists() {
                warn!(
                    call_id = %record.call_id,
                    track_id,
                    path,
                    "recording upload skipped missing local media"
                );
                continue;
            }

            let data = match tokio::fs::read(&path).await {
                Ok(data) => data,
                Err(err) => {
                    warn!(
                        call_id = %record.call_id,
                        track_id,
                        path,
                        "recording upload failed to read local media: {err}"
                    );
                    continue;
                }
            };
            let data_len = data.len();
            let key = self.storage_key(record, &track_id, &path);
            let upload = match self.policy.recording_type.unwrap_or_default() {
                RecordingType::Local => Ok(path.clone()),
                RecordingType::Http => self.upload_http(record, &track_id, &path, data).await,
                RecordingType::S3 => self.upload_s3(&key, data).await,
            };

            match upload {
                Ok(url) => {
                    info!(
                        call_id = %record.call_id,
                        track_id,
                        url,
                        bytes = data_len,
                        "recording uploaded"
                    );
                    if first_uploaded_url.is_none() {
                        first_uploaded_url = Some(url.clone());
                    }
                    if let Some(media) = record.recorder.get_mut(index) {
                        let extra = media.extra.get_or_insert_with(HashMap::new);
                        extra.insert("uploadUrl".to_string(), json!(url));
                    }
                }
                Err(err) => {
                    warn!(
                        call_id = %record.call_id,
                        track_id,
                        path,
                        "recording upload failed: {err}"
                    );
                }
            }
        }

        // Determine the url/path for RecordEnd (upload URL or local file path).
        let recording_url = first_uploaded_url
            .clone()
            .or_else(|| record.recorder.first().map(|m| m.path.clone()));

        if let Some(ref url) = first_uploaded_url {
            record.details.recording_url = Some(url.clone());
            record.details.recording_duration_secs =
                Some((record.end_time - record.start_time).num_seconds().max(0) as i32);

            // Emit RecordingMetadataAvailable webhook so external systems
            // (e.g. MQ consumers) are notified that the recording is ready.
            if let Some(ref gw) = self.rwi_gateway {
                use crate::rwi::proto::RecordingMetadata;
                let metadata = RecordingMetadata {
                    filename: record
                        .recorder
                        .first()
                        .and_then(|m| {
                            Path::new(&m.path)
                                .file_name()
                                .map(|f| f.to_string_lossy().to_string())
                        })
                        .unwrap_or_default(),
                    unique_id: record.call_id.clone(),
                    file_size: record.recorder.first().map(|m| m.size).unwrap_or(0),
                    download_url: Some(url.clone()),
                    caller_name: extract_sip_username(&record.caller),
                    callee_name: extract_sip_username(&record.callee),
                    called_phone: extract_sip_username(&record.callee),
                    call_type: record.details.direction.clone(),
                    agent_id: None,
                    agent_name: None,
                    call_start_time: Some(record.start_time.to_rfc3339()),
                    call_end_time: Some(record.end_time.to_rfc3339()),
                    upload_time: Some(chrono::Utc::now().to_rfc3339()),
                    switch_flag: None,
                    process_flag: None,
                    root_call_id: None,
                };
                let gw_ref = gw.read();
                gw_ref.send_to_owner(&crate::rwi::RecordingMetadataAvailable {
                    call_id: record.call_id.clone(),
                    metadata,
                });
                info!(
                    call_id = %record.call_id,
                    url = %url,
                    "RecordingMetadataAvailable event emitted"
                );
            }
        }

        // Emit RecordEnd with url (upload URL or local path), duration and file size.
        if let Some(ref gw) = self.rwi_gateway {
            let gw_ref = gw.read();
            gw_ref.send_to_owner(&crate::rwi::RecordEnd {
                call_id: record.call_id.clone(),
                url: recording_url,
                duration_secs: (record.end_time - record.start_time).num_seconds().max(0) as u64,
                file_size: record.recorder.first().map(|m| m.size).unwrap_or(0),
            });
            info!(
                call_id = %record.call_id,
                "RecordEnd event emitted"
            );
        }

        Ok(())
    }
}
