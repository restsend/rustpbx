use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Result, anyhow};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use bytes::Bytes;
use object_store::{Attribute, Attributes, PutOptions};
use serde_json::json;
use tracing::{info, warn};

use crate::{
    callrecord::{
        CallRecord, CallRecordHook, CallRecordMedia, UploadFailedMarker, is_direct_child_of_root,
    },
    config::{RecordingPolicy, RecordingType},
    models::call_record::extract_sip_username,
    rwi::{RwiGatewayRef, proto::RecordingMetadata},
    storage::{Storage, StorageConfig},
};

/// Shared handle to the `[recording]` policy. The SIP server swaps the inner
/// value on hot-reload (`reload_recording_settings` / `reload_proxy_config`),
/// so upload paths observe the current policy without a restart.
pub type RecordingPolicySlot = Arc<ArcSwap<Option<RecordingPolicy>>>;

/// One resolved S3 upload target: the live [`Storage`] plus the URL/key
/// parameters (bucket, endpoint, vendor, key prefix) it was built from.
/// The default target is derived from the main `[recording]` fields; a
/// per-source target from its `[recording.sources.<tag>]` entry.
#[derive(Clone)]
pub struct S3TargetSpec {
    pub storage: Storage,
    pub bucket: String,
    pub endpoint: Option<String>,
    pub vendor: Option<crate::storage::S3Vendor>,
    pub root: Option<String>,
}

impl S3TargetSpec {
    /// Remote URL of `key` in this target (virtual-host style for Aliyun,
    /// path style otherwise; `s3://{bucket}/{key}` without an endpoint).
    pub fn s3_url(&self, key: &str) -> String {
        RecordingUploadHook::s3_url_str(
            self.endpoint.as_deref(),
            self.vendor.as_ref() == Some(&crate::storage::S3Vendor::Aliyun),
            &self.bucket,
            key,
        )
    }
}

/// Upload targets derived from one `[recording]` policy revision: the
/// default S3/HTTP storage plus optional per-source S3 overrides. Media is
/// routed by its canonical source tag (see [`media_source_tag`]); tags
/// without an override (or whose override failed to build) fall back to the
/// default target.
#[derive(Default)]
pub struct RecordingTargets {
    recording_type: RecordingType,
    /// Default target: S3 spec in `s3` mode.
    default: Option<S3TargetSpec>,
    /// HTTP uploader in `http` mode (per-source overrides unsupported).
    http: Option<Storage>,
    /// Per-source S3 overrides keyed by normalized canonical source tag.
    by_tag: HashMap<String, S3TargetSpec>,
}

impl RecordingTargets {
    pub fn recording_type(&self) -> RecordingType {
        self.recording_type
    }

    /// Target for a media entry tagged `tag` (canonical source name); `None`
    /// tags and unmapped/failed tags resolve to the default target.
    pub fn spec_for_tag(&self, tag: Option<&str>) -> Option<&S3TargetSpec> {
        let normalized = tag
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(|tag| tag.to_ascii_lowercase());
        normalized
            .and_then(|tag| self.by_tag.get(&tag))
            .or(self.default.as_ref())
    }

    /// Primary upload storage: the default S3 target in `s3` mode or the
    /// HTTP uploader in `http` mode; `None` for local/sipflow.
    pub fn default_storage(&self) -> Option<Storage> {
        match (&self.default, &self.http) {
            (Some(spec), _) => Some(spec.storage.clone()),
            (None, Some(http)) => Some(http.clone()),
            _ => None,
        }
    }

    pub fn http_storage(&self) -> Option<&Storage> {
        self.http.as_ref()
    }

    /// All S3 targets (default first, then per-source overrides) — the
    /// presign candidate set for on-demand download URLs.
    pub fn s3_targets(&self) -> impl Iterator<Item = &S3TargetSpec> {
        self.default
            .iter()
            .chain(self.by_tag.values())
    }
}

