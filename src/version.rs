use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

pub fn get_version_info() -> &'static str {
    let version = env!("CARGO_PKG_VERSION");
    let build_time = env!("BUILD_TIME");
    let git_commit = env!("GIT_COMMIT_HASH");
    let git_branch = env!("GIT_BRANCH");
    let git_dirty = env!("GIT_DIRTY");

    let build_timestamp: i64 = build_time.parse().unwrap_or(0);
    let build_datetime: DateTime<Local> = DateTime::from_timestamp(build_timestamp, 0)
        .map(|utc| utc.with_timezone(&Local))
        .unwrap_or_else(Local::now);
    let build_time_str = build_datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string();

    Box::leak(
        format!(
            "rustpbx {} ({})\n\
         Build Time: {}\n\
         Git Commit: {}\n\
         Git Branch: {}\n\
         Git Status: {}",
            version,
            rsipstack::VERSION,
            build_time_str,
            git_commit,
            git_branch,
            git_dirty
        )
        .into_boxed_str(),
    )
}

pub fn get_short_version() -> &'static str {
    let version = env!("CARGO_PKG_VERSION");
    let git_commit = env!("GIT_COMMIT_HASH");
    let git_dirty = env!("GIT_DIRTY");
    let commercial = if cfg!(feature = "commerce") {
        "commerce"
    } else {
        "community"
    };
    if git_dirty == "dirty" {
        Box::leak(format!("{}-{}-dirty-{commercial}", version, git_commit).into_boxed_str())
    } else {
        Box::leak(format!("{}-{}-{commercial}", version, git_commit).into_boxed_str())
    }
}

pub fn get_useragent() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let build_time = env!("BUILD_TIME");

    let build_timestamp: i64 = build_time.parse().unwrap_or(0);
    let build_datetime: DateTime<Local> = DateTime::from_timestamp(build_timestamp, 0)
        .map(|utc| utc.with_timezone(&Local))
        .unwrap_or_else(Local::now);
    let build_time_str = build_datetime.format("%Y-%m-%d").to_string();
    format!(
        "rustpbx/{} (built {} {})",
        version,
        build_time_str,
        rsipstack::VERSION
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

/// Query `https://miuda.ai/api/check_update` with current version + edition.
/// Returns `UpdateInfo` on success.
pub async fn check_update() -> anyhow::Result<UpdateInfo> {
    let version = env!("CARGO_PKG_VERSION");
    let edition = if cfg!(feature = "commerce") {
        "commerce"
    } else {
        "community"
    };
    let client = reqwest::Client::new();
    let resp = client
        .get("https://miuda.ai/api/check_update")
        .query(&[("version", version), ("edition", edition)])
        .header("User-Agent", get_useragent())
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;
    let info: UpdateInfo = resp.json().await?;
    Ok(info)
}

/// Spawn a background task that periodically checks for updates (at startup and
/// every 24 hours).  When a new version is found a `system_notification` row is
/// inserted into the database (deduped by title so the same version only appears
/// once).
pub fn spawn_update_checker(
    db: sea_orm::DatabaseConnection,
    token: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            match check_update().await {
                Ok(info) if info.has_update => {
                    use crate::models::system_notification::{ActiveModel, Column, Entity};
                    use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

                    let title = format!("New version available: {}", info.latest_version);
                    let exists = Entity::find()
                        .filter(Column::Title.eq(&title))
                        .one(&db)
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
                        match am.insert(&db).await {
                            Ok(_) => info!(latest = %info.latest_version, "update notification created"),
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
