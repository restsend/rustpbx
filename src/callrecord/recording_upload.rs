use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use object_store::{Attribute, Attributes, PutOptions};
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
    s3_upload_sender: Option<tokio::sync::mpsc::Sender<PathBuf>>,
}

pub struct RecordingUploadManager {
    policy: RecordingPolicy,
    storage: Storage,
    receiver: tokio::sync::mpsc::Receiver<PathBuf>,
}

const RECORDING_UPLOAD_CHANNEL_CAPACITY: usize = 65_536;

impl RecordingUploadHook {
    pub fn new(policy: RecordingPolicy) -> Result<(Self, Option<RecordingUploadManager>)> {
        let recording_type = policy.effective_recording_type();
        let (s3_upload_sender, upload_manager) = if recording_type == RecordingType::S3 {
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
            let (sender, receiver) = tokio::sync::mpsc::channel(RECORDING_UPLOAD_CHANNEL_CAPACITY);
            (
                Some(sender),
                Some(RecordingUploadManager {
                    policy: policy.clone(),
                    storage,
                    receiver,
                }),
            )
        } else {
            (None, None)
        };

        Ok((
            Self {
                policy,
                rwi_gateway: None,
                client: crate::http_util::build_keepalive_client(
                    Some(CALL_RECORD_HTTP_TIMEOUT),
                    Some(CALL_RECORD_HTTP_CONNECT_TIMEOUT),
                )?,
                s3_upload_sender,
            },
            upload_manager,
        ))
    }

    pub fn with_rwi_gateway(mut self, gw: RwiGatewayRef) -> Self {
        self.rwi_gateway = Some(gw);
        self
    }

    /// Rename local recording artifacts (wav + signaling jsonl sidecar) into
    /// their daily/hourly archive subdirectory during enrichment, i.e.
    /// BEFORE the CDR row is persisted, so `recording_url`, `sipflow_jsonl`
    /// and `recording_segments` metadata reference the final on-disk layout.
    /// Archiving only in `on_record_completed` left stale pre-archive paths
    /// in the database (downloads then 404'd on the moved files).
    async fn archive_local_artifacts(&self, record: &mut CallRecord) {
        use crate::callrecord::{RecordingSubdir, local_archive_path};

        let subdir = RecordingSubdir::parse(self.policy.subdir.as_deref());
        let root = self.policy.recorder_path();

        let mut renames: HashMap<String, String> = HashMap::new();
        let mut first_media_url: Option<String> = None;

        for index in 0..record.recorder.len() {
            let (track_id, path) = {
                let media = &record.recorder[index];
                (media.track_id.clone(), media.path.clone())
            };
            // Only archive artifacts this pipeline generated directly under
            // the recorder root (segment WAVs + signaling sidecars). Files
            // recorded to operator-supplied custom paths (e.g. an RWI
            // `record` option pointing outside the root) keep their original
            // location until the completed stage, matching historical
            // behavior.
            if !is_direct_child_of_root(&root, Path::new(&path)) {
                continue;
            }
            let dest = local_archive_path(&root, Path::new(&path), subdir, record.start_time);
            if dest.as_path() == Path::new(&path) {
                continue;
            }
            if let Some(parent) = dest.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            match tokio::fs::rename(&path, &dest).await {
                Ok(()) => {
                    let archived = dest.to_string_lossy().into_owned();
                    info!(
                        call_id = %record.call_id,
                        track_id,
                        from = %path,
                        to = %archived,
                        "local recording archived"
                    );
                    if first_media_url.is_none() && track_id != "signaling" {
                        first_media_url = Some(archived.clone());
                    }
                    if let Some(media) = record.recorder.get_mut(index) {
                        media.path = archived.clone();
                        let extra = media.extra.get_or_insert_with(HashMap::new);
                        extra.insert("uploadUrl".to_string(), json!(archived.clone()));
                    }
                    renames.insert(path, archived);
                }
                Err(err) => {
                    warn!(
                        call_id = %record.call_id,
                        from = %path,
                        to = %dest.display(),
                        %err,
                        "local recording archive failed; keeping original path"
                    );
                }
            }
        }

        if renames.is_empty() {
            return;
        }

        if let Some(url) = first_media_url {
            record.details.recording_url = Some(url);
        }

        if let Some(metadata) = record.details.metadata.as_mut() {
            if let Some(serde_json::Value::String(jsonl)) = metadata.get_mut("sipflow_jsonl")
                && let Some(archived) = renames.get(jsonl)
            {
                *jsonl = archived.clone();
            }
            if let Some(serde_json::Value::Array(segments)) = metadata.get_mut("recording_segments")
            {
                for segment in segments.iter_mut() {
                    if let Some(serde_json::Value::String(path)) = segment.get_mut("path")
                        && let Some(archived) = renames.get(path)
                    {
                        *path = archived.clone();
                    }
                }
            }
        }
    }

