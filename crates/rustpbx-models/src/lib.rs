use anyhow::{Context, Result};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod add_leg_timeline_column;
pub mod add_metadata_column;
pub mod cluster_session;
pub mod add_outbound_sip_trunk_id;
pub mod add_rewrite_columns;
pub mod add_sip_trunk_register_columns;
pub mod add_sip_trunk_rewrite_hostport;
pub mod add_user_mfa_columns;
pub mod alter_rewrite_columns_length;
pub mod call_record;
pub mod call_record_dashboard_index;
pub mod call_record_from_number_index;
pub mod call_record_indices;
pub mod call_record_optimization_indices;
pub mod config_entry;
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

fn default_max_connections() -> u32 {
    64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabasePoolConfig {
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default)]
    pub min_connections: Option<u32>,
    #[serde(default)]
    pub acquire_timeout_secs: Option<u64>,
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
    #[serde(default)]
    pub max_lifetime_secs: Option<u64>,
}

impl Default for DatabasePoolConfig {
    fn default() -> Self {
        Self {
            max_connections: default_max_connections(),
            min_connections: None,
            acquire_timeout_secs: None,
            idle_timeout_secs: None,
            max_lifetime_secs: None,
        }
    }
}

pub async fn prepare_sqlite_database(database_url: &str) -> Result<()> {
    let Some(path_part) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };

    let (path_str, _) = path_part.split_once('?').unwrap_or((path_part, ""));
    if path_str.is_empty() || path_str.starts_with(':') {
        return Ok(());
    }

    let path = std::path::Path::new(path_str);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "failed to create directory for console database at {}",
                parent.display()
            )
        })?;
    }

    if !tokio::fs::try_exists(path).await? {
        tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .await
            .with_context(|| {
                format!(
                    "failed to create console database file at {}",
                    path.display()
                )
            })?;
    }

    Ok(())
}

fn apply_pool_config(pool_config: Option<&DatabasePoolConfig>, opt: &mut ConnectOptions) {
    let cfg = match pool_config {
        Some(c) => c,
        None => return,
    };
    opt.max_connections(cfg.max_connections);
    if let Some(v) = cfg.min_connections {
        opt.min_connections(v);
    }
    if let Some(v) = cfg.acquire_timeout_secs {
        opt.acquire_timeout(Duration::from_secs(v));
    }
    if let Some(v) = cfg.idle_timeout_secs {
        opt.idle_timeout(Duration::from_secs(v));
    }
    if let Some(v) = cfg.max_lifetime_secs {
        opt.max_lifetime(Duration::from_secs(v));
    }
}

pub async fn connect_db(
    database_url: &str,
    pool_config: Option<&DatabasePoolConfig>,
) -> Result<DatabaseConnection> {
    if database_url.starts_with("sqlite://") {
        prepare_sqlite_database(database_url).await.map_err(|e| {
            tracing::error!("failed to prepare SQLite database {database_url} {:?}", e);
            let msg = format!("failed to prepare SQLite database {database_url}: {e}");
            anyhow::anyhow!(msg)
        })?;
    }

    let mut opt = ConnectOptions::new(database_url.to_owned());
    apply_pool_config(pool_config, &mut opt);
    Database::connect(opt).await.map_err(|e: sea_orm::DbErr| {
        tracing::error!("failed to connect to database {:?}", e);
        let msg = format!("failed to connect to database {database_url}: {e}");
        anyhow::anyhow!(msg)
    })
}

pub async fn create_db(
    database_url: &str,
    pool_config: Option<&DatabasePoolConfig>,
) -> Result<DatabaseConnection> {
    if database_url.starts_with("sqlite://") {
        prepare_sqlite_database(database_url).await.map_err(|e| {
            tracing::error!("failed to prepare SQLite database {database_url} {:?}", e);
            let msg = format!("failed to prepare SQLite database {database_url}: {e}");
            anyhow::anyhow!(msg)
        })?;
    }

    let mut opt = ConnectOptions::new(database_url.to_owned());
    apply_pool_config(pool_config, &mut opt);
    let db = Database::connect(opt).await.map_err(|e: sea_orm::DbErr| {
        tracing::error!("failed to connect to database {:?}", e);
        let msg = format!("failed to connect to database {database_url}: {e}");
        anyhow::anyhow!(msg)
    })?;

    if let Err(e) = migration::Migrator::up(&db, None).await {
        let msg = e.to_string();
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
