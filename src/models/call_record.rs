use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, double, integer, json_null, pk_auto, string, string_null, timestamp, timestamp_null,
};
use sea_orm_migration::sea_query::ForeignKeyAction as MigrationForeignKeyAction;
use sea_query::Expr;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "rustpbx_call_records")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub call_id: String,
    pub display_id: Option<String>,
    pub direction: String,
    pub status: String,
    pub started_at: DateTime,
    pub ended_at: Option<DateTime>,
    pub duration_secs: i32,
    pub from_number: Option<String>,
    pub to_number: Option<String>,
    pub caller_name: Option<String>,
    pub agent_name: Option<String>,
    pub queue: Option<String>,
    pub department_id: Option<i64>,
    pub extension_id: Option<i64>,
    pub sip_trunk_id: Option<i64>,
    pub route_id: Option<i64>,
    pub sip_gateway: Option<String>,
    pub recording_url: Option<String>,
    pub recording_duration_secs: Option<i32>,
    pub has_transcript: bool,
    pub transcript_status: String,
    pub transcript_language: Option<String>,
    pub tags: Option<Json>,
    pub quality_mos: Option<f64>,
    pub quality_latency_ms: Option<f64>,
    pub quality_jitter_ms: Option<f64>,
    pub quality_packet_loss_percent: Option<f64>,
    pub analytics: Option<Json>,
    pub metadata: Option<Json>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub archived_at: Option<DateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::department::Entity",
        from = "Column::DepartmentId",
        to = "super::department::Column::Id",
        on_delete = "SetNull",
        on_update = "Cascade"
    )]
    Department,
    #[sea_orm(
        belongs_to = "super::extension::Entity",
        from = "Column::ExtensionId",
        to = "super::extension::Column::Id",
        on_delete = "SetNull",
        on_update = "Cascade"
    )]
    Extension,
    #[sea_orm(
        belongs_to = "super::sip_trunk::Entity",
        from = "Column::SipTrunkId",
        to = "super::sip_trunk::Column::Id",
        on_delete = "SetNull",
        on_update = "Cascade"
    )]
    SipTrunk,
    #[sea_orm(
        belongs_to = "super::routing::Entity",
        from = "Column::RouteId",
        to = "super::routing::Column::Id",
        on_delete = "SetNull",
        on_update = "Cascade"
    )]
    Route,
}

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
                    .col(pk_auto(Column::Id))
                    .col(string(Column::CallId).char_len(120))
                    .col(string_null(Column::DisplayId).char_len(120))
                    .col(string(Column::Direction).char_len(16))
                    .col(string(Column::Status).char_len(32))
                    .col(timestamp(Column::StartedAt))
                    .col(timestamp_null(Column::EndedAt))
                    .col(integer(Column::DurationSecs).not_null().default(0))
                    .col(string_null(Column::FromNumber).char_len(64))
                    .col(string_null(Column::ToNumber).char_len(64))
                    .col(string_null(Column::CallerName).char_len(160))
                    .col(string_null(Column::AgentName).char_len(160))
                    .col(string_null(Column::Queue).char_len(120))
                    .col(integer(Column::DepartmentId).null())
                    .col(integer(Column::ExtensionId).null())
                    .col(integer(Column::SipTrunkId).null())
                    .col(integer(Column::RouteId).null())
                    .col(string_null(Column::SipGateway).char_len(160))
                    .col(string_null(Column::RecordingUrl).char_len(255))
                    .col(integer(Column::RecordingDurationSecs).null())
                    .col(boolean(Column::HasTranscript).default(false))
                    .col(
                        string(Column::TranscriptStatus)
                            .char_len(32)
                            .default("pending"),
                    )
                    .col(string_null(Column::TranscriptLanguage).char_len(16))
                    .col(json_null(Column::Tags))
                    .col(double(Column::QualityMos).null())
                    .col(double(Column::QualityLatencyMs).null())
                    .col(double(Column::QualityJitterMs).null())
                    .col(double(Column::QualityPacketLossPercent).null())
                    .col(json_null(Column::Analytics))
                    .col(json_null(Column::Metadata))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .col(timestamp_null(Column::ArchivedAt))
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_call_records_department")
                            .from(Entity, Column::DepartmentId)
                            .to(super::department::Entity, super::department::Column::Id)
                            .on_delete(MigrationForeignKeyAction::SetNull)
                            .on_update(MigrationForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_call_records_extension")
                            .from(Entity, Column::ExtensionId)
                            .to(super::extension::Entity, super::extension::Column::Id)
                            .on_delete(MigrationForeignKeyAction::SetNull)
                            .on_update(MigrationForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_call_records_sip_trunk")
                            .from(Entity, Column::SipTrunkId)
                            .to(super::sip_trunk::Entity, super::sip_trunk::Column::Id)
                            .on_delete(MigrationForeignKeyAction::SetNull)
                            .on_update(MigrationForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_call_records_route")
                            .from(Entity, Column::RouteId)
                            .to(super::routing::Entity, super::routing::Column::Id)
                            .on_delete(MigrationForeignKeyAction::SetNull)
                            .on_update(MigrationForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_call_records_call_id")
                    .table(Entity)
                    .col(Column::CallId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_call_records_started_at")
                    .table(Entity)
                    .col(Column::StartedAt)
                    .col(Column::Direction)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_call_records_status")
                    .table(Entity)
                    .col(Column::Status)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_call_records_extension")
                    .table(Entity)
                    .col(Column::ExtensionId)
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