/// Canonical source tag used to route a media entry to its upload target:
/// the segment's `segment_type` extra classified through
/// [`crate::callrecord::RecordingSource`]. The dialplan-level whole-call
/// artifact (`track_id = "mixed"`) has no segment bookkeeping and maps to
/// `full`; entries without a tag do too. Custom tags classify as `external`.
pub(crate) fn media_source_tag(media: &CallRecordMedia) -> String {
    let raw_tag = if media.track_id == "mixed" {
        None
    } else {
        media
            .extra
            .as_ref()
            .and_then(|extra| extra.get("segment_type"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
    };
    crate::callrecord::RecordingSource::classify(raw_tag.unwrap_or("full"))
        .as_str()
        .to_string()
}

/// Late-bound `[recording]` policy + derived upload storage, shared between
/// the upload hook, the background S3 uploader, the retry worker and the
/// console (on-demand presigned URLs). The storage is rebuilt lazily whenever
/// the slot content changes.
pub struct RecordingUploadRuntime {
    slot: RecordingPolicySlot,
    cached: parking_lot::Mutex<(String, Option<Arc<RecordingTargets>>)>,
}

impl RecordingUploadRuntime {
    pub fn new(slot: RecordingPolicySlot) -> Self {
        Self {
            slot,
            cached: parking_lot::Mutex::new((String::new(), None)),
        }
    }

    /// Standalone runtime seeded with a fixed policy (tests, embedded callers
    /// without a server slot).
    pub fn for_policy(policy: Option<RecordingPolicy>) -> Self {
        Self::new(Arc::new(ArcSwap::new(Arc::new(policy))))
    }

    /// Current `[recording]` policy, if any. `None` when the section is
    /// absent (upload paths become no-ops).
    pub fn policy(&self) -> Option<RecordingPolicy> {
        self.slot.load().as_ref().clone()
    }

    /// Current policy + full upload target set, rebuilding when the policy
    /// changed since the last call. Returns `None` when the recording section
    /// is absent or the mode needs no uploads (local/sipflow).
    pub fn resolve_targets(&self) -> Result<Option<(RecordingPolicy, Arc<RecordingTargets>)>> {
        let Some(policy) = self.policy() else {
            return Ok(None);
        };
        let signature = format!("{policy:?}");
        let mut cached = self.cached.lock();
        if cached.0 != signature {
            let targets = match build_recording_targets(&policy) {
                Ok(targets) => Arc::new(targets),
                Err(err) => {
                    tracing::error!(%err, "failed to rebuild recording upload storage");
                    return Err(err);
                }
            };
            if cached.0.is_empty() {
                info!("recording upload storage initialised");
            } else {
                info!("recording upload storage rebuilt after policy reload");
            }
            cached.1 = Some(targets);
            cached.0 = signature;
        }
        Ok(cached.1.clone().map(|targets| (policy, targets)))
    }

    /// Current policy + primary upload storage (default S3 target / HTTP
    /// uploader), for callers that do not care about per-source routing.
    pub fn resolve(&self) -> Result<Option<(RecordingPolicy, Option<Storage>)>> {
        Ok(self
            .resolve_targets()?
            .map(|(policy, targets)| (policy, targets.default_storage())))
    }

    /// Test-only: pre-seed the targets cache so `resolve()` returns the given
    /// storage as the default target for this policy signature without
    /// deriving it from the mode (unit tests drive the manager with a local
    /// object store).
    #[cfg(test)]
    pub(crate) fn seed_storage(&self, policy: &RecordingPolicy, storage: Storage) {
        self.seed_targets(
            policy,
            RecordingTargets {
                recording_type: RecordingType::S3,
                default: Some(S3TargetSpec {
                    storage,
                    bucket: policy.bucket.clone().unwrap_or_default(),
                    endpoint: policy.endpoint.clone(),
                    vendor: policy.vendor.clone(),
                    root: policy.root.clone(),
                }),
                http: None,
                by_tag: HashMap::new(),
            },
        );
    }

    /// Test-only: pre-seed the full targets cache (default + per-source).
    #[cfg(test)]
    pub(crate) fn seed_targets(&self, policy: &RecordingPolicy, targets: RecordingTargets) {
        let mut cached = self.cached.lock();
        cached.0 = format!("{policy:?}");
        cached.1 = Some(Arc::new(targets));
    }
}

/// Build one S3 target spec from its parameters. `Storage::new` rejects a
/// partial access/secret pair; omitting both selects anonymous/public access.
#[allow(clippy::too_many_arguments)]
fn build_s3_spec(
    vendor: Option<crate::storage::S3Vendor>,
    bucket: String,
    region: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    endpoint: Option<String>,
    root: Option<String>,
) -> Result<S3TargetSpec> {
    let endpoint = endpoint
        .as_deref()
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .map(str::to_string);
    let storage = Storage::new(&StorageConfig::S3 {
        vendor: vendor.clone().unwrap_or_default(),
        bucket: bucket.clone(),
        region: region.unwrap_or_default(),
        access_key,
        secret_key,
        endpoint: endpoint.clone(),
        prefix: None,
    })?;
    Ok(S3TargetSpec {
        storage,
        bucket,
        endpoint,
        vendor,
        root,
    })
}

fn normalize_source_tag(tag: &str) -> String {
    tag.trim().to_ascii_lowercase()
}

/// Build the upload target set described by a policy: the default S3/HTTP
/// storage from the main section fields plus per-source S3 overrides. A
/// default-target construction error fails the build (historical behavior:
/// `Storage::new` errors fail fast); a per-source entry that fails to build
/// is dropped with an error log and its source falls back to the default
/// target. Per-source overrides apply only in `s3` mode.
fn build_recording_targets(policy: &RecordingPolicy) -> Result<RecordingTargets> {
    let mut targets = RecordingTargets {
        recording_type: policy.effective_recording_type(),
        ..Default::default()
    };
    match targets.recording_type {
        RecordingType::S3 => {
            targets.default = Some(build_s3_spec(
                policy.vendor.clone(),
                policy.bucket.clone().unwrap_or_default(),
                policy.region.clone(),
                policy.access_key.clone(),
                policy.secret_key.clone(),
                policy.endpoint.clone(),
                policy.root.clone(),
            )?);
            let Some(sources) = policy.sources.as_ref().filter(|sources| !sources.is_empty())
            else {
                return Ok(targets);
            };
            for (tag, source_target) in sources {
                let normalized = normalize_source_tag(tag);
                match build_s3_spec(
                    source_target.vendor.clone(),
                    source_target.bucket.clone(),
                    source_target.region.clone(),
                    source_target.access_key.clone(),
                    source_target.secret_key.clone(),
                    source_target.endpoint.clone(),
                    source_target.root.clone(),
                ) {
                    Ok(spec) => {
                        targets.by_tag.insert(normalized, spec);
                    }
                    Err(err) => {
                        tracing::error!(
                            tag = %tag,
                            %err,
                            "failed to build [recording.sources] target; \
                             this source falls back to the default bucket"
                        );
                    }
                }
            }
        }
        RecordingType::Http => {
            if policy.sources.as_ref().is_some_and(|sources| !sources.is_empty()) {
                warn!(
                    "[recording].sources requires type = \"s3\"; ignoring per-source targets"
                );
            }
            targets.http = policy
                .http_upload_config()
                .map(Storage::from_http)
                .transpose()?;
        }
        RecordingType::Local | RecordingType::Sipflow => {
            if policy.sources.as_ref().is_some_and(|sources| !sources.is_empty()) {
                warn!(
                    ty = ?targets.recording_type,
                    "[recording].sources requires type = \"s3\"; ignoring per-source targets"
                );
            }
        }
    }
    Ok(targets)
}

/// One queued background S3 upload: the local file plus the canonical source
/// tag selecting its target bucket (`[recording].sources` override or the
/// default).
#[derive(Debug, Clone)]
pub(crate) struct PendingRecordingUpload {
    pub path: PathBuf,
    pub source_tag: String,
}

pub struct RecordingUploadHook {
    /// Late-bound `[recording]` policy + upload storage; re-resolved on every
    /// batch so config hot-reloads take effect without a restart.
    runtime: Arc<RecordingUploadRuntime>,
    rwi_gateway: Option<RwiGatewayRef>,
    s3_upload_sender: Option<tokio::sync::mpsc::Sender<PendingRecordingUpload>>,
}

pub struct RecordingUploadManager {
    runtime: Arc<RecordingUploadRuntime>,
    receiver: tokio::sync::mpsc::Receiver<PendingRecordingUpload>,
}

const RECORDING_UPLOAD_CHANNEL_CAPACITY: usize = 65_536;

impl RecordingUploadHook {
    /// Builds the hook plus (in S3 mode) the background upload manager and the
    /// shared storage handle. The storage clone is returned so callers can
    /// expose it for on-demand presigned URL generation.
    pub fn new(
        policy: RecordingPolicy,
    ) -> Result<(Self, Option<RecordingUploadManager>, Option<Storage>)> {
        Self::with_runtime(Arc::new(RecordingUploadRuntime::for_policy(Some(
            policy,
        ))))
    }

    /// Same as [`Self::new`] but attached to a shared late-bound runtime, so
    /// the hook, the background manager and the retry worker all follow
    /// `[recording]` policy hot-reloads.
    pub fn with_runtime(
        runtime: Arc<RecordingUploadRuntime>,
    ) -> Result<(Self, Option<RecordingUploadManager>, Option<Storage>)> {
        // Validate at startup: S3 storage construction errors must fail fast
        // (historical behavior), and the storage clone seeds the console's
        // presign path until the first reload.
        let (_, upload_storage) = runtime
            .resolve()?
            .unwrap_or_else(|| (RecordingPolicy::default(), None));
        let recording_type = runtime
            .policy()
            .unwrap_or_default()
            .effective_recording_type();
        let (s3_upload_sender, upload_manager) = if recording_type == RecordingType::S3 {
            let (sender, receiver) = tokio::sync::mpsc::channel(RECORDING_UPLOAD_CHANNEL_CAPACITY);
            (
                Some(sender),
                Some(RecordingUploadManager {
                    runtime: runtime.clone(),
                    receiver,
                }),
            )
        } else {
            (None, None)
        };

        Ok((
            Self {
                runtime,
                rwi_gateway: None,
                s3_upload_sender,
            },
            upload_manager,
            upload_storage,
        ))
    }

    /// Re-exported for app wiring that reports the storage type at startup.
    pub fn runtime(&self) -> Arc<RecordingUploadRuntime> {
        self.runtime.clone()
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
    async fn archive_local_artifacts(&self, policy: &RecordingPolicy, record: &mut CallRecord) {
        use crate::callrecord::{RecordingSubdir, local_archive_path};

        let subdir = RecordingSubdir::parse(policy.subdir.as_deref());
        let root = policy.recorder_path();

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

    /// Object key for `media_path`: its path relative to the recorder root,
    /// optionally prefixed by the target's `root` (main `[recording].root`
    /// for the default target, per-entry for source overrides).
    fn storage_key_with_prefix(
        policy: &RecordingPolicy,
        key_root: Option<&str>,
        media_path: &Path,
    ) -> String {
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
        match key_root
            .map(str::trim)
            .filter(|root| !root.is_empty())
        {
            Some(root) => format!("{}/{}", root.trim_end_matches('/'), key),
            None => key,
        }
    }

    /// URL rendering shared by the default target and per-source targets.
    fn s3_url_str(
        endpoint: Option<&str>,
        aliyun_virtual_host_style: bool,
        bucket: &str,
        key: &str,
    ) -> String {
        let bucket = bucket.trim().trim_matches('/');
        let key = key.trim_start_matches('/');
        let Some(endpoint) = endpoint.map(str::trim).filter(|s| !s.is_empty()) else {
            return format!("s3://{bucket}/{key}");
        };
        let endpoint = endpoint.trim_end_matches('/');
        if aliyun_virtual_host_style {
            return format!("{endpoint}/{key}");
        }
        format!("{endpoint}/{bucket}/{key}")
    }
}

impl RecordingUploadManager {
    pub async fn serve(&mut self) {
        while let Some(pending) = self.receiver.recv().await {
            let resolved = match self.runtime.resolve_targets() {
                Ok(Some((policy, targets))) => Some((policy, targets)),
                Ok(None) => None,
                Err(err) => {
                    warn!(
                        path = %pending.path.display(),
                        %err,
                        "recording uploader storage unavailable; local file retained"
                    );
                    continue;
                }
            };
            let Some((policy, targets)) = resolved else {
                warn!(
                    path = %pending.path.display(),
                    "recording uploader disabled by policy reload; local file retained"
                );
                continue;
            };
            if targets.recording_type() != RecordingType::S3 {
                warn!(
                    path = %pending.path.display(),
                    ty = ?targets.recording_type(),
                    "recording uploader disabled by policy reload; local file retained"
                );
                continue;
            }
            // Target selection: the queued source tag's override, or the
            // default bucket when unmapped / its override failed to build.
            let Some(spec) = targets.spec_for_tag(Some(&pending.source_tag)) else {
                warn!(
                    path = %pending.path.display(),
                    "recording uploader has no S3 target; local file retained"
                );
                continue;
            };
            let key = RecordingUploadHook::storage_key_with_prefix(
                &policy,
                spec.root.as_deref(),
                &pending.path,
            );
            let data = match tokio::fs::read(&pending.path).await {
                Ok(data) => data,
                Err(err) => {
                    warn!(
                        path = %pending.path.display(),
                        %err,
                        "recording uploader failed to read local media"
                    );
                    continue;
                }
            };
            let bytes = data.len();
            let content_type = match pending.path.extension().and_then(|extension| extension.to_str())
            {
                Some(extension) if extension.eq_ignore_ascii_case("wav") => "audio/wav",
                Some(extension) if extension.eq_ignore_ascii_case("jsonl") => "application/jsonl",
                _ => "application/octet-stream",
            };
            let attributes = Attributes::from_iter([(Attribute::ContentType, content_type)]);
            let started = std::time::Instant::now();
            if let Err(err) = spec
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
                    path = %pending.path.display(),
                    key,
                    bucket = %spec.bucket,
                    %err,
                    "recording upload failed"
                );
                crate::metrics::recording::upload_failure("s3");
                let address = if spec.bucket.trim().is_empty() {
                    "s3".to_string()
                } else {
                    spec.bucket.clone()
                };
                if let Err(write_err) = crate::callrecord::write_upload_failed_marker_ex(
                    &pending.path,
                    &address,
                    started.elapsed().as_millis() as u64,
                    &err.to_string(),
                    None,
                    Some(&pending.source_tag),
                )
                .await
                {
                    warn!(
                        path = %pending.path.display(),
                        %write_err,
                        "failed to write upload failure marker"
                    );
                }
                continue;
            }
            info!(
                path = %pending.path.display(),
                key,
                bucket = %spec.bucket,
                source = %pending.source_tag,
                bytes,
                content_type,
                "recording uploaded"
            );
            crate::metrics::recording::upload_success("s3");
            crate::metrics::recording::upload_latency_seconds(
                started.elapsed().as_secs_f64(),
                "s3",
            );
            if let Err(err) = tokio::fs::remove_file(&pending.path).await {
                warn!(
                    path = %pending.path.display(),
                    %err,
                    "failed to remove local recording after upload"
                );
            } else {
                let marker = crate::callrecord::upload_failed_marker_path(&pending.path);
                let _ = tokio::fs::remove_file(marker).await;
            }
        }
    }
}

/// Periodically scans `[recording].path` for `.upload_failed.*` markers and
/// retries remote uploads (HTTP or S3). Tracks hangup→success latency against
/// the configured SLA window (default 10 minutes).
pub struct RecordingRetryWorker {
    runtime: Arc<RecordingUploadRuntime>,
}

impl RecordingRetryWorker {
    pub fn with_runtime(runtime: Arc<RecordingUploadRuntime>) -> Self {
        Self { runtime }
    }

    pub async fn serve(self) {
        let Some(policy) = self.runtime.policy() else {
            info!("recording upload retry worker idle (no [recording] policy)");
            return;
        };
        let interval = policy.effective_retry_interval_secs();
        if interval == 0 {
            info!("recording upload retry worker disabled (retry_interval_secs=0)");
            return;
        }
        let ty = policy.effective_recording_type();
        if !matches!(ty, RecordingType::Http | RecordingType::S3) {
            info!(?ty, "recording upload retry worker idle (local/sipflow)");
            return;
        }
        info!(
            interval_secs = interval,
            sla_secs = policy.effective_upload_sla_secs(),
            "recording upload retry worker started"
        );
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval));
        loop {
            ticker.tick().await;
            // Re-resolve per scan so policy hot-reloads take effect.
            let resolved = match self.runtime.resolve_targets() {
                Ok(resolved) => resolved,
                Err(err) => {
                    warn!(%err, "recording upload retry storage unavailable");
                    continue;
                }
            };
            if let Err(err) = self.scan_once(resolved).await {
                warn!(%err, "recording upload retry scan failed");
            }
        }
    }

    async fn scan_once(
        &self,
        resolved: Option<(RecordingPolicy, Arc<RecordingTargets>)>,
    ) -> Result<()> {
        let Some((policy, _)) = resolved.as_ref() else {
            return Ok(());
        };
        let root = PathBuf::from(policy.recorder_path());
        if !root.exists() {
            crate::metrics::recording::set_pending_failed(0);
            return Ok(());
        }
        let markers = collect_upload_failed_markers(&root).await?;
        crate::metrics::recording::set_pending_failed(markers.len());
        let max_attempts = policy.effective_retry_max_attempts();
        for marker_path in markers {
            if let Err(err) = self
                .retry_marker(resolved.as_ref(), &marker_path, max_attempts)
                .await
            {
                warn!(
                    marker = %marker_path.display(),
                    %err,
                    "recording upload retry failed"
                );
            }
        }
        Ok(())
    }

    async fn retry_marker(
        &self,
        resolved: Option<&(RecordingPolicy, Arc<RecordingTargets>)>,
        marker_path: &Path,
        max_attempts: u32,
    ) -> Result<()> {
        let Some((policy, targets)) = resolved else {
            return Ok(());
        };
        let source = source_path_from_marker(marker_path).ok_or_else(|| {
            anyhow!(
                "cannot derive source path from marker {}",
                marker_path.display()
            )
        })?;
        if !source.exists() {
            // Orphan marker — drop it.
            let _ = tokio::fs::remove_file(marker_path).await;
            return Ok(());
        }
        let marker: UploadFailedMarker =
            serde_json::from_slice(&tokio::fs::read(marker_path).await?)?;
        if marker.attempts >= max_attempts {
            let dest = match policy.effective_recording_type() {
                RecordingType::Http => "http",
                RecordingType::S3 => "s3",
                _ => "unknown",
            };
            if let Some(age) = marker_age_secs(&marker)
                && age > policy.effective_upload_sla_secs() as f64
            {
                crate::metrics::recording::upload_sla_breach(dest);
            }
            return Ok(());
        }
        let dest = match policy.effective_recording_type() {
            RecordingType::Http => "http",
            RecordingType::S3 => "s3",
            _ => return Ok(()),
        };
        crate::metrics::recording::retry_attempt(dest);
        let started = std::time::Instant::now();
        match self
            .upload_source(
                policy,
                targets,
                &source,
                marker.call_id.as_deref(),
                marker.source.as_deref(),
            )
            .await
        {
            Ok(()) => {
                let latency =
                    marker_age_secs(&marker).unwrap_or_else(|| started.elapsed().as_secs_f64());
                crate::metrics::recording::upload_success(dest);
                crate::metrics::recording::upload_latency_seconds(latency, dest);
                if latency > policy.effective_upload_sla_secs() as f64 {
                    crate::metrics::recording::upload_sla_breach(dest);
                }
                let _ = tokio::fs::remove_file(&source).await;
                let _ = tokio::fs::remove_file(marker_path).await;
                info!(
                    path = %source.display(),
                    attempts = marker.attempts + 1,
                    latency_secs = latency,
                    "recording upload retry succeeded"
                );
            }
            Err(err) => {
                crate::metrics::recording::upload_failure(dest);
                let address = marker.address.clone();
                let _ = crate::callrecord::write_upload_failed_marker_ex(
                    &source,
                    &address,
                    started.elapsed().as_millis() as u64,
                    &err.to_string(),
                    marker.call_id.as_deref(),
                    marker.source.as_deref(),
                )
                .await;
            }
        }
        Ok(())
    }

    async fn upload_source(
        &self,
        policy: &RecordingPolicy,
        targets: &RecordingTargets,
        source: &Path,
        call_id: Option<&str>,
        source_tag: Option<&str>,
    ) -> Result<()> {
        let data = tokio::fs::read(source).await?;
        match policy.effective_recording_type() {
            RecordingType::Http => {
                let storage = targets
                    .http_storage()
                    .ok_or_else(|| anyhow!("HTTP storage unavailable for retry"))?;
                let file_name = source
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("recording.wav"))
                    .to_string_lossy()
                    .to_string();
                let request = crate::storage::UploadRequest {
                    key: file_name.clone(),
                    file_name: Some(file_name.clone()),
                    content_type: None,
                    file_field: None,
                    body_field: None,
                    vars: HashMap::from([
                        ("call_id".to_string(), call_id.unwrap_or("retry").to_string()),
                        ("track_id".to_string(), "retry".to_string()),
                        ("filename".to_string(), file_name),
                    ]),
                    bytes: Bytes::from(data),
                };
                storage.upload(request).await?;
                Ok(())
            }
            RecordingType::S3 => {
                // Retry the source's own target; markers written before the
                // per-source routing existed (no `source` field) retry into
                // the default bucket.
                let spec = targets
                    .spec_for_tag(source_tag)
                    .ok_or_else(|| anyhow!("S3 storage unavailable for retry"))?;
                let key = RecordingUploadHook::storage_key_with_prefix(
                    policy,
                    spec.root.as_deref(),
                    source,
                );
                let content_type = match source.extension().and_then(|e| e.to_str()) {
                    Some(ext) if ext.eq_ignore_ascii_case("wav") => "audio/wav",
                    Some(ext) if ext.eq_ignore_ascii_case("jsonl") => "application/jsonl",
                    _ => "application/octet-stream",
                };
                let attributes = Attributes::from_iter([(Attribute::ContentType, content_type)]);
                spec.storage
                    .write_opts(
                        &key,
                        Bytes::from(data),
                        PutOptions {
                            attributes,
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| anyhow!(e))?;
                Ok(())
            }
            other => Err(anyhow!("unsupported recording type for retry: {other:?}")),
        }
    }
}

