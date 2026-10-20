//! Release / version management.
//!
//! Metadata lives in a single `release.toml` document managed through the
//! generated-config store ([`GeneratedConfigStore`], category `release`):
//! a plain file under the config directory, or a row in the `config_entry`
//! table when `proxy.use_db_config()` is enabled — no dedicated DB table.
//!
//! Artifacts are stored through a local directory or an S3-compatible
//! bucket — the release pipeline has its **own** storage configuration,
//! independent from call-recording storage, while sharing the same
//! [`crate::storage`] engine underneath. The backend comes from (highest
//! priority first):
//!
//! 1. the storage saved on the console releases page — persisted as
//!    `release/storage.toml` in the generated-config store (auth-required
//!    `GET/PUT/DELETE {api_prefix}/release/storage`);
//! 2. `[release].storage` in `config.toml`;
//! 3. the default local `storage/releases` directory.
//!
//! Each upload may pick `local` or `s3` explicitly (multipart field
//! `storage`); without the field the effective default above applies.
//! S3 objects are recorded as `s3://{bucket}/{key}`; download endpoints
//! rewrite them into presigned URLs on demand (default 24h, see
//! `[release].signed_url_expiry_secs`).
//!
//! Public (unauthenticated) endpoints, consumed by desktop update clients
//! (e.g. restsend-pc `update.rs`):
//! - `GET {api_prefix}/release`                      — bare `UpdateMeta` JSON of the latest published release
//! - `GET {api_prefix}/release/list`                 — published releases
//! - `GET {api_prefix}/release/{version}/download`   — stream local file / 302 to presigned URL
//!
//! Releases may be tagged with one or more environments
//! ([`RELEASE_ENVIRONMENTS`]: `testing` / `stable` / `preview`). Update
//! clients pass their environment as the `env` query param; only releases
//! tagged with that environment (or untagged ones — visible everywhere, the
//! legacy default) are considered for the "latest" pick.
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
/// Config-store entry holding the storage backend saved on the releases
/// page (`[storage]` table with a [`ArtifactStorage`] payload). Overrides
/// `[release].storage` from `config.toml` when present.
pub const RELEASE_STORAGE_ENTRY: &str = "storage.toml";

/// Closed set of release environments. A release tagged with one or more
/// environments is only served to update clients whose `env` query param
/// matches one of the tags; untagged releases (empty `environments`) stay
/// visible to every environment — the legacy default, so manifests written
/// before this field existed keep working unchanged.
pub const RELEASE_ENVIRONMENTS: [&str; 3] = ["testing", "stable", "preview"];

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
    /// Environments this release targets ([`RELEASE_ENVIRONMENTS`]). Empty
    /// (or missing — `#[serde(default)]`) means "visible to every
    /// environment", which is what legacy manifests deserialize to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environments: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Root document of `release.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ReleaseFile {
    releases: Vec<ReleaseDoc>,
}

/// Root document of `release/storage.toml` — the storage backend saved from
/// the console releases page. Wrapped in a table so the TOML file reads
/// `[storage] type = "s3" …`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ReleaseStorageFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage: Option<ArtifactStorage>,
}

/// Which backend an upload targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    Local,
    S3,
}

impl StorageKind {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "local" => Some(Self::Local),
            "s3" => Some(Self::S3),
            _ => None,
        }
    }
}

fn storage_kind_of(cfg: &ArtifactStorage) -> StorageKind {
    match cfg {
        ArtifactStorage::Local { .. } => StorageKind::Local,
        ArtifactStorage::S3 { .. } => StorageKind::S3,
    }
}