    fn required(value: &Option<String>, name: &str) -> Result<String> {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("recording.{name} is required"))
    }

    fn storage_key(policy: &RecordingPolicy, media_path: &Path) -> String {
        let recorder_root = policy.recorder_path();
        let relative = media_path
            .strip_prefix(Path::new(&recorder_root))
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| {
                media_path
                    .file_name()
                    .map(Path::new)
                    .unwrap_or_else(|| Path::new("recording.wav"))
            });
        let key = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        match policy
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
}

impl RecordingUploadManager {
    pub async fn serve(&mut self) {
        while let Some(path) = self.receiver.recv().await {
            let key = RecordingUploadHook::storage_key(&self.policy, &path);
            let data = match tokio::fs::read(&path).await {
                Ok(data) => data,
                Err(err) => {
                    warn!(
                        path = %path.display(),
                        %err,
                        "recording uploader failed to read local media"
                    );
                    continue;
                }
            };
            let bytes = data.len();
            let content_type = match path.extension().and_then(|extension| extension.to_str()) {
                Some(extension) if extension.eq_ignore_ascii_case("wav") => "audio/wav",
                Some(extension) if extension.eq_ignore_ascii_case("jsonl") => "application/jsonl",
                _ => "application/octet-stream",
            };
            let attributes = Attributes::from_iter([(Attribute::ContentType, content_type)]);
            if let Err(err) = self
                .storage
                .write_opts(
                    &key,
                    Bytes::from(data),
                    PutOptions {
                        attributes,
                        ..Default::default()
                    },
                )
                .await
            {
                warn!(
                    path = %path.display(),
                    key,
                    %err,
                    "recording upload failed"
                );
                continue;
            }
            info!(
                path = %path.display(),
                key,
                bytes,
                content_type,
                "recording uploaded"
            );
            if let Err(err) = tokio::fs::remove_file(&path).await {
                warn!(
                    path = %path.display(),
                    %err,
                    "failed to remove local recording after upload"
                );
            }
        }
    }
}

