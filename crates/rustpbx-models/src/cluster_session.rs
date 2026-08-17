use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::*;
use serde::{Deserialize, Serialize};

/// Distributed session-location registry row.
///
/// One row per live call across the cluster.  Written at session birth,
/// refreshed in bulk by the owning node's `NodeHeartbeat`, and reclaimed by
/// the SWEA sweeper once `last_updated_at` exceeds the TTL (crash recovery).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "cluster_sessions")]
pub struct Model {
    /// SIP Call-ID or proxy session id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub call_id: String,
    /// Owning PBX node, e.g. `"10.0.0.2:5060"`.
    pub node_id: String,
    pub caller: String,
    pub callee: String,
    /// `"inbound"` | `"outbound"`.
    pub direction: String,
    pub started_at: DateTimeUtc,
    /// Refreshed by the node heartbeat; the sweeper deletes rows older than
    /// the TTL using the `idx_cluster_sessions_updated` index.
    pub last_updated_at: DateTimeUtc,
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
                    .col(string(Column::CallId).string_len(200).primary_key())
                    .col(string(Column::NodeId).string_len(64))
                    .col(string(Column::Caller).string_len(160))
                    .col(string(Column::Callee).string_len(160))
                    .col(string(Column::Direction).string_len(16))
                    .col(
                        timestamp(Column::StartedAt).default(Expr::current_timestamp()),
                    )
                    .col(
                        timestamp(Column::LastUpdatedAt).default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .table(Entity)
                    .name("idx_cluster_sessions_updated")
                    .col(Column::LastUpdatedAt)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .table(Entity)
                    .name("idx_cluster_sessions_node")
                    .col(Column::NodeId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Entity).to_owned())
            .await
    }
}