fn default_local_storage() -> ArtifactStorage {
    ArtifactStorage::Local {
        path: "storage/releases".to_string(),
    }
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
        .route(
            "/release/storage",
            get(get_storage_handler)
                .put(put_storage_handler)
                .delete(delete_storage_handler),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
}

// ── Public handlers ─────────────────────────────────────────────────────

/// GET {api_prefix}/release — bare `UpdateMeta`-shaped JSON for the latest
/// published release. 404 when nothing is published (clients treat that as
/// "no update service" and stay silent). Query params carried by clients
/// (`version`, `build_time`, `git_commit`, `env`) are logged for
/// observability. `env` selects the release environment: only releases
/// tagged with it (plus untagged ones) are eligible for the "latest" pick.
pub async fn check_update_handler(
    State(state): State<Arc<ConsoleState>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let requested_env = params.get("env").map(String::as_str);
    if let Some(current) = params.get("version") {
        tracing::debug!(
            current = %current,
            env = requested_env.unwrap_or(""),
            build_time = params.get("build_time").map(String::as_str).unwrap_or(""),
            git_commit = params.get("git_commit").map(String::as_str).unwrap_or(""),
            "release check request"
        );
    }
    let docs = load_docs(&config_store(&state)).await;
    let Some(doc) = pick_latest_published_for_env(&docs, requested_env) else {
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
    let mut envs: Vec<String> = Vec::new();
    let mut storage_choice: Option<StorageKind> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name().unwrap_or_default() {
            "version" => {
                version = field.text().await.ok().map(|v| v.trim().to_string());
            }
            "storage" => {
                let raw = field.text().await.unwrap_or_default();
                match StorageKind::parse(&raw) {
                    Some(kind) => storage_choice = Some(kind),
                    None if raw.trim().is_empty() => {}
                    None => {
                        return json_error(
                            StatusCode::BAD_REQUEST,
                            format!(
                                "unknown storage `{raw}` (allowed: local, s3)"
                            ),
                        )
                    }
                }
            }
            "release_notes" => {
                notes = field.text().await.ok().map(|v| v.trim().to_string());
            }
            "published" => {
                published = !matches!(field.text().await.as_deref(), Ok("false" | "0" | ""));
            }
            // Checkbox group: the field may repeat and each value may carry
            // comma-separated extras.
            "environments" => {
                if let Ok(text) = field.text().await {
                    for part in text.split(',') {
                        let v = part.trim().to_ascii_lowercase();
                        if !v.is_empty() {
                            envs.push(v);
                        }
                    }
                }
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
    match normalize_environments(std::mem::take(&mut envs)) {
        Ok(list) => doc.environments = list,
        Err(bad) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown environment `{bad}` (allowed: {})",
                    RELEASE_ENVIRONMENTS.join(", ")
                ),
            )
        }
    }
    if let Some(w) = &doc.window
        && let Err(e) = validate_window(w)
    {
        return json_error(StatusCode::BAD_REQUEST, e);
    }

    // Fail fast on an unusable backend choice before reading the whole
    // manifest (an unconfigured S3 target is an operator error → 400).
    if let Some(StorageKind::S3) = storage_choice
        && let Err(e) = storage_of_kind(&state, StorageKind::S3).await
    {
        return json_error(StatusCode::BAD_REQUEST, e);
    }

    match create_release(
        &state,
        CreateReleaseParams {
            version,
            file_name,
            data,
            release_notes: notes.filter(|n| !n.is_empty()),
            published,
            extra: doc,
            storage: storage_choice,
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
    if let Some(v) = body.window {
        let trimmed = v.trim().to_string();
        if !trimmed.is_empty() && let Err(e) = validate_window(&trimmed) {
            return json_error(StatusCode::BAD_REQUEST, e);
        }
        doc.window = Some(trimmed).filter(|n| !n.is_empty());
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
    if let Some(envs) = body.environments.as_ref() {
        match normalize_environments(envs.clone()) {
            Ok(list) => doc.environments = list,
            Err(bad) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "unknown environment `{bad}` (allowed: {})",
                        RELEASE_ENVIRONMENTS.join(", ")
                    ),
                )
            }
        }
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
    match release_storage_for(&state, &doc.storage_path).await {
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
    // Tera lacks a `starting_with` test — precompute the storage badge.
    let releases: Vec<serde_json::Value> = docs
        .iter()
        .map(|d| {
            let mut v = serde_json::to_value(d).unwrap_or(json!({}));
            v["storage_is_s3"] = json!(d.storage_path.trim().starts_with("s3://"));
            v
        })
        .collect();
    state.render_with_headers(
        "console/releases.html",
        json!({
            "nav_active": "releases",
            "releases": releases,
            "current_user": current_user,
        }),
        &headers,
    )
}

// ── Storage settings (releases page) ────────────────────────────────────

/// GET {api_prefix}/release/storage — the storage backend currently in
/// effect for release artifacts, with a `source` marker (`stored` = saved on
/// this page, `config` = `config.toml [release].storage`, `default` = built-in
/// local directory). Credentials are never returned; only `has_credentials`.
pub async fn get_storage_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
) -> Response {
    let store = config_store(&state);
    let stored = stored_storage(&store).await;
    let from_toml = state
        .config()
        .release
        .as_ref()
        .and_then(|r| r.storage.clone());
    let (cfg, source) = match (stored, from_toml) {
        (Some(cfg), _) => (cfg, "stored"),
        (None, Some(cfg)) => (cfg, "config"),
        (None, None) => (default_local_storage(), "default"),
    };
    Json(storage_json(&cfg, source)).into_response()
}

/// PUT {api_prefix}/release/storage — save the storage backend on this page
/// (overrides `config.toml [release].storage`). `type` selects `local`
/// (`path`) or `s3` (`vendor`/`bucket`/`region`/`endpoint`/`prefix` +
/// credentials). Blank credential fields keep the credentials already saved
/// here; `clear_credentials: true` removes them (anonymous bucket access).
pub async fn put_storage_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
    Json(body): Json<UpdateStorageBody>,
) -> Response {
    let store = config_store(&state);
    let existing = stored_storage(&store).await;
    let cfg = match storage_from_update_body(&body, existing.as_ref()) {
        Ok(cfg) => cfg,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
    };
    let content = match toml::to_string_pretty(&ReleaseStorageFile {
        storage: Some(cfg.clone()),
    }) {
        Ok(c) => c,
        Err(e) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to encode storage.toml: {e}"),
            )
        }
    };
    if let Err(e) = store
        .write(RELEASE_CATEGORY, RELEASE_STORAGE_ENTRY, &content)
        .await
    {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to persist storage.toml: {e}"),
        );
    }
    Json(storage_json(&cfg, "stored")).into_response()
}

/// DELETE {api_prefix}/release/storage — drop the page-saved storage config
/// and fall back to `config.toml [release].storage` (or the default local
/// directory).
pub async fn delete_storage_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
) -> Response {
    let store = config_store(&state);
    if let Err(e) = store
        .delete(RELEASE_CATEGORY, RELEASE_STORAGE_ENTRY)
        .await
    {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to remove storage.toml: {e}"),
        );
    }
    Json(json!({ "deleted": true })).into_response()
}

// ── Request bodies ──────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct UpdateStorageBody {
    /// `local` or `s3`.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Local artifact directory (`type = "local"`).
    pub path: Option<String>,
    pub vendor: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    /// Optional key prefix inside the bucket.
    pub prefix: Option<String>,
    /// Non-empty values (re)set the credential; blank keeps what is already
    /// saved on this page.
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    /// Drop both credentials → anonymous/public bucket access.
    #[serde(default)]
    pub clear_credentials: bool,
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
    /// Explicit backend for this artifact (`local` / `s3` from the upload
    /// form). `None` = the effective default backend.
    pub storage: Option<StorageKind>,
}

// ── Storage resolution ──────────────────────────────────────────────────

/// The storage backend saved on the releases page, if any.
async fn stored_storage(store: &GeneratedConfigStore) -> Option<ArtifactStorage> {
    match store.read(RELEASE_CATEGORY, RELEASE_STORAGE_ENTRY).await {
        Ok(Some(content)) => match toml::from_str::<ReleaseStorageFile>(&content) {
            Ok(file) => file.storage,
            Err(e) => {
                tracing::warn!(error = %e, "failed to parse release storage.toml, ignoring");
                None
            }
        },
        _ => None,
    }
}

/// Effective default backend: page-saved config → `config.toml
/// [release].storage` → built-in local `storage/releases`.
async fn effective_release_storage(state: &ConsoleState) -> ArtifactStorage {
    if let Some(cfg) = stored_storage(&config_store(state)).await {
        return cfg;
    }
    state
        .config()
        .release
        .as_ref()
        .and_then(|r| r.storage.clone())
        .unwrap_or_else(default_local_storage)
}

