use anyhow::{Context, Result};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::{MigratorTrait, seaql_migrations};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod add_leg_timeline_column;
pub mod add_metadata_column;
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
pub mod cluster_session;
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
    16
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

/// Remove applied-migration records whose migration file is no longer
/// registered in the given migrator (renamed, removed, or moved to an addon).
///
/// Each migrator owns its own tracking table (core: `seaql_migrations`,
/// addons: their own), so a record that the migrator does not register is
/// orphaned and safe to drop.  Without this, `Migrator::up` aborts on the
/// first missing file and refuses to apply any pending migration — e.g. the
/// `cluster_sessions` table would never be created.
///
/// This is deliberately conservative: it is only invoked when a migration run
/// is blocked by the missing-file error, never proactively.
pub async fn prune_stale_migrations<M: MigratorTrait>(
    db: &DatabaseConnection,
) -> Result<usize, sea_orm::DbErr> {
    use sea_orm::ConnectionTrait;
    use sea_orm::sea_query::{Expr, ExprTrait, Query};

    let table_name = M::migration_table_name();
    let registered: std::collections::HashSet<String> = M::migrations()
        .iter()
        .map(|m| m.name().to_string())
        .collect();

    let applied = M::get_migration_models(db).await?;
    let mut removed = 0usize;
    for row in applied {
        if registered.contains(&row.version) {
            continue;
        }
        let stmt = Query::delete()
            .from_table(table_name.clone())
            .and_where(Expr::col(seaql_migrations::Column::Version).eq(row.version))
            .to_owned();
        db.execute(&stmt).await?;
        removed += 1;
    }
    Ok(removed)
}

/// Run `M::up`, recovering once if sea-orm refuses to apply pending
/// migrations because the tracking table holds records whose migration files
/// are no longer registered (moved/renamed).
///
/// Recovery is lazy and targeted: nothing is touched on a healthy run, and on
/// a blocked run only the orphaned records that caused the blockage are
/// dropped before retrying.  Returns the number of orphaned records pruned.
pub async fn migrate_with_stale_recovery<M: MigratorTrait>(
    db: &DatabaseConnection,
) -> Result<usize, anyhow::Error> {
    match M::up(db, None).await {
        Ok(()) => Ok(0),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("is missing, this migration has been applied but its file is missing") {
                let removed = prune_stale_migrations::<M>(db).await.map_err(|pe| {
                    anyhow::anyhow!("failed to prune stale migration records: {pe}")
                })?;
                tracing::warn!(
                    removed,
                    "pruned stale migration records that blocked startup migrations; retrying"
                );
                M::up(db, None).await.map_err(|e| {
                    anyhow::anyhow!("database migration failed after pruning stale records: {e}")
                })?;
                Ok(removed)
            } else {
                Err(anyhow::anyhow!("failed to run database migrations: {e}"))
            }
        }
    }
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

    migrate_with_stale_recovery::<migration::Migrator>(&db)
        .await
        .map_err(|e| {
            tracing::error!("failed to run database migrations on {:?}", e);
            anyhow::anyhow!("failed to run database migrations on {database_url}: {e}")
        })?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveValue, EntityTrait};
    use sea_orm_migration::seaql_migrations;

    fn temp_db_url() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rustpbx-models-migrate-{}-{nanos}.sqlite3",
            std::process::id()
        ));
        format!("sqlite://{}", path.display())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_db_prunes_stale_records_and_still_applies_pending() {
        let url = temp_db_url();

        // First boot: full migration, including `cluster_sessions`.
        let db = create_db(&url, None).await.expect("first migrate");

        // Simulate a migration that was applied by an older core version and
        // has since moved to an addon: its record lingers in seaql_migrations.
        seaql_migrations::Entity::insert(seaql_migrations::ActiveModel {
            version: ActiveValue::Set("queue".to_owned()),
            applied_at: ActiveValue::Set(0),
        })
        .exec(&db)
        .await
        .expect("insert stale record");

        // Second boot: without pruning, `Migrator::up` aborts on the missing
        // `queue` file and refuses to apply pending migrations; with pruning
        // the stale record is dropped and the pending migrations still run.
        let db2 = create_db(&url, None).await.expect("second migrate");

        let applied = migration::Migrator::get_migration_models(&db2)
            .await
            .expect("read applied migrations");
        assert!(
            !applied.iter().any(|m| m.version == "queue"),
            "stale record should be pruned"
        );

        // The pending cluster_session migration still got applied.
        cluster_session::Entity::find()
            .all(&db2)
            .await
            .expect("cluster_sessions table exists");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prune_stale_migrations_keeps_registered_records() {
        let db = create_db(&temp_db_url(), None).await.expect("migrate");
        let removed = prune_stale_migrations::<migration::Migrator>(&db)
            .await
            .expect("prune");
        assert_eq!(removed, 0, "fresh db has no stale records");

        let registered: Vec<String> = migration::Migrator::migrations()
            .iter()
            .map(|m| m.name().to_string())
            .collect();
        let applied = migration::Migrator::get_migration_models(&db)
            .await
            .expect("read applied");
        assert_eq!(registered.len(), applied.len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrate_with_stale_recovery_is_lazy_on_healthy_db() {
        let db = create_db(&temp_db_url(), None).await.expect("migrate");
        let applied_before = migration::Migrator::get_migration_models(&db)
            .await
            .expect("read applied");

        // Healthy run: no recovery, no pruning, tracking table untouched.
        let pruned = migrate_with_stale_recovery::<migration::Migrator>(&db)
            .await
            .expect("migrate");
        assert_eq!(pruned, 0, "no stale records to prune on a healthy db");

        let applied_after = migration::Migrator::get_migration_models(&db)
            .await
            .expect("read applied");
        assert_eq!(applied_before.len(), applied_after.len());
    }
}
