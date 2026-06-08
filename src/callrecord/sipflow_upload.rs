use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Local, TimeZone};
use sea_orm::DatabaseConnection;
use tracing::{info, warn};

use crate::{
    callrecord::{CallRecord, CallRecordFormatter, CallRecordHook},
    config::SipFlowUploadConfig,
    sipflow::SipFlowBackend,
};

pub struct SipFlowUploadHook {
    pub backend: Arc<dyn SipFlowBackend>,
    pub upload_config: SipFlowUploadConfig,
    pub db: Option<DatabaseConnection>,
    pub formatter: Arc<dyn CallRecordFormatter>,
}

#[async_trait]
impl CallRecordHook for SipFlowUploadHook {
    async fn on_record_completed(&self, record: &mut CallRecord) -> anyhow::Result<()> {
        if record.answer_time.is_none() {
            return Ok(());
        }

        let backend = self.backend.clone();
        let upload_config = self.upload_config.clone();
        let db = self.db.clone();
        let formatter = self.formatter.clone();
        let call_id = record.call_id.clone();
        let start = Local.from_utc_datetime(&record.start_time.naive_utc());
        let end = Local.from_utc_datetime(&record.end_time.naive_utc());
        let duration_secs = (record.end_time - record.start_time).num_seconds() as i32;

        let media_key = formatter.format_sipflow_media_key(record);
        let signaling_key = formatter.format_sipflow_signaling_key(record);
        let media_file_name = formatter.format_sipflow_media_file_name(record);
        let signaling_file_name = formatter.format_sipflow_signaling_file_name(record);

        crate::utils::spawn(async move {
            crate::callrecord::sipflow_upload::do_upload(
                backend,
                upload_config,
                formatter,
                db,
                &call_id,
                start,
                end,
                duration_secs,
                &media_key,
                &signaling_key,
                &media_file_name,
                &signaling_file_name,
            )
            .await;
        });

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn do_upload(
    backend: Arc<dyn SipFlowBackend>,
    upload_config: SipFlowUploadConfig,
    _formatter: Arc<dyn CallRecordFormatter>,
    db: Option<DatabaseConnection>,
    call_id: &str,
    start: DateTime<Local>,
    end: DateTime<Local>,
    duration_secs: i32,
    media_key: &str,
    signaling_key: &str,
    media_file_name: &str,
    signaling_file_name: &str,
) {
    if let Err(e) = backend.flush().await {
        warn!(call_id, "SipFlowUploadHook: flush failed: {e}");
    }

    let media_enabled = match &upload_config {
        SipFlowUploadConfig::S3 { media, .. } => media.unwrap_or(true),
        SipFlowUploadConfig::Http { media, .. } => media.unwrap_or(true),
    };

    if !media_enabled {
        info!(
            call_id,
            "SipFlowUploadHook: media upload disabled, skipping"
        );
    } else {
        upload_media(
            backend.as_ref(),
            &upload_config,
            call_id,
            start,
            end,
            media_key,
            db.as_ref(),
            duration_secs,
        )
        .await;
    }

    let signaling = match &upload_config {
        SipFlowUploadConfig::S3 { signaling, .. } => signaling.unwrap_or(false),
        SipFlowUploadConfig::Http { signaling, .. } => signaling.unwrap_or(false),
    };

    if signaling {
        upload_signaling_flow(
            &upload_config,
            backend.as_ref(),
            call_id,
            start,
            end,
            signaling_key,
            signaling_file_name,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_media(
    backend: &dyn SipFlowBackend,
    upload_config: &SipFlowUploadConfig,
    call_id: &str,
    start: DateTime<Local>,
    end: DateTime<Local>,
    media_key: &str,
    db: Option<&DatabaseConnection>,
    duration_secs: i32,
) {
    let temp_file: tempfile::NamedTempFile =
        match backend.generate_wav_file(call_id, start, end, None).await {
            Ok(f) => f,
            Err(e) => {
                warn!(call_id, "SipFlowUploadHook: generate_wav_file failed: {e}");
                return;
            }
        };

    let temp_path = temp_file.path().to_owned();
    let file_size = match std::fs::metadata(&temp_path) {
        Ok(m) => m.len() as usize,
        Err(e) => {
            warn!(call_id, "SipFlowUploadHook: temp file metadata failed: {e}");
            return;
        }
    };

    if file_size <= 44 {
        return;
    }

    let url_result = match upload_config {
        SipFlowUploadConfig::S3 {
            vendor,
            bucket,
            region,
            access_key,
            secret_key,
            endpoint,
            root,
            ..
        } => {
            let full_key = if root.is_empty() {
                media_key.to_string()
            } else {
                format!("{}/{}", root.trim_end_matches('/'), media_key)
            };
            let wav_bytes = match tokio::fs::read(&temp_path).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(call_id, "SipFlowUploadHook: read temp file failed: {e}");
                    return;
                }
            };
            upload_s3(
                vendor, bucket, region, access_key, secret_key, endpoint, &full_key, wav_bytes,
            )
            .await
            .map(|_| {
                format!(
                    "{}/{}/{}",
                    endpoint.trim_end_matches('/'),
                    bucket.trim_matches('/'),
                    full_key.trim_start_matches('/')
                )
            })
        }
        SipFlowUploadConfig::Http { url, headers, .. } => {
            upload_http_file(url, headers.as_ref(), call_id, &temp_path).await
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
        }
        Err(e) => {
            warn!(call_id, "SipFlowUploadHook: upload failed: {e}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_signaling_flow(
    upload_config: &SipFlowUploadConfig,
    backend: &dyn SipFlowBackend,
    call_id: &str,
    start: DateTime<Local>,
    end: DateTime<Local>,
    signaling_key: &str,
    signaling_file_name: &str,
) {
    let flow_items = match backend.query_flow(call_id, start, end).await {
        Ok(items) => items,
        Err(e) => {
            warn!(call_id, "SipFlowUploadHook: query_flow failed: {e}");
            return;
        }
    };

    if flow_items.is_empty() {
        return;
    }

    let jsonl = crate::sipflow::SipFlowQuery::export_jsonl(&flow_items);
    let data = jsonl.into_bytes();

    let result = match upload_config {
        SipFlowUploadConfig::S3 {
            vendor,
            bucket,
            region,
            access_key,
            secret_key,
            endpoint,
            root,
            ..
        } => {
            let full_key = if root.is_empty() {
                signaling_key.to_string()
            } else {
                format!("{}/{}", root.trim_end_matches('/'), signaling_key)
            };
            upload_s3(
                vendor, bucket, region, access_key, secret_key, endpoint, &full_key, data,
            )
            .await
        }
        SipFlowUploadConfig::Http { url, headers, .. } => {
            upload_http_jsonl(url, headers.as_ref(), signaling_file_name, data).await
        }
    };

    match result {
        Ok(_) => {
            info!(call_id, "SipFlowUploadHook: signaling uploaded");
        }
        Err(e) => {
            warn!(call_id, "SipFlowUploadHook: signaling upload failed: {e}");
        }
    }
}

// ── Internal upload helpers ───────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn upload_s3(
    vendor: &crate::config::S3Vendor,
    bucket: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    endpoint: &str,
    key: &str,
    data: Vec<u8>,
) -> Result<()> {
    use crate::storage::{Storage, StorageConfig};
    let storage = Storage::new(&StorageConfig::S3 {
        vendor: vendor.clone(),
        bucket: bucket.to_string(),
        region: region.to_string(),
        access_key: access_key.to_string(),
        secret_key: secret_key.to_string(),
        endpoint: Some(endpoint.to_string()),
        prefix: None,
    })?;
    storage.write(key, Bytes::from(data)).await?;
    Ok(())
}

async fn upload_http_file(
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
    call_id: &str,
    file_path: &std::path::Path,
) -> Result<String> {
    let client = reqwest::Client::new();
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
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
    file_name: &str,
    data: Vec<u8>,
) -> Result<()> {
    let client = reqwest::Client::new();
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
    use crate::callrecord::DefaultCallRecordFormatter;
    use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMediaStats};
    use chrono::{DateTime, Local};

    struct MockBackend {
        media: Vec<u8>,
        flush_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SipFlowBackend for MockBackend {
        fn record(&self, _call_id: &str, _item: SipFlowItem) -> anyhow::Result<()> {
            Ok(())
        }
        async fn flush(&self) -> anyhow::Result<()> {
            self.flush_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        async fn query_flow(
            &self,
            _call_id: &str,
            _start: DateTime<Local>,
            _end: DateTime<Local>,
        ) -> anyhow::Result<Vec<SipFlowItem>> {
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
    async fn test_hook_returns_immediately() {
        let flush_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook = SipFlowUploadHook {
            backend: Arc::new(MockBackend {
                media: vec![],
                flush_count: flush_count.clone(),
            }),
            upload_config: SipFlowUploadConfig::Http {
                url: "http://localhost:9999/upload".to_string(),
                headers: None,
                signaling: None,
                media: None,
            },
            db: None,
            formatter: Arc::new(DefaultCallRecordFormatter::default()),
        };
        let mut record = make_record();
        let start = std::time::Instant::now();
        hook.on_record_completed(&mut record).await.unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() < 100, "hook blocked for {elapsed:?}");
        assert!(record.details.recording_url.is_none());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(flush_count.load(std::sync::atomic::Ordering::Relaxed), 1);
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
