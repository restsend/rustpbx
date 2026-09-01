use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Local, TimeZone};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, warn};

use crate::{
    callrecord::{
        CALL_RECORD_HTTP_CONNECT_TIMEOUT, CALL_RECORD_HTTP_TIMEOUT, CallRecord, CallRecordHook,
        format_sipflow_media_key, format_sipflow_signaling_file_name, format_sipflow_signaling_key,
        sipflow::SipFlowSlot,
    },
    config::SipFlowUploadConfig,
    sipflow::SipFlowBackend,
    storage::{Storage, StorageConfig},
};

pub struct SipFlowUploadHook {
    backend: Arc<dyn SipFlowBackend>,
    /// Late-bound handle to the SipFlow wrapper (writer batch + backend);
    /// flushed before uploading so the tail messages are persisted.
    sipflow: SipFlowSlot,
    upload_config: SipFlowUploadConfig,
    db: Option<DatabaseConnection>,
    client: reqwest::Client,
    s3_storage: Option<Storage>,
}

impl SipFlowUploadHook {
    pub fn new(
        backend: Arc<dyn SipFlowBackend>,
        sipflow: SipFlowSlot,
        upload_config: SipFlowUploadConfig,
        db: Option<DatabaseConnection>,
    ) -> Result<Self> {
        let s3_storage = build_s3_storage(&upload_config)?;

        Ok(Self {
            backend,
            sipflow,
            upload_config,
            db,
            client: crate::http_util::build_keepalive_client(
                Some(CALL_RECORD_HTTP_TIMEOUT),
                Some(CALL_RECORD_HTTP_CONNECT_TIMEOUT),
            )?,
            s3_storage,
        })
    }
}

