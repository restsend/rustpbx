use anyhow::{Context, Result};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

pub mod bill_template;
pub mod call_record;
pub mod department;
pub mod extension;
pub mod migration;
pub mod routing;
pub mod sip_trunk;
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
    prepare_sqlite_database(database_url)?;
    let db = Database::connect(database_url)
        .await
        .with_context(|| format!("failed to connect admin database: {}", database_url))?;

    migration::Migrator::up(&db, None)
        .await
        .context("failed to run database migrations")?;
    Ok(db)
}
