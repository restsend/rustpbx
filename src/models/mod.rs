use anyhow::{Context, Result};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

pub mod add_rewrite_columns;
pub mod add_sip_trunk_register_columns;
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
pub mod wholesale_agent;

pub fn prepare_sqlite_database(database_url: &str) -> Result<()> {
    let Some(path_part) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };

    let (path_str, _) = path_part.split_once('?').unwrap_or((path_part, ""));
    if path_str.is_empty() || path_str.starts_with(':') {
        return Ok(());
    }

    let path = std::path::Path::new(path_str);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create directory for console database at {}",
                    parent.display()
                )
            })?;
        }
    }

    if !path.exists() {
        std::fs::OpenOptions::new()
            .create(true)
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

    migration::Migrator::up(&db, None).await.map_err(|e| {
        tracing::error!("failed to run database migrations on {:?}", e);
        let msg = format!("failed to run database migrations on {database_url}: {e}");
        anyhow::anyhow!(msg)
    })?;
    Ok(db)
}
