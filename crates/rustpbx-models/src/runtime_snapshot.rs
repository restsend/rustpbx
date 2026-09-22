use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::*;
use serde::{Deserialize, Serialize};

/// Generic runtime-metrics snapshot (`rustpbx_runtime_snapshots`).
///
/// ONE table for all periodic live-state sampling so future domains never
/// need another migration: `kind` discriminates the source, the numeric
/// columns carry the common counters and `data` holds structured extras
/// (e.g. by-transport maps, per-trunk details).
///
/// Kinds in use:
/// - `locator` — num1 = online_locations, num2 = online_users,
///   num3 = webrtc_locations, data = by_transport map.
/// - `system_capacity` — num1 = active calls, num2 = max concurrency.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "rustpbx_runtime_snapshots")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    /// Snapshot source discriminator (locator / system_capacity / ...).
    pub kind: String,
    pub created_at: DateTimeUtc,
    pub num1: i64,
    pub num2: i64,
    pub num3: i64,
    pub num4: f64,
    pub data: Option<Json>,
    pub instance_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Entity)
                    .if_not_exists()
                    .col(big_integer(Column::Id).primary_key().auto_increment())
                    .col(string(Column::Kind).char_len(32))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(big_integer(Column::Num1).default(0))
                    .col(big_integer(Column::Num2).default(0))
                    .col(big_integer(Column::Num3).default(0))
                    .col(double(Column::Num4).default(0.0))
                    .col(json_null(Column::Data))
                    .col(string_null(Column::InstanceId).char_len(64))
                    .to_owned(),
            )
            .await?;
        // MySQL does not support CREATE INDEX IF NOT EXISTS.
        // A previous interrupted migration may already have created the index.
        if !manager.has_index("rustpbx_runtime_snapshots", "idx_runtime_snapshots_kind_time").await? {
            manager
                .create_index(
                    Index::create()
                        .if_not_exists()
                        .table(Entity)
                        .name("idx_runtime_snapshots_kind_time")
                        .col(Column::Kind)
                        .col(Column::CreatedAt)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Entity).to_owned())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, Database, NotSet, Set};

    #[tokio::test]
    async fn snapshot_roundtrip() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migration.up(&SchemaManager::new(&db)).await.unwrap();
        ActiveModel {
            id: NotSet,
            kind: Set("locator".to_string()),
            created_at: Set(chrono::Utc::now()),
            num1: Set(12),
            num2: Set(9),
            num3: Set(3),
            num4: Set(0.0),
            data: Set(Some(serde_json::json!({"UDP": 9, "WSS": 3}))),
            instance_id: Set(Some("n1".to_string())),
        }
        .insert(&db)
        .await
        .unwrap();
        let rows = Entity::find().all(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].num1, 12);
        assert_eq!(
            rows[0].data.as_ref().unwrap().get("WSS").and_then(|v| v.as_i64()),
            Some(3)
        );
    }
}
