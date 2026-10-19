//! Release / version management.
//!
//! Metadata lives in a single `release.toml` document managed through the
//! generated-config store ([`GeneratedConfigStore`], category `release`):
//! a plain file under the config directory, or a row in the `config_entry`
//! table when `proxy.use_db_config()` is enabled — no dedicated DB table.
//!
//! Artifacts are stored through the `[release].storage` backend (local
//! directory or S3-compatible bucket). S3 objects are recorded as
//! `s3://{bucket}/{key}`; download endpoints rewrite them into presigned
//! URLs on demand (default 24h, see `[release].signed_url_expiry_secs`).
//!
//! Public (unauthenticated) endpoints, consumed by desktop update clients
//! (e.g. restsend-pc `update.rs`):
//! - `GET {api_prefix}/release`                      — bare `UpdateMeta` JSON of the latest published release
//! - `GET {api_prefix}/release/list`                 — published releases
//! - `GET {api_prefix}/release/{version}/download`   — stream local file / 302 to presigned URL
//!
//! Mutations require authentication (session cookie or Bearer token).

use crate::config::ReleaseConfig;
use crate::config_store::GeneratedConfigStore;
use crate::console::ConsoleState;
use crate::console::config_helpers::{json_error, ok_json};
use crate::console::middleware::AuthRequired;
use crate::storage::{Storage, StorageConfig as ArtifactStorage};
use axum::{
    Json, Router,
    body::Body,
    extract::{Multipart, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

/// Config-store category holding `release.toml`.
pub const RELEASE_CATEGORY: &str = "release";
/// Config-store entry name for the release manifest.
pub const RELEASE_ENTRY: &str = "release.toml";

/// Upload body limit for release artifacts (2 GiB). Axum's default 2 MiB
/// limit is far too small for application packages.
const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Serializes read-modify-write cycles on `release.toml`.
static RELEASE_FILE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One `[[releases]]` entry of `release.toml`. The optional policy fields are
/// passed through verbatim to update clients (restsend-pc `UpdateMeta`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReleaseDoc {
    pub version: String,
    pub file_name: String,
    /// Local key relative to the artifact storage root, `s3://{bucket}/{key}`
    /// for S3 objects, or an external `http(s)://` URL.
    pub storage_path: String,
    #[serde(default)]
    pub file_size: u64,
    #[serde(default)]
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_notes: Option<String>,
    #[serde(default)]
    pub published: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_supported: Option<String>,
    /// Non-forced download window `HH:MM-HH:MM`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_kbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_rate_limit_kbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Root document of `release.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ReleaseFile {
    releases: Vec<ReleaseDoc>,
}

// ── Routes ──────────────────────────────────────────────────────────────

/// Console page routes (nested under base_path).
pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new().route("/releases", get(releases_page))
}

/// API routes (nested under api_prefix). GETs under `/release` are exempt
/// from `api_auth_middleware` (see `src/api/mod.rs`) so update clients can
/// check/download without credentials; mutations stay authenticated.
pub fn api_urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/release", get(check_update_handler).post(upload_handler))
        .route("/release/list", get(list_releases_handler))
        .route("/release/{version}/download", get(download_handler))
        .route(
            "/release/{version}",
            axum::routing::put(update_release_handler).delete(delete_release_handler),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
}

// ── Public handlers ─────────────────────────────────────────────────────