impl RecordingUploadHook {
    fn preconstruct_s3_urls(&self, record: &mut CallRecord) -> Result<()> {
        let bucket = Self::required(&self.policy.bucket, "bucket")?;
        let mut first_media_url = None;

        for media in &mut record.recorder {
            let key = Self::storage_key(&self.policy, Path::new(&media.path));
            let url = Self::s3_url(self.policy.endpoint.as_deref(), &bucket, &key);
            let extra = media.extra.get_or_insert_with(HashMap::new);
            extra.insert("uploadUrl".to_string(), json!(url.clone()));
            if first_media_url.is_none() && media.track_id != "signaling" {
                first_media_url = Some(url);
            }
        }

        if let Some(url) = first_media_url {
            record.details.recording_url = Some(url);
            record.details.recording_duration_secs =
                Some((record.end_time - record.start_time).num_seconds().max(0) as i32);
        }
        Ok(())
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
    async fn on_record_enrich(&self, records: &mut [CallRecord]) -> anyhow::Result<()> {
        for record in records {
            match self.policy.effective_recording_type() {
                RecordingType::Local => self.archive_local_artifacts(record).await,
                RecordingType::S3 => {
                    // Move generated artifacts into their final dated layout first,
                    // then persist the remote URL before the asynchronous upload.
                    self.archive_local_artifacts(record).await;
                    self.preconstruct_s3_urls(record)?;
                }
                RecordingType::Http | RecordingType::Sipflow => {}
            }
        }
        Ok(())
    }

    async fn on_record_completed(&self, records: &mut [CallRecord]) -> anyhow::Result<()> {
        use crate::callrecord::{RecordingSubdir, local_archive_path, write_upload_failed_marker};
        use std::time::Instant;

        let recording_type = self.policy.effective_recording_type();
        if !recording_type.is_file_media() {
            return Ok(());
        }
        let subdir = RecordingSubdir::parse(self.policy.subdir.as_deref());
        let root = self.policy.recorder_path();

        for record in records {
            // File entries are finalized recording artifacts (wav + optional jsonl).
            let mut first_uploaded_url = None;
            let mut segment_summaries = Vec::new();

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

                if recording_type == RecordingType::S3 {
                    let key = Self::storage_key(&self.policy, Path::new(&path));
                    let bucket = Self::required(&self.policy.bucket, "bucket")?;
                    let url = Self::s3_url(self.policy.endpoint.as_deref(), &bucket, &key);
                    match self
                        .s3_upload_sender
                        .as_ref()
                        .ok_or_else(|| anyhow!("recording S3 uploader is not initialized"))?
                        .try_send(PathBuf::from(&path))
                    {
                        Ok(()) => info!(
                            call_id = %record.call_id,
                            track_id,
                            path,
                            key,
                            "recording queued for upload"
                        ),
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => warn!(
                            call_id = %record.call_id,
                            track_id,
                            path,
                            "recording upload channel full; local file retained"
                        ),
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => warn!(
                            call_id = %record.call_id,
                            track_id,
                            path,
                            "recording uploader stopped; local file retained"
                        ),
                    }
                    if first_uploaded_url.is_none() && track_id != "signaling" {
                        first_uploaded_url = Some(url.clone());
                    }
                    segment_summaries.push(json!({
                        "path": path,
                        "track_id": track_id,
                        "size": record.recorder[index].size,
                        "upload_url": url,
                    }));
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

                match recording_type {
                    RecordingType::Local => {
                        let dest =
                            local_archive_path(&root, Path::new(&path), subdir, record.start_time);
                        if let Some(parent) = dest.parent() {
                            let _ = tokio::fs::create_dir_all(parent).await;
                        }
                        let archived = if dest.as_path() != Path::new(&path) {
                            match tokio::fs::rename(&path, &dest).await {
                                Ok(()) => dest.to_string_lossy().into_owned(),
                                Err(err) => {
                                    warn!(
                                        call_id = %record.call_id,
                                        from = %path,
                                        to = %dest.display(),
                                        %err,
                                        "local recording archive failed; keeping original path"
                                    );
                                    path.clone()
                                }
                            }
                        } else {
                            path.clone()
                        };
                        info!(
                            call_id = %record.call_id,
                            track_id,
                            path = %archived,
                            bytes = data_len,
                            "recording archived locally"
                        );
                        if first_uploaded_url.is_none() && track_id != "signaling" {
                            first_uploaded_url = Some(archived.clone());
                        }
                        if let Some(media) = record.recorder.get_mut(index) {
                            media.path = archived.clone();
                            let extra = media.extra.get_or_insert_with(HashMap::new);
                            extra.insert("uploadUrl".to_string(), json!(archived));
                        }
                        segment_summaries.push(json!({
                            "path": archived,
                            "track_id": track_id,
                            "size": data_len,
                        }));
                    }
                    RecordingType::Http => {
                        let address = self
                            .policy
                            .url
                            .clone()
                            .unwrap_or_else(|| "http".to_string());
                        let started = Instant::now();
                        let upload = self.upload_http(record, &track_id, &path, data).await;
                        let elapsed_ms = started.elapsed().as_millis() as u64;
                        match upload {
                            Ok(url) => {
                                info!(
                                    call_id = %record.call_id,
                                    track_id,
                                    url,
                                    bytes = data_len,
                                    "recording uploaded"
                                );
                                if first_uploaded_url.is_none() && track_id != "signaling" {
                                    first_uploaded_url = Some(url.clone());
                                }
                                if let Some(media) = record.recorder.get_mut(index) {
                                    let extra = media.extra.get_or_insert_with(HashMap::new);
                                    extra.insert("uploadUrl".to_string(), json!(url));
                                }
                                // Only delete local file after a successful remote upload.
                                if let Err(err) = tokio::fs::remove_file(&path).await {
                                    warn!(
                                        call_id = %record.call_id,
                                        path,
                                        %err,
                                        "failed to remove local recording after upload"
                                    );
                                } else {
                                    // Drop companion failure marker if a prior attempt left one.
                                    let marker = crate::callrecord::upload_failed_marker_path(
                                        Path::new(&path),
                                    );
                                    let _ = tokio::fs::remove_file(marker).await;
                                }
                                segment_summaries.push(json!({
                                    "path": path,
                                    "track_id": track_id,
                                    "size": data_len,
                                    "upload_url": url,
                                }));
                            }
                            Err(err) => {
                                warn!(
                                    call_id = %record.call_id,
                                    track_id,
                                    path,
                                    "recording upload failed: {err}"
                                );
                                if let Err(write_err) = write_upload_failed_marker(
                                    Path::new(&path),
                                    &address,
                                    elapsed_ms,
                                    &err.to_string(),
                                )
                                .await
                                {
                                    warn!(
                                        call_id = %record.call_id,
                                        path,
                                        %write_err,
                                        "failed to write upload failure marker"
                                    );
                                }
                            }
                        }
                    }
                    RecordingType::S3 => unreachable!("S3 uploads are queued before file reads"),
                    RecordingType::Sipflow => unreachable!("file upload path filtered earlier"),
                }
            }

            // Determine the URL/path from concrete recording evidence only.
            let recording_url = first_uploaded_url
                .clone()
                .or_else(|| record.details.recording_url.clone())
                .or_else(|| {
                    record
                        .recorder
                        .iter()
                        .find(|m| m.track_id != "signaling")
                        .map(|m| m.path.clone())
                });

            // No file was recorded/uploaded and no SipFlow upload URL was supplied.
            if recording_url.is_none() {
                continue;
            }

            let emit_url = first_uploaded_url.as_deref().or_else(|| {
                (record.recorder.is_empty() && record.details.recording_url.is_some())
                    .then(|| recording_url.as_deref().unwrap_or(""))
            });

            if let Some(url) = emit_url {
                let duration_secs =
                    (record.end_time - record.start_time).num_seconds().max(0) as i32;
                record.details.recording_url = Some(url.to_string());
                record.details.recording_duration_secs = Some(duration_secs);

                if !segment_summaries.is_empty() {
                    let meta = record.details.metadata.get_or_insert_with(HashMap::new);
                    meta.insert("recording_segments".to_string(), json!(segment_summaries));
                }

                if let Some(ref gw) = self.rwi_gateway {
                    use crate::rwi::proto::RecordingMetadata;
                    let mut extra = record.details.metadata.clone().map(|m| {
                        m.into_iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                            .collect::<HashMap<_, _>>()
                    });
                    if !segment_summaries.is_empty() {
                        let bag = extra.get_or_insert_with(HashMap::new);
                        if let Ok(s) = serde_json::to_string(&segment_summaries) {
                            bag.insert("recording_segments".to_string(), s);
                        }
                    }
                    let metadata = RecordingMetadata {
                        filename: recording_filename(record, url),
                        file_size: recording_file_size(record),
                        download_url: Some(url.to_string()),
                        caller_name: extract_sip_username(&record.caller),
                        callee_name: extract_sip_username(&record.callee),
                        call_type: record.details.direction.clone(),
                        call_start_time: Some(record.start_time.to_rfc3339()),
                        call_end_time: Some(record.end_time.to_rfc3339()),
                        upload_time: Some(chrono::Utc::now().to_rfc3339()),
                        extra,
                    };
                    let gw_ref = gw.read();
                    gw_ref.send_to_owner(&crate::rwi::RecordingMetadataAvailable {
                        call_id: record.call_id.clone(),
                        metadata,
                    });
                }
            }

            // Emit RecordEnd with url (upload URL, local path, or sipflow reference).
            if let Some(ref gw) = self.rwi_gateway {
                let gw_ref = gw.read();
                gw_ref.send_to_owner(&crate::rwi::RecordEnd {
                    call_id: record.call_id.clone(),
                    url: recording_url,
                    duration_secs: (record.end_time - record.start_time).num_seconds().max(0)
                        as u64,
                    file_size: record
                        .recorder
                        .iter()
                        .find(|m| m.track_id != "signaling")
                        .map(|m| m.size)
                        .unwrap_or(0),
                });
            }
        }

        Ok(())
    }
}

/// True when `path` sits directly inside `root` (ignoring `./` prefixes),
/// i.e. it is a pipeline-generated artifact name like `{root}/{file}.wav`.
fn is_direct_child_of_root(root: &str, path: &Path) -> bool {
    let normalize = |p: &Path| -> Vec<String> {
        p.components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect()
    };
    match path.parent() {
        Some(parent) => normalize(parent) == normalize(Path::new(root)),
        None => false,
    }
}

/// Derive the recording file name for `recording_metadata_available`. A local
/// WAV recorder file wins; when media was captured via SipFlow there is no
/// local file, so fall back to the last path segment of the stashed URL and
/// finally to `{call_id}.wav`. Recordings are always WAV, so the returned name
/// always carries a `.wav` extension.
fn recording_filename(record: &CallRecord, url: &str) -> String {
    if let Some(name) = record.recorder.first().and_then(|m| {
        Path::new(&m.path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
    }) {
        return name;
    }
    match url.split(['/', '\\', '?', '#']).rfind(|s| !s.is_empty()) {
        Some(segment) => {
            let stem = segment.rsplit('.').next_back().unwrap_or(segment);
            format!("{stem}.wav")
        }
        None => format!("{}.wav", record.call_id),
    }
}

/// Resolve the recording file size for `recording_metadata_available`: the
/// local WAV recorder file size, else the size stashed by the SipFlow upload
/// hooks, else 0.
fn recording_file_size(record: &CallRecord) -> u64 {
    record
        .recorder
        .first()
        .map(|m| m.size)
        .or_else(|| {
            record
                .extensions
                .get::<crate::callrecord::RecordingFileSize>()
                .map(|s| s.0)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callrecord::{CallDetails, CallRecordMedia};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn s3_enrich_preconstructs_recording_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder_root = dir.path().join("recorders");
        tokio::fs::create_dir_all(&recorder_root)
            .await
            .expect("create recorder root");
        let recording = recorder_root.join("call.wav");
        tokio::fs::write(&recording, b"wav")
            .await
            .expect("write recording");
        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::S3),
            path: Some(recorder_root.to_string_lossy().into_owned()),
            vendor: Some(crate::storage::S3Vendor::Minio),
            bucket: Some("recordings-bucket".into()),
            region: Some("test".into()),
            access_key: Some("test".into()),
            secret_key: Some("test".into()),
            endpoint: Some("http://127.0.0.1:9000".into()),
            root: Some("recordings".into()),
            ..Default::default()
        };
        let (hook, _upload_manager) = RecordingUploadHook::new(policy).expect("recording hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "preconstructed-url".into(),
            start_time: now,
            end_time: now + chrono::Duration::seconds(60),
            recorder: vec![CallRecordMedia {
                track_id: "mixed".into(),
                path: recording.to_string_lossy().into_owned(),
                size: 3,
                extra: None,
            }],
            details: CallDetails::default(),
            ..Default::default()
        };

        hook.on_record_enrich(std::slice::from_mut(&mut record))
            .await
            .expect("enrich");

        let day = now.format("%Y%m%d");
        let expected = format!("http://127.0.0.1:9000/recordings-bucket/recordings/{day}/call.wav");
        assert_eq!(
            record.details.recording_url.as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(record.details.recording_duration_secs, Some(60));
        assert_eq!(
            record.recorder[0]
                .extra
                .as_ref()
                .and_then(|extra| extra.get("uploadUrl"))
                .and_then(|url| url.as_str()),
            Some(expected.as_str())
        );
        assert!(Path::new(&record.recorder[0].path).exists());
        assert!(!recording.exists());
    }

    #[tokio::test]
    async fn s3_uploader_continues_after_failure_and_removes_successful_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder_root = dir.path().join("recorders");
        let dated_root = recorder_root.join("20260826");
        tokio::fs::create_dir_all(&dated_root)
            .await
            .expect("create recorder root");
        let missing = dated_root.join("missing.wav");
        let recording = dated_root.join("call.wav");
        tokio::fs::write(&recording, b"wav")
            .await
            .expect("write recording");
        let object_root = dir.path().join("objects");
        let storage = Storage::new(&StorageConfig::Local {
            path: object_root.to_string_lossy().into_owned(),
        })
        .expect("local object storage");
        let policy = RecordingPolicy {
            path: Some(recorder_root.to_string_lossy().into_owned()),
            root: Some("recordings".into()),
            ..Default::default()
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let mut manager = RecordingUploadManager {
            policy,
            storage,
            receiver,
        };
        let uploader = crate::utils::spawn(async move {
            manager.serve().await;
        });

        sender.send(missing).await.expect("queue missing path");
        sender
            .send(recording.clone())
            .await
            .expect("queue recording");
        drop(sender);
        uploader.await.expect("uploader task");

        assert!(!recording.exists(), "successful upload removes local file");
        assert_eq!(
            tokio::fs::read(object_root.join("recordings/20260826/call.wav"))
                .await
                .expect("uploaded object"),
            b"wav"
        );
    }

    #[tokio::test]
    async fn uploads_file_recording_from_unanswered_early_media_call() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = axum::Router::new().route(
            "/recording",
            axum::routing::post(move |_request: axum::extract::Request| {
                let request_count = request_count.clone();
                async move {
                    request_count.fetch_add(1, Ordering::Relaxed);
                    "https://recordings.example/early-media.wav"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upload server");
        let address = listener.local_addr().expect("upload server address");
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("early-media.wav");
        tokio::fs::write(&path, b"recorded early media")
            .await
            .expect("write recording");
        let path = path.to_string_lossy().into_owned();
        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Http),
            url: Some(format!("http://{address}/recording")),
            ..Default::default()
        };
        let (hook, _upload_manager) = RecordingUploadHook::new(policy).expect("recording hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "early-media-call".to_string(),
            start_time: now - chrono::Duration::seconds(8),
            answer_time: None,
            end_time: now,
            recorder: vec![CallRecordMedia {
                track_id: "mixed".to_string(),
                path: path.clone(),
                size: 20,
                extra: None,
            }],
            details: CallDetails {
                status: "failed".to_string(),
                recording_url: Some(path.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .expect("upload early media");

        assert_eq!(requests.load(Ordering::Relaxed), 1);
        assert_eq!(
            record.details.recording_url.as_deref(),
            Some("https://recordings.example/early-media.wav")
        );
        assert_eq!(record.details.recording_duration_secs, Some(8));
        assert_eq!(
            record.recorder[0]
                .extra
                .as_ref()
                .and_then(|extra| extra.get("uploadUrl"))
                .and_then(|url| url.as_str()),
            Some("https://recordings.example/early-media.wav")
        );
        assert!(
            !Path::new(&path).exists(),
            "local wav should be deleted after successful HTTP upload"
        );
    }

    #[tokio::test]
    async fn writes_upload_failed_marker_and_keeps_local_file() {
        use crate::callrecord::upload_failed_marker_path;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fail.wav");
        tokio::fs::write(&path, b"wav-bytes")
            .await
            .expect("write recording");
        let path_str = path.to_string_lossy().into_owned();
        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Http),
            url: Some("http://127.0.0.1:1/recording".to_string()),
            ..Default::default()
        };
        let (hook, _upload_manager) = RecordingUploadHook::new(policy).expect("recording hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "upload-fail-call".to_string(),
            start_time: now - chrono::Duration::seconds(5),
            answer_time: Some(now - chrono::Duration::seconds(4)),
            end_time: now,
            recorder: vec![CallRecordMedia {
                track_id: "mixed".to_string(),
                path: path_str.clone(),
                size: 9,
                extra: None,
            }],
            details: CallDetails::default(),
            ..Default::default()
        };
        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .expect("hook should not fail hard");

        assert!(path.exists(), "local file kept after upload failure");
        let marker = upload_failed_marker_path(&path);
        assert!(marker.exists(), "failure marker written");
        let body = tokio::fs::read_to_string(&marker)
            .await
            .expect("read marker");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("marker json");
        assert!(parsed.get("time").is_some());
        assert!(parsed.get("address").is_some());
        assert!(parsed.get("duration_ms").is_some());
        assert!(parsed.get("error").is_some());
    }

    #[tokio::test]
    async fn local_type_archives_into_daily_subdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_string_lossy().into_owned();
        let path = dir.path().join("sess.wav");
        tokio::fs::write(&path, b"wav").await.expect("write");
        let path_str = path.to_string_lossy().into_owned();
        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Local),
            path: Some(root.clone()),
            subdir: Some("daily".into()),
            ..Default::default()
        };
        let (hook, _upload_manager) = RecordingUploadHook::new(policy).expect("hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "local-archive".into(),
            start_time: now,
            answer_time: Some(now),
            end_time: now,
            recorder: vec![CallRecordMedia {
                track_id: "mixed".into(),
                path: path_str,
                size: 3,
                extra: None,
            }],
            details: CallDetails::default(),
            ..Default::default()
        };
        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .expect("archive");
        let day = now.format("%Y%m%d").to_string();
        let archived = Path::new(&root).join(&day).join("sess.wav");
        assert!(archived.exists(), "archived under daily subdir");
        assert_eq!(
            record.details.recording_url.as_deref(),
            Some(archived.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn local_type_archives_into_hourly_subdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_string_lossy().into_owned();
        let path = dir.path().join("sess.wav");
        tokio::fs::write(&path, b"wav").await.expect("write");
        let path_str = path.to_string_lossy().into_owned();
        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Local),
            path: Some(root.clone()),
            subdir: Some("hourly".into()),
            ..Default::default()
        };
        let (hook, _upload_manager) = RecordingUploadHook::new(policy).expect("hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "local-hourly".into(),
            start_time: now,
            answer_time: Some(now),
            end_time: now,
            recorder: vec![CallRecordMedia {
                track_id: "mixed".into(),
                path: path_str,
                size: 3,
                extra: None,
            }],
            details: CallDetails::default(),
            ..Default::default()
        };
        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .expect("archive");
        let day = now.format("%Y%m%d").to_string();
        let hour = now.format("%H").to_string();
        let archived = Path::new(&root).join(&day).join(&hour).join("sess.wav");
        assert!(archived.exists(), "archived under hourly subdir");
    }

    #[tokio::test]
    async fn uploads_wav_and_jsonl_then_deletes_both() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = axum::Router::new().route(
            "/recording",
            axum::routing::post(move |_request: axum::extract::Request| {
                let request_count = request_count.clone();
                async move {
                    let n = request_count.fetch_add(1, Ordering::Relaxed);
                    format!("https://recordings.example/file-{n}")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let wav = dir.path().join("a.wav");
        let jsonl = dir.path().join("a.jsonl");
        tokio::fs::write(&wav, b"wav").await.unwrap();
        tokio::fs::write(&jsonl, b"{}\n").await.unwrap();
        let wav_s = wav.to_string_lossy().into_owned();
        let jsonl_s = jsonl.to_string_lossy().into_owned();

        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Http),
            url: Some(format!("http://{address}/recording")),
            ..Default::default()
        };
        let (hook, _upload_manager) = RecordingUploadHook::new(policy).unwrap();
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "multi-artifact".into(),
            start_time: now - chrono::Duration::seconds(3),
            answer_time: Some(now - chrono::Duration::seconds(2)),
            end_time: now,
            recorder: vec![
                CallRecordMedia {
                    track_id: "segment:ivr:1".into(),
                    path: wav_s.clone(),
                    size: 3,
                    extra: None,
                },
                CallRecordMedia {
                    track_id: "signaling".into(),
                    path: jsonl_s.clone(),
                    size: 3,
                    extra: None,
                },
            ],
            details: CallDetails::default(),
            ..Default::default()
        };
        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .unwrap();

        assert_eq!(requests.load(Ordering::Relaxed), 2);
        assert!(!Path::new(&wav_s).exists());
        assert!(!Path::new(&jsonl_s).exists());
        assert!(
            record
                .details
                .recording_url
                .as_deref()
                .is_some_and(|u| u.starts_with("https://recordings.example/"))
        );
    }

    /// Regression: local artifacts must be archived during enrichment (before
    /// the CDR row is persisted) so `recording_url`, `sipflow_jsonl` and
    /// `recording_segments` metadata reference the final daily layout —
    /// archiving only in `on_record_completed` left stale pre-archive paths
    /// in the database and downloads 404'd.
    #[tokio::test]
    async fn enrich_archives_into_daily_subdir_and_rewrites_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_string_lossy().into_owned();
        let wav = dir.path().join("sess.wav");
        let jsonl = dir.path().join("sess.jsonl");
        tokio::fs::write(&wav, b"wav").await.expect("write wav");
        tokio::fs::write(&jsonl, b"{}\n")
            .await
            .expect("write jsonl");
        let wav_s = wav.to_string_lossy().into_owned();
        let jsonl_s = jsonl.to_string_lossy().into_owned();

        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Local),
            path: Some(root.clone()),
            subdir: Some("daily".into()),
            ..Default::default()
        };
        let (hook, _upload_manager) = RecordingUploadHook::new(policy).expect("hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "enrich-archive".into(),
            start_time: now,
            answer_time: Some(now),
            end_time: now,
            recorder: vec![
                CallRecordMedia {
                    track_id: "segment:full:1".into(),
                    path: wav_s,
                    size: 3,
                    extra: None,
                },
                CallRecordMedia {
                    track_id: "signaling".into(),
                    path: jsonl_s,
                    size: 2,
                    extra: None,
                },
            ],
            details: CallDetails {
                metadata: Some(HashMap::from([
                    (
                        "sipflow_jsonl".to_string(),
                        json!(jsonl.to_string_lossy().into_owned()),
                    ),
                    (
                        "recording_segments".to_string(),
                        json!([{ "path": wav.to_string_lossy().into_owned() }]),
                    ),
                ])),
                ..Default::default()
            },
            ..Default::default()
        };

        hook.on_record_enrich(std::slice::from_mut(&mut record))
            .await
            .expect("enrich");

        let day = now.format("%Y%m%d").to_string();
        let archived_wav = Path::new(&root).join(&day).join("sess.wav");
        let archived_jsonl = Path::new(&root).join(&day).join("sess.jsonl");
        assert!(archived_wav.exists(), "wav archived under daily subdir");
        assert!(archived_jsonl.exists(), "jsonl archived under daily subdir");
        assert!(!wav.exists() && !jsonl.exists(), "originals moved");

        assert_eq!(
            record.recorder[0].path,
            archived_wav.to_string_lossy().into_owned()
        );
        assert_eq!(
            record.details.recording_url.as_deref(),
            Some(archived_wav.to_string_lossy().as_ref()),
            "recording_url must reference the archived path before the DB save"
        );
        let meta = record.details.metadata.as_ref().expect("metadata kept");
        assert_eq!(
            meta.get("sipflow_jsonl"),
            Some(&json!(archived_jsonl.to_string_lossy().into_owned()))
        );
        assert_eq!(
            meta.get("recording_segments")
                .and_then(|s| s.get(0))
                .and_then(|s| s.get("path")),
            Some(&json!(archived_wav.to_string_lossy().into_owned()))
        );

        // completed after enrich is idempotent: no second move, URL stable.
        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .expect("completed");
        assert!(archived_wav.exists() && archived_jsonl.exists());
        assert_eq!(
            record.details.recording_url.as_deref(),
            Some(archived_wav.to_string_lossy().as_ref())
        );
    }

    /// Files recorded to operator-supplied paths outside the recorder root
    /// (e.g. an RWI `record` option) must not be moved during enrichment —
    /// they keep the historical completed-stage behavior.
    #[tokio::test]
    async fn enrich_keeps_custom_path_recordings_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("recorders");
        tokio::fs::create_dir_all(&root).await.expect("mkdir root");
        let custom_dir = dir.path().join("custom");
        tokio::fs::create_dir_all(&custom_dir)
            .await
            .expect("mkdir custom");
        let wav = custom_dir.join("ob.wav");
        tokio::fs::write(&wav, b"wav").await.expect("write");

        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Local),
            path: Some(root.to_string_lossy().into_owned()),
            subdir: Some("daily".into()),
            ..Default::default()
        };
        let (hook, _upload_manager) = RecordingUploadHook::new(policy).expect("hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "custom-path".into(),
            start_time: now,
            answer_time: Some(now),
            end_time: now,
            recorder: vec![CallRecordMedia {
                track_id: "mixed".into(),
                path: wav.to_string_lossy().into_owned(),
                size: 3,
                extra: None,
            }],
            details: CallDetails::default(),
            ..Default::default()
        };

        hook.on_record_enrich(std::slice::from_mut(&mut record))
            .await
            .expect("enrich");
        assert!(
            wav.exists(),
            "enrich must not move files outside the recorder root"
        );
        assert_eq!(record.recorder[0].path, wav.to_string_lossy().into_owned());
        assert_eq!(
            record.details.recording_url, None,
            "enrich must not synthesize a recording_url when nothing moved"
        );
    }
}
