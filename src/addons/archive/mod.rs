use crate::addons::{Addon, SidebarItem};
use crate::app::AppState;
use async_trait::async_trait;
use axum::{
    Extension, Router,
    routing::{get, post},
};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::time;
use tracing::{error, info};

mod handlers;

#[derive(Debug, Clone)]
pub struct ManualTaskStatus {
    pub status: String, // "running", "success", "error"
    pub archived: usize,
    pub total: usize,
    pub message: String,
    /// Set when status transitions to "success" or "error"
    pub completed_at: Option<std::time::Instant>,
}

#[derive(Clone)]
pub struct ArchiveState {
    pub last_run: Arc<RwLock<Option<DateTime<Utc>>>>,
    pub config: Arc<RwLock<Option<crate::config::ArchiveConfig>>>,
    pub manual_tasks: Arc<RwLock<HashMap<String, Arc<RwLock<ManualTaskStatus>>>>>,
}

pub struct ArchiveAddon {
    state: ArchiveState,
}

impl ArchiveAddon {
    pub fn new() -> Self {
        Self {
            state: ArchiveState {
                last_run: Arc::new(RwLock::new(None)),
                config: Arc::new(RwLock::new(None)),
                manual_tasks: Arc::new(RwLock::new(HashMap::new())),
            },
        }
    }

    /// Archive records within a specific date range [start_date, end_date).
    /// Updates `task_status` as work progresses.
    pub async fn perform_archive_range(
        db: &sea_orm::DatabaseConnection,
        start_date: NaiveDate,
        end_date: NaiveDate,
        archive_dir: &str,
        task_status: Arc<RwLock<ManualTaskStatus>>,
    ) -> anyhow::Result<()> {
        use crate::models::call_record;
        use flate2::Compression;
        use sea_orm::PaginatorTrait;

        // Count total records for progress tracking
        let start_dt: DateTime<Utc> =
            DateTime::from_naive_utc_and_offset(start_date.and_hms_opt(0, 0, 0).unwrap(), Utc);
        let end_dt: DateTime<Utc> =
            DateTime::from_naive_utc_and_offset(end_date.and_hms_opt(0, 0, 0).unwrap(), Utc);

        let total = call_record::Entity::find()
            .filter(call_record::Column::StartedAt.gte(start_dt))
            .filter(call_record::Column::StartedAt.lt(end_dt))
            .count(db)
            .await? as usize;

        {
            let mut s = task_status.write().unwrap();
            s.total = total;
            s.message = format!("Found {} records to archive", total);
        }

        if total == 0 {
            let mut s = task_status.write().unwrap();
            s.status = "success".to_string();
            s.message = "No records found in the selected date range.".to_string();
            s.completed_at = Some(std::time::Instant::now());
            return Ok(());
        }

        let mut current_date = start_date;
        let mut total_archived = 0usize;

        while current_date < end_date {
            let next_date = current_date + chrono::Duration::days(1);

            let day_start: DateTime<Utc> = DateTime::from_naive_utc_and_offset(
                current_date.and_hms_opt(0, 0, 0).unwrap(),
                Utc,
            );
            let day_end: DateTime<Utc> =
                DateTime::from_naive_utc_and_offset(next_date.and_hms_opt(0, 0, 0).unwrap(), Utc);

            let batch_size: u64 = 1000;
            let mut last_id: i64 = 0;
            let date_str = current_date.format("%Y-%m-%d").to_string();
            let filename = format!("{}/{}-callrecords.gz", archive_dir, date_str);
            let tmp_filename = format!("{}.tmp", filename);

            if let Some(parent) = std::path::Path::new(&filename).parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            let file = std::fs::File::create(&tmp_filename)?;
            let encoder = flate2::write::GzEncoder::new(file, Compression::default());
            let mut wtr = csv::Writer::from_writer(encoder);
            let mut day_archived = 0usize;

            loop {
                let batch = call_record::Entity::find()
                    .filter(call_record::Column::StartedAt.gte(day_start))
                    .filter(call_record::Column::StartedAt.lt(day_end))
                    .filter(call_record::Column::Id.gt(last_id))
                    .order_by_asc(call_record::Column::Id)
                    .limit(batch_size)
                    .all(db)
                    .await?;

                if batch.is_empty() {
                    break;
                }

                let batch_ids: Vec<i64> = batch.iter().map(|r| r.id).collect();
                last_id = *batch_ids.last().unwrap();

                for record in &batch {
                    wtr.serialize(record)?;
                }
                day_archived += batch.len();
                total_archived += batch.len();

                call_record::Entity::delete_many()
                    .filter(call_record::Column::Id.is_in(batch_ids))
                    .exec(db)
                    .await?;

                // Update progress
                {
                    let mut s = task_status.write().unwrap();
                    s.archived = total_archived;
                    s.message = format!(
                        "Processing {}: archived {} / {} records",
                        date_str, total_archived, s.total
                    );
                }
            }

            wtr.into_inner()?.finish()?;

            if day_archived > 0 {
                tokio::fs::rename(&tmp_filename, &filename).await?;
                // Write sidecar count file
                let count_path = format!("{}.count", filename);
                let _ = tokio::fs::write(&count_path, day_archived.to_string()).await;
                info!("Archived {} records for {}", day_archived, date_str);
            } else {
                let _ = tokio::fs::remove_file(&tmp_filename).await;
            }

            current_date = next_date;
        }

        {
            let mut s = task_status.write().unwrap();
            s.status = "success".to_string();
            s.archived = total_archived;
            s.message = format!("Completed. Archived {} records.", total_archived);
            s.completed_at = Some(std::time::Instant::now());
        }

        Ok(())
    }

