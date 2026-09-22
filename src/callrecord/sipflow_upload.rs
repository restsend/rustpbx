use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Local, TimeZone};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::{
    callrecord::{
        CallRecord, CallRecordHook, format_sipflow_media_key, format_sipflow_signaling_file_name,
        format_sipflow_signaling_key, sipflow::SipFlowSlot,
    },
    config::SipFlowUploadConfig,
    sipflow::SipFlowBackend,
    storage::{Storage, StorageConfig},
};

/// Shared handle to the `[sipflow]` config section. The SIP server swaps the
/// inner value on hot-reload (`SipServerInner::reload_sipflow`), so upload
/// hooks and the console observe the current `[sipflow.upload]` without a
/// restart.
pub type SipFlowConfigSlot = Arc<ArcSwap<Option<crate::config::SipFlowConfig>>>;

/// Late-bound `[sipflow.upload]` config plus its derived upload storage.
///
/// The storage is rebuilt lazily whenever the slot content changes (config
/// hot-reload); S3 storages construct an HTTP client pool so they are cached
/// and only recreated when the config signature actually differs.
pub struct SipFlowUploadRuntime {
    slot: SipFlowConfigSlot,
    cached: parking_lot::Mutex<(String, Option<Storage>)>,
    /// Dedicated storage for signaling uploads, cached like the media
    /// storage. `None` ⇒ the signaling target equals the media target and
    /// callers reuse the media storage.
    signaling_cached: parking_lot::Mutex<(String, Option<Storage>)>,
}

impl SipFlowUploadRuntime {
    pub fn new(slot: SipFlowConfigSlot) -> Self {
        Self {
            slot,
            cached: parking_lot::Mutex::new((String::new(), None)),
            signaling_cached: parking_lot::Mutex::new((String::new(), None)),
        }
    }

    /// Build a standalone runtime seeded with a fixed config (tests, embedded
    /// callers without a server slot).
    pub fn for_config(config: Option<crate::config::SipFlowConfig>) -> Self {
        Self::new(Arc::new(ArcSwap::new(Arc::new(config))))
    }

    /// Current `[sipflow.upload]` config, if any. `None` when `[sipflow]` is
    /// absent or carries no `upload` section (hooks become no-ops).
    pub fn config(&self) -> Option<SipFlowUploadConfig> {
        self.slot
            .load()
            .as_ref()
            .as_ref()
            .and_then(|cfg| match cfg {
                crate::config::SipFlowConfig::Local { upload, .. } => upload.clone(),
                crate::config::SipFlowConfig::Remote { upload, .. } => upload.clone(),
            })
    }

    /// Current upload config + storage, rebuilding the storage when the config
    /// changed since the last call. Returns `None` when upload is unconfigured.
    pub fn resolve(&self) -> Result<Option<(SipFlowUploadConfig, Option<Storage>)>> {
        let Some(config) = self.config() else {
            return Ok(None);
        };
        let signature = format!("{config:?}");
        let mut cached = self.cached.lock();
        if cached.0 != signature {
            cached.1 = build_storage(&config)?;
            cached.0 = signature;
            info!(
                upload_type = if matches!(config, SipFlowUploadConfig::Http { .. }) {
                    "http"
                } else {
                    "s3"
                },
                "sipflow upload storage (re)built"
            );
        }
        Ok(Some((config, cached.1.clone())))
    }

    /// Dedicated storage for signaling uploads when the configured signaling
    /// target (`signaling_bucket` / `signaling_url`) differs from the media
    /// target. `None` ⇒ callers reuse the media storage.
    pub fn signaling_storage(&self) -> Result<Option<Storage>> {
        let Some(config) = self.config() else {
            return Ok(None);
        };
        let signature = format!("{config:?}");
        let mut cached = self.signaling_cached.lock();
        if cached.0 != signature {
            cached.1 = build_signaling_storage(&config)?;
            cached.0 = signature;
        }
        Ok(cached.1.clone())
    }
}

pub struct SipFlowUploadHook {
    backend: Arc<dyn SipFlowBackend>,
    /// Late-bound handle to the SipFlow wrapper (writer batch + backend);
    /// flushed before uploading so the tail messages are persisted.
    sipflow: SipFlowSlot,
    /// Late-bound `[sipflow.upload]` config + storage; re-resolved on every
    /// batch so config hot-reloads take effect without a restart.
    runtime: Arc<SipFlowUploadRuntime>,
    db: Option<DatabaseConnection>,
}

