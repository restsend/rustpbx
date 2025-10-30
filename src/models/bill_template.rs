use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    double, integer, json_null, pk_auto, string, string_null, text_null, timestamp,
};
use sea_query::Expr;
use serde::{Deserialize, Serialize};

#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    EnumIter,
    PartialOrd,
    Ord,
    DeriveActiveEnum,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum BillingInterval {
    #[sea_orm(string_value = "daily")]
    Daily,
    #[sea_orm(string_value = "weekly")]
    Weekly,
    #[sea_orm(string_value = "monthly")]
    Monthly,
    #[sea_orm(string_value = "yearly")]
    Yearly,
}

#[derive(Clone, Debug, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "rustpbx_bill_templates")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub currency: String,
    pub billing_interval: BillingInterval,
    pub included_minutes: i32,
    pub included_messages: i32,
    pub initial_increment_secs: i32,
    pub billing_increment_secs: i32,
    pub overage_rate_per_minute: f64,
    pub setup_fee: f64,
    pub tax_percent: f64,
    pub metadata: Option<Json>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::sip_trunk::Entity")]
    SipTrunk,
}

impl Related<super::sip_trunk::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SipTrunk.def()
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
                    .col(text_null(Column::Description))
                    .col(string(Column::Currency).char_len(8).default("USD"))
                    .col(
                        string(Column::BillingInterval)
                            .char_len(32)
                            .default("monthly"),
                    )
                    .col(integer(Column::IncludedMinutes).not_null().default(0))
                    .col(integer(Column::IncludedMessages).not_null().default(0))
                    .col(integer(Column::InitialIncrementSecs).not_null().default(60))
                    .col(integer(Column::BillingIncrementSecs).not_null().default(60))
                    .col(double(Column::OverageRatePerMinute).default(0.0))
                    .col(double(Column::SetupFee).default(0.0))
                    .col(double(Column::TaxPercent).default(0.0))
                    .col(json_null(Column::Metadata))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_bill_templates_name")
                    .table(Entity)
                    .col(Column::Name)
                    .unique()
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