    async fn run_scheduler(state: AppState, archive_state: ArchiveState) {
        let mut interval = time::interval(time::Duration::from_secs(60));
        loop {
            interval.tick().await;

            let archive_config = {
                let guard = archive_state.config.read().unwrap();
                match &*guard {
                    Some(c) => c.clone(),
                    None => continue,
                }
            };

            if !archive_config.enabled {
                continue;
            }

            let timezone: Tz = match archive_config.timezone.as_deref().unwrap_or("UTC").parse() {
                Ok(tz) => tz,
                Err(e) => {
                    error!("Invalid timezone in archive config: {}", e);
                    continue;
                }
            };

            let now = Utc::now().with_timezone(&timezone);
            let archive_time =
                match NaiveTime::parse_from_str(&archive_config.archive_time, "%H:%M") {
                    Ok(t) => t,
                    Err(e) => {
                        error!("Invalid archive_time format (expected HH:MM): {}", e);
                        continue;
                    }
                };

            // Check if it's time to run
            // We want to run if current time is >= archive_time and we haven't run today yet.
            // Or simpler: just check if HH:MM matches. But we might miss it if the loop is slow.
            // Better: Check if we have run today.

            let last_run = *archive_state.last_run.read().unwrap();
            let should_run = match last_run {
                Some(last) => {
                    let last_local = last.with_timezone(&timezone);
                    last_local.date_naive() < now.date_naive() && now.time() >= archive_time
                }
                None => now.time() >= archive_time,
            };

            if should_run {
                info!("Starting scheduled archive job");
                let archive_dir = state.config().archive_dir();
                if let Err(e) =
                    Self::perform_archive(state.db(), &archive_config, &archive_dir).await
                {
                    error!("Archive job failed: {}", e);
                } else {
                    info!("Archive job completed successfully");
                    *archive_state.last_run.write().unwrap() = Some(Utc::now());
                }
            }
        }
    }

