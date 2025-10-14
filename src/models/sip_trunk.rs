use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, double_null, integer_null, json_null, pk_auto, string, string_null, text_null,
    timestamp, timestamp_null,
};
use sea_orm_migration::sea_query::ForeignKeyAction as MigrationForeignKeyAction;
use sea_query::Expr;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize)]
#[sea_orm(table_name = "rustpbx_sip_trunks")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub name: String,
    pub display_name: Option<String>,
    pub carrier: Option<String>,
    pub description: Option<String>,
    pub status: String,
    pub direction: String,
    pub sip_server: Option<String>,
    pub sip_transport: String,
    pub outbound_proxy: Option<String>,
    pub auth_username: Option<String>,
    pub auth_password: Option<String>,
    pub default_route_label: Option<String>,
    pub billing_template_id: Option<i64>,
    pub max_cps: Option<i32>,
    pub max_concurrent: Option<i32>,
    pub max_call_duration: Option<i32>,
    pub utilisation_percent: Option<f64>,
    pub warning_threshold_percent: Option<f64>,
    pub allowed_ips: Option<Json>,
    pub did_numbers: Option<Json>,
    pub billing_snapshot: Option<Json>,
    pub analytics: Option<Json>,
    pub tags: Option<Json>,
    pub is_active: bool,
    pub metadata: Option<Json>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub last_health_check_at: Option<DateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::bill_template::Entity",
        from = "Column::BillingTemplateId",
        to = "super::bill_template::Column::Id",
        on_delete = "SetNull",
        on_update = "Cascade"
    )]
    BillTemplate,
}

impl Related<super::bill_template::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::BillTemplate.def()
    }
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
                    .col(string(Column::Name).char_len(120))
                    .col(string_null(Column::DisplayName).char_len(160))
                    .col(string_null(Column::Carrier).char_len(160))
                    .col(text_null(Column::Description))
                    .col(string(Column::Status).char_len(32).default("healthy"))
                    .col(
                        string(Column::Direction)
                            .char_len(32)
                            .default("bidirectional"),
                    )
                    .col(string_null(Column::SipServer).char_len(160))
                    .col(string(Column::SipTransport).char_len(16).default("udp"))
                    .col(string_null(Column::OutboundProxy).char_len(160))
                    .col(string_null(Column::AuthUsername).char_len(160))
                    .col(string_null(Column::AuthPassword).char_len(160))
                    .col(string_null(Column::DefaultRouteLabel).char_len(160))
                    .col(integer_null(Column::BillingTemplateId))
                    .col(integer_null(Column::MaxCps))
                    .col(integer_null(Column::MaxConcurrent))
                    .col(integer_null(Column::MaxCallDuration))
                    .col(double_null(Column::UtilisationPercent))
                    .col(double_null(Column::WarningThresholdPercent))
                    .col(json_null(Column::AllowedIps))
                    .col(json_null(Column::DidNumbers))
                    .col(json_null(Column::BillingSnapshot))
                    .col(json_null(Column::Analytics))
                    .col(json_null(Column::Tags))
                    .col(boolean(Column::IsActive).default(true))
                    .col(json_null(Column::Metadata))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .col(timestamp_null(Column::LastHealthCheckAt))
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_trunk_bill_template")
                            .from(Entity, Column::BillingTemplateId)
                            .to(
                                super::bill_template::Entity,
                                super::bill_template::Column::Id,
                            )
                            .on_delete(MigrationForeignKeyAction::SetNull)
                            .on_update(MigrationForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_sip_trunks_name")
                    .table(Entity)
                    .col(Column::Name)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_sip_trunks_status")
                    .table(Entity)
                    .col(Column::Status)
                    .col(Column::IsActive)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_sip_trunks_direction")
                    .table(Entity)
                    .col(Column::Direction)
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