/// Backend of a specific kind. Resolution walks the page-saved config then
/// `config.toml`; `local` falls back to the built-in directory, `s3` fails
/// with an actionable message when neither source configures a bucket.
async fn storage_of_kind(state: &ConsoleState, kind: StorageKind) -> Result<ArtifactStorage, String> {
    let stored = stored_storage(&config_store(state)).await;
    let from_toml = state
        .config()
        .release
        .as_ref()
        .and_then(|r| r.storage.clone());
    for cfg in [stored, from_toml].into_iter().flatten() {
        if storage_kind_of(&cfg) == kind {
            return Ok(cfg);
        }
    }
    match kind {
        StorageKind::Local => Ok(default_local_storage()),
        StorageKind::S3 => Err(
            "S3 storage is not configured — save it on the releases page or set [release].storage in config.toml"
                .into(),
        ),
    }
}

/// Backend owning a stored artifact path: `s3://…` → the configured S3
/// bucket, anything else → the local directory.
async fn storage_for_path(state: &ConsoleState, storage_path: &str) -> Result<ArtifactStorage, String> {
    if storage_path.trim().starts_with("s3://") {
        storage_of_kind(state, StorageKind::S3).await
    } else {
        storage_of_kind(state, StorageKind::Local).await
    }
}

/// Build the effective backend for an upload with an explicit choice.
async fn resolve_upload_storage(
    state: &ConsoleState,
    choice: Option<StorageKind>,
) -> Result<ArtifactStorage, String> {
    match choice {
        Some(kind) => storage_of_kind(state, kind).await,
        None => Ok(effective_release_storage(state).await),
    }
}

/// Console-facing JSON for a storage config. Secrets never leave the server;
/// S3 configs report `has_credentials` instead.
fn storage_json(cfg: &ArtifactStorage, source: &str) -> serde_json::Value {
    match cfg {
        ArtifactStorage::Local { path } => json!({
            "type": "local",
            "path": path,
            "source": source,
        }),
        ArtifactStorage::S3 {
            vendor,
            bucket,
            region,
            access_key,
            secret_key,
            endpoint,
            prefix,
        } => json!({
            "type": "s3",
            "vendor": vendor,
            "bucket": bucket,
            "region": region,
            "endpoint": endpoint,
            "prefix": prefix,
            "has_credentials": access_key.is_some() || secret_key.is_some(),
            "source": source,
        }),
    }
}