fn source_path_from_marker(marker_path: &Path) -> Option<PathBuf> {
    let name = marker_path.file_name()?.to_str()?;
    let rest = name.strip_prefix(".upload_failed.")?;
    Some(marker_path.parent()?.join(rest))
}

fn marker_age_secs(marker: &UploadFailedMarker) -> Option<f64> {
    let t = chrono::DateTime::parse_from_rfc3339(&marker.time).ok()?;
    let age = chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc));
    Some(age.num_milliseconds().max(0) as f64 / 1000.0)
}

async fn collect_upload_failed_markers(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with(".upload_failed.") {
                    out.push(path);
                }
            }
        }
    }
    Ok(out)
}

impl RecordingUploadHook {
    /// Pre-build the remote URL for every media entry from its own upload
    /// target (per-source override or the default bucket), before the
    /// asynchronous upload runs.
    fn preconstruct_s3_urls(
        policy: &RecordingPolicy,
        targets: &RecordingTargets,
        record: &mut CallRecord,
    ) -> Result<()> {
        let mut first_media_url = None;

        for media in &mut record.recorder {
            let source_tag = media_source_tag(media);
            // No override and no default (unexpected in S3 mode): keep the
            // entry untouched rather than misattributing its URL.
            let Some(spec) = targets.spec_for_tag(Some(&source_tag)) else {
                continue;
            };
            let key = Self::storage_key_with_prefix(
                policy,
                spec.root.as_deref(),
                Path::new(&media.path),
            );
            let url = spec.s3_url(&key);
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
        storage: &Storage,
        policy: &RecordingPolicy,
        record: &CallRecord,
        track_id: &str,
        media_path: &str,
        data: Vec<u8>,
    ) -> Result<String> {
        let url = Self::required(&policy.url, "url")?;
        let file_name = Path::new(media_path)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("recording.wav"))
            .to_string_lossy()
            .to_string();
        let request = crate::storage::UploadRequest {
            key: file_name.clone(),
            file_name: Some(file_name.clone()),
            content_type: None,
            file_field: None,
            body_field: None,
            vars: HashMap::from([
                ("call_id".to_string(), record.call_id.clone()),
                ("track_id".to_string(), track_id.to_string()),
                ("filename".to_string(), file_name),
            ]),
            bytes: Bytes::from(data),
        };
        let uploaded = storage.upload(request).await?;
        Ok(uploaded.url.unwrap_or(url))
    }