#[async_trait]
impl CallRecordHook for SipFlowUploadHook {
    async fn on_record_completed(&self, records: &mut [CallRecord]) -> anyhow::Result<()> {
        for record in records {
            let call_id = record.call_id.as_str();
            let signaling_call_ids = record
                .sip_leg_roles
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            let start = Local.from_utc_datetime(&record.start_time.naive_utc());
            let end = Local.from_utc_datetime(&record.end_time.naive_utc());
            let duration_secs = (record.end_time - record.start_time).num_seconds() as i32;

            let media_key = format_sipflow_media_key(record);
            let signaling_key = format_sipflow_signaling_key(record);
            let signaling_file_name = format_sipflow_signaling_file_name(record);

            // When the call used file media (local/http/s3), WAV artifacts are in
            // `record.recorder` and RecordingUploadHook owns media upload. Skip
            // sipflow media upload to avoid a redundant empty WAV — but still
            // upload signalling if configured.
            let skip_media = record.recorder.iter().any(|m| m.track_id != "signaling");

            // File-media hybrid: default signaling upload on when unset.
            // Full sipflow media path: keep historical default (signaling off).
            let signaling_default = skip_media;

            if let Some((url, size)) = crate::callrecord::sipflow_upload::do_upload(
                self.backend.as_ref(),
                &self.sipflow,
                &self.upload_config,
                self.db.as_ref(),
                &self.client,
                self.s3_storage.as_ref(),
                call_id,
                &signaling_call_ids,
                start,
                end,
                duration_secs,
                &media_key,
                &signaling_key,
                &signaling_file_name,
                skip_media,
                signaling_default,
            )
            .await
            {
                record.details.recording_url = Some(url);
                record.details.recording_duration_secs = Some(duration_secs.max(0));
                record
                    .extensions
                    .insert(crate::callrecord::RecordingFileSize(size));
            }
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn do_upload(
    backend: &dyn SipFlowBackend,
    sipflow: &crate::callrecord::sipflow::SipFlowSlot,
    upload_config: &SipFlowUploadConfig,
    db: Option<&DatabaseConnection>,
    client: &reqwest::Client,
    s3_storage: Option<&Storage>,
    call_id: &str,
    signaling_call_ids: &[String],
    start: DateTime<Local>,
    end: DateTime<Local>,
    duration_secs: i32,
    media_key: &str,
    signaling_key: &str,
    signaling_file_name: &str,
    skip_media: bool,
    signaling_default: bool,
) -> Option<(String, u64)> {
    // Flush the writer batch + backend pipeline so the tail messages (BYE /
    // 200 OK) are persisted before querying/uploading.
    crate::callrecord::sipflow::flush_hook_pipeline(sipflow, backend).await;

    let root = match upload_config {
        SipFlowUploadConfig::S3 { root, .. } => root.as_str(),
        SipFlowUploadConfig::Http { .. } => "",
    };
    let full_media_key = join_root(root, media_key);
    let full_signaling_key = join_root(root, signaling_key);

    let media_enabled = !skip_media
        && match upload_config {
            SipFlowUploadConfig::S3 { media, .. } => media.unwrap_or(true),
            SipFlowUploadConfig::Http { media, .. } => media.unwrap_or(true),
        };

    let mut first_uploaded_url = None;
    let mut uploaded_file_size = 0u64;
    if !media_enabled {
        info!(
            call_id,
            "SipFlowUploadHook: media upload disabled, skipping"
        );
    } else {
        if let Some((url, size)) = upload_media(
            backend,
            upload_config,
            call_id,
            start,
            end,
            &full_media_key,
            db,
            duration_secs,
            client,
            s3_storage,
        )
        .await
        {
            first_uploaded_url = Some(url);
            uploaded_file_size = size;
        }
    }

    let signaling = match upload_config {
        SipFlowUploadConfig::S3 { signaling, .. } => signaling.unwrap_or(signaling_default),
        SipFlowUploadConfig::Http { signaling, .. } => signaling.unwrap_or(signaling_default),
    };

    if signaling {
        upload_signaling_flow(
            upload_config,
            backend,
            call_id,
            signaling_call_ids,
            start,
            end,
            &full_signaling_key,
            signaling_file_name,
            client,
            s3_storage,
        )
        .await;
    }

    first_uploaded_url.map(|url| (url, uploaded_file_size))
}

#[allow(clippy::too_many_arguments)]
/// Returns the upload URL and file size on success, None otherwise.
pub async fn upload_media(
    backend: &dyn SipFlowBackend,
    upload_config: &SipFlowUploadConfig,
    call_id: &str,
    start: DateTime<Local>,
    end: DateTime<Local>,
    full_media_key: &str,
    db: Option<&DatabaseConnection>,
    duration_secs: i32,
    client: &reqwest::Client,
    s3_storage: Option<&Storage>,
) -> Option<(String, u64)> {
    let temp_file: tempfile::NamedTempFile =
        match backend.generate_wav_file(call_id, start, end, None).await {
            Ok(f) => f,
            Err(e) => {
                warn!(call_id, "SipFlowUploadHook: generate_wav_file failed: {e}");
                return None;
            }
        };

    let temp_path = temp_file.path().to_owned();
    let file_size = match tokio::fs::metadata(&temp_path).await {
        Ok(m) => m.len() as usize,
        Err(e) => {
            warn!(call_id, "SipFlowUploadHook: temp file metadata failed: {e}");
            return None;
        }
    };

    if file_size <= 44 {
        return None;
    }

    let url_result = match upload_config {
        SipFlowUploadConfig::S3 {
            bucket, endpoint, ..
        } => {
            let wav_bytes = match tokio::fs::read(&temp_path).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(call_id, "SipFlowUploadHook: read temp file failed: {e}");
                    return None;
                }
            };
            let Some(storage) = s3_storage else {
                return None;
            };
            upload_s3(storage, full_media_key, wav_bytes)
                .await
                .map(|_| sipflow_s3_url(endpoint, bucket, full_media_key))
        }
        SipFlowUploadConfig::Http { url, headers, .. } => {
            upload_http_file(client, url, headers.as_ref(), call_id, &temp_path).await
        }
    };

    match url_result {
        Ok(url) => {
            info!(
                call_id,
                url,
                bytes = file_size,
                "SipFlowUploadHook: recording uploaded"
            );
            if let Some(db) = db {
                if let Err(e) = crate::models::call_record::update_recording_url(
                    db,
                    call_id,
                    &url,
                    duration_secs,
                )
                .await
                {
                    warn!(
                        call_id,
                        "SipFlowUploadHook: failed to update recording_url: {e}"
                    );
                }
            }
            Some((url, file_size as u64))
        }
        Err(e) => {
            warn!(call_id, "SipFlowUploadHook: upload failed: {e}");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// Returns true if signaling was successfully uploaded, false otherwise.
pub async fn upload_signaling_flow(
    upload_config: &SipFlowUploadConfig,
    backend: &dyn SipFlowBackend,
    call_id: &str,
    signaling_call_ids: &[String],
    start: DateTime<Local>,
    end: DateTime<Local>,
    full_signaling_key: &str,
    signaling_file_name: &str,
    client: &reqwest::Client,
    s3_storage: Option<&Storage>,
) -> bool {
    let mut query_call_ids = signaling_call_ids.to_vec();
    if !query_call_ids.iter().any(|id| id == call_id) {
        query_call_ids.push(call_id.to_string());
    }

    let mut flow_items = Vec::new();
    for leg_call_id in query_call_ids {
        match backend.query_flow(&leg_call_id, start, end).await {
            Ok(mut items) => flow_items.append(&mut items),
            Err(e) => {
                warn!(call_id, leg_call_id, "SipFlowUploadHook: query_flow failed: {e}");
                return false;
            }
        }
    }
    flow_items.sort_by_key(|item| (item.timestamp, item.seq));

    if flow_items.is_empty() {
        return false;
    }

    let jsonl = crate::sipflow::SipFlowQuery::export_jsonl(&flow_items);
    let data = jsonl.into_bytes();

    let result = match upload_config {
        SipFlowUploadConfig::S3 { .. } => {
            let Some(storage) = s3_storage else {
                warn!(call_id, "SipFlowUploadHook: S3 storage is not initialized");
                return false;
            };
            upload_s3(storage, full_signaling_key, data).await
        }
        SipFlowUploadConfig::Http { url, headers, .. } => {
            upload_http_jsonl(client, url, headers.as_ref(), signaling_file_name, data).await
        }
    };

    match result {
        Ok(_) => {
            info!(call_id, "SipFlowUploadHook: signaling uploaded");
            true
        }
        Err(e) => {
            warn!(call_id, "SipFlowUploadHook: signaling upload failed: {e}");
            false
        }
    }
}

// ── Shared helpers (used by bin and hook) ─────────────────────────────────────

pub fn build_s3_storage(upload_config: &SipFlowUploadConfig) -> Result<Option<Storage>> {
    match upload_config {
        SipFlowUploadConfig::S3 {
            vendor,
            bucket,
            region,
            access_key,
            secret_key,
            endpoint,
            ..
        } => Ok(Some(Storage::new(&StorageConfig::S3 {
            vendor: vendor.clone(),
            bucket: bucket.clone(),
            region: region.clone(),
            access_key: access_key.clone(),
            secret_key: secret_key.clone(),
            endpoint: Some(endpoint.clone()),
            prefix: None,
        })?)),
        SipFlowUploadConfig::Http { .. } => Ok(None),
    }
}

pub fn join_root(root: &str, key: &str) -> String {
    if root.is_empty() {
        key.to_string()
    } else {
        format!("{}/{}", root.trim_end_matches('/'), key)
    }
}

// ── Wire types for POST /upload ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SipFlowUploadRequest {
    pub call_id: String,
    #[serde(default)]
    pub signaling_call_ids: Vec<String>,
    pub start: i64,
    pub end: i64,
    pub upload: SipFlowUploadConfig,
    #[serde(default)]
    pub media_key: Option<String>,
    #[serde(default)]
    pub signaling_key: Option<String>,
    #[serde(default)]
    pub signaling_file_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SipFlowUploadResponse {
    pub media_url: Option<String>,
    pub media_size: u64,
    pub signaling_uploaded: bool,
}

// ── Internal upload helpers ───────────────────────────────────────────────────

pub(crate) fn sipflow_s3_url(endpoint: &str, bucket: &str, key: &str) -> String {
    format!(
        "{}/{}/{}",
        endpoint.trim_end_matches('/'),
        bucket.trim_matches('/'),
        key.trim_start_matches('/')
    )
}

async fn upload_s3(storage: &Storage, key: &str, data: Vec<u8>) -> Result<()> {
    storage.write(key, Bytes::from(data)).await?;
    Ok(())
}

async fn upload_http_file(
    client: &reqwest::Client,
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
    call_id: &str,
    file_path: &std::path::Path,
) -> Result<String> {
    let file_name = format!("{}.wav", call_id);
    let file = tokio::fs::File::open(file_path).await?;
    let part = reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(
        tokio_util::io::ReaderStream::new(file),
    ))
    .file_name(file_name)
    .mime_str("audio/wav")?;
    let form = reqwest::multipart::Form::new().part("recording", part);

    let mut req = client.post(url).multipart(form);
    if let Some(h) = headers {
        for (k, v) in h {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    let response = req.send().await?;
    if response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        let recording_url = if body.starts_with("http") {
            body.trim().to_string()
        } else {
            url.to_string()
        };
        Ok(recording_url)
    } else {
        Err(anyhow::anyhow!(
            "HTTP upload failed: {} – {}",
            response.status(),
            response.text().await.unwrap_or_default()
        ))
    }
}

async fn upload_http_jsonl(
    client: &reqwest::Client,
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
    file_name: &str,
    data: Vec<u8>,
) -> Result<()> {
    let part = reqwest::multipart::Part::bytes(data)
        .file_name(file_name.to_string())
        .mime_str("application/jsonl")?;
    let form = reqwest::multipart::Form::new().part("signaling", part);

    let mut req = client.post(url).multipart(form);
    if let Some(h) = headers {
        for (k, v) in h {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    let response = req.send().await?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "HTTP signaling upload failed: {} – {}",
            response.status(),
            response.text().await.unwrap_or_default()
        ))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMediaStats};
    use chrono::{DateTime, Local};
    use std::borrow::Cow;

    struct MockBackend {
        media: Vec<u8>,
        flush_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        queried_call_ids: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl SipFlowBackend for MockBackend {
        fn record(&self, _call_id: Cow<'_, str>, _item: SipFlowItem) -> anyhow::Result<()> {
            Ok(())
        }
        async fn flush(&self) -> anyhow::Result<()> {
            self.flush_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        async fn query_flow(
            &self,
            call_id: &str,
            _start: DateTime<Local>,
            _end: DateTime<Local>,
        ) -> anyhow::Result<Vec<SipFlowItem>> {
            self.queried_call_ids
                .lock()
                .unwrap()
                .push(call_id.to_string());
            Ok(vec![])
        }
        async fn query_media_stats(
            &self,
            _call_id: &str,
            _start: DateTime<Local>,
            _end: DateTime<Local>,
        ) -> anyhow::Result<Vec<SipFlowMediaStats>> {
            Ok(vec![])
        }
        async fn query_media(
            &self,
            _call_id: &str,
            _start: DateTime<Local>,
            _end: DateTime<Local>,
        ) -> anyhow::Result<Vec<u8>> {
            Ok(self.media.clone())
        }
    }

    fn make_record() -> CallRecord {
        use crate::callrecord::CallDetails;
        let now = chrono::Utc::now();
        let mut record = CallRecord::default();
        record.call_id = "test-call-id".to_string();
        record.start_time = now - chrono::Duration::seconds(30);
        record.answer_time = Some(now - chrono::Duration::seconds(20));
        record.end_time = now;
        record.caller = "alice".to_string();
        record.callee = "bob".to_string();
        record.details = CallDetails {
            direction: "inbound".to_string(),
            status: "completed".to_string(),
            ..Default::default()
        };
        record
    }

    #[tokio::test]
    async fn test_hook_runs_inline() {
        let flush_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook = SipFlowUploadHook::new(
            Arc::new(MockBackend {
                media: vec![],
                flush_count: flush_count.clone(),
                queried_call_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            Arc::new(std::sync::OnceLock::new()),
            SipFlowUploadConfig::Http {
                url: "http://localhost:9999/upload".to_string(),
                headers: None,
                signaling: None,
                media: None,
                force_pcm: None,
                pcm_sample_rate: None,
            },
            None,
        )
        .unwrap();
        let mut record = make_record();
        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .unwrap();
        assert!(record.details.recording_url.is_none());
        assert_eq!(flush_count.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_hook_checks_backend_for_unanswered_early_media() {
        let flush_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook = SipFlowUploadHook::new(
            Arc::new(MockBackend {
                media: vec![],
                flush_count: flush_count.clone(),
                queried_call_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            Arc::new(std::sync::OnceLock::new()),
            SipFlowUploadConfig::Http {
                url: "http://localhost:9999/upload".to_string(),
                headers: None,
                signaling: Some(false),
                media: Some(false),
                force_pcm: None,
                pcm_sample_rate: None,
            },
            None,
        )
        .unwrap();
        let mut record = make_record();
        record.answer_time = None;

        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .unwrap();

        assert_eq!(flush_count.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn signaling_query_uses_roles_and_adds_root_only_when_missing() {
        let queried_call_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
        let backend = MockBackend {
            media: vec![],
            flush_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            queried_call_ids: queried_call_ids.clone(),
        };
        let upload_config = SipFlowUploadConfig::Http {
            url: "http://localhost:9999/upload".to_string(),
            headers: None,
            signaling: Some(true),
            media: Some(false),
            force_pcm: None,
            pcm_sample_rate: None,
        };
        let client = crate::http_util::build_keepalive_client(None, None).unwrap();
        let now = Local::now();

        upload_signaling_flow(
            &upload_config,
            &backend,
            "caller-call-id",
            &["caller-call-id".to_string(), "callee-call-id".to_string()],
            now - chrono::Duration::seconds(1),
            now + chrono::Duration::seconds(1),
            "flow.jsonl",
            "flow.jsonl",
            &client,
            None,
        )
        .await;
        assert_eq!(
            *queried_call_ids.lock().unwrap(),
            ["caller-call-id", "callee-call-id"]
        );

        queried_call_ids.lock().unwrap().clear();
        upload_signaling_flow(
            &upload_config,
            &backend,
            "caller-call-id",
            &["callee-call-id".to_string()],
            now - chrono::Duration::seconds(1),
            now + chrono::Duration::seconds(1),
            "flow.jsonl",
            "flow.jsonl",
            &client,
            None,
        )
        .await;
        assert_eq!(
            *queried_call_ids.lock().unwrap(),
            ["callee-call-id", "caller-call-id"]
        );
    }

    #[test]
    fn test_upload_config_parse_s3() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"

[upload]
type = "s3"
vendor = "aws"
bucket = "my-recordings"
region = "us-east-1"
access_key = "AKID"
secret_key = "SECRET"
endpoint = "https://s3.amazonaws.com"
root = "recordings"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse s3 upload config");
        match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                let upload = upload.expect("upload should be set");
                match upload {
                    SipFlowUploadConfig::S3 { bucket, region, .. } => {
                        assert_eq!(bucket, "my-recordings");
                        assert_eq!(region, "us-east-1");
                    }
                    _ => panic!("expected S3 variant"),
                }
            }
            _ => panic!("expected Local sipflow config"),
        }
    }

    #[test]
    fn test_upload_config_parse_http() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"

[upload]
type = "http"
url = "https://example.com/recordings"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse http upload config");
        match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                let upload = upload.expect("upload should be set");
                match upload {
                    SipFlowUploadConfig::Http { url, .. } => {
                        assert_eq!(url, "https://example.com/recordings");
                    }
                    _ => panic!("expected Http variant"),
                }
            }
            _ => panic!("expected Local sipflow config"),
        }
    }

    #[test]
    fn test_upload_config_parse_remote_http() {
        let toml_str = r#"
type = "remote"
udp_addr = "127.0.0.1:3000"
http_addr = "http://127.0.0.1:3001"

[upload]
type = "http"
url = "https://example.com/recordings"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse remote sipflow with upload");
        match cfg {
            crate::config::SipFlowConfig::Remote { upload, .. } => {
                let upload = upload.expect("upload should be set");
                match upload {
                    SipFlowUploadConfig::Http { url, .. } => {
                        assert_eq!(url, "https://example.com/recordings");
                    }
                    _ => panic!("expected Http variant"),
                }
            }
            _ => panic!("expected Remote sipflow config"),
        }
    }

    #[test]
    fn test_sipflow_config_default_no_upload() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse sipflow without upload");
        match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                assert!(upload.is_none(), "upload should default to None");
            }
            _ => panic!("expected Local sipflow config"),
        }
    }

    #[test]
    fn test_sipflow_remote_config_default_no_upload() {
        let toml_str = r#"
type = "remote"
udp_addr = "127.0.0.1:3000"
http_addr = "http://127.0.0.1:3001"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse remote sipflow without upload");
        match cfg {
            crate::config::SipFlowConfig::Remote { upload, .. } => {
                assert!(upload.is_none(), "upload should default to None");
            }
            _ => panic!("expected Remote sipflow config"),
        }
    }

    #[test]
    fn test_upload_config_signaling_default_none() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"

[upload]
type = "http"
url = "https://example.com/recordings"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse http upload config");
        match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                let upload = upload.expect("upload should be set");
                match upload {
                    SipFlowUploadConfig::Http { signaling, .. } => {
                        assert_eq!(signaling, None, "signaling should default to None");
                    }
                    _ => panic!("expected Http variant"),
                }
            }
            _ => panic!("expected Local sipflow config"),
        }
    }

    #[test]
    fn test_upload_config_signaling_enabled_s3() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"

[upload]
type = "s3"
vendor = "aws"
bucket = "my-bucket"
region = "us-east-1"
access_key = "AKID"
secret_key = "SECRET"
endpoint = "https://s3.amazonaws.com"
root = "recordings"
signaling = true
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse s3 upload config with signaling");
        match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                let upload = upload.expect("upload should be set");
                match upload {
                    SipFlowUploadConfig::S3 { signaling, .. } => {
                        assert_eq!(signaling, Some(true));
                    }
                    _ => panic!("expected S3 variant"),
                }
            }
            _ => panic!("expected Local sipflow config"),
        }
    }

    #[test]
    fn test_upload_config_signaling_enabled_http() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"