impl SipFlowUploadHook {
    pub fn new(
        backend: Arc<dyn SipFlowBackend>,
        sipflow: SipFlowSlot,
        runtime: Arc<SipFlowUploadRuntime>,
        db: Option<DatabaseConnection>,
    ) -> Result<Self> {
        Ok(Self {
            backend,
            sipflow,
            runtime,
            db,
        })
    }
}

#[async_trait]
impl CallRecordHook for SipFlowUploadHook {
    async fn on_record_enrich(&self, records: &mut [CallRecord]) -> anyhow::Result<()> {
        let Some((upload_config, _)) = self.runtime.resolve()? else {
            return Ok(());
        };
        for record in records {
            let skip_media = record.recorder.iter().any(|m| m.track_id != "signaling");
            preconstruct_signaling_url(record, &upload_config, skip_media);
        }
        Ok(())
    }

    async fn on_record_completed(&self, records: &mut [CallRecord]) -> anyhow::Result<()> {
        let Some((upload_config, storage)) = self.runtime.resolve()? else {
            return Ok(());
        };
        let signaling_storage = self.runtime.signaling_storage()?;
        for record in records {
            let call_id = record.call_id.as_str();
            let signaling_call_ids = record.sip_leg_roles.keys().cloned().collect::<Vec<_>>();
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

            let outcome = crate::callrecord::sipflow_upload::do_upload(
                self.backend.as_ref(),
                &self.sipflow,
                &upload_config,
                self.db.as_ref(),
                storage.as_ref(),
                signaling_storage.as_ref(),
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
            .await;

            if let Some(url) = outcome.media_url {
                record.details.recording_url = Some(url);
                record.details.recording_duration_secs = Some(duration_secs.max(0));
                record
                    .extensions
                    .insert(crate::callrecord::RecordingFileSize(outcome.media_size));
            }
            if let Some(jsonl_url) = outcome.signaling_url {
                stash_sipflow_jsonl(record, &jsonl_url, self.db.as_ref()).await;
            }
        }

        Ok(())
    }
}

/// Record the signaling JSONL URL on the in-memory record (for downstream
/// hooks/events) and persist it into the CDR row's metadata when enrichment
/// has not already stored the same URL. HTTP uploaders only learn the final
/// URL from the upload response — which happens after the row was persisted —
/// so the metadata column is updated in place here.
pub async fn stash_sipflow_jsonl(
    record: &mut CallRecord,
    url: &str,
    db: Option<&DatabaseConnection>,
) {
    let already_stored = record
        .details
        .metadata
        .as_ref()
        .and_then(|m| m.get("sipflow_jsonl"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|existing| existing == url);
    record
        .details
        .metadata
        .get_or_insert_with(Default::default)
        .insert(
            "sipflow_jsonl".to_string(),
            serde_json::Value::String(url.to_string()),
        );
    if already_stored {
        return;
    }
    let Some(db) = db else {
        return;
    };
    if let Err(e) =
        crate::models::call_record::update_sipflow_jsonl(db, &record.call_id, url).await
    {
        warn!(
            call_id = %record.call_id,
            "SipFlowUploadHook: failed to persist sipflow_jsonl metadata: {e}"
        );
    }
}

/// Result of one call's sipflow upload pass.
#[derive(Debug, Default)]
pub struct UploadOutcome {
    /// Resolved URL of the uploaded media WAV (None when media was skipped or
    /// the upload failed).
    pub media_url: Option<String>,
    pub media_size: u64,
    /// Resolved URL of the uploaded signaling JSONL (None when signaling was
    /// skipped or the upload failed).
    pub signaling_url: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn do_upload(
    backend: &dyn SipFlowBackend,
    sipflow: &crate::callrecord::sipflow::SipFlowSlot,
    upload_config: &SipFlowUploadConfig,
    db: Option<&DatabaseConnection>,
    storage: Option<&Storage>,
    signaling_storage: Option<&Storage>,
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
) -> UploadOutcome {
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
            storage,
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

    let signaling_url = if signaling {
        upload_signaling_flow(
            upload_config,
            backend,
            call_id,
            signaling_call_ids,
            start,
            end,
            &full_signaling_key,
            signaling_file_name,
            storage,
            signaling_storage,
        )
        .await
    } else {
        None
    };

    UploadOutcome {
        media_url: first_uploaded_url,
        media_size: uploaded_file_size,
        signaling_url,
    }
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
    storage: Option<&Storage>,
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

    let wav_bytes = match tokio::fs::read(&temp_path).await {
        Ok(b) => b,
        Err(e) => {
            warn!(call_id, "SipFlowUploadHook: read temp file failed: {e}");
            return None;
        }
    };

    let url_result = match upload_config {
        SipFlowUploadConfig::S3 {
            vendor,
            bucket,
            endpoint,
            ..
        } => {
            let Some(storage) = storage else {
                return None;
            };
            upload_s3(storage, full_media_key, wav_bytes)
                .await
                .map(|_| sipflow_s3_url(vendor, endpoint, bucket, full_media_key))
        }
        SipFlowUploadConfig::Http {
            file_field,
            content_type,
            ..
        } => {
            let Some(storage) = storage else {
                return None;
            };
            let file_name = format!("{call_id}.wav");
            let request = crate::storage::UploadRequest {
                key: full_media_key.to_string(),
                file_name: Some(file_name.clone()),
                // Config wins; these are only historical fallbacks.
                content_type: content_type
                    .clone()
                    .or_else(|| Some("audio/wav".to_string())),
                file_field: file_field
                    .clone()
                    .or_else(|| Some("recording".to_string())),
                body_field: None,
                vars: std::collections::HashMap::from([
                    ("call_id".to_string(), call_id.to_string()),
                    ("filename".to_string(), file_name),
                ]),
                bytes: Bytes::from(wav_bytes),
            };
            storage
                .upload(request)
                .await
                .map(|uploaded| uploaded.url.unwrap_or_else(|| full_media_key.to_string()))
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
/// Uploads the signaling JSONL and returns the resolved URL on success, None
/// otherwise. The S3 URL is assembled from the config (matching what
/// `preconstruct_signaling_url` stores); the HTTP URL comes from the upload
/// response (`response_url_path`, the body when it looks like a URL, or the
/// request URL). `signaling_storage` carries the dedicated storage for a
/// separate signaling bucket / endpoint (`None` ⇒ reuse `storage`).
pub async fn upload_signaling_flow(
    upload_config: &SipFlowUploadConfig,
    backend: &dyn SipFlowBackend,
    call_id: &str,
    signaling_call_ids: &[String],
    start: DateTime<Local>,
    end: DateTime<Local>,
    full_signaling_key: &str,
    signaling_file_name: &str,
    storage: Option<&Storage>,
    signaling_storage: Option<&Storage>,
) -> Option<String> {
    let query_start = start - chrono::Duration::seconds(1);
    let query_end = end + chrono::Duration::seconds(1);
    let mut query_call_ids = signaling_call_ids.to_vec();
    if !query_call_ids.iter().any(|id| id == call_id) {
        query_call_ids.push(call_id.to_string());
    }

    let mut flow_items = Vec::new();
    for leg_call_id in &query_call_ids {
        match backend
            .query_flow(leg_call_id, query_start, query_end)
            .await
        {
            Ok(mut items) => flow_items.append(&mut items),
            Err(e) => {
                warn!(
                    call_id,
                    leg_call_id, "SipFlowUploadHook: query_flow failed: {e}"
                );
                return None;
            }
        }
    }
    flow_items.sort_by_key(|item| (item.timestamp, item.seq));

    if flow_items.is_empty() {
        error!(
            call_id,
            queried_call_ids = ?query_call_ids,
            "SipFlowUploadHook: signaling query returned no flow items; upload skipped"
        );
        return None;
    }

    let jsonl = crate::sipflow::SipFlowQuery::export_jsonl(&flow_items);
    let data = jsonl.into_bytes();

    let result: Result<String> = match upload_config {
        SipFlowUploadConfig::S3 { vendor, endpoint, .. } => {
            // The signaling upload writes to the dedicated signaling storage
            // when one is configured, and the URL must name the signaling
            // bucket (`signaling_bucket`, falling back to the media bucket).
            let Some(signaling_storage) = signaling_storage.or(storage) else {
                warn!(call_id, "SipFlowUploadHook: S3 storage is not initialized");
                return None;
            };
            let Some(bucket) = upload_config.signaling_bucket() else {
                warn!(call_id, "SipFlowUploadHook: signaling bucket is not configured");
                return None;
            };
            upload_s3(signaling_storage, full_signaling_key, data)
                .await
                .map(|_| sipflow_s3_url(vendor, endpoint, bucket, full_signaling_key))
        }
        SipFlowUploadConfig::Http {
            file_field,
            content_type,
            ..
        } => {
            // `signaling_storage` carries the dedicated `signaling_url`
            // endpoint when one is configured.
            let Some(signaling_storage) = signaling_storage.or(storage) else {
                warn!(call_id, "SipFlowUploadHook: HTTP storage is not initialized");
                return None;
            };
            let request = crate::storage::UploadRequest {
                key: full_signaling_key.to_string(),
                file_name: Some(signaling_file_name.to_string()),
                // Config wins; these are only historical fallbacks.
                content_type: content_type
                    .clone()
                    .or_else(|| Some("application/jsonl".to_string())),
                file_field: file_field
                    .clone()
                    .or_else(|| Some("signaling".to_string())),
                body_field: None,
                vars: std::collections::HashMap::from([
                    ("call_id".to_string(), call_id.to_string()),
                    ("filename".to_string(), signaling_file_name.to_string()),
                ]),
                bytes: Bytes::from(data),
            };
            signaling_storage
                .upload(request)
                .await
                .map(|uploaded| uploaded.url.unwrap_or_else(|| full_signaling_key.to_string()))
        }
    };

    match result {
        Ok(url) => {
            info!(call_id, url = %url, "SipFlowUploadHook: signaling uploaded");
            Some(url)
        }
        Err(e) => {
            warn!(call_id, "SipFlowUploadHook: signaling upload failed: {e}");
            None
        }
    }
}

// ── Shared helpers (used by bin and hook) ─────────────────────────────────────

pub fn build_storage(upload_config: &SipFlowUploadConfig) -> Result<Option<Storage>> {
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
        SipFlowUploadConfig::Http { .. } => Ok(upload_config
            .http_upload_config()
            .map(Storage::from_http)
            .transpose()?),
    }
}

/// Build a dedicated storage for signaling uploads when the configured
/// signaling target (`signaling_bucket` for S3, `signaling_url` for HTTP)
/// differs from the media target. Returns `None` when the signaling target
/// equals the media target — callers then reuse the media storage.
pub fn build_signaling_storage(upload_config: &SipFlowUploadConfig) -> Result<Option<Storage>> {
    match upload_config {
        SipFlowUploadConfig::S3 {
            vendor,
            bucket,
            region,
            access_key,
            secret_key,
            endpoint,
            signaling_bucket,
            ..
        } => {
            let dedicated = signaling_bucket
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .filter(|b| *b != bucket.trim());
            match dedicated {
                Some(signaling_bucket) => Ok(Some(Storage::new(&StorageConfig::S3 {
                    vendor: vendor.clone(),
                    bucket: signaling_bucket.to_string(),
                    region: region.clone(),
                    access_key: access_key.clone(),
                    secret_key: secret_key.clone(),
                    endpoint: Some(endpoint.clone()),
                    prefix: None,
                })?)),
                None => Ok(None),
            }
        }
        SipFlowUploadConfig::Http {
            url, signaling_url, ..
        } => {
            let dedicated = signaling_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .filter(|u| *u != url.trim());
            match dedicated {
                Some(signaling_url) => {
                    let mut config = upload_config.clone();
                    if let SipFlowUploadConfig::Http {
                        url,
                        signaling_url: sig,
                        ..
                    } = &mut config
                    {
                        *url = signaling_url.to_string();
                        *sig = None;
                    }
                    Ok(config
                        .http_upload_config()
                        .map(Storage::from_http)
                        .transpose()?)
                }
                None => Ok(None),
            }
        }
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
    /// Resolved URL of the uploaded signaling JSONL. Optional for wire
    /// compatibility with older bins that only reported the boolean.
    #[serde(default)]
    pub signaling_url: Option<String>,
}

// ── Internal upload helpers ───────────────────────────────────────────────────

pub(crate) fn sipflow_s3_url(
    vendor: &crate::storage::S3Vendor,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> String {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let bucket = bucket.trim().trim_matches('/');
    let key = key.trim_start_matches('/');
    if *vendor == crate::storage::S3Vendor::Aliyun {
        return format!("{endpoint}/{key}");
    }
    format!("{endpoint}/{bucket}/{key}")
}

pub(crate) fn preconstruct_signaling_url(
    record: &mut CallRecord,
    upload_config: &SipFlowUploadConfig,
    signaling_default: bool,
) {
    let signaling_enabled = match upload_config {
        SipFlowUploadConfig::S3 { signaling, .. } | SipFlowUploadConfig::Http { signaling, .. } => {
            signaling.unwrap_or(signaling_default)
        }
    };
    if !signaling_enabled {
        return;
    }
    let (vendor, endpoint, root) = match upload_config {
        SipFlowUploadConfig::S3 {
            vendor,
            endpoint,
            root,
            ..
        } => (vendor, endpoint.as_str(), root.as_str()),
        // HTTP upload URLs are only known from the upload response.
        SipFlowUploadConfig::Http { .. } => return,
    };
    let Some(bucket) = upload_config.signaling_bucket() else {
        return;
    };

    let key = join_root(root, &format_sipflow_signaling_key(record));
    let url = sipflow_s3_url(vendor, endpoint, bucket, &key);
    record
        .details
        .metadata
        .get_or_insert_with(Default::default)
        .insert("sipflow_jsonl".to_string(), serde_json::Value::String(url));
}

async fn upload_s3(storage: &Storage, key: &str, data: Vec<u8>) -> Result<()> {
    storage.write(key, Bytes::from(data)).await?;
    Ok(())
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
        queried_ranges: std::sync::Arc<std::sync::Mutex<Vec<(i64, i64)>>>,
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
            start: DateTime<Local>,
            end: DateTime<Local>,
        ) -> anyhow::Result<Vec<SipFlowItem>> {
            self.queried_call_ids
                .lock()
                .unwrap()
                .push(call_id.to_string());
            self.queried_ranges
                .lock()
                .unwrap()
                .push((start.timestamp_micros(), end.timestamp_micros()));
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

    fn http_upload_config(
        url: &str,
        signaling: Option<bool>,
        media: Option<bool>,
    ) -> SipFlowUploadConfig {
        SipFlowUploadConfig::Http {
            url: url.to_string(),
            signaling_url: None,
            headers: None,
            method: None,
            file_field: None,
            body_field: None,
            file_name: None,
            content_type: None,
            fields: None,
            response_url_path: None,
            response_success: None,
            connect_timeout_ms: None,
            request_timeout_ms: None,
            signaling,
            media,
            force_pcm: None,
            pcm_sample_rate: None,
        }
    }

    /// Backend returning a single flow item so signaling upload proceeds.
    struct SingleFlowBackend;

    #[async_trait::async_trait]
    impl SipFlowBackend for SingleFlowBackend {
        fn record(&self, _: std::borrow::Cow<'_, str>, _: SipFlowItem) -> anyhow::Result<()> {
            Ok(())
        }
        async fn flush(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn query_flow(
            &self,
            _: &str,
            _: DateTime<Local>,
            _: DateTime<Local>,
        ) -> anyhow::Result<Vec<SipFlowItem>> {
            Ok(vec![SipFlowItem {
                timestamp: 1,
                seq: 0,
                leg: None,
                msg_type: crate::sipflow::SipFlowMsgType::Sip,
                src_addr: "127.0.0.1:5060".to_string(),
                dst_addr: String::new(),
                payload: Bytes::from_static(b"INVITE sip:x SIP/2.0"),
            }])
        }
        async fn query_media_stats(
            &self,
            _: &str,
            _: DateTime<Local>,
            _: DateTime<Local>,
        ) -> anyhow::Result<Vec<SipFlowMediaStats>> {
            Ok(vec![])
        }
        async fn query_media(
            &self,
            _: &str,
            _: DateTime<Local>,
            _: DateTime<Local>,
        ) -> anyhow::Result<Vec<u8>> {
            Ok(vec![])
        }
    }

    async fn spawn_capture_server() -> (String, Arc<std::sync::Mutex<Vec<u8>>>) {
        let captured = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let captured_clone = captured.clone();
        let app = axum::Router::new().route(
            "/upload",
            axum::routing::post(move |body: axum::body::Bytes| {
                let captured = captured_clone.clone();
                async move {
                    captured.lock().unwrap().extend_from_slice(&body);
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind capture server");
        let address = listener.local_addr().expect("capture server address");
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{address}/upload"), captured)
    }

    /// JSON capture server: stores the raw request body and replies with a
    /// `{"data":{"url": ...}}` envelope so `response_url_path` resolves.
    async fn spawn_json_capture_server(
        stored_url: &'static str,
    ) -> (String, Arc<std::sync::Mutex<Vec<u8>>>) {
        let captured = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let captured_clone = captured.clone();
        let app = axum::Router::new().route(
            "/upload",
            axum::routing::post(move |body: axum::body::Bytes| {
                let captured = captured_clone.clone();
                async move {
                    captured.lock().unwrap().extend_from_slice(&body);
                    axum::Json(serde_json::json!({
                        "code": 0,
                        "data": {"url": stored_url}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind json capture server");
        let address = listener.local_addr().expect("json capture server address");
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{address}/upload"), captured)
    }

    /// Full `[sipflow]` local config carrying the given upload section — the
    /// slot content shape consumed by `SipFlowUploadRuntime`.
    fn local_sipflow_config(
        upload: Option<SipFlowUploadConfig>,
    ) -> crate::config::SipFlowConfig {
        crate::config::SipFlowConfig::Local {
            root: "./config/sipflow".to_string(),
            subdirs: Default::default(),
            flush_count: 1000,
            flush_interval_secs: 5,
            id_cache_size: 8192,
            compress: true,
            compress_level: 6,
            shards: 4,
            upload,
            blocking_backpressure: false,
        }
    }

    #[tokio::test]
    async fn http_signaling_upload_honors_configured_file_field() {
        let now = Local::now();
        let call_ids = vec!["flow-call".to_string()];
        let window = |now: DateTime<Local>| (now - chrono::Duration::seconds(1), now + chrono::Duration::seconds(1));

        // A configured file_field must win over the built-in fallback.
        let (url, captured) = spawn_capture_server().await;
        let upload: SipFlowUploadConfig = toml::from_str(&format!(
            r#"type = "http"
url = "{url}"
file_field = "filecontent"
"#
        ))
        .expect("parse http upload config");
        let storage = build_storage(&upload).unwrap().expect("http storage");
        let (start, end) = window(now);
        assert!(
            upload_signaling_flow(
                &upload,
                &SingleFlowBackend,
                "flow-call",
                &call_ids,
                start,
                end,
                "flow.jsonl",
                "flow.jsonl",
                Some(&storage),
                None,
            )
            .await
            .is_some()
        );
        let body = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(body.contains("name=\"filecontent\""), "configured field: {body}");
        assert!(
            !body.contains("name=\"signaling\""),
            "fallback must not shadow config: {body}"
        );

        // Without configuration the historical part name is preserved.
        let (url, captured) = spawn_capture_server().await;
        let upload: SipFlowUploadConfig = toml::from_str(&format!(
            r#"type = "http"
url = "{url}"
"#
        ))
        .expect("parse http upload config");
        let storage = build_storage(&upload).unwrap().expect("http storage");
        let now = Local::now();
        let (start, end) = window(now);
        assert!(
            upload_signaling_flow(
                &upload,
                &SingleFlowBackend,
                "flow-call",
                &call_ids,
                start,
                end,
                "flow.jsonl",
                "flow.jsonl",
                Some(&storage),
                None,
            )
            .await
            .is_some()
        );
        let body = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(body.contains("name=\"signaling\""), "historical fallback: {body}");
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

    #[test]
    fn aliyun_empty_bucket_and_region_generate_signaling_url() {
        let config: SipFlowUploadConfig = toml::from_str(
            r#"
            type = "s3"
            vendor = "aliyun"
            bucket = ""
            region = ""
            endpoint = "https://test-bucket.oss-cn-beijing.aliyuncs.com"
            access_key = "test"
            secret_key = "test"
            root = "sipflow"
            signaling = true
            media = false
        "#,
        )
        .unwrap();
        let mut record = make_record();
        preconstruct_signaling_url(&mut record, &config, false);
        let raw = record.details.metadata.as_ref().unwrap()["sipflow_jsonl"]
            .as_str()
            .unwrap();
        let key = format!(
            "sipflow/{}/test-call-id.jsonl",
            record.start_time.format("%Y%m%d")
        );
        assert_eq!(
            raw,
            format!("https://test-bucket.oss-cn-beijing.aliyuncs.com/{key}")
        );
        assert!(build_storage(&config).unwrap().is_some());
    }

    #[test]
    fn preconstructs_signaling_s3_url_in_metadata() {
        let mut record = make_record();
        let upload_config = SipFlowUploadConfig::S3 {
            vendor: crate::config::S3Vendor::Minio,
            bucket: "recordings".to_string(),
            signaling_bucket: None,
            region: "us-east-1".to_string(),
            access_key: Some("access".to_string()),
            secret_key: Some("secret".to_string()),
            endpoint: "http://127.0.0.1:9000".to_string(),
            root: "sipflow-root".to_string(),
            signaling: Some(true),
            media: Some(true),
            force_pcm: None,
            pcm_sample_rate: None,
            signed_url_expiry_secs: None,
        };

        preconstruct_signaling_url(&mut record, &upload_config, false);

        let expected = format!(
            "http://127.0.0.1:9000/recordings/sipflow-root/{}/test-call-id.jsonl",
            record.start_time.format("%Y%m%d")
        );
        assert_eq!(
            record
                .details
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("sipflow_jsonl"))
                .and_then(serde_json::Value::as_str),
            Some(expected.as_str())
        );
    }

    #[tokio::test]
    async fn test_hook_runs_inline() {
        let flush_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runtime = Arc::new(SipFlowUploadRuntime::for_config(Some(local_sipflow_config(
            Some(http_upload_config("http://localhost:9999/upload", None, None)),
        ))));
        let hook = SipFlowUploadHook::new(
            Arc::new(MockBackend {
                media: vec![],
                flush_count: flush_count.clone(),
                queried_call_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
                queried_ranges: Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            Arc::new(std::sync::OnceLock::new()),
            runtime,
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
        let runtime = Arc::new(SipFlowUploadRuntime::for_config(Some(local_sipflow_config(
            Some(http_upload_config("http://localhost:9999/upload", Some(false), Some(false))),
        ))));
        let hook = SipFlowUploadHook::new(
            Arc::new(MockBackend {
                media: vec![],
                flush_count: flush_count.clone(),
                queried_call_ids: Arc::new(std::sync::Mutex::new(Vec::new())),
                queried_ranges: Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            Arc::new(std::sync::OnceLock::new()),
            runtime,
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

    /// Regression for the console "sipflow_jsonl still points at the old S3"
    /// report: with an HTTP uploader, the URL resolved from the upload
    /// response must be stashed into `details.metadata["sipflow_jsonl"]` (and
    /// replacing any stale preconstructed value).
    #[tokio::test]
    async fn http_signaling_upload_stashes_response_url_in_metadata() {
        const STORED_URL: &str = "https://archive.example.com/flows/call-42.jsonl";
        let (url, captured) = spawn_json_capture_server(STORED_URL).await;
        let upload: SipFlowUploadConfig = toml::from_str(&format!(
            r#"type = "http"
url = "{url}"
signaling = true
media = false
response_url_path = "data.url"
response_success = {{ path = "code", equals = 0 }}
"#
        ))
        .expect("parse http upload config");
        let runtime = Arc::new(SipFlowUploadRuntime::for_config(Some(
            local_sipflow_config(Some(upload)),
        )));
        let hook = SipFlowUploadHook::new(
            Arc::new(SingleFlowBackend),
            Arc::new(std::sync::OnceLock::new()),
            runtime,
            None,
        )
        .unwrap();

        let mut record = make_record();
        record.call_id = "call-42".to_string();
        // Simulate a stale preconstructed S3 URL from an earlier config.
        record.details.metadata = Some(std::collections::HashMap::from([(
            "sipflow_jsonl".to_string(),
            serde_json::Value::String("http://old-minio:9000/bucket/sipflow/call-42.jsonl".into()),
        )]));

        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .unwrap();

        assert_eq!(
            record
                .details
                .metadata
                .as_ref()
                .and_then(|m| m.get("sipflow_jsonl"))
                .and_then(serde_json::Value::as_str),
            Some(STORED_URL)
        );
        let body = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(body.contains("name=\"signaling\""), "wire shape: {body}");
    }

    /// The upload config is late-bound: swapping the slot between calls (the
    /// `reload_sipflow` path) must switch the destination without rebuilding
    /// the hook.
    #[tokio::test]
    async fn hook_follows_hot_reloaded_upload_config() {
        use arc_swap::ArcSwap;

        // Start with an S3 upload config; the hook must preconstruct S3 URLs.
        let s3_config: SipFlowUploadConfig = toml::from_str(
            r#"
            type = "s3"
            vendor = "minio"
            bucket = "old-bucket"
            region = "us-east-1"
            endpoint = "http://127.0.0.1:9000"
            root = "sipflow"
            signaling = true
            media = false
        "#,
        )
        .unwrap();
        let slot: SipFlowConfigSlot =
            Arc::new(ArcSwap::new(Arc::new(Some(local_sipflow_config(Some(
                s3_config,
            ))))));
        let runtime = Arc::new(SipFlowUploadRuntime::new(slot.clone()));
        let hook = SipFlowUploadHook::new(
            Arc::new(SingleFlowBackend),
            Arc::new(std::sync::OnceLock::new()),
            runtime,
            None,
        )
        .unwrap();

        let mut record = make_record();
        hook.on_record_enrich(std::slice::from_mut(&mut record))
            .await
            .unwrap();
        assert!(
            record
                .details
                .metadata
                .as_ref()
                .unwrap()["sipflow_jsonl"]
                .as_str()
                .unwrap()
                .starts_with("http://127.0.0.1:9000/old-bucket/"),
            "S3 URL expected before reload"
        );

        // Hot-reload: swap the slot to an HTTP uploader.
        let (url, captured) = spawn_capture_server().await;
        let http_config: SipFlowUploadConfig = toml::from_str(&format!(
            r#"type = "http"
url = "{url}"
signaling = true
media = false
"#
        ))
        .unwrap();
        slot.store(Arc::new(Some(local_sipflow_config(Some(http_config)))));

        let mut record = make_record();
        hook.on_record_enrich(std::slice::from_mut(&mut record))
            .await
            .unwrap();
        assert!(
            record
                .details
                .metadata
                .as_ref()
                .map(|m| m.get("sipflow_jsonl"))
                .unwrap_or(None)
                .is_none(),
            "HTTP mode must not preconstruct an S3 URL"
        );

        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(
            body.contains("name=\"signaling\""),
            "jsonl must be POSTed to the HTTP endpoint after reload: {body}"
        );
    }

    #[tokio::test]
    async fn signaling_query_uses_roles_and_adds_root_only_when_missing() {
        let queried_call_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
        let queried_ranges = Arc::new(std::sync::Mutex::new(Vec::new()));
        let backend = MockBackend {
            media: vec![],
            flush_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            queried_call_ids: queried_call_ids.clone(),
            queried_ranges: queried_ranges.clone(),
        };
        let upload_config =
            http_upload_config("http://localhost:9999/upload", Some(true), Some(false));
        let now = Local::now();
        let start = now - chrono::Duration::seconds(1);
        let end = now + chrono::Duration::seconds(1);

        upload_signaling_flow(
            &upload_config,
            &backend,
            "caller-call-id",
            &["caller-call-id".to_string(), "callee-call-id".to_string()],
            start,
            end,
            "flow.jsonl",
            "flow.jsonl",
            None,
            None,
        )
        .await;
        assert_eq!(
            *queried_call_ids.lock().unwrap(),
            ["caller-call-id", "callee-call-id"]
        );
        assert_eq!(
            *queried_ranges.lock().unwrap(),
            vec![
                (
                    (start - chrono::Duration::seconds(1)).timestamp_micros(),
                    (end + chrono::Duration::seconds(1)).timestamp_micros(),
                );
                2
            ]
        );

        queried_call_ids.lock().unwrap().clear();
        queried_ranges.lock().unwrap().clear();
        upload_signaling_flow(
            &upload_config,
            &backend,
            "caller-call-id",
            &["callee-call-id".to_string()],
            now - chrono::Duration::seconds(1),
            now + chrono::Duration::seconds(1),
            "flow.jsonl",
            "flow.jsonl",
            None,
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
    fn http_upload_config_builds_generic_scheme() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"

[upload]
type = "http"
url = "https://example.com/recordings/{key}"
file_field = "filecontent"
response_url_path = "data.url"
response_success = { path = "code", equals = 0 }
connect_timeout_ms = 1500
request_timeout_ms = 7000
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse http upload config");
        let upload = match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                upload.expect("upload should be set")
            }
            _ => panic!("expected Local sipflow config"),
        };
        let http = upload.http_upload_config().expect("http config");
        assert_eq!(http.url, "https://example.com/recordings/{key}");
        assert_eq!(http.file_field.as_deref(), Some("filecontent"));
        assert_eq!(http.response_url_path.as_deref(), Some("data.url"));
        assert_eq!(http.connect_timeout_ms, Some(1500));
        assert_eq!(http.request_timeout_ms, Some(7000));
        // The generic config builds a valid upload-only storage backend.
        assert!(crate::storage::Storage::from_http(http).is_ok());
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