    /// Emit one `recording_metadata_available` per recording segment. The
    /// `filename` / `download_url` / `file_size` describe this segment only;
    /// `extra` flattens the call-level metadata (agent_id, ivr, …) plus the
    /// segment extras (segment_type, segment_id, seq, label, time window) so
    /// consumers can reconcile multi-segment calls (IVR stage + agent stage).
    fn emit_segment_metadata(&self, record: &CallRecord, media: &CallRecordMedia, url: &str) {
        let Some(ref gw) = self.rwi_gateway else {
            return;
        };
        if media.track_id == "signaling" {
            return;
        }
        let metadata = build_segment_recording_metadata(record, media, url);
        let gw_ref = gw.read();
        gw_ref.send_to_owner(&crate::rwi::RecordingMetadataAvailable {
            call_id: record.call_id.clone(),
            metadata,
        });
    }
}

/// Metadata keys that stay CDR/console-only and are stripped from the
/// `recording_metadata_available` payload: console timeline data (`trace`),
/// the full segment array (`recording_segments` — the event already carries
/// the current segment's own extras), RTP quality stats (`media_quality`),
/// node identity duplicated by the gateway-injected `node_ip` (`self_ip`),
/// and the root session id duplicated by the event's `session_id` context
/// field (`session_id`).
const EVENT_METADATA_EXCLUDED_KEYS: &[&str] = &[
    "trace",
    "recording_segments",
    "self_ip",
    "media_quality",
    "session_id",
];

/// Pure builder for per-segment `recording_metadata_available` payloads.
/// `extra` flattens call-level metadata first, then the segment's own extras
/// (segment_type/segment_id/seq/label/…) so segment-specific values win.
/// Keys in [`EVENT_METADATA_EXCLUDED_KEYS`] are dropped (CDR keeps them).
fn build_segment_recording_metadata(
    record: &CallRecord,
    media: &CallRecordMedia,
    url: &str,
) -> RecordingMetadata {
    fn flatten(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            other => serde_json::to_string(other).ok(),
        }
    }
    let mut extra = record.details.metadata.clone().map(|m| {
        m.into_iter()
            .filter(|(k, _)| !EVENT_METADATA_EXCLUDED_KEYS.contains(&k.as_str()))
            .filter_map(|(k, v)| flatten(&v).map(|s| (k, s)))
            .collect::<HashMap<_, _>>()
    });
    if let Some(media_extra) = &media.extra {
        let bag = extra.get_or_insert_with(HashMap::new);
        for (key, value) in media_extra {
            if EVENT_METADATA_EXCLUDED_KEYS.contains(&key.as_str()) {
                continue;
            }
            if let Some(s) = flatten(value) {
                bag.insert(key.clone(), s);
            }
        }
    }
    let filename = Path::new(&media.path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("{}.wav", record.call_id));
    // Canonical source, classified from the segment's `segment_type` tag.
    // The dialplan-level whole-call artifact (`track_id = "mixed"`) has no
    // in-session bookkeeping and classifies as `full`. Emitted only as the
    // first-class `source` field — it must not also go into the flattened
    // `extra` bag (duplicate JSON key).
    let source = if media.track_id == "mixed" {
        crate::callrecord::RecordingSource::Full
    } else {
        media
            .extra
            .as_ref()
            .and_then(|e| e.get("segment_type"))
            .and_then(|v| v.as_str())
            .map(crate::callrecord::RecordingSource::classify)
            .unwrap_or(crate::callrecord::RecordingSource::External)
    };
    RecordingMetadata {
        // Minted by the reporter for every persisted media entry; the
        // fallback only covers exotic paths (external URLs, legacy CDRs) so
        // the vendor-required `unique_id` field is always present.
        unique_id: Some(
            media
                .unique_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        ),
        filename,
        file_size: media.size,
        download_url: Some(url.to_string()),
        caller_name: extract_sip_username(&record.caller),
        callee_name: extract_sip_username(&record.callee),
        call_type: record.details.direction.clone(),
        call_start_time: Some(record.start_time.to_rfc3339()),
        call_end_time: Some(record.end_time.to_rfc3339()),
        upload_time: Some(chrono::Utc::now().to_rfc3339()),
        // One slice of a (possibly) segmented recording — not the full call.
        full: false,
        source: Some(source.as_str().to_string()),
        extra,
    }
}

#[async_trait]
impl CallRecordHook for RecordingUploadHook {
    async fn on_record_enrich(&self, records: &mut [CallRecord]) -> anyhow::Result<()> {
        let Some((policy, targets)) = self.runtime.resolve_targets()? else {
            return Ok(());
        };
        for record in records {
            match policy.effective_recording_type() {
                RecordingType::Local => self.archive_local_artifacts(&policy, record).await,
                RecordingType::S3 => {
                    // Move generated artifacts into their final dated layout first,
                    // then persist the remote URL before the asynchronous upload.
                    self.archive_local_artifacts(&policy, record).await;
                    Self::preconstruct_s3_urls(&policy, &targets, record)?;
                }
                RecordingType::Http | RecordingType::Sipflow => {}
            }
        }
        Ok(())
    }