[upload]
type = "http"
url = "https://example.com/recordings"
signaling = true
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse http upload config with signaling");
        match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                let upload = upload.expect("upload should be set");
                match upload {
                    SipFlowUploadConfig::Http { signaling, .. } => {
                        assert_eq!(signaling, Some(true));
                    }
                    _ => panic!("expected Http variant"),
                }
            }
            _ => panic!("expected Local sipflow config"),
        }
    }

    #[test]
    fn test_upload_config_signaling_remote_s3() {
        let toml_str = r#"
type = "remote"
udp_addr = "127.0.0.1:3000"
http_addr = "http://127.0.0.1:3001"

[upload]
type = "s3"
vendor = "minio"
bucket = "my-bucket"
region = "us-east-1"
access_key = "AKID"
secret_key = "SECRET"
endpoint = "http://minio:9000"
root = "sipflow"
signaling = true
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse remote s3 upload with signaling");
        match cfg {
            crate::config::SipFlowConfig::Remote { upload, .. } => {
                let upload = upload.expect("upload should be set");
                match upload {
                    SipFlowUploadConfig::S3 {
                        signaling, bucket, ..
                    } => {
                        assert_eq!(signaling, Some(true));
                        assert_eq!(bucket, "my-bucket");
                    }
                    _ => panic!("expected S3 variant"),
                }
            }
            _ => panic!("expected Remote sipflow config"),
        }
    }
}