/// GET {api_prefix}/release — bare `UpdateMeta`-shaped JSON for the latest
/// published release. 404 when nothing is published (clients treat that as
/// "no update service" and stay silent). Query params carried by clients
/// (`version`, `build_time`, `git_commit`) are logged for observability.
pub async fn check_update_handler(
    State(state): State<Arc<ConsoleState>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Some(current) = params.get("version") {
        tracing::debug!(
            current = %current,
            build_time = params.get("build_time").map(String::as_str).unwrap_or(""),
            git_commit = params.get("git_commit").map(String::as_str).unwrap_or(""),
            "release check request"
        );
    }
    let docs = load_docs(&config_store(&state)).await;
    let Some(doc) = pick_latest_published(&docs) else {
        return json_error(StatusCode::NOT_FOUND, "no release available");
    };
    match build_update_meta(&state, doc, &headers).await {
        Ok(meta) => Json(meta).into_response(),
        Err(e) => {
            tracing::warn!(version = %doc.version, error = %e, "release check failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}

/// GET {api_prefix}/release/list — published releases, newest version first.
pub async fn list_releases_handler(State(state): State<Arc<ConsoleState>>) -> Response {
    let mut docs: Vec<ReleaseDoc> = load_docs(&config_store(&state)).await;
    docs.retain(|d| d.published);
    sort_by_version_desc(&mut docs);
    Json(json!({ "releases": docs })).into_response()
}

/// GET {api_prefix}/release/{version}/download — stream a local artifact or
/// 302-redirect to a presigned/external URL.
pub async fn download_handler(
    State(state): State<Arc<ConsoleState>>,
    AxumPath(version): AxumPath<String>,
) -> Response {
    let docs = load_docs(&config_store(&state)).await;
    let Some(doc) = docs
        .iter()
        .find(|d| d.published && d.version == version)
        .cloned()
    else {
        return json_error(StatusCode::NOT_FOUND, "release not found");
    };
    match serve_artifact(&state, &doc).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(version = %version, error = %e, "release download failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}

// ── Authenticated handlers ──────────────────────────────────────────────

/// POST {api_prefix}/release — multipart upload (`file` + `version` +
/// optional `release_notes` / `published` / policy fields).
pub async fn upload_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
    mut multipart: Multipart,
) -> Response {
    let mut version: Option<String> = None;
    let mut file_name: Option<String> = None;
    let mut data: Option<bytes::Bytes> = None;
    let mut notes: Option<String> = None;
    let mut published = true;
    let mut doc = ReleaseDoc::default();

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name().unwrap_or_default() {
            "version" => {
                version = field.text().await.ok().map(|v| v.trim().to_string());
            }
            "release_notes" => {
                notes = field.text().await.ok().map(|v| v.trim().to_string());
            }
            "published" => {
                published = !matches!(field.text().await.as_deref(), Ok("false" | "0" | ""));
            }
            "build_time" => doc.build_time = field.text().await.ok().and_then(trim_non_empty),
            "git_commit" => doc.git_commit = field.text().await.ok().and_then(trim_non_empty),
            "min_supported" => doc.min_supported = field.text().await.ok().and_then(trim_non_empty),
            "window" => doc.window = field.text().await.ok().and_then(trim_non_empty),
            "deadline" => doc.deadline = field.text().await.ok().and_then(trim_non_empty),
            "force" => doc.force = field.text().await.ok().map(|v| v == "true" || v == "1"),
            "rate_limit_kbps" => doc.rate_limit_kbps = field.text().await.ok().and_then(parse_u32),
            "force_rate_limit_kbps" => {
                doc.force_rate_limit_kbps = field.text().await.ok().and_then(parse_u32)
            }
            "max_attempts" => doc.max_attempts = field.text().await.ok().and_then(parse_u32),
            "file" => {
                file_name = field.file_name().map(str::to_string);
                match field.bytes().await {
                    Ok(b) if !b.is_empty() => data = Some(b),
                    _ => {
                        return json_error(StatusCode::BAD_REQUEST, "empty upload");
                    }
                }
            }
            _ => {}
        }
    }

    let Some(data) = data else {
        return json_error(StatusCode::BAD_REQUEST, "missing `file` field");
    };
    let Some(version) = version.filter(|v| !v.is_empty()) else {
        return json_error(StatusCode::BAD_REQUEST, "`version` is required");
    };
    let Some(file_name) = file_name.filter(|n| !n.trim().is_empty()) else {
        return json_error(StatusCode::BAD_REQUEST, "upload is missing a file name");
    };

    match create_release(
        &state,
        CreateReleaseParams {
            version,
            file_name,
            data,
            release_notes: notes.filter(|n| !n.is_empty()),
            published,
            extra: doc,
        },
    )
    .await
    {
        Ok(saved) => ok_json(json!({ "release": saved })),
        Err(e) => {
            tracing::warn!(error = %e, "release upload failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}

/// PUT {api_prefix}/release/{version} — patch metadata (notes, published,
/// force / window / limits…).
pub async fn update_release_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
    AxumPath(version): AxumPath<String>,
    Json(body): Json<UpdateReleaseBody>,
) -> Response {
    let _guard = RELEASE_FILE_LOCK.lock().await;
    let store = config_store(&state);
    let mut docs = load_docs(&store).await;
    let Some(doc) = docs.iter_mut().find(|d| d.version == version) else {
        return json_error(StatusCode::NOT_FOUND, "release not found");
    };
    if body.release_notes.is_some() {
        doc.release_notes = body.release_notes.filter(|n| !n.is_empty());
    }
    if let Some(v) = body.published {
        doc.published = v;
    }
    if body.force.is_some() {
        doc.force = body.force;
    }
    if body.min_supported.is_some() {
        doc.min_supported = body.min_supported.filter(|n| !n.is_empty());
    }
    if body.window.is_some() {
        doc.window = body.window.filter(|n| !n.is_empty());
    }
    if body.deadline.is_some() {
        doc.deadline = body.deadline.filter(|n| !n.is_empty());
    }
    if body.rate_limit_kbps.is_some() {
        doc.rate_limit_kbps = body.rate_limit_kbps;
    }
    if body.force_rate_limit_kbps.is_some() {
        doc.force_rate_limit_kbps = body.force_rate_limit_kbps;
    }
    if body.max_attempts.is_some() {
        doc.max_attempts = body.max_attempts;
    }
    let saved = doc.clone();
    if let Err(e) = save_docs(&store, &docs).await {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    ok_json(json!({ "release": saved }))
}

/// DELETE {api_prefix}/release/{version} — removes the artifact and the
/// manifest entry.
pub async fn delete_release_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
    AxumPath(version): AxumPath<String>,
) -> Response {
    let _guard = RELEASE_FILE_LOCK.lock().await;
    let store = config_store(&state);
    let mut docs = load_docs(&store).await;
    let Some(pos) = docs.iter().position(|d| d.version == version) else {
        return json_error(StatusCode::NOT_FOUND, "release not found");
    };
    let doc = docs.remove(pos);
    match release_storage(&state) {
        Ok(storage) => {
            if let Some(key) = artifact_object_key(&storage, &doc.storage_path) {
                if let Err(e) = storage.delete(&key).await {
                    tracing::warn!(version = %version, error = %e, "failed to delete release artifact");
                }
            }
        }
        Err(e) => tracing::warn!(version = %version, error = %e, "release storage unavailable"),
    }
    if let Err(e) = save_docs(&store, &docs).await {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    ok_json(json!({ "deleted": version }))
}

/// GET {base_path}/releases — console management page.
pub async fn releases_page(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let mut docs = load_docs(&config_store(&state)).await;
    sort_by_version_desc(&mut docs);
    let current_user = state.build_current_user_ctx(&user).await;
    state.render_with_headers(
        "console/releases.html",
        json!({
            "nav_active": "releases",
            "releases": docs,
            "current_user": current_user,
        }),
        &headers,
    )
}

// ── Request bodies ──────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct UpdateReleaseBody {
    pub release_notes: Option<String>,
    pub published: Option<bool>,
    pub force: Option<bool>,
    pub min_supported: Option<String>,
    pub window: Option<String>,
    pub rate_limit_kbps: Option<u32>,
    pub force_rate_limit_kbps: Option<u32>,
    pub max_attempts: Option<u32>,
    pub deadline: Option<String>,
}

/// Parameters for [`create_release`] (kept non-multipart so tests and future
/// callers can upload without building multipart bodies).
pub struct CreateReleaseParams {
    pub version: String,
    pub file_name: String,
    pub data: bytes::Bytes,
    pub release_notes: Option<String>,
    pub published: bool,
    /// Policy passthrough fields collected from the upload form.
    pub extra: ReleaseDoc,
}

/// Store the artifact and upsert the manifest entry (same version replaces).
pub async fn create_release(
    state: &Arc<ConsoleState>,
    params: CreateReleaseParams,
) -> Result<ReleaseDoc, String> {
    let version = sanitize_version(&params.version);
    if version.is_empty() {
        return Err("invalid version".into());
    }
    let safe_name = sanitize_file_name(&params.file_name);
    if safe_name.is_empty() {
        return Err("invalid file name".into());
    }
    let key = format!("{}/{}", version, safe_name);

    let storage_cfg = effective_release_storage(state);
    let storage = Storage::new(&storage_cfg).map_err(|e| format!("storage unavailable: {e}"))?;
    storage
        .write(&key, params.data.clone())
        .await
        .map_err(|e| format!("failed to store artifact: {e}"))?;
    let storage_path = artifact_storage_path(&storage_cfg, &key);

    let mut hasher = Sha256::new();
    hasher.update(&params.data);
    let sha256: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let _guard = RELEASE_FILE_LOCK.lock().await;
    let store = config_store(state);
    let mut docs = load_docs(&store).await;
    // Re-upload of the same version: drop the previous artifact if it now
    // points somewhere else, then replace the entry.
    if let Some(pos) = docs.iter().position(|d| d.version == version) {
        let old = docs.remove(pos);
        if old.storage_path != storage_path {
            if let Some(old_key) = artifact_object_key(&storage, &old.storage_path) {
                if let Err(e) = storage.delete(&old_key).await {
                    tracing::warn!(version = %version, error = %e, "failed to clean previous artifact");
                }
            }
        }
    }

    let doc = ReleaseDoc {
        version,
        file_name: safe_name,
        storage_path,
        file_size: params.data.len() as u64,
        sha256,
        release_notes: params.release_notes,
        published: params.published,
        build_time: params.extra.build_time,
        git_commit: params.extra.git_commit,
        force: params.extra.force,
        min_supported: params.extra.min_supported,
        window: params.extra.window,
        rate_limit_kbps: params.extra.rate_limit_kbps,
        force_rate_limit_kbps: params.extra.force_rate_limit_kbps,
        max_attempts: params.extra.max_attempts,
        deadline: params.extra.deadline,
        created_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    };
    docs.push(doc.clone());
    save_docs(&store, &docs).await?;
    Ok(doc)
}

// ── Internals ───────────────────────────────────────────────────────────

/// Generated-config store for `release.toml`: file-backed or DB-backed
/// (`config_entry`) following `proxy.use_db_config()`. Falls back to the
/// shared console database when the console runs without an app context
/// (e.g. tests).
fn config_store(state: &ConsoleState) -> GeneratedConfigStore {
    match state.app_state() {
        Some(app) => GeneratedConfigStore::from_config(&app.config(), app.db()),
        None => GeneratedConfigStore::db(state.db().clone()),
    }
}

fn release_config(state: &ConsoleState) -> ReleaseConfig {
    state.config().release.clone().unwrap_or_default()
}

fn effective_release_storage(state: &ConsoleState) -> ArtifactStorage {
    release_config(state).effective_storage()
}

async fn load_docs(store: &GeneratedConfigStore) -> Vec<ReleaseDoc> {
    match store.read(RELEASE_CATEGORY, RELEASE_ENTRY).await {
        Ok(Some(content)) => toml::from_str::<ReleaseFile>(&content)
            .map(|f| f.releases)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to parse release.toml, treating as empty");
                Vec::new()
            }),
        _ => Vec::new(),
    }
}

async fn save_docs(store: &GeneratedConfigStore, docs: &[ReleaseDoc]) -> Result<(), String> {
    let content = toml::to_string_pretty(&ReleaseFile {
        releases: docs.to_vec(),
    })
    .map_err(|e| format!("failed to encode release.toml: {e}"))?;
    store
        .write(RELEASE_CATEGORY, RELEASE_ENTRY, &content)
        .await
        .map_err(|e| format!("failed to persist release.toml: {e}"))
}

/// Object key for a stored `storage_path`, resolvable against `storage`.
/// Returns `None` for external URLs or paths belonging to another bucket.
/// Plain relative keys (local artifacts) are returned as-is.
fn artifact_object_key(storage: &Storage, storage_path: &str) -> Option<String> {
    let raw = storage_path.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return storage.object_key_from_url(raw);
    }
    if let Some(rest) = raw.strip_prefix("s3://") {
        // `s3://{bucket}/{key}` — drop the bucket segment.
        let key = rest.split_once('/').map(|(_, k)| k).unwrap_or("");
        let key = key.trim_start_matches('/');
        return (!key.is_empty()).then(|| key.to_string());
    }
    let key = raw.trim_start_matches('/');
    (!key.is_empty()).then(|| key.to_string())
}

/// Canonical `storage_path` for a freshly uploaded key: `s3://{bucket}/{key}`
/// for S3 buckets, plain relative key for local directories.
fn artifact_storage_path(storage_cfg: &ArtifactStorage, key: &str) -> String {
    match storage_cfg {
        ArtifactStorage::S3 { bucket, .. } => {
            format!(
                "s3://{}/{}",
                bucket.trim_matches('/'),
                key.trim_start_matches('/')
            )
        }
        _ => key.trim_start_matches('/').to_string(),
    }
}

/// The latest published release by version order.
fn pick_latest_published(docs: &[ReleaseDoc]) -> Option<&ReleaseDoc> {
    docs.iter()
        .filter(|d| d.published && !d.version.trim().is_empty())
        .max_by(|a, b| version_cmp(&a.version, &b.version))
}

fn sort_by_version_desc(docs: &mut Vec<ReleaseDoc>) {
    docs.sort_by(|a, b| version_cmp(&b.version, &a.version));
}

/// UpdateMeta-compatible JSON (field names match restsend-pc `update::UpdateMeta`).
async fn build_update_meta(
    state: &Arc<ConsoleState>,
    doc: &ReleaseDoc,
    headers: &HeaderMap,
) -> Result<serde_json::Value, String> {
    let url = download_url_for(state, doc, headers).await?;
    Ok(json!({
        "version": doc.version,
        "url": url,
        "sha256": doc.sha256,
        "notes": doc.release_notes,
        "build_time": doc.build_time,
        "git_commit": doc.git_commit,
        "force": doc.force.unwrap_or(false),
        "min_supported": doc.min_supported,
        "window": doc.window,
        "rate_limit_kbps": doc.rate_limit_kbps,
        "force_rate_limit_kbps": doc.force_rate_limit_kbps,
        "max_attempts": doc.max_attempts,
        "deadline": doc.deadline,
        "size_bytes": (doc.file_size > 0).then_some(doc.file_size),
    }))
}

/// Absolute download URL for a release: presigned/external URL for S3 and
/// external artifacts, `{proto}://{host}{api_prefix}/release/{v}/download`
/// for local files.
async fn download_url_for(
    state: &Arc<ConsoleState>,
    doc: &ReleaseDoc,
    headers: &HeaderMap,
) -> Result<String, String> {
    let raw = doc.storage_path.trim();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Ok(raw.to_string());
    }
    if raw.starts_with("s3://") {
        let storage = release_storage(state)?;
        return presign_artifact(&storage, raw, state)
            .await
            .map(|opt| opt.unwrap_or_else(|| local_download_url(state, &doc.version, headers)));
    }
    Ok(local_download_url(state, &doc.version, headers))
}

fn local_download_url(state: &ConsoleState, version: &str, headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!(
        "{}://{}{}/release/{}/download",
        proto,
        host,
        state.api_prefix(),
        version
    )
}

/// Presign an `s3://…` path; `Ok(None)` when the bucket is anonymous/public
/// and an endpoint is configured (fall back to the plain path-style URL).
async fn presign_artifact(
    storage: &Storage,
    storage_path: &str,
    state: &Arc<ConsoleState>,
) -> Result<Option<String>, String> {
    let Some(key) = artifact_object_key(storage, storage_path) else {
        return Err("artifact does not belong to the configured bucket".into());
    };
    let expiry =
        std::time::Duration::from_secs(release_config(state).effective_signed_url_expiry_secs());
    if storage.supports_presign() {
        return storage
            .presign_read_url(&key, expiry)
            .await
            .map(Some)
            .map_err(|e| format!("failed to presign artifact: {e}"));
    }
    // Anonymous bucket: build the plain path-style URL when an endpoint is
    // configured; otherwise we cannot produce a fetchable URL.
    if let ArtifactStorage::S3 {
        bucket,
        endpoint: Some(endpoint),
        ..
    } = effective_release_storage(state)
    {
        let bucket = bucket.trim_matches('/');
        let base = endpoint.trim_end_matches('/');
        if !bucket.is_empty() && !base.is_empty() {
            return Ok(Some(format!("{base}/{bucket}/{key}")));
        }
    }
    Ok(None)
}

/// Serve the artifact bytes / redirect for a single release.
async fn serve_artifact(state: &Arc<ConsoleState>, doc: &ReleaseDoc) -> Result<Response, String> {
    let raw = doc.storage_path.trim();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Ok((StatusCode::FOUND, [(header::LOCATION, raw.to_string())]).into_response());
    }
    let storage = release_storage(state)?;
    if raw.starts_with("s3://") {
        return match presign_artifact(&storage, raw, state).await? {
            Some(url) => Ok((StatusCode::FOUND, [(header::LOCATION, url)]).into_response()),
            None => Err("artifact bucket is not presignable and has no public endpoint".into()),
        };
    }
    // Local artifact: stream from disk when possible.
    if let Some(path) = storage.local_path(raw) {
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| format!("artifact missing: {e}"))?;
        let meta = file
            .metadata()
            .await
            .map_err(|e| format!("artifact metadata unavailable: {e}"))?;
        let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);
        let body = Body::from_stream(stream);
        let mut resp = Response::new(body);
        *resp.status_mut() = StatusCode::OK;
        resp.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from(meta.len()),
        );
        resp.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            header::HeaderValue::from_str(&format!(
                "attachment; filename=\"{}\"",
                doc.file_name.replace('"', "")
            ))
            .map_err(|_| "invalid file name".to_string())?,
        );
        return Ok(resp);
    }
    // Non-local backend without presign: read through the store.
    let bytes = storage
        .read(raw)
        .await
        .map_err(|e| format!("failed to read artifact: {e}"))?;
    Ok((
        StatusCode::OK,
        [(
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}\"",
                doc.file_name.replace('"', "")
            ),
        )],
        bytes.to_vec(),
    )
        .into_response())
}