/// Materialize the page-saved backend from a PUT body. Blank credential
/// fields inherit the credentials of the currently saved S3 config (so
/// editing e.g. the region does not silently drop the keys);
/// `clear_credentials` removes them.
fn storage_from_update_body(
    body: &UpdateStorageBody,
    existing: Option<&ArtifactStorage>,
) -> Result<ArtifactStorage, String> {
    let kind = body
        .kind
        .as_deref()
        .map(StorageKind::parse)
        .unwrap_or(None);
    match kind {
        Some(StorageKind::Local) => {
            let path = body
                .path
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .unwrap_or("storage/releases");
            Ok(ArtifactStorage::Local {
                path: path.to_string(),
            })
        }
        Some(StorageKind::S3) => {
            let field = |v: &Option<String>| -> Option<String> {
                v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
            };
            let vendor_raw =
                field(&body.vendor).ok_or("S3 storage requires a vendor")?;
            let vendor: crate::storage::S3Vendor = serde_json::from_value(json!(vendor_raw))
                .map_err(|_| {
                    format!(
                        "unknown S3 vendor `{vendor_raw}` (expected aws/gcp/azure/aliyun/tencent/minio/digitalocean)"
                    )
                })?;
            let bucket = field(&body.bucket).ok_or("S3 storage requires a bucket")?;
            let region = field(&body.region).ok_or("S3 storage requires a region")?;
            let endpoint = field(&body.endpoint).ok_or("S3 storage requires an endpoint")?;
            let (prev_access, prev_secret) = match existing {
                Some(ArtifactStorage::S3 {
                    access_key,
                    secret_key,
                    ..
                }) => (access_key.clone(), secret_key.clone()),
                _ => (None, None),
            };
            let merge = |provided: &Option<String>,
                         previous: Option<String>|
             -> Option<String> {
                if body.clear_credentials {
                    return None;
                }
                match field(provided) {
                    Some(v) => Some(v),
                    None => previous,
                }
            };
            let access_key = merge(&body.access_key, prev_access);
            let secret_key = merge(&body.secret_key, prev_secret);
            if access_key.is_some() != secret_key.is_some() {
                return Err(
                    "access_key and secret_key must be set together (or cleared together)"
                        .into(),
                );
            }
            Ok(ArtifactStorage::S3 {
                vendor,
                bucket: bucket.to_string(),
                region: region.to_string(),
                access_key,
                secret_key,
                endpoint: Some(endpoint.to_string()),
                prefix: body
                    .prefix
                    .as_deref()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
            })
        }
        None => Err("storage `type` must be `local` or `s3`".into()),
    }
}

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
    /// Replace the environment tags. An explicit empty array resets the
    /// release to "visible to every environment".
    pub environments: Option<Vec<String>>,
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

    let storage_cfg = match resolve_upload_storage(state, params.storage).await {
        Ok(cfg) => cfg,
        Err(e) => return Err(e),
    };
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
    // points somewhere else, then replace the entry. The old artifact may
    // live in the other backend (local ↔ S3), so resolve its storage from
    // the recorded path kind.
    if let Some(pos) = docs.iter().position(|d| d.version == version) {
        let old = docs.remove(pos);
        if old.storage_path != storage_path {
            match storage_for_path(state, &old.storage_path).await {
                Ok(old_cfg) => {
                    if let Ok(old_storage) = Storage::new(&old_cfg) {
                        if let Some(old_key) =
                            artifact_object_key(&old_storage, &old.storage_path)
                        {
                            if let Err(e) = old_storage.delete(&old_key).await {
                                tracing::warn!(version = %version, error = %e, "failed to clean previous artifact");
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(version = %version, error = %e, "previous artifact backend unavailable");
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
        environments: params.extra.environments,
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

/// The latest published release by version order, restricted to `env`.
///
/// `env` is the update client's environment (`?env=` query param):
/// - `None`/empty (legacy clients) — no filtering, every published doc is
///   eligible (unchanged pre-environments behaviour);
/// - some `env` — docs tagged with that environment **plus** untagged docs
///   (`environments` empty = visible everywhere) are eligible.
fn pick_latest_published_for_env<'a>(
    docs: &'a [ReleaseDoc],
    env: Option<&str>,
) -> Option<&'a ReleaseDoc> {
    let env = env
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    docs.iter()
        .filter(|d| d.published && !d.version.trim().is_empty())
        .filter(|d| match &env {
            None => true,
            Some(env) => {
                d.environments.is_empty() || d.environments.iter().any(|e| e.eq_ignore_ascii_case(env))
            }
        })
        .max_by(|a, b| version_cmp(&a.version, &b.version))
}

fn sort_by_version_desc(docs: &mut Vec<ReleaseDoc>) {
    docs.sort_by(|a, b| version_cmp(&b.version, &a.version));
}

/// Lowercase + trim a single environment value; `Err` when it is not in
/// [`RELEASE_ENVIRONMENTS`] (typo protection — an out-of-vocabulary tag
/// would otherwise be permanently invisible to every client).
fn normalize_environment(raw: &str) -> Result<String, String> {
    let v = raw.trim().to_ascii_lowercase();
    if RELEASE_ENVIRONMENTS.contains(&v.as_str()) {
        Ok(v)
    } else {
        Err(raw.trim().to_string())
    }
}

/// Normalize + dedupe environment values, preserving first-seen order.
/// Returns the first offending value on failure.
fn normalize_environments(raw: Vec<String>) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for v in raw {
        let v = normalize_environment(&v)?;
        if !out.contains(&v) {
            out.push(v);
        }
    }
    Ok(out)
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
        let storage = release_storage_for(state, raw).await?;
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
    if let Ok(ArtifactStorage::S3 {
        bucket,
        endpoint: Some(endpoint),
        ..
    }) = storage_of_kind(state, StorageKind::S3).await
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
    let storage = release_storage_for(state, raw).await?;
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

/// Storage backend for a stored artifact path (`s3://…` → S3 bucket, else
/// the local directory).
async fn release_storage_for(state: &Arc<ConsoleState>, storage_path: &str) -> Result<Storage, String> {
    let cfg = storage_for_path(state, storage_path).await?;
    Storage::new(&cfg).map_err(|e| format!("storage unavailable: {e}"))
}

// ── Small helpers ───────────────────────────────────────────────────────

fn trim_non_empty(v: String) -> Option<String> {
    let v = v.trim().to_string();
    (!v.is_empty()).then_some(v)
}

fn parse_u32(v: String) -> Option<u32> {
    v.trim().parse().ok()
}

/// Validate the optional daily download window `HH:MM-HH:MM`. Cross-midnight
/// ranges (`22:00-07:00`) are fine; the value is passed through to update
/// clients verbatim, so reject typos early instead of letting the client
/// fail to parse it later.
fn validate_window(raw: &str) -> Result<(), String> {
    let bad = || format!("invalid window `{raw}` (expected HH:MM-HH:MM)");
    let (start, end) = raw.split_once('-').ok_or_else(bad)?;
    let parse_hhmm = |s: &str| -> Result<(), String> {
        let (h, m) = s.split_once(':').ok_or_else(bad)?;
        let digits =
            |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
        if !digits(h) || !digits(m) {
            return Err(bad());
        }
        if h.parse::<u32>().unwrap() > 23 || m.parse::<u32>().unwrap() > 59 {
            return Err(format!(
                "window time `{s}` out of range (00:00-23:59)"
            ));
        }
        Ok(())
    };
    parse_hhmm(start.trim())?;
    parse_hhmm(end.trim())?;
    Ok(())
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
    fn window_validation_accepts_daily_and_cross_midnight() {
        assert!(validate_window("00:00-23:59").is_ok());
        // Cross-midnight is the common case for overnight pushes.
        assert!(validate_window("22:00-07:00").is_ok());
        assert!(validate_window("22:00 - 07:00").is_ok());
        assert!(validate_window("9:5-7:0").is_ok());
        assert!(validate_window("24:00-07:00").is_err());
        assert!(validate_window("22:60-07:00").is_err());
        assert!(validate_window("22:00").is_err());
        assert!(validate_window("22:00-").is_err());
        assert!(validate_window("ab-cd").is_err());
        assert!(validate_window("2a:00-07:00").is_err());
        assert!(validate_window("").is_err());
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
        assert_eq!(
            pick_latest_published_for_env(&docs, None).unwrap().version,
            "0.1.13"
        );
        let drafts: Vec<ReleaseDoc> = docs.iter().filter(|d| !d.published).cloned().collect();
        assert!(pick_latest_published_for_env(&drafts, None).is_none());
    }

    fn doc_env(version: &str, published: bool, envs: &[&str]) -> ReleaseDoc {
        ReleaseDoc {
            version: version.into(),
            published,
            environments: envs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn pick_latest_for_env_matches_tags_and_untagged_docs() {
        let docs = vec![
            doc_env("0.1.10", true, &["stable"]),
            doc_env("0.1.11", true, &["testing"]),
            doc_env("0.1.12", true, &["preview"]),
            doc_env("0.1.13", false, &["testing"]), // draft: always invisible
        ];
        // Each env resolves to its own newest tagged release.
        assert_eq!(
            pick_latest_published_for_env(&docs, Some("stable"))
                .unwrap()
                .version,
            "0.1.10"
        );
        assert_eq!(
            pick_latest_published_for_env(&docs, Some("testing"))
                .unwrap()
                .version,
            "0.1.11"
        );
        assert_eq!(
            pick_latest_published_for_env(&docs, Some("preview"))
                .unwrap()
                .version,
            "0.1.12"
        );
        // Legacy clients (no env param) keep the unfiltered behaviour.
        assert_eq!(
            pick_latest_published_for_env(&docs, None).unwrap().version,
            "0.1.12"
        );
        // Empty-string env is treated like a missing param.
        assert_eq!(
            pick_latest_published_for_env(&docs, Some(""))
                .unwrap()
                .version,
            "0.1.12"
        );

        // Untagged docs are visible to every env: a newer untagged release
        // beats an older tagged one for the other channel.
        let mixed = vec![
            doc_env("0.2.0", true, &["stable"]),
            doc_env("0.1.9", true, &[]),
        ];
        assert_eq!(
            pick_latest_published_for_env(&mixed, Some("stable"))
                .unwrap()
                .version,
            "0.2.0"
        );
        assert_eq!(
            pick_latest_published_for_env(&mixed, Some("testing"))
                .unwrap()
                .version,
            "0.1.9"
        );
    }

    #[test]
    fn pick_latest_for_env_returns_none_when_no_doc_carries_env() {
        // Every doc pinned to stable only: a testing client gets nothing
        // (handler answers 404 and the client stays silent).
        let docs = vec![doc_env("0.1.10", true, &["stable"]), doc_env("0.1.11", true, &["stable"])];
        assert!(pick_latest_published_for_env(&docs, Some("testing")).is_none());
        assert!(pick_latest_published_for_env(&docs, Some("stable")).is_some());
    }

    #[test]
    fn environments_normalize_and_validate_closed_vocabulary() {
        assert_eq!(
            normalize_environments(vec![
                " Stable ".into(),
                "TESTING".into(),
                "stable".into()
            ])
            .unwrap(),
            vec!["stable".to_string(), "testing".to_string()]
        );
        assert!(normalize_environments(vec![]).unwrap().is_empty());
        assert_eq!(
            normalize_environments(vec!["prod".into()]).unwrap_err(),
            "prod"
        );
        assert_eq!(normalize_environment(" preview ").unwrap(), "preview");
        assert!(normalize_environment("").is_err());
    }

    #[test]
    fn toml_omits_empty_environments_and_roundtrips_tags() {
        let untagged: ReleaseDoc = doc_env("0.1.10", true, &[]);
        let s = toml::to_string(&untagged).unwrap();
        assert!(!s.contains("environments"));
        let back: ReleaseDoc = toml::from_str(&s).unwrap();
        assert!(back.environments.is_empty());

        let tagged = doc_env("0.1.11", true, &["testing", "preview"]);
        let s = toml::to_string(&tagged).unwrap();
        let back: ReleaseDoc = toml::from_str(&s).unwrap();
        assert_eq!(back.environments, vec!["testing", "preview"]);
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

    /// Environment routing end-to-end: multipart upload with `environments`
    /// checkbox fields (repeated + mixed case), `?env=` routing on the public
    /// check endpoint, unknown tag rejected, PUT retag.
    #[tokio::test]
    async fn env_tagged_release_routes_by_env_query() {
        let (state, _tmp) = setup_release_stack().await;

        // Legacy untagged release: visible to every environment.
        seed(&state, vec![doc("0.1.13", true)]).await;

        // Upload a testing+stable tagged release via checkbox fields.
        let boundary = "relboundary42";
        let body = multipart_body(
            boundary,
            &[
                ("version", "0.2.0"),
                ("published", "true"),
                ("environments", "testing"),
                ("environments", " Stable "),
            ],
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
        assert_eq!(resp.status(), StatusCode::OK, "tagged upload must succeed");

        let docs = load_docs(&config_store(&state)).await;
        assert_eq!(
            docs.iter().find(|d| d.version == "0.2.0").unwrap().environments,
            vec!["testing", "stable"],
            "tags must be normalized (lowercase, deduped)"
        );

        // env clients resolve their newest eligible release; untagged docs
        // stay visible everywhere.
        for (env, expected) in [
            ("testing", "0.2.0"),
            ("stable", "0.2.0"),
            ("preview", "0.1.13"), // falls back to the untagged release
        ] {
            let resp = get(&state, &format!("/api/release?env={env}&version=0.1.0")).await;
            assert_eq!(resp.status(), StatusCode::OK, "env={env}");
            assert_eq!(
                json_body(resp).await["version"], expected,
                "env={env}"
            );
        }

        // Unknown tag on upload → 400 and the manifest is untouched.
        let body = multipart_body(
            "relboundary42",
            &[
                ("version", "0.2.1"),
                ("published", "true"),
                ("environments", "prod"),
            ],
            Some(("file", "bad.zip", b"bad".as_slice())),
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
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let docs = load_docs(&config_store(&state)).await;
        assert_eq!(docs.len(), 2, "rejected upload must not add an entry");

        // PUT retag: narrow 0.2.0 to testing only.
        let resp = crate::api::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/release/0.2.0")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::from("{\"environments\":[\"testing\"]}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let docs = load_docs(&config_store(&state)).await;
        assert_eq!(
            docs.iter().find(|d| d.version == "0.2.0").unwrap().environments,
            vec!["testing"]
        );
        // stable now falls back to the untagged release; PUT with an unknown
        // tag is rejected.
        let resp = get(&state, "/api/release?env=stable").await;
        assert_eq!(json_body(resp).await["version"], "0.1.13");
        let resp = crate::api::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/release/0.2.0")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {TEST_TOKEN}"))
                    .body(Body::from("{\"environments\":[\"nightly\"]}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Console page renders with environment checkboxes (stable pre-checked),
    /// per-release env badges and the "all environments" fallback badge.
    #[tokio::test]
    async fn releases_page_renders_environment_checkboxes_and_badges() {
        let (state, _tmp) = setup_release_stack().await;
        seed(
            &state,
            vec![
                ReleaseDoc {
                    environments: vec!["testing".into()],
                    ..doc("0.2.0", true)
                },
                doc("0.1.13", true), // untagged → "all environments" badge
                ReleaseDoc {
                    storage_path: "s3://rels/0.1.12/app.zip".into(),
                    ..doc("0.1.12", true) // S3-uploaded → storage badge
                },
            ],
        )
        .await;
        let mut docs = load_docs(&config_store(&state)).await;
        sort_by_version_desc(&mut docs);
        // Mirror the page handler: precompute the storage badge flag.
        let releases: Vec<serde_json::Value> = docs
            .iter()
            .map(|d| {
                let mut v = serde_json::to_value(d).unwrap();
                v["storage_is_s3"] = json!(d.storage_path.trim().starts_with("s3://"));
                v
            })
            .collect();

        let resp = state.render_with_locale(
            "console/releases.html",
            json!({ "nav_active": "releases", "releases": releases }),
            "zh",
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("适用环境"), "upload form label missing");
        assert!(
            html.contains(r#"name="environments" value="testing" checked"#),
            "testing checkbox must be pre-checked"
        );
        assert!(
            !html.contains(r#"name="environments" value="stable" checked"#),
            "stable checkbox must not be pre-checked"
        );
        assert!(html.contains("更新策略"), "policy section missing");
        assert!(html.contains(r#"name="window""#), "window input missing");
        assert!(html.contains(r#"name="force""#), "force checkbox missing");
        assert!(html.contains("不勾选 = 所有环境可见"), "hint missing");
        assert!(html.contains(">testing</span>"), "env badge missing");
        assert!(html.contains("全部"), "all-environments badge missing");
        // Storage badges: S3 vs local, derived from the recorded path.
        assert!(html.contains("存储位置"), "upload storage selector missing");
        assert!(html.contains("存储配置"), "storage settings card missing");
        assert!(html.contains(">S3</span>"), "s3 storage badge missing");
        assert!(html.contains(">本地</span>"), "local storage badge missing");
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

    // ── storage settings: local + S3 on the releases page ───────────────

    /// Policy fields (force / window / min_supported / deadline) flow through
    /// upload → UpdateMeta, PUT rejects a malformed window, and PUT with an
    /// empty window clears the stored value.
    #[tokio::test]
    async fn policy_fields_flow_through_upload_and_put_validates_window() {
        let (state, _tmp) = setup_release_stack().await;

        let boundary = "relpolicy1";
        let body = multipart_body(
            boundary,
            &[
                ("version", "0.3.0"),
                ("published", "true"),
                ("force", "true"),
                ("window", "22:00-07:00"),
                ("min_supported", "0.1.10"),
                ("deadline", "2026-11-01T00:00:00Z"),
            ],
            Some(("file", "app.zip", b"policy payload".as_slice())),
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
        let saved = json_body(resp.into_response()).await;
        assert_eq!(saved["release"]["force"], true);
        assert_eq!(saved["release"]["window"], "22:00-07:00");
        assert_eq!(saved["release"]["min_supported"], "0.1.10");
        assert_eq!(saved["release"]["deadline"], "2026-11-01T00:00:00Z");

        // The check endpoint passes the policy through to update clients.
        let meta = json_body(get(&state, "/api/release").await).await;
        assert_eq!(meta["force"], true);
        assert_eq!(meta["window"], "22:00-07:00");
        assert_eq!(meta["min_supported"], "0.1.10");
        assert_eq!(meta["deadline"], "2026-11-01T00:00:00Z");

        // Malformed window → 400, stored value untouched.
        let resp = authed(
            &state,
            "PUT",
            "/api/release/0.3.0",
            Body::from("{\"window\":\"25:00-07:00\"}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let docs = load_docs(&config_store(&state)).await;
        assert_eq!(
            docs.iter().find(|d| d.version == "0.3.0").unwrap().window,
            Some("22:00-07:00".into())
        );

        // Empty window → clears the stored value.
        let resp = authed(
            &state,
            "PUT",
            "/api/release/0.3.0",
            Body::from("{\"window\":\"\"}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let docs = load_docs(&config_store(&state)).await;
        assert!(
            docs.iter()
                .find(|d| d.version == "0.3.0")
                .unwrap()
                .window
                .is_none(),
            "empty window must clear the stored value"
        );
    }

    async fn authed(state: &Arc<ConsoleState>, method: &str, uri: &str, body: Body) -> Response {
        crate::api::router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", format!("Bearer {TEST_TOKEN}"))
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap()
            .into_response()
    }

    #[test]
    fn storage_kind_parse_is_closed_vocabulary() {
        assert_eq!(StorageKind::parse("local"), Some(StorageKind::Local));
        assert_eq!(StorageKind::parse(" S3 "), Some(StorageKind::S3));
        assert_eq!(StorageKind::parse("oss"), None);
        assert_eq!(StorageKind::parse(""), None);
    }

    #[test]
    fn release_storage_toml_roundtrip() {
        let file = ReleaseStorageFile {
            storage: Some(ArtifactStorage::S3 {
                vendor: crate::storage::S3Vendor::Minio,
                bucket: "rels".into(),
                region: "us-east-1".into(),
                access_key: Some("ak".into()),
                secret_key: Some("sk".into()),
                endpoint: Some("http://127.0.0.1:9000".into()),
                prefix: Some("pkgs".into()),
            }),
        };
        let s = toml::to_string_pretty(&file).unwrap();
        assert!(s.contains("[storage]"), "wrapped as a [storage] table: {s}");
        let back: ReleaseStorageFile = toml::from_str(&s).unwrap();
        match back.storage.unwrap() {
            ArtifactStorage::S3 {
                bucket,
                prefix,
                access_key,
                ..
            } => {
                assert_eq!(bucket, "rels");
                assert_eq!(prefix.as_deref(), Some("pkgs"));
                assert_eq!(access_key.as_deref(), Some("ak"));
            }
            other => panic!("expected s3 config, got {other:?}"),
        }

        // storage: absent → None (back to config.toml / default).
        let empty = toml::to_string_pretty(&ReleaseStorageFile { storage: None }).unwrap();
        let back: ReleaseStorageFile = toml::from_str(&empty).unwrap();
        assert!(back.storage.is_none());
    }

    #[test]
    fn storage_update_body_validates_and_inherits_credentials() {
        // local: default path when blank.
        let cfg = storage_from_update_body(
            &UpdateStorageBody {
                kind: Some("local".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(storage_kind_of(&cfg), StorageKind::Local);
        match cfg {
            ArtifactStorage::Local { path } => assert_eq!(path, "storage/releases"),
            other => panic!("expected local, got {other:?}"),
        }

        // type is mandatory.
        assert!(storage_from_update_body(&UpdateStorageBody::default(), None).is_err());

        // s3 requires vendor/bucket/region/endpoint (first miss reported).
        let err = storage_from_update_body(
            &UpdateStorageBody {
                kind: Some("s3".into()),
                vendor: Some("minio".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert!(err.contains("bucket"), "got: {err}");

        // Unknown vendor rejected with the closed list.
        let err = storage_from_update_body(
            &UpdateStorageBody {
                kind: Some("s3".into()),
                vendor: Some("oss9".into()),
                bucket: Some("b".into()),
                region: Some("r".into()),
                endpoint: Some("http://ep".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert!(err.contains("unknown S3 vendor"), "got: {err}");

        // Partial credential pair rejected.
        let err = storage_from_update_body(
            &UpdateStorageBody {
                kind: Some("s3".into()),
                vendor: Some("minio".into()),
                bucket: Some("b".into()),
                region: Some("r".into()),
                endpoint: Some("http://ep".into()),
                access_key: Some("ak".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert!(err.contains("together"), "got: {err}");

        let existing = ArtifactStorage::S3 {
            vendor: crate::storage::S3Vendor::Minio,
            bucket: "old".into(),
            region: "us-east-1".into(),
            access_key: Some("ak".into()),
            secret_key: Some("sk".into()),
            endpoint: Some("http://ep".into()),
            prefix: None,
        };

        // Blank credentials keep the saved pair; other fields update.
        let cfg = storage_from_update_body(
            &UpdateStorageBody {
                kind: Some("s3".into()),
                vendor: Some("minio".into()),
                bucket: Some("new".into()),
                region: Some("us-east-1".into()),
                endpoint: Some("http://ep".into()),
                ..Default::default()
            },
            Some(&existing),
        )
        .unwrap();
        match cfg {
            ArtifactStorage::S3 {
                bucket, access_key, secret_key, ..
            } => {
                assert_eq!(bucket, "new");
                assert_eq!(access_key.as_deref(), Some("ak"), "blank must inherit");
                assert_eq!(secret_key.as_deref(), Some("sk"));
            }
            other => panic!("expected s3, got {other:?}"),
        }

        // clear_credentials drops both (anonymous bucket).
        let cfg = storage_from_update_body(
            &UpdateStorageBody {
                kind: Some("s3".into()),
                vendor: Some("minio".into()),
                bucket: Some("new".into()),
                region: Some("us-east-1".into()),
                endpoint: Some("http://ep".into()),
                clear_credentials: true,
                ..Default::default()
            },
            Some(&existing),
        )
        .unwrap();
        match cfg {
            ArtifactStorage::S3 {
                access_key, secret_key, ..
            } => {
                assert!(access_key.is_none() && secret_key.is_none());
            }
            other => panic!("expected s3, got {other:?}"),
        }

        // Explicit non-empty credentials replace the saved pair.
        let cfg = storage_from_update_body(
            &UpdateStorageBody {
                kind: Some("s3".into()),
                vendor: Some("minio".into()),
                bucket: Some("new".into()),
                region: Some("us-east-1".into()),
                endpoint: Some("http://ep".into()),
                access_key: Some(" ak2 ".into()),
                secret_key: Some("sk2".into()),
                ..Default::default()
            },
            Some(&existing),
        )
        .unwrap();
        match cfg {
            ArtifactStorage::S3 { access_key, .. } => {
                assert_eq!(access_key.as_deref(), Some("ak2"), "trimmed replacement");
            }
            other => panic!("expected s3, got {other:?}"),
        }
    }

    #[test]
    fn storage_json_never_leaks_secrets() {
        let cfg = ArtifactStorage::S3 {
            vendor: crate::storage::S3Vendor::Minio,
            bucket: "rels".into(),
            region: "us-east-1".into(),
            access_key: Some("secret-ak".into()),
            secret_key: Some("secret-sk".into()),
            endpoint: Some("http://127.0.0.1:9000".into()),
            prefix: None,
        };
        let v = storage_json(&cfg, "stored");
        assert_eq!(v["type"], "s3");
        assert_eq!(v["bucket"], "rels");
        assert_eq!(v["has_credentials"], true);
        assert_eq!(v["source"], "stored");
        let s = v.to_string();
        assert!(!s.contains("secret-ak") && !s.contains("secret-sk"), "secrets leaked: {s}");
        assert!(v.get("access_key").is_none() && v.get("secret_key").is_none());

        let local = ArtifactStorage::Local {
            path: "storage/releases".into(),
        };
        let v = storage_json(&local, "default");
        assert_eq!(v["type"], "local");
        assert_eq!(v["path"], "storage/releases");
        assert_eq!(v["source"], "default");
    }

    /// GET/PUT/DELETE `/release/storage`: masked reads, validation, blank
    /// credentials inherit, clear drops, reset falls back to config.toml —
    /// and the endpoint stays auth-required after the public-read exemption
    /// was narrowed.
    #[tokio::test]
    async fn storage_settings_roundtrip_masks_secrets_and_requires_auth() {
        let (state, _tmp) = setup_release_stack().await;

        // Storage settings are not public even though /release reads are.
        assert_eq!(
            get(&state, "/api/release/storage").await.status(),
            StatusCode::UNAUTHORIZED
        );
        // The narrowed exemption keeps the documented public reads open.
        assert_eq!(get(&state, "/api/release/list").await.status(), StatusCode::OK);
        assert_eq!(get(&state, "/api/release").await.status(), StatusCode::NOT_FOUND);

        // Initial state: config.toml [release].storage (the test stack's
        // local tmp dir).
        let cfg = json_body(authed(&state, "GET", "/api/release/storage", Body::empty()).await).await;
        assert_eq!(cfg["type"], "local");
        assert_eq!(cfg["source"], "config");

        // PUT with a missing mandatory field → 400, nothing stored.
        let resp = authed(
            &state,
            "PUT",
            "/api/release/storage",
            Body::from(r#"{"type":"s3","vendor":"minio","region":"r","endpoint":"http://ep"}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let stored = stored_storage(&config_store(&state)).await;
        assert!(stored.is_none(), "rejected save must not persist");

        // PUT full s3 → stored; response carries has_credentials, no secrets.
        let resp = authed(
            &state,
            "PUT",
            "/api/release/storage",
            Body::from(
                r#"{"type":"s3","vendor":"minio","bucket":"rels","region":"us-east-1","endpoint":"http://127.0.0.1:9000","access_key":"ak","secret_key":"sk","prefix":"pkgs"}"#,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cfg = json_body(resp).await;
        assert_eq!(cfg["type"], "s3");
        assert_eq!(cfg["has_credentials"], true);
        assert_eq!(cfg["source"], "stored");
        assert!(cfg.get("access_key").is_none());

        let cfg = json_body(authed(&state, "GET", "/api/release/storage", Body::empty()).await).await;
        assert_eq!(cfg["source"], "stored");
        assert_eq!(cfg["bucket"], "rels");
        assert_eq!(cfg["prefix"], "pkgs");

        // Re-save without credentials → the saved pair is kept.
        let resp = authed(
            &state,
            "PUT",
            "/api/release/storage",
            Body::from(
                r#"{"type":"s3","vendor":"minio","bucket":"rels2","region":"us-east-1","endpoint":"http://127.0.0.1:9000"}"#,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cfg = json_body(resp).await;
        assert_eq!(cfg["bucket"], "rels2");
        assert_eq!(
            cfg["has_credentials"], true,
            "blank credentials must keep the saved pair"
        );

        // Anonymous bucket: clear_credentials drops the pair.
        let resp = authed(
            &state,
            "PUT",
            "/api/release/storage",
            Body::from(
                r#"{"type":"s3","vendor":"minio","bucket":"rels","region":"us-east-1","endpoint":"http://127.0.0.1:9000","clear_credentials":true}"#,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cfg = json_body(resp).await;
        assert_eq!(cfg["has_credentials"], false);

        // Switch to local.
        let resp = authed(
            &state,
            "PUT",
            "/api/release/storage",
            Body::from(r#"{"type":"local","path":"storage/pkg-releases"}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cfg = json_body(resp).await;
        assert_eq!(cfg["type"], "local");
        assert_eq!(cfg["path"], "storage/pkg-releases");

        // Reset → back to config.toml.
        let resp = authed(&state, "DELETE", "/api/release/storage", Body::empty()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cfg = json_body(authed(&state, "GET", "/api/release/storage", Body::empty()).await).await;
        assert_eq!(cfg["source"], "config");
        assert_eq!(cfg["type"], "local");
    }

    /// The multipart `storage` field picks the backend per upload: unknown
    /// values and an unconfigured S3 target are rejected up front (400),
    /// explicit `local` and the default (no field) both land in the
    /// configured local directory.
    #[tokio::test]
    async fn upload_storage_field_routes_local_and_rejects_bad_choices() {
        let (state, tmp) = setup_release_stack().await;

        let post = |fields: &[(&str, &str)]| {
            let body = multipart_body(
                "relboundary42",
                fields,
                Some(("file", "app.zip", b"payload".as_slice())),
            );
            crate::api::router(state.clone())
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
        };

        // Unknown backend name → 400.
        let resp = post(&[
            ("version", "0.3.0"),
            ("storage", "ceph"),
        ])
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // S3 chosen while unconfigured → 400 with the actionable message.
        let resp = post(&[
            ("version", "0.3.0"),
            ("storage", "s3"),
        ])
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("not configured"), "got: {text}");
        let docs = load_docs(&config_store(&state)).await;
        assert!(docs.is_empty(), "failed uploads must not add entries");

        // Explicit local → stored in the configured local dir.
        let resp = post(&[
            ("version", "0.3.0"),
            ("storage", "local"),
        ])
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "explicit local must upload");
        let saved = json_body(resp).await["release"].clone();
        assert_eq!(saved["storage_path"], "0.3.0/app.zip");
        assert!(tmp.path().join("0.3.0/app.zip").exists(), "file must land in the local dir");

        // No field → effective default (config.toml local here) — same dir.
        let resp = post(&[("version", "0.3.1")]).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let saved = json_body(resp).await["release"].clone();
        assert_eq!(saved["storage_path"], "0.3.1/app.zip");
        assert!(tmp.path().join("0.3.1/app.zip").exists());
    }

    /// An `s3://…` artifact is served through the page-saved S3 storage:
    /// offline SigV4 presigning for both the check endpoint (bare UpdateMeta
    /// `url`) and the download redirect — and fails with a clear error while
    /// no S3 storage is configured.
    #[tokio::test]
    async fn s3_artifacts_presign_via_page_saved_storage() {
        let (state, _tmp) = setup_release_stack().await;
        seed(
            &state,
            vec![ReleaseDoc {
                version: "0.1.13".into(),
                published: true,
                file_name: "app.zip".into(),
                storage_path: "s3://rels/0.1.13/app.zip".into(),
                sha256: "cafe".into(),
                ..Default::default()
            }],
        )
        .await;

        // No S3 storage anywhere → the artifact is unreachable with a clear
        // message (clients treat non-200 as "no update" and stay silent).
        let resp = get(&state, "/api/release/0.1.13/download").await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(get(&state, "/api/release").await.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Save the S3 backend on the page.
        let resp = authed(
            &state,
            "PUT",
            "/api/release/storage",
            Body::from(
                r#"{"type":"s3","vendor":"minio","bucket":"rels","region":"us-east-1","endpoint":"http://127.0.0.1:9000","access_key":"ak","secret_key":"sk"}"#,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Check endpoint: presigned URL built offline from the saved keys.
        let resp = get(&state, "/api/release?version=0.1.9").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let meta = json_body(resp).await;
        let url = meta["url"].as_str().unwrap().to_string();
        assert!(
            url.starts_with("http://127.0.0.1:9000/rels/0.1.13/app.zip"),
            "presigned url: {url}"
        );
        assert!(url.contains("X-Amz-Signature"), "must be signed: {url}");

        // Download endpoint: 302 to the same presigned URL.
        let resp = get(&state, "/api/release/0.1.13/download").await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(loc.contains("X-Amz-Signature"), "redirect: {loc}");

        // Reset to config.toml (local) → unreachable again.
        let resp = authed(&state, "DELETE", "/api/release/storage", Body::empty()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            get(&state, "/api/release/0.1.13/download").await.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