    async fn on_record_completed(&self, records: &mut [CallRecord]) -> anyhow::Result<()> {
        use crate::callrecord::{
            RecordingSubdir, local_archive_path, write_upload_failed_marker_ex,
        };
        use std::time::Instant;

        let Some((policy, targets)) = self.runtime.resolve_targets()? else {
            return Ok(());
        };
        let recording_type = policy.effective_recording_type();
        if !recording_type.is_file_media() {
            return Ok(());
        }
        let subdir = RecordingSubdir::parse(policy.subdir.as_deref());
        let root = policy.recorder_path();

        for record in records {
            // File entries are finalized recording artifacts (wav + optional jsonl).
            let mut first_uploaded_url = None;
            let mut segment_summaries = Vec::new();

            for index in 0..record.recorder.len() {
                let (track_id, path, source_tag) = {
                    let media = &record.recorder[index];
                    (
                        media.track_id.clone(),
                        media.path.clone(),
                        media_source_tag(media),
                    )
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
                    // Route this entry to its own target: the source tag's
                    // override bucket, or the default bucket when unmapped /
                    // its override failed to build.
                    let Some(spec) = targets.spec_for_tag(Some(&source_tag)) else {
                        warn!(
                            call_id = %record.call_id,
                            track_id,
                            path,
                            "recording upload skipped: no S3 target available"
                        );
                        continue;
                    };
                    let key = Self::storage_key_with_prefix(
                        &policy,
                        spec.root.as_deref(),
                        Path::new(&path),
                    );
                    let url = spec.s3_url(&key);
                    match self
                        .s3_upload_sender
                        .as_ref()
                        .ok_or_else(|| anyhow!("recording S3 uploader is not initialized"))?
                        .try_send(PendingRecordingUpload {
                            path: PathBuf::from(&path),
                            source_tag: source_tag.clone(),
                        })
                    {
                        Ok(()) => info!(
                            call_id = %record.call_id,
                            track_id,
                            path,
                            key,
                            bucket = %spec.bucket,
                            source = %source_tag,
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
                    if let Some(media) = record.recorder.get(index) {
                        self.emit_segment_metadata(record, media, &url);
                    }
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
                        if let Some(media) = record.recorder.get(index) {
                            self.emit_segment_metadata(record, media, &archived);
                        }
                    }
                    RecordingType::Http => {
                        let address = policy.url.clone().unwrap_or_else(|| "http".to_string());
                        let started = Instant::now();
                        let storage = targets
                            .http_storage()
                            .ok_or_else(|| {
                                anyhow!("recording http upload is not configured (missing url?)")
                            })?;
                        let upload = self
                            .upload_http(storage, &policy, record, &track_id, &path, data)
                            .await;
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
                                crate::metrics::recording::upload_success("http");
                                if let Ok(age) = (chrono::Utc::now() - record.end_time).to_std() {
                                    let secs = age.as_secs_f64();
                                    crate::metrics::recording::upload_latency_seconds(secs, "http");
                                    if secs > policy.effective_upload_sla_secs() as f64 {
                                        crate::metrics::recording::upload_sla_breach("http");
                                    }
                                }
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
                                if let Some(media) = record.recorder.get(index) {
                                    self.emit_segment_metadata(record, media, &url);
                                }
                            }
                            Err(err) => {
                                warn!(
                                    call_id = %record.call_id,
                                    track_id,
                                    path,
                                    "recording upload failed: {err}"
                                );
                                crate::metrics::recording::upload_failure("http");
                                if let Err(write_err) = write_upload_failed_marker_ex(
                                    Path::new(&path),
                                    &address,
                                    elapsed_ms,
                                    &err.to_string(),
                                    Some(record.call_id.as_str()),
                                    Some(&source_tag),
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

                // Single-notification contract: one `recording_metadata_available`
                // per recording artifact. File-media calls were already notified
                // per segment above (`emit_segment_metadata`); this call-level
                // event now fires ONLY when the call's single artifact was
                // captured by SipFlow (no local segment files, URL stashed by
                // SipFlowUploadHook). The former duplicate notifications —
                // the `full=true` aggregate on segmented calls and `record_end`
                // — are no longer emitted.
                let per_segment_notified =
                    record.recorder.iter().any(|m| m.track_id != "signaling");
                if !per_segment_notified && let Some(ref gw) = self.rwi_gateway {
                    // Same exclusion policy as the per-segment events (see
                    // build_segment_recording_metadata).
                    let extra = record.details.metadata.clone().map(|m| {
                        m.into_iter()
                            .filter(|(k, _)| !EVENT_METADATA_EXCLUDED_KEYS.contains(&k.as_str()))
                            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                            .collect::<HashMap<_, _>>()
                    });
                    let metadata = RecordingMetadata {
                        // SipFlow capture has no local segment bookkeeping, so
                        // the recording identifier is minted here.
                        unique_id: Some(uuid::Uuid::new_v4().to_string()),
                        filename: recording_filename(record, url),
                        file_size: recording_file_size(record),
                        download_url: Some(url.to_string()),
                        caller_name: extract_sip_username(&record.caller),
                        callee_name: extract_sip_username(&record.callee),
                        call_type: record.details.direction.clone(),
                        call_start_time: Some(record.start_time.to_rfc3339()),
                        call_end_time: Some(record.end_time.to_rfc3339()),
                        upload_time: Some(chrono::Utc::now().to_rfc3339()),
                        // The whole-call SipFlow artifact — the only
                        // recording of this call.
                        full: true,
                        source: Some(crate::callrecord::RecordingSource::Full.as_str().to_string()),
                        extra,
                    };
                    let gw_ref = gw.read();
                    gw_ref.send_to_owner(&crate::rwi::RecordingMetadataAvailable {
                        call_id: record.call_id.clone(),
                        metadata,
                    });
                }
            }
        }

        Ok(())
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

    /// Prints the exact wire shape produced by the real serialization path
    /// (`build_segment_recording_metadata` + `RecordingMetadataAvailable`
    /// serde): zero-padded seq in the file name, RFC3339 timestamps, string
    /// extras. Run with `cargo test segment_metadata -- --nocapture`.
    #[test]
    fn segment_metadata_wire_shape() {
        let root = tempfile::tempdir().unwrap();
        let root_session = "0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40";
        // Real naming function — zero-pads seq, sanitizes the label.
        let (seq, path) = crate::callrecord::segmented_wav_path(
            root.path().to_str().unwrap(),
            root_session,
            2,
            "1001",
        );
        let started = chrono::Utc::now();
        let ended = started + chrono::Duration::seconds(28);

        let mut details = CallDetails::default();
        details.direction = "inbound".to_string();
        // Keys the CC session hook publishes into session extensions, copied
        // verbatim into CallDetails.metadata by record_snapshot — plus the
        // console-only heavy keys that must NOT leak into the RWI event.
        details.metadata = Some(std::collections::HashMap::from([
            ("agent_id".to_string(), json!("1001")),
            ("agent_name".to_string(), json!("Agent 1001")),
            ("queue_id".to_string(), json!("support")),
            ("self_ip".to_string(), json!("10.0.0.7")),
            (
                "media_quality".to_string(),
                json!([{"codec": "Opus", "lossPct": 0.0}]),
            ),
            (
                "recording_segments".to_string(),
                json!([{"segmentType": "agent", "size": 153344}]),
            ),
            (
                "trace".to_string(),
                json!([{"kind": "answer", "message": "Call answered"}]),
            ),
        ]));

        // CallRecordMedia.extra exactly as reporter::collect_recording_artifacts writes it.
        let media = CallRecordMedia {
            unique_id: None,
            track_id: format!("segment:agent:{seq}"),
            path: path.to_string_lossy().into_owned(),
            size: 153_344,
            extra: Some(std::collections::HashMap::from([
                ("session_id".to_string(), json!(root_session)),
                ("segment_type".to_string(), json!("agent")),
                ("segment_id".to_string(), json!("9c1f02ab")),
                ("seq".to_string(), json!(seq)),
                ("label".to_string(), json!("1001")),
                ("started_at".to_string(), json!(started.to_rfc3339())),
                ("ended_at".to_string(), json!(ended.to_rfc3339())),
            ])),
        };
        let record = CallRecord {
            call_id: root_session.to_string(),
            start_time: started - chrono::Duration::seconds(17),
            end_time: ended + chrono::Duration::seconds(2),
            caller: "sip:330909@192.168.1.10:5060".to_string(),
            callee: "sip:1001@192.168.1.5:5060".to_string(),
            recorder: vec![media.clone()],
            details,
            ..Default::default()
        };

        let event = crate::rwi::RecordingMetadataAvailable {
            call_id: record.call_id.clone(),
            metadata: build_segment_recording_metadata(
                &record,
                &media,
                path.to_string_lossy().as_ref(),
            ),
        };
        println!("{}", serde_json::to_string_pretty(&event).unwrap());

        let value = serde_json::to_value(&event).unwrap();
        let meta = &value["metadata"];
        assert_eq!(
            meta["filename"].as_str(),
            Some(format!("{}_{}_{}.wav", root_session, "02", "1001").as_str())
        );
        assert_eq!(meta["seq"].as_str(), Some("2"));
        assert_eq!(
            meta["full"].as_bool(),
            Some(false),
            "per-segment event must be marked full=false: {meta}"
        );
        assert_eq!(
            meta["source"].as_str(),
            Some("agent"),
            "per-segment event must carry the canonical source: {meta}"
        );
        assert!(meta.get("caller_name").is_some());

        // Console/CDR-only keys are stripped from the event payload; the
        // event's own segment extras and call-level business keys survive.
        for excluded in [
            "session_id",
            "self_ip",
            "media_quality",
            "recording_segments",
            "trace",
        ] {
            assert!(
                meta.get(excluded).is_none(),
                "event metadata must not carry `{excluded}`: {meta}"
            );
        }
        for expected in [
            "agent_id",
            "agent_name",
            "queue_id",
            "segment_type",
            "segment_id",
        ] {
            assert!(
                meta.get(expected).is_some(),
                "event metadata must keep `{expected}`: {meta}"
            );
        }
    }

    #[test]
    fn segment_metadata_carries_segment_extras_and_call_context() {
        let now = chrono::Utc::now();
        let mut details = CallDetails::default();
        details.direction = "inbound".to_string();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("agent_id".to_string(), json!("1001"));
        metadata.insert("ivr".to_string(), json!("main"));
        details.metadata = Some(metadata);
        let record = CallRecord {
            call_id: "call-1".into(),
            start_time: now,
            end_time: now + chrono::Duration::seconds(30),
            recorder: vec![],
            details,
            ..Default::default()
        };
        let mut seg_extra = std::collections::HashMap::new();
        seg_extra.insert("segment_type".to_string(), json!("agent"));
        seg_extra.insert("segment_id".to_string(), json!("ab12"));
        seg_extra.insert("seq".to_string(), json!(2));
        seg_extra.insert("label".to_string(), json!("1001"));
        seg_extra.insert("started_at".to_string(), json!("t0"));
        seg_extra.insert("ended_at".to_string(), json!("t1"));
        let media = CallRecordMedia {
            unique_id: None,
            track_id: "segment:agent:ab12".into(),
            path: "/recorders/call-1_02_1001.wav".into(),
            size: 4096,
            extra: Some(seg_extra),
        };
        let meta =
            build_segment_recording_metadata(&record, &media, "https://up/call-1_02_1001.wav");
        assert_eq!(meta.filename, "call-1_02_1001.wav");
        assert_eq!(meta.file_size, 4096);
        assert_eq!(
            meta.download_url.as_deref(),
            Some("https://up/call-1_02_1001.wav")
        );
        assert_eq!(meta.call_type, "inbound");
        let extra = meta.extra.expect("extra");
        assert_eq!(extra.get("agent_id").map(String::as_str), Some("1001"));
        assert_eq!(extra.get("ivr").map(String::as_str), Some("main"));
        assert_eq!(extra.get("seq").map(String::as_str), Some("2"));
        assert_eq!(extra.get("label").map(String::as_str), Some("1001"));
        assert_eq!(extra.get("segment_type").map(String::as_str), Some("agent"));
        assert_eq!(extra.get("started_at").map(String::as_str), Some("t0"));
    }

    /// The vendor acceptance sheet requires `event.metadata.unique_id` (录音
    /// 唯一标识): the per-segment metadata event must carry the recording's
    /// unique id — the same id `record_started` / `record_stopped` use.
    #[test]
    fn segment_metadata_carries_recording_unique_id() {
        let now = chrono::Utc::now();
        let record = CallRecord {
            call_id: "call-uid".into(),
            start_time: now,
            end_time: now + chrono::Duration::seconds(10),
            recorder: vec![],
            details: CallDetails::default(),
            ..Default::default()
        };
        let media = CallRecordMedia {
            unique_id: Some("0e1c8a52-6f1e-4c8d-9a52-6ff5b0f5f9b1".into()),
            track_id: "segment:ivr:1".into(),
            path: "/recorders/call-uid_01_main.wav".into(),
            size: 1024,
            extra: None,
        };
        let meta =
            build_segment_recording_metadata(&record, &media, "https://up/call-uid_01_main.wav");
        assert_eq!(
            meta.unique_id.as_deref(),
            Some("0e1c8a52-6f1e-4c8d-9a52-6ff5b0f5f9b1"),
            "metadata.unique_id must mirror the media entry's id, not be minted fresh"
        );

        // Serialized payload exposes the snake_case key the consumer reads.
        let value = serde_json::to_value(&crate::rwi::RecordingMetadataAvailable {
            call_id: record.call_id.clone(),
            metadata: meta,
        })
        .unwrap();
        assert_eq!(
            value["metadata"]["unique_id"].as_str(),
            Some("0e1c8a52-6f1e-4c8d-9a52-6ff5b0f5f9b1")
        );
    }

    /// Legacy/external media entries without a persisted id still get a
    /// `unique_id` minted (fallback), so the acceptance field is always
    /// present on the event.
    #[test]
    fn segment_metadata_mints_unique_id_when_media_lacks_one() {
        let now = chrono::Utc::now();
        let record = CallRecord {
            call_id: "call-uid-legacy".into(),
            start_time: now,
            end_time: now + chrono::Duration::seconds(10),
            recorder: vec![],
            details: CallDetails::default(),
            ..Default::default()
        };
        let media = CallRecordMedia {
            unique_id: None,
            track_id: "mixed".into(),
            path: "/recorders/legacy.wav".into(),
            size: 1024,
            extra: None,
        };
        let meta = build_segment_recording_metadata(&record, &media, "https://up/legacy.wav");
        let unique_id = meta.unique_id.expect("unique_id must be present");
        assert_eq!(unique_id.len(), 36, "fallback must be a UUID v4 string");
    }

    #[tokio::test]
    async fn aliyun_empty_bucket_and_region_initialize_recording_hook() {
        let policy = RecordingPolicy {
            recording_type: Some(RecordingType::S3),
            vendor: Some(crate::storage::S3Vendor::Aliyun),
            bucket: Some(String::new()),
            region: Some(String::new()),
            endpoint: Some("https://test-bucket.oss-cn-beijing.aliyuncs.com".into()),
            access_key: Some("test".into()),
            secret_key: Some("test".into()),
            root: Some("recordings".into()),
            ..Default::default()
        };
        let (_hook, _, _) = RecordingUploadHook::new(policy.clone()).unwrap();
        let targets = build_recording_targets(&policy).unwrap();
        let mut record = CallRecord {
            recorder: vec![CallRecordMedia {
                unique_id: None,
                track_id: "mixed".into(),
                path: "call.wav".into(),
                size: 44,
                extra: None,
            }],
            ..Default::default()
        };
        RecordingUploadHook::preconstruct_s3_urls(&policy, &targets, &mut record).unwrap();
        let raw = record.details.recording_url.unwrap();
        assert_eq!(
            raw,
            "https://test-bucket.oss-cn-beijing.aliyuncs.com/recordings/call.wav"
        );
    }

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
        let (hook, _upload_manager, _) = RecordingUploadHook::new(policy).expect("recording hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "preconstructed-url".into(),
            start_time: now,
            end_time: now + chrono::Duration::seconds(60),
            recorder: vec![CallRecordMedia {
                unique_id: None,
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
        let runtime = Arc::new(RecordingUploadRuntime::for_policy(Some(policy.clone())));
        runtime.seed_storage(&policy, storage);
        let mut manager = RecordingUploadManager {
            runtime,
            receiver,
        };
        let uploader = crate::utils::spawn(async move {
            manager.serve().await;
        });

        sender
            .send(PendingRecordingUpload {
                path: missing,
                source_tag: "full".to_string(),
            })
            .await
            .expect("queue missing path");
        sender
            .send(PendingRecordingUpload {
                path: recording.clone(),
                source_tag: "full".to_string(),
            })
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
        let (hook, _upload_manager, _) = RecordingUploadHook::new(policy).expect("recording hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "early-media-call".to_string(),
            start_time: now - chrono::Duration::seconds(8),
            answer_time: None,
            end_time: now,
            recorder: vec![CallRecordMedia {
                unique_id: None,
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
    async fn http_upload_uses_configurable_scheme() {
        use axum::body::Bytes as AxumBytes;

        let captured = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let captured_request = captured.clone();
        let app = axum::Router::new().route(
            "/upload/{key}",
            axum::routing::post(
                move |path: axum::extract::Path<String>, body: AxumBytes| {
                    let captured = captured_request.clone();
                    async move {
                        captured.lock().unwrap().extend_from_slice(&body);
                        axum::Json(serde_json::json!({
                            "code": 0,
                            "data": { "url": format!("https://cdn.example/{}", path.0) }
                        }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upload server");
        let address = listener.local_addr().expect("upload server address");
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scheme.wav");
        tokio::fs::write(&path, b"scheme-bytes")
            .await
            .expect("write recording");
        let path = path.to_string_lossy().into_owned();

        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Http),
            url: Some(format!("http://{address}/upload/{{key}}")),
            file_field: Some("filecontent".to_string()),
            content_type: Some("application/octet-stream".to_string()),
            fields: Some(HashMap::from([(
                "call_id".to_string(),
                "{call_id}".to_string(),
            )])),
            response_url_path: Some("data.url".to_string()),
            response_success: Some(crate::storage::SuccessRule {
                path: "code".to_string(),
                equals: serde_json::json!(0),
            }),
            ..Default::default()
        };
        let (hook, _upload_manager, _) = RecordingUploadHook::new(policy).expect("recording hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "scheme-call".to_string(),
            start_time: now - chrono::Duration::seconds(3),
            answer_time: Some(now - chrono::Duration::seconds(2)),
            end_time: now,
            recorder: vec![CallRecordMedia {
                unique_id: None,
                track_id: "mixed".to_string(),
                path: path.clone(),
                size: 12,
                extra: None,
            }],
            details: CallDetails::default(),
            ..Default::default()
        };
        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .expect("upload via configurable scheme");

        assert_eq!(
            record.details.recording_url.as_deref(),
            Some("https://cdn.example/scheme.wav")
        );

        let body = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(body.contains("name=\"filecontent\""), "custom file field: {body}");
        assert!(body.contains("filename=\"scheme.wav\""), "file name: {body}");
        assert!(body.contains("name=\"call_id\""), "extra field: {body}");
        assert!(body.contains("scheme-call"), "extra field value: {body}");
        assert!(!Path::new(&path).exists(), "local file removed after upload");
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
        let (hook, _upload_manager, _) = RecordingUploadHook::new(policy).expect("recording hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "upload-fail-call".to_string(),
            start_time: now - chrono::Duration::seconds(5),
            answer_time: Some(now - chrono::Duration::seconds(4)),
            end_time: now,
            recorder: vec![CallRecordMedia {
                unique_id: None,
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
    async fn retry_worker_clears_marker_after_http_success() {
        use axum::{Router, body::Bytes, routing::post};
        use std::net::SocketAddr;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("retry.wav");
        tokio::fs::write(&path, b"wav-bytes")
            .await
            .expect("write recording");
        let marker = crate::callrecord::upload_failed_marker_path(&path);
        let body = serde_json::json!({
            "time": chrono::Utc::now().to_rfc3339(),
            "address": "http://placeholder",
            "duration_ms": 1,
            "error": "previous failure",
            "attempts": 1,
            "call_id": "retry-call",
        });
        tokio::fs::write(&marker, serde_json::to_vec_pretty(&body).unwrap())
            .await
            .expect("write marker");

        let hit = Arc::new(AtomicBool::new(false));
        let hit_flag = hit.clone();
        let app = Router::new().route(
            "/recording",
            post(move |_body: Bytes| {
                let hit_flag = hit_flag.clone();
                async move {
                    hit_flag.store(true, Ordering::SeqCst);
                    axum::http::StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Http),
            path: Some(dir.path().to_string_lossy().into_owned()),
            url: Some(format!("http://{addr}/recording")),
            retry_interval_secs: Some(60),
            retry_max_attempts: Some(5),
            upload_sla_secs: Some(600),
            ..Default::default()
        };
        let worker =
            RecordingRetryWorker::with_runtime(Arc::new(RecordingUploadRuntime::for_policy(
                Some(policy),
            )));
        worker
            .scan_once(worker.runtime.resolve_targets().ok().flatten())
            .await
            .expect("scan");

        assert!(hit.load(Ordering::SeqCst), "upload endpoint hit");
        assert!(!path.exists(), "local file removed after retry success");
        assert!(!marker.exists(), "marker cleared after retry success");
    }

    #[test]
    fn source_path_from_upload_failed_marker() {
        let marker = PathBuf::from("/rec/20260101/.upload_failed.call.wav");
        assert_eq!(
            source_path_from_marker(&marker),
            Some(PathBuf::from("/rec/20260101/call.wav"))
        );
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
        let (hook, _upload_manager, _) = RecordingUploadHook::new(policy).expect("hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "local-archive".into(),
            start_time: now,
            answer_time: Some(now),
            end_time: now,
            recorder: vec![CallRecordMedia {
                unique_id: None,
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
        let (hook, _upload_manager, _) = RecordingUploadHook::new(policy).expect("hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "local-hourly".into(),
            start_time: now,
            answer_time: Some(now),
            end_time: now,
            recorder: vec![CallRecordMedia {
                unique_id: None,
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
        let (hook, _upload_manager, _) = RecordingUploadHook::new(policy).unwrap();
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "multi-artifact".into(),
            start_time: now - chrono::Duration::seconds(3),
            answer_time: Some(now - chrono::Duration::seconds(2)),
            end_time: now,
            recorder: vec![
                CallRecordMedia {
                    unique_id: None,
                    track_id: "segment:ivr:1".into(),
                    path: wav_s.clone(),
                    size: 3,
                    extra: None,
                },
                CallRecordMedia {
                    unique_id: None,
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
        let (hook, _upload_manager, _) = RecordingUploadHook::new(policy).expect("hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "enrich-archive".into(),
            start_time: now,
            answer_time: Some(now),
            end_time: now,
            recorder: vec![
                CallRecordMedia {
                    unique_id: None,
                    track_id: "segment:full:1".into(),
                    path: wav_s,
                    size: 3,
                    extra: None,
                },
                CallRecordMedia {
                    unique_id: None,
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
        let (hook, _upload_manager, _) = RecordingUploadHook::new(policy).expect("hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "custom-path".into(),
            start_time: now,
            answer_time: Some(now),
            end_time: now,
            recorder: vec![CallRecordMedia {
                unique_id: None,
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

    /// Single-notification contract: a segmented (file-media) call must emit
    /// exactly one `recording_metadata_available` per segment (`full=false`)
    /// and never the former call-level `full=true` aggregate — the duplicate
    /// notifications that consumers received under the old contract.
    #[tokio::test]
    async fn segmented_call_emits_only_per_segment_events() {
        use crate::rwi::RwiGateway;
        use parking_lot::RwLock;
        use std::sync::Arc;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("tempdir");
        let recorder_root = dir.path().join("recorders");
        tokio::fs::create_dir_all(&recorder_root)
            .await
            .expect("create recorder root");
        let seg_paths: Vec<std::path::PathBuf> = ["call_01_ivr.wav", "call_02_agent.wav"]
            .iter()
            .map(|name| {
                let path = recorder_root.join(name);
                std::fs::write(&path, b"wav").expect("write segment file");
                path
            })
            .collect();

        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let mut event_rx = gateway.read().subscribe_events();

        let policy = RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::Local),
            path: Some(recorder_root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let (hook, _upload_manager, _) =
            RecordingUploadHook::new(policy).expect("recording hook");
        let hook = hook.with_rwi_gateway(gateway.clone());

        let now = chrono::Utc::now();
        let mk_media = |idx: usize, seg_type: &str| CallRecordMedia {
            unique_id: Some(format!("uid-{idx}")),
            track_id: format!("segment:{seg_type}:{idx}"),
            path: seg_paths[idx].to_string_lossy().into_owned(),
            size: 3,
            extra: Some(std::collections::HashMap::from([
                (
                    "segment_type".to_string(),
                    serde_json::Value::String(seg_type.to_string()),
                ),
                ("segment_id".to_string(), serde_json::Value::String(idx.to_string())),
            ])),
        };
        let mut record = CallRecord {
            call_id: "segmented-call".into(),
            start_time: now,
            end_time: now + chrono::Duration::seconds(30),
            recorder: vec![mk_media(0, "ivr"), mk_media(1, "agent")],
            details: CallDetails::default(),
            ..Default::default()
        };

        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .expect("on_record_completed");

        let mut per_segment = 0u32;
        let mut aggregate = 0u32;
        let deadline = Duration::from_millis(200);
        let start = std::time::Instant::now();
        loop {
            match tokio::time::timeout(deadline.saturating_sub(start.elapsed()), event_rx.recv())
                .await
            {
                Ok(Ok(entry)) => {
                    if entry.call_id != "segmented-call" {
                        continue;
                    }
                    if entry.event.event_type == "recording_metadata_available" {
                        let full = entry.event.payload["metadata"]["full"].as_bool();
                        if full == Some(true) {
                            aggregate += 1;
                        } else {
                            per_segment += 1;
                            let source = entry.event.payload["metadata"]["source"].as_str();
                            assert!(
                                source == Some("ivr") || source == Some("agent"),
                                "per-segment events must carry their canonical source, got {source:?}"
                            );
                        }
                    }
                    assert!(
                        entry.event.event_type != "record_end",
                        "record_end must no longer be emitted"
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    panic!("event tap lagged by {n} messages");
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_timeout) => break,
            }
        }

        assert_eq!(
            per_segment, 2,
            "each segment must get exactly one full=false event"
        );
        assert_eq!(
            aggregate, 0,
            "segmented calls must not receive the former full=true aggregate event"
        );
    }

    fn source_media(path: &Path, track_id: &str, segment_type: &str) -> CallRecordMedia {
        CallRecordMedia {
            unique_id: None,
            track_id: track_id.to_string(),
            path: path.to_string_lossy().into_owned(),
            size: 1,
            extra: Some(std::collections::HashMap::from([(
                "segment_type".to_string(),
                json!(segment_type),
            )])),
        }
    }

    #[test]
    fn media_source_tag_classifies_entries() {
        let base = Path::new("/tmp/x.wav");
        let tag = |media: CallRecordMedia| media_source_tag(&media);
        // Dialplan-level whole-call artifact → full.
        assert_eq!(
            tag(CallRecordMedia {
                unique_id: None,
                track_id: "mixed".into(),
                path: base.to_string_lossy().into_owned(),
                size: 0,
                extra: None,
            }),
            "full"
        );
        // Segment extras drive the canonical tag.
        assert_eq!(tag(source_media(base, "segment:agent:1", "agent")), "agent");
        assert_eq!(
            tag(source_media(base, "segment:ringing:1", "ringing")),
            "ringing"
        );
        // Custom tags classify as external; missing tag → full.
        assert_eq!(
            tag(source_media(base, "segment:csat:1", "csat")),
            "external"
        );
        assert_eq!(
            tag(CallRecordMedia {
                unique_id: None,
                track_id: "segment:ivr:1".into(),
                path: base.to_string_lossy().into_owned(),
                size: 0,
                extra: None,
            }),
            "full"
        );
    }

    fn s3_policy_with_sources() -> RecordingPolicy {
        RecordingPolicy {
            enabled: Some(true),
            recording_type: Some(RecordingType::S3),
            path: Some("/tmp/recorders".into()),
            vendor: Some(crate::storage::S3Vendor::Minio),
            bucket: Some("default-bucket".into()),
            region: Some("us-east-1".into()),
            access_key: Some("ak".into()),
            secret_key: Some("sk".into()),
            endpoint: Some("http://default:9000".into()),
            root: Some("base".into()),
            sources: Some(std::collections::HashMap::from([
                (
                    "agent".to_string(),
                    crate::config::RecordingS3Target {
                        bucket: "isc_sr".into(),
                        vendor: Some(crate::storage::S3Vendor::Aliyun),
                        region: None,
                        access_key: Some("a2".into()),
                        secret_key: Some("s2".into()),
                        endpoint: Some("http://v2:9000".into()),
                        root: Some("agent".into()),
                    },
                ),
                (
                    "Ringing".to_string(),
                    crate::config::RecordingS3Target {
                        bucket: "test2".into(),
                        vendor: None,
                        region: None,
                        access_key: None,
                        secret_key: None,
                        endpoint: None,
                        root: None,
                    },
                ),
            ])),
            ..Default::default()
        }
    }

    #[test]
    fn build_recording_targets_maps_sources_and_falls_back() {
        let policy = s3_policy_with_sources();
        let targets = build_recording_targets(&policy).expect("targets");

        // Key normalization: config keys are case/space insensitive.
        assert_eq!(
            targets.spec_for_tag(Some("agent")).unwrap().bucket,
            "isc_sr"
        );
        assert_eq!(
            targets
                .spec_for_tag(Some("agent"))
                .unwrap()
                .root
                .as_deref(),
            Some("agent")
        );
        assert_eq!(
            targets.spec_for_tag(Some("RINGING")).unwrap().bucket,
            "test2"
        );
        // Unmapped sources and missing tags resolve to the default bucket.
        assert_eq!(
            targets.spec_for_tag(Some("ivr")).unwrap().bucket,
            "default-bucket"
        );
        assert_eq!(
            targets.spec_for_tag(Some("full")).unwrap().bucket,
            "default-bucket"
        );
        assert_eq!(targets.spec_for_tag(None).unwrap().bucket, "default-bucket");
        // The default target carries the main section's key prefix.
        assert_eq!(
            targets.spec_for_tag(None).unwrap().root.as_deref(),
            Some("base")
        );
        // All S3 targets are presign candidates: the default bucket first,
        // then the per-source overrides (HashMap order).
        let buckets: Vec<&str> = targets
            .s3_targets()
            .map(|spec| spec.bucket.as_str())
            .collect();
        assert_eq!(buckets.first(), Some(&"default-bucket"));
        assert_eq!(buckets.len(), 3);
        assert!(buckets.contains(&"isc_sr") && buckets.contains(&"test2"));
    }

    #[test]
    fn broken_source_target_degrades_to_default_bucket() {
        // R3: a per-source entry with a partial credential pair fails its
        // Storage build but must not poison the whole target set.
        let mut policy = s3_policy_with_sources();
        policy.sources.as_mut().unwrap().insert(
            "ivr".to_string(),
            crate::config::RecordingS3Target {
                bucket: "test1".into(),
                access_key: Some("only-access-key".into()),
                secret_key: None,
                ..Default::default()
            },
        );
        let targets = build_recording_targets(&policy).expect("targets still build");
        assert_eq!(
            targets.spec_for_tag(Some("ivr")).unwrap().bucket,
            "default-bucket",
            "broken ivr override must fall back to the default bucket"
        );
        assert_eq!(
            targets.spec_for_tag(Some("agent")).unwrap().bucket,
            "isc_sr",
            "healthy overrides must be unaffected"
        );
    }

    #[test]
    fn preconstruct_s3_urls_routes_per_source() {
        let policy = s3_policy_with_sources();
        let targets = build_recording_targets(&policy).expect("targets");
        let mut record = CallRecord {
            call_id: "multi".into(),
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now() + chrono::Duration::seconds(10),
            recorder: vec![
                source_media(Path::new("/tmp/recorders/a.wav"), "segment:agent:1", "agent"),
                source_media(Path::new("/tmp/recorders/r.wav"), "segment:ringing:1", "ringing"),
                CallRecordMedia {
                    unique_id: None,
                    track_id: "mixed".into(),
                    path: "/tmp/recorders/full.wav".into(),
                    size: 1,
                    extra: None,
                },
            ],
            details: CallDetails::default(),
            ..Default::default()
        };

        RecordingUploadHook::preconstruct_s3_urls(&policy, &targets, &mut record)
            .expect("preconstruct");

        let url = |index: usize| {
            record.recorder[index]
                .extra
                .as_ref()
                .unwrap()
                .get("uploadUrl")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        };
        // agent → v2/isc_sr with its own prefix; ringing → test2 (no prefix,
        // no endpoint of its own → bare s3:// URL); full → default bucket
        // with the main root prefix. Per-entry fields are independent — the
        // ringing target does NOT inherit the main endpoint. The Aliyun
        // vendor renders virtual-host style URLs (no bucket path segment).
        assert_eq!(url(0), "http://v2:9000/agent/a.wav");
        assert_eq!(url(1), "s3://test2/r.wav");
        assert_eq!(url(2), "http://default:9000/default-bucket/base/full.wav");
        // The call-level recording URL is the first non-signaling media URL.
        assert_eq!(
            record.details.recording_url.as_deref(),
            Some("http://v2:9000/agent/a.wav")
        );
    }

    #[tokio::test]
    async fn serve_routes_uploads_by_source_tag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recorder_root = dir.path().join("recorders");
        tokio::fs::create_dir_all(&recorder_root)
            .await
            .expect("create recorder root");
        let agent_wav = recorder_root.join("agent.wav");
        let full_wav = recorder_root.join("full.wav");
        tokio::fs::write(&agent_wav, b"agent").await.expect("write");
        tokio::fs::write(&full_wav, b"full").await.expect("write");

        let default_objects = dir.path().join("objects-default");
        let agent_objects = dir.path().join("objects-agent");
        let policy = RecordingPolicy {
            path: Some(recorder_root.to_string_lossy().into_owned()),
            root: Some("base".into()),
            ..Default::default()
        };
        let runtime = Arc::new(RecordingUploadRuntime::for_policy(Some(policy.clone())));
        runtime.seed_targets(
            &policy,
            RecordingTargets {
                recording_type: RecordingType::S3,
                default: Some(S3TargetSpec {
                    storage: Storage::new(&StorageConfig::Local {
                        path: default_objects.to_string_lossy().into_owned(),
                    })
                    .expect("default storage"),
                    bucket: "default-bucket".into(),
                    endpoint: None,
                    vendor: None,
                    root: Some("base".into()),
                }),
                http: None,
                by_tag: std::collections::HashMap::from([(
                    "agent".to_string(),
                    S3TargetSpec {
                        storage: Storage::new(&StorageConfig::Local {
                            path: agent_objects.to_string_lossy().into_owned(),
                        })
                        .expect("agent storage"),
                        bucket: "isc_sr".into(),
                        endpoint: None,
                        vendor: None,
                        root: Some("agent".into()),
                    },
                )]),
            },
        );
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let mut manager = RecordingUploadManager {
            runtime,
            receiver,
        };
        let uploader = crate::utils::spawn(async move { manager.serve().await });
        sender
            .send(PendingRecordingUpload {
                path: agent_wav.clone(),
                source_tag: "agent".to_string(),
            })
            .await
            .expect("queue agent");
        sender
            .send(PendingRecordingUpload {
                path: full_wav.clone(),
                source_tag: "ivr".to_string(),
            })
            .await
            .expect("queue unmapped tag → default");
        drop(sender);
        uploader.await.expect("uploader task");

        // agent tag → per-source bucket + prefix; unmapped tag → default.
        assert_eq!(
            tokio::fs::read(agent_objects.join("agent/agent.wav"))
                .await
                .expect("agent object"),
            b"agent"
        );
        assert_eq!(
            tokio::fs::read(default_objects.join("base/full.wav"))
                .await
                .expect("default object"),
            b"full"
        );
        assert!(!agent_wav.exists() && !full_wav.exists());
    }

    #[test]
    fn upload_failed_marker_carries_source_and_reads_legacy_markers() {
        // Legacy marker (pre per-source routing): no `source` field → None →
        // retries into the default bucket.
        let legacy: UploadFailedMarker = serde_json::from_str(
            r#"{
                "time": "2026-01-01T00:00:00Z",
                "address": "s3://old-bucket/k",
                "duration_ms": 10,
                "error": "boom",
                "attempts": 2,
                "call_id": "c1"
            }"#,
        )
        .expect("legacy marker parses");
        assert_eq!(legacy.source, None);
        assert_eq!(legacy.attempts, 2);

        // New markers round-trip the source tag.
        let marker = UploadFailedMarker {
            time: "2026-01-01T00:00:00Z".into(),
            address: "s3://isc_sr/k".into(),
            duration_ms: 5,
            error: "boom".into(),
            attempts: 1,
            call_id: Some("c1".into()),
            source: Some("agent".into()),
        };
        let json = serde_json::to_value(&marker).expect("serialize");
        assert_eq!(json["source"], "agent");
        let parsed: UploadFailedMarker = serde_json::from_value(json).expect("round-trip");
        assert_eq!(parsed.source.as_deref(), Some("agent"));
    }
}
