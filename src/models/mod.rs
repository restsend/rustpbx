use anyhow::{Context, Result};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

pub mod add_leg_timeline_column;
pub mod add_metadata_column;
pub mod add_rewrite_columns;
pub mod add_sip_trunk_register_columns;
pub mod add_sip_trunk_rewrite_hostport;
pub mod add_user_mfa_columns;
pub mod call_record;

pub mod call_record_dashboard_index;
pub mod call_record_from_number_index;
pub mod call_record_indices;
pub mod call_record_optimization_indices;
pub mod department;
pub mod extension;
pub mod extension_department;
pub mod frequency_limit;
pub mod migration;
pub mod policy;
pub mod presence;
pub mod rbac;
pub mod routing;
pub mod sip_trunk;
pub mod system_notification;
pub mod user;

pub fn prepare_sqlite_database(database_url: &str) -> Result<()> {
    let Some(path_part) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };

    let (path_str, _) = path_part.split_once('?').unwrap_or((path_part, ""));
    if path_str.is_empty() || path_str.starts_with(':') {
        return Ok(());
    }

    let path = std::path::Path::new(path_str);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create directory for console database at {}",
                    parent.display()
                )
            })?;
        }

    if !path.exists() {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .with_context(|| {
                format!(
                    "failed to create console database file at {}",
                    path.display()
                )
            })?;
    }

    Ok(())
}

pub async fn connect_db(database_url: &str) -> Result<DatabaseConnection> {
    if database_url.starts_with("sqlite://") {
        prepare_sqlite_database(database_url).map_err(|e| {
            tracing::error!("failed to prepare SQLite database {database_url} {:?}", e);
            let msg = format!("failed to prepare SQLite database {database_url}: {e}");
            anyhow::anyhow!(msg)
        })?;
    }

    Database::connect(database_url)
        .await
        .map_err(|e: sea_orm::DbErr| {
            tracing::error!("failed to connect to database {:?}", e);
            let msg = format!("failed to connect to database {database_url}: {e}");
            anyhow::anyhow!(msg)
        })
}

pub async fn create_db(database_url: &str) -> Result<DatabaseConnection> {
    if database_url.starts_with("sqlite://") {
        prepare_sqlite_database(database_url).map_err(|e| {
            tracing::error!("failed to prepare SQLite database {database_url} {:?}", e);
            let msg = format!("failed to prepare SQLite database {database_url}: {e}");
            anyhow::anyhow!(msg)
        })?;
    }

    let db = Database::connect(database_url)
        .await
        .map_err(|e: sea_orm::DbErr| {
            tracing::error!("failed to connect to database {:?}", e);
            let msg = format!("failed to connect to database {database_url}: {e}");
            anyhow::anyhow!(msg)
        })?;

    if let Err(e) = migration::Migrator::up(&db, None).await {
        let msg = e.to_string();
        // sea-orm errors when a migration version exists in the tracking table but no
        // corresponding MigrationTrait is registered (e.g. after moving a migration to
        // an addon-specific migrator). These rows are already applied and their tables
        // exist, so it is safe to ignore the error and proceed.
        if msg.contains("is missing, this migration has been applied but its file is missing") {
            tracing::warn!(
                "some previously-applied migrations are no longer registered in the core \
                 migrator (likely moved to an addon); skipping: {msg}"
            );
        } else {
            tracing::error!("failed to run database migrations on {:?}", e);
            return Err(anyhow::anyhow!(
                "failed to run database migrations on {database_url}: {e}"
            ));
        }
    }
    Ok(db)
}