    pub async fn perform_archive(
        db: &sea_orm::DatabaseConnection,
        config: &crate::config::ArchiveConfig,
        archive_dir: &str,
    ) -> anyhow::Result<()> {
        use crate::models::call_record;
        use flate2::Compression;

        let archive_after_days = config.archive_after_days as i64;

        // Determine the cutoff date for archiving:
        // - If archive_after_days > 0: archive records older than that many days
        // - If archive_after_days == 0: archive records from yesterday (previous day)
        let cutoff_date = if archive_after_days > 0 {
            Utc::now() - Duration::days(archive_after_days)
        } else {
            // Archive yesterday's records (from midnight yesterday to midnight today)
            let today = Utc::now().date_naive();
            let yesterday = today - chrono::Duration::days(1);
            DateTime::from_naive_utc_and_offset(yesterday.and_hms_opt(0, 0, 0).unwrap(), Utc)
        };

        // Find records to archive
        // We want to archive records strictly older than retention_days
        // And maybe group them by day?
        // The requirement says: "archive/{date}-callrecords.gz"
        // If we run this daily, we can just query all records < cutoff.
        // But if we have many days of backlog, we might want to split them into multiple files?
        // "archive/{date}-callrecords.gz" implies one file per date.
        // So we should find all distinct dates < cutoff.

        // For simplicity, let's just take all records < cutoff and put them in one file named with today's date?
        // No, "archive/{date}-callrecords.gz" usually means the date of the records.
        // So we should iterate over days.

        // Let's find the oldest record.
        let oldest_record = call_record::Entity::find()
            .filter(call_record::Column::StartedAt.lt(cutoff_date))
            .order_by_asc(call_record::Column::StartedAt)
            .one(db)
            .await?;

        if oldest_record.is_none() {
            info!("No records to archive");
            return Ok(());
        }

        // Iterate from oldest record date up to cutoff date
        let mut current_date = oldest_record.unwrap().started_at.date_naive();
        let cutoff_date_naive = cutoff_date.date_naive();

        while current_date < cutoff_date_naive {
            let next_date = current_date + chrono::Duration::days(1);

            let start_of_day: DateTime<Utc> = DateTime::from_naive_utc_and_offset(
                current_date.and_hms_opt(0, 0, 0).unwrap(),
                Utc,
            );
            let end_of_day: DateTime<Utc> =
                DateTime::from_naive_utc_and_offset(next_date.and_hms_opt(0, 0, 0).unwrap(), Utc);

            // --- cursor-based batching to avoid loading entire day into memory ---
            let batch_size: u64 = 1000;
            let mut last_id: i64 = 0;
            let mut total_archived = 0usize;
            let date_str = current_date.format("%Y-%m-%d").to_string();
            let filename = format!("{}/{}-callrecords.gz", archive_dir, date_str);

            // We build a single compressed file for the day, writing in batches.
            // Use a temp path while writing, then rename on success.
            let tmp_filename = format!("{}.tmp", filename);

            // Create archive directory if not exists
            if let Some(parent) = std::path::Path::new(&filename).parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            let file = std::fs::File::create(&tmp_filename)?;
            let encoder = flate2::write::GzEncoder::new(file, Compression::default());
            let mut wtr = csv::Writer::from_writer(encoder);

            loop {
                let batch = call_record::Entity::find()
                    .filter(call_record::Column::StartedAt.gte(start_of_day))
                    .filter(call_record::Column::StartedAt.lt(end_of_day))
                    .filter(call_record::Column::Id.gt(last_id))
                    .order_by_asc(call_record::Column::Id)
                    .limit(batch_size)
                    .all(db)
                    .await?;

                if batch.is_empty() {
                    break;
                }

                let batch_ids: Vec<i64> = batch.iter().map(|r| r.id).collect();
                last_id = *batch_ids.last().unwrap();

                for record in &batch {
                    wtr.serialize(record)?;
                }
                total_archived += batch.len();

                // Delete this batch from DB immediately to keep memory bounded
                call_record::Entity::delete_many()
                    .filter(call_record::Column::Id.is_in(batch_ids))
                    .exec(db)
                    .await?;
            }

            // Drop writer to flush+finish the gzip stream
            wtr.into_inner()?.finish()?;

            if total_archived > 0 {
                // Rename tmp file to final name
                tokio::fs::rename(&tmp_filename, &filename).await?;
                // Write sidecar count file
                let count_path = format!("{}.count", filename);
                let _ = tokio::fs::write(&count_path, total_archived.to_string()).await;
                info!(
                    "Archived {} records for {} to {}",
                    total_archived, date_str, filename
                );
            } else {
                // No records for this day, remove the empty tmp file
                let _ = tokio::fs::remove_file(&tmp_filename).await;
            }

            current_date = next_date;
        }

        Ok(())
    }
}

