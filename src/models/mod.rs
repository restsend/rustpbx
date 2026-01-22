use anyhow::{Context, Result};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use sqlx::Connection;
use url::Url;

pub mod add_rewrite_columns;
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

async fn prepare_mysql_database(database_url: &str) -> Result<()> {
    let url = Url::parse(database_url)?;

    let database_name = url.path().trim_start_matches('/');
    if database_name.is_empty() {
        return Err(anyhow::anyhow!("No database specified"));
    }

    let mut server_url = url.clone();
    server_url.set_path("/mysql");
    server_url.set_query(None);
    let server_url_str = server_url.to_string();

    let mut conn = sqlx::MySqlConnection::connect(&server_url_str)
        .await
        .with_context(|| format!("failed to connect to MySQL server: {}", server_url_str))?;

    sqlx::query(&format!(
        "CREATE DATABASE IF NOT EXISTS `{}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
        database_name
    ))
    .execute(&mut conn)
    .await
    .with_context(|| format!("failed to create database: {}", database_name))?;

    conn.close().await?;
    Ok(())
}

pub async fn create_db(database_url: &str) -> Result<DatabaseConnection> {
    if database_url.starts_with("sqlite://") {
        prepare_sqlite_database(database_url)?;
    } else if database_url.starts_with("mysql://") || database_url.starts_with("mysqlx://") {
        prepare_mysql_database(database_url).await?;
    }

    let db = Database::connect(database_url)
        .await
        .with_context(|| format!("failed to connect admin database: {}", database_url))?;

    migration::Migrator::up(&db, None)
        .await
        .context("failed to run database migrations")?;
    Ok(db)
}
