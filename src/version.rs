use sea_orm::{EntityTrait, PaginatorTrait};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tracing::{debug, info};

const VERSION_INFO: &str = concat!(
    "rustpbx ",
    env!("CARGO_PKG_VERSION"),
    "\nBuild Time: ",
    env!("BUILD_TIME_FMT"),
    "\nGit Commit: ",
    env!("GIT_COMMIT_HASH"),
    "\nGit Branch: ",
    env!("GIT_BRANCH"),
    "\nGit Status: ",
    env!("GIT_DIRTY")
);

const SHORT_VERSION: &str = env!("SHORT_VERSION");

pub fn get_version_info() -> &'static str {
    VERSION_INFO
}

pub fn get_short_version() -> &'static str {
    SHORT_VERSION
}

pub fn get_useragent() -> String {
    format!(
        "rustpbx/{} (built {})",
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_DATE")
    )
}

// ─── Update check ────────────────────────────────────────────────────────────

/// Response from the miuda.ai update-check endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub has_update: bool,
    pub latest_version: String,
    pub release_notes: Option<String>,
    pub download_url: Option<String>,
}

/// Query `https://miuda.ai/api/check_update` with current version + edition
/// plus deployment stats (uptime, total calls, extensions, wholesale calls).
/// Returns `UpdateInfo` on success.
pub async fn check_update(
    state: &crate::app::AppState,
    start_time: Instant,
) -> anyhow::Result<UpdateInfo> {
    let version = env!("CARGO_PKG_VERSION");
    let edition = if cfg!(feature = "commerce") {
        "commerce"
    } else {
        "community"
    };
    let uptime_secs = start_time.elapsed().as_secs();
    let build_time = env!("BUILD_TIME_FMT");

    let total_calls = state.total_calls.load(std::sync::atomic::Ordering::Relaxed);

    let extensions_count = crate::models::extension::Entity::find()
        .count(state.db())
        .await
        .unwrap_or(0);

    #[cfg(feature = "addon-wholesale")]
    let wholesale_calls = crate::addons::wholesale::models::wholesale_cdr::Entity::find()
        .count(state.db())
        .await
        .unwrap_or(0);
    #[cfg(not(feature = "addon-wholesale"))]
    let wholesale_calls = 0u64;

    let opts = crate::http_util::HttpFetchOptions::new()
        .with_timeout(std::time::Duration::from_secs(5))
        .with_header("User-Agent", &get_useragent());

    #[allow(unused_mut)]
    let mut params: Vec<(String, String)> = vec![
        ("version".to_string(), version.to_string()),
        ("edition".to_string(), edition.to_string()),
        ("uptime".to_string(), uptime_secs.to_string()),
        ("build_time".to_string(), build_time.to_string()),
        ("total_calls".to_string(), total_calls.to_string()),
        ("extensions_count".to_string(), extensions_count.to_string()),
        ("wholesale_calls".to_string(), wholesale_calls.to_string()),
    ];

    #[cfg(feature = "commerce")]
    if let Some(digest) = compute_license_digest(state) {
        params.push(("license_digest".to_string(), digest));
    }

    let req = crate::http_util::shared_keepalive_client()
        .get("https://miuda.ai/api/check_update")
        .query(&params);
    let resp = match crate::http_util::execute_request(req, &opts.headers, opts.timeout).await {
        Ok(r) => r,
        Err(e) => {
            let s = e.to_string();
            if s.contains("timed out") || s.contains("connect") {
                anyhow::bail!("version check unreachable (network/timeout): {}", s);
            }
            anyhow::bail!("version check request error: {}", s);
        }
    };
    let status = resp.status();
    let body = resp.text().await?;
    debug!("version check response: status={} body={}", status, body);
    let info: UpdateInfo = serde_json::from_str(&body).map_err(|e| {
        anyhow::anyhow!("version check parse error: {e}, status={status}, body={body}")
    })?;
    Ok(info)
}

/// Compute a short digest of the first configured license key (first 8
/// characters of the key sorted by key name for determinism). Returns `None`
/// when no license key is configured.
#[cfg(feature = "commerce")]
fn compute_license_digest(state: &crate::app::AppState) -> Option<String> {
    let licenses = state.config().licenses.as_ref()?;
    let key = licenses.keys.iter().min_by(|a, b| a.0.cmp(b.0))?.1.trim();
    if key.is_empty() {
        return None;
    }
    Some(key.chars().take(8).collect())
}

/// Spawn a background task that periodically checks for updates (at startup and
/// every 24 hours).  When a new version is found a `system_notification` row is
/// inserted into the database (deduped by title so the same version only appears
/// once).
pub fn spawn_update_checker(
    state: crate::app::AppState,
    token: tokio_util::sync::CancellationToken,
) {
    // Skip update check in debug/development mode
    #[cfg(debug_assertions)]
    {
        debug!("Skipping update check in debug mode");
        let _ = &state;
        let _ = token;
    }

    #[cfg(not(debug_assertions))]
    crate::utils::spawn(async move {
        let start_time = Instant::now();
        loop {
            match check_update(&state, start_time).await {
                Ok(info) if info.has_update => {
                    use crate::models::system_notification::{ActiveModel, Column, Entity};
                    use sea_orm::{
                        ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter,
                    };

                    let db = state.db();
                    let title = format!("New version available: {}", info.latest_version);
                    let exists = Entity::find()
                        .filter(Column::Title.eq(&title))
                        .one(db)
                        .await
                        .ok()
                        .flatten()
                        .is_some();

                    if !exists {
                        let body = info.release_notes.clone().unwrap_or_default();
                        let am = ActiveModel {
                            id: sea_orm::ActiveValue::NotSet,
                            kind: Set("update".to_string()),
                            title: Set(title.clone()),
                            body: Set(body),
                            read: Set(false),
                            created_at: Set(chrono::Utc::now()),
                        };
                        match am.insert(db).await {
                            Ok(_) => {
                                info!(latest = %info.latest_version, "update notification created")
                            }
                            Err(e) => debug!("failed to insert update notification: {e}"),
                        }
                    }
                }
                Ok(_) => debug!("version check: already up-to-date"),
                Err(e) => debug!("version check failed: {e}"),
            }

            tokio::select! {
                _ = token.cancelled() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)) => {}
            }
        }
    });
}