#[async_trait]
impl Addon for ArchiveAddon {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn id(&self) -> &'static str {
        "archive"
    }
    fn name(&self) -> &'static str {
        "Archive"
    }
    fn description(&self) -> &'static str {
        "Archive call records"
    }
    fn screenshots(&self) -> Vec<&'static str> {
        vec![]
    }
    async fn initialize(&self, state: AppState) -> anyhow::Result<()> {
        // Initialize config from AppState
        if let Some(c) = &state.config().archive {
            *self.state.config.write().unwrap() = Some(c.clone());
        }

        let archive_state = self.state.clone();
        crate::utils::spawn(async move {
            Self::run_scheduler(state, archive_state).await;
        });
        Ok(())
    }

    fn router(&self, state: AppState) -> Option<Router> {
        // Get base_path and api_prefix from console config if available
        let (base_path, api_prefix) = state
            .console
            .as_ref()
            .map(|c| (c.base_path().to_string(), c.api_prefix().to_string()))
            .unwrap_or_else(|| ("/console".to_string(), "/api".to_string()));

        // Re-creating router with middleware logic
        let mut protected = Router::new()
            .route(&format!("{}/archive", base_path), get(handlers::ui_index))
            .route(
                &format!("{}/archive/list", api_prefix),
                get(handlers::list_archives),
            )
            .route(
                &format!("{}/archive/delete", api_prefix),
                post(handlers::delete_archive),
            )
            .route(
                &format!("{}/archive/config", api_prefix),
                post(handlers::update_config),
            )
            .route(
                &format!("{}/archive/count", api_prefix),
                get(handlers::count_records),
            )
            .route(
                &format!("{}/archive/manual", api_prefix),
                post(handlers::manual_archive),
            )
            .route(
                &format!("{}/archive/task/{{task_id}}", api_prefix),
                get(handlers::task_status),
            );

        #[cfg(feature = "console")]
        if let Some(console_state) = state.console.clone() {
            protected = protected.route_layer(axum::middleware::from_extractor_with_state::<
                crate::console::middleware::AuthRequired,
                std::sync::Arc<crate::console::ConsoleState>,
            >(console_state));
        }

        Some(
            protected
                .with_state(state)
                .layer(Extension(self.state.clone())),
        )
    }

    fn locales_dir(&self) -> Option<String> {
        let dev = "src/addons/archive/locales";
        let deployed = "locales/archive";
        if std::path::Path::new(dev).exists() {
            Some(dev.to_string())
        } else {
            Some(deployed.to_string())
        }
    }

    fn sidebar_items(&self, state: AppState) -> Vec<SidebarItem> {
        let base_path = state
            .console
            .as_ref()
            .map(|c| c.base_path().to_string())
            .unwrap_or_else(|| "/console".to_string());
        vec![SidebarItem {
            name: "Archive".to_string(),
            name_key: Some("archive.sidebar_name".to_string()),
            url: format!("{}/archive", base_path),
            icon: r#"<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-5">
  <path stroke-linecap="round" stroke-linejoin="round" d="m20.25 7.5-.625 10.632a2.25 2.25 0 0 1-2.247 2.118H6.622a2.25 2.25 0 0 1-2.247-2.118L3.75 7.5M10 11.25h4M3.375 7.5h17.25c.621 0 1.125-.504 1.125-1.125v-1.5c0-.621-.504-1.125-1.125-1.125H3.375c-.621 0-1.125.504-1.125 1.125v1.5c0 .621.504 1.125 1.125 1.125Z" />
</svg>
"#.to_string(),
            permission: None,
        }]
    }
}
