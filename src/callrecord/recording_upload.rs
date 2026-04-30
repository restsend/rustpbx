use std::{collections::HashMap, path::Path};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::multipart::{Form, Part};
use serde_json::json;
use tracing::{info, warn};

use crate::{
    callrecord::{CallRecord, CallRecordHook},
    config::{RecordingPolicy, RecordingType},
    storage::{Storage, StorageConfig},
};

pub struct RecordingUploadHook {
    policy: RecordingPolicy,
}

impl RecordingUploadHook {
    pub fn new(policy: RecordingPolicy) -> Self {
        Self { policy }
    }

    fn required(value: &Option<String>, name: &str) -> Result<String> {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("recording.{name} is required"))
    }

    fn storage_key(&self, record: &CallRecord, track_id: &str, media_path: &str) -> String {
        let file_name = Path::new(media_path)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("recording.wav"))
            .to_string_lossy();
        let key = format!(
            "{}/{}_{}_{}",
            record.start_time.format("%Y%m%d"),
            record.call_id,
            track_id,
            file_name
        );
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
        match endpoint.map(str::trim).filter(|endpoint| !endpoint.is_empty()) {
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
        let bucket = Self::required(&self.policy.bucket, "bucket")?;
        let region = Self::required(&self.policy.region, "region")?;
        let access_key = Self::required(&self.policy.access_key, "access_key")?;
        let secret_key = Self::required(&self.policy.secret_key, "secret_key")?;
        let endpoint = self
            .policy
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .map(str::to_string);
        let vendor = self.policy.vendor.clone().unwrap_or_default();
        let storage = Storage::new(&StorageConfig::S3 {
            vendor,
            bucket: bucket.clone(),
            region,
            access_key,
            secret_key,
            endpoint: endpoint.clone(),
            prefix: None,
        })?;
        storage.write(key, Bytes::from(data)).await?;
        Ok(Self::s3_url(endpoint.as_deref(), &bucket, key))
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
        let client = reqwest::Client::new();
        let mut request = client.post(&url).multipart(form);
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
        if !self.policy.uploads_recording() {
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
            let upload = match self.policy.recording_type {
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

        if let Some(url) = first_uploaded_url {
            record.details.recording_url = Some(url);
            record.details.recording_duration_secs =
                Some((record.end_time - record.start_time).num_seconds().max(0) as i32);
        }

        Ok(())
    }
}