fn release_storage(state: &Arc<ConsoleState>) -> Result<Storage, String> {
    Storage::new(&effective_release_storage(state)).map_err(|e| format!("storage unavailable: {e}"))
}

// ── Small helpers ───────────────────────────────────────────────────────

fn trim_non_empty(v: String) -> Option<String> {
    let v = v.trim().to_string();
    (!v.is_empty()).then_some(v)
}

fn parse_u32(v: String) -> Option<u32> {
    v.trim().parse().ok()
}

/// Keep versions filesystem-safe: letters, digits, `.`, `-`, `_`, `+`.
/// Consecutive dots collapse to one so the version can never introduce a
/// `..` traversal segment into the artifact key.
fn sanitize_version(raw: &str) -> String {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '+') {
            out.push(ch);
        } else if ch == '.' {
            if !out.ends_with('.') {
                out.push('.');
            }
        } else {
            out.push('-');
        }
    }
    out
}

/// Basename + keep a conservative character set for uploaded file names
/// (path traversal segments are dropped entirely).
fn sanitize_file_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let mut out = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+' | '@' | '(' | ')') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

/// Numeric version ordering compatible with restsend-pc's `version_is_newer`:
/// split into numeric components (`0.1.9` < `0.1.10` < `0.2.0`), ignoring a
/// leading `v`. Versions whose numeric components match compare Equal.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let parse = |ver: &str| -> Vec<u64> {
        ver.trim()
            .trim_start_matches(['v', 'V'])
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<u64>().ok())
            .collect()
    };
    let (va, vb) = (parse(a), parse(b));
    let len = va.len().max(vb.len());
    for i in 0..len {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiTokenConfig, Config, ConsoleConfig};
    use tower::ServiceExt;

    const TEST_TOKEN: &str = "release-test-token";

    // ── pure-function tests ─────────────────────────────────────────────

    #[test]
    fn toml_roundtrip_preserves_fields() {
        let doc = ReleaseDoc {
            version: "0.1.13".into(),
            file_name: "restsend-pc.zip".into(),
            storage_path: "s3://bucket/releases/0.1.13/restsend-pc.zip".into(),
            file_size: 1234,
            sha256: "ab12".into(),
            release_notes: Some("fixes".into()),
            published: true,
            build_time: Some("2026-10-19".into()),
            force: Some(false),
            min_supported: Some("0.1.0".into()),
            window: Some("12:10-13:20".into()),
            rate_limit_kbps: Some(2048),
            ..Default::default()
        };
        let file = ReleaseFile {
            releases: vec![doc],
        };
        let toml_s = toml::to_string_pretty(&file).unwrap();
        assert!(toml_s.contains("[[releases]]"));
        let parsed: ReleaseFile = toml::from_str(&toml_s).unwrap();
        assert_eq!(parsed.releases.len(), 1);
        let back = &parsed.releases[0];
        assert_eq!(back.version, "0.1.13");
        assert_eq!(
            back.storage_path,
            "s3://bucket/releases/0.1.13/restsend-pc.zip"
        );
        assert_eq!(back.sha256, "ab12");
        assert!(back.published);
        assert_eq!(back.window.as_deref(), Some("12:10-13:20"));
        assert_eq!(back.rate_limit_kbps, Some(2048));
    }

    #[test]
    fn version_cmp_orders_semver() {
        use std::cmp::Ordering::*;
        assert_eq!(version_cmp("0.1.9", "0.1.10"), Less);
        assert_eq!(version_cmp("0.1.10", "0.2.0"), Less);
        assert_eq!(version_cmp("v1.0.0", "1.0.0"), Equal);
        assert_eq!(version_cmp("1.0", "1.0.0"), Equal);
        assert_eq!(version_cmp("2.0.0", "10.0.0"), Less);
    }

    #[test]
    fn pick_latest_published_ignores_drafts() {
        let docs = vec![
            ReleaseDoc {
                version: "0.1.9".into(),
                published: true,
                ..Default::default()
            },
            ReleaseDoc {
                version: "0.9.0".into(),
                published: false,
                ..Default::default()
            },
            ReleaseDoc {
                version: "0.1.13".into(),
                published: true,
                ..Default::default()
            },
        ];
        assert_eq!(pick_latest_published(&docs).unwrap().version, "0.1.13");
        let drafts: Vec<ReleaseDoc> = docs.iter().filter(|d| !d.published).cloned().collect();
        assert!(pick_latest_published(&drafts).is_none());
    }

    #[test]
    fn sanitize_rejects_traversal_and_weird_chars() {
        // Traversal input collapses to a harmless single-dot path.
        assert_eq!(sanitize_version("../../etc"), ".-.-etc");
        assert_eq!(sanitize_version("v0.1..13"), "0.1.13");
        assert_eq!(sanitize_version("  0.1.13 "), "0.1.13");
        // File names reduce to their basename with a safe charset.
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("a b\\c.zip"), "c.zip");
        assert_eq!(sanitize_file_name("app-0.1.13.zip"), "app-0.1.13.zip");
    }

    #[test]
    fn artifact_path_roundtrip_between_storage_kinds() {
        let s3_cfg = ArtifactStorage::S3 {
            vendor: crate::storage::S3Vendor::Minio,
            bucket: "rels".into(),
            region: "us-east-1".into(),
            access_key: Some("ak".into()),
            secret_key: Some("sk".into()),
            endpoint: Some("http://127.0.0.1:9000".into()),
            prefix: None,
        };
        let path = artifact_storage_path(&s3_cfg, "0.1.13/app.zip");
        assert_eq!(path, "s3://rels/0.1.13/app.zip");
        let storage = Storage::new(&s3_cfg).unwrap();
        assert_eq!(
            artifact_object_key(&storage, &path).as_deref(),
            Some("0.1.13/app.zip")
        );

        let local_cfg = ArtifactStorage::Local {
            path: "storage/releases".into(),
        };
        assert_eq!(
            artifact_storage_path(&local_cfg, "0.1.13/app.zip"),
            "0.1.13/app.zip"
        );
        let storage = Storage::new(&local_cfg).unwrap();
        assert_eq!(
            artifact_object_key(&storage, "0.1.13/app.zip").as_deref(),
            Some("0.1.13/app.zip")
        );
    }

    // ── handler / client-contract tests ─────────────────────────────────

    async fn setup_release_stack() -> (Arc<ConsoleState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let mut console_cfg = ConsoleConfig::default();
        console_cfg.api_tokens = vec![ApiTokenConfig {
            token: TEST_TOKEN.into(),
            scopes: vec![],
        }];
        let config = Config {
            database_url: "sqlite::memory:".into(),
            // Keep the release manifest in this app's own in-memory DB
            // (`config_entry`) so tests never share the on-disk config dir.
            proxy: crate::config::ProxyConfig {
                generated_db: true,
                ..Default::default()
            },
            console: Some(console_cfg),
            release: Some(ReleaseConfig {
                storage: Some(ArtifactStorage::Local {
                    path: tmp.path().to_string_lossy().to_string(),
                }),
                signed_url_expiry_secs: None,
            }),
            ..Default::default()
        };
        let app = crate::app::AppStateBuilder::new()
            .with_config(config)
            .with_skip_sip_bind()
            .build()
            .await
            .unwrap();
        let state = crate::console::handlers::test_helpers::setup_state().await;
        state.set_app_state(Some(Arc::downgrade(&app)));
        (state, tmp)
    }

    fn doc(version: &str, published: bool) -> ReleaseDoc {
        ReleaseDoc {
            version: version.into(),
            published,
            ..Default::default()
        }
    }

    async fn seed(state: &Arc<ConsoleState>, docs: Vec<ReleaseDoc>) {
        let store = config_store(state);
        save_docs(&store, &docs).await.unwrap();
    }

    async fn get(state: &Arc<ConsoleState>, uri: &str) -> Response {
        crate::api::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .into_response()
    }

    async fn json_body(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn multipart_body(
        boundary: &str,
        fields: &[(&str, &str)],
        file: Option<(&str, &str, &[u8])>,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                    .as_bytes(),
            );
        }
        if let Some((name, filename, data)) = file {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }

    /// restsend-pc update.rs contract: GET /api/release (no auth) must return
    /// a bare UpdateMeta — `version` / `url` / `sha256` present, no envelope —
    /// for the highest *published* version only.
    #[tokio::test]
    async fn check_update_returns_bare_update_meta_for_latest_published() {
        let (state, _tmp) = setup_release_stack().await;

        // Nothing published yet → 404 (client treats as "not configured").
        let resp = get(&state, "/api/release").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        seed(
            &state,
            vec![
                doc("0.1.9", true),
                doc("0.2.0", false), // draft must be invisible
                ReleaseDoc {
                    version: "0.1.13".into(),
                    published: true,
                    file_name: "app.zip".into(),
                    storage_path: "0.1.13/app.zip".into(),
                    sha256: "cafe".into(),
                    file_size: 42,
                    release_notes: Some("bugfix release".into()),
                    min_supported: Some("0.1.0".into()),
                    ..Default::default()
                },
            ],
        )
        .await;

        let resp = get(
            &state,
            "/api/release?version=0.1.9&build_time=x&git_commit=y",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let meta = json_body(resp).await;
        // Bare UpdateMeta: no {status, data} envelope the client can't read.
        assert!(meta.get("status").is_none());
        assert_eq!(meta["version"], "0.1.13");
        assert_eq!(meta["sha256"], "cafe");
        assert_eq!(meta["size_bytes"], 42);
        assert_eq!(meta["notes"], "bugfix release");
        assert_eq!(meta["min_supported"], "0.1.0");
        assert_eq!(meta["force"], false);
        let url = meta["url"].as_str().unwrap();
        assert!(
            url.ends_with("/api/release/0.1.13/download") && url.starts_with("http"),
            "url must be an absolute download link, got {url}"
        );

        // Client's version_is_newer("0.1.13", "0.1.9") must see an update —
        // mirror the comparison server-side to pin ordering.
        assert_eq!(version_cmp("0.1.13", "0.1.9"), std::cmp::Ordering::Greater);

        // list: published only.
        let resp = get(&state, "/api/release/list").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        let versions: Vec<&str> = list["releases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["version"].as_str().unwrap())
            .collect();
        assert_eq!(versions, vec!["0.1.13", "0.1.9"]);
    }

    /// Public reads pass without credentials; mutations stay behind auth.
    #[tokio::test]
    async fn public_reads_open_but_mutations_require_auth() {
        let (state, _tmp) = setup_release_stack().await;

        let resp = get(&state, "/api/release/list").await;
        assert_eq!(resp.status(), StatusCode::OK, "list must be public");

        for (method, uri) in [
            ("POST", "/api/release"),
            ("DELETE", "/api/release/0.1.13"),
            ("PUT", "/api/release/0.1.13"),
        ] {
            let resp = crate::api::router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} must require auth"
            );
        }
    }

    /// Full client-visible flow: Bearer-token multipart upload → disk
    /// artifact + manifest entry → public check/download. Re-upload of the
    /// same version replaces the entry and cleans the previous file.
    #[tokio::test]
    async fn upload_via_bearer_then_public_download_and_replace() {
        let (state, tmp) = setup_release_stack().await;

        let payload: &[u8] = b"PK\x03\x04 fake package payload for 0.1.13";
        let boundary = "relboundary42";
        let body = multipart_body(
            boundary,
            &[
                ("version", "0.1.13"),
                ("release_notes", "first"),
                ("published", "true"),
            ],
            Some(("file", "app-0.1.13.zip", payload)),
        );
        let resp = crate::api::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/release")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .header("authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "Bearer upload must succeed");
        let created = json_body(resp.into_response()).await;
        assert_eq!(created["release"]["version"], "0.1.13");
        let sha = created["release"]["sha256"].as_str().unwrap().to_string();
        assert_eq!(sha.len(), 64);

        // Artifact landed in the configured local storage root.
        let artifact = tmp.path().join("0.1.13").join("app-0.1.13.zip");
        assert!(artifact.exists(), "artifact must exist at {:?}", artifact);
        let stored = tokio::fs::read(&artifact).await.unwrap();
        assert_eq!(stored.as_slice(), payload);

        // Public download streams the exact bytes.
        let resp = get(&state, "/api/release/0.1.13/download").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let downloaded = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(&downloaded[..], payload);

        // Check endpoint reflects the upload.
        let resp = get(&state, "/api/release").await;
        let meta = json_body(resp).await;
        assert_eq!(meta["version"], "0.1.13");
        assert_eq!(meta["sha256"], sha);
        assert_eq!(meta["size_bytes"], payload.len() as u64);

        // Re-upload the same version with a new file: manifest replaced,
        // old artifact removed.
        let payload2: &[u8] = b"PK\x03\x04 replacement payload";
        let body = multipart_body(
            "relboundary42",
            &[("version", "0.1.13"), ("published", "true")],
            Some(("file", "app-fixed.zip", payload2)),
        );
        let resp = crate::api::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/release")
                    .header(
                        "content-type",
                        "multipart/form-data; boundary=relboundary42",
                    )
                    .header("authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!artifact.exists(), "previous artifact must be cleaned up");

        let store = config_store(&state);
        let docs = load_docs(&store).await;
        assert_eq!(docs.len(), 1, "same version must replace, not duplicate");
        assert_eq!(docs[0].file_name, "app-fixed.zip");

        let resp = get(&state, "/api/release/0.1.13/download").await;
        let downloaded = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(&downloaded[..], payload2);
    }

    /// Drafts are invisible to every public endpoint; publishing via PUT
    /// makes them visible.
    #[tokio::test]
    async fn draft_hidden_until_published_via_put() {
        let (state, _tmp) = setup_release_stack().await;

        let boundary = "relboundary42";
        let body = multipart_body(
            boundary,
            &[("version", "0.2.0"), ("published", "false")],
            Some(("file", "beta.zip", b"beta payload".as_slice())),
        );
        let resp = crate::api::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/release")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .header("authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        assert_eq!(
            get(&state, "/api/release").await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(&state, "/api/release/0.2.0/download").await.status(),
            StatusCode::NOT_FOUND
        );
        let list = json_body(get(&state, "/api/release/list").await).await;
        assert_eq!(list["releases"].as_array().unwrap().len(), 0);

        // Publish it.
        let resp = crate::api::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/release/0.2.0")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::from(
                        "{\"published\":true,\"release_notes\":\"ga now\"}",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = get(&state, "/api/release").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let meta = json_body(resp).await;
        assert_eq!(meta["version"], "0.2.0");
        assert_eq!(meta["notes"], "ga now");

        // Delete removes artifact + entry.
        let resp = crate::api::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/release/0.2.0")
                    .header("authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            get(&state, "/api/release").await.status(),
            StatusCode::NOT_FOUND
        );
        let docs = load_docs(&config_store(&state)).await;
        assert!(docs.is_empty(), "manifest entry must be gone");
    }
}
