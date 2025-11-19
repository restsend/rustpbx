use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, integer, json_null, string, string_null, text_null, timestamp, timestamp_null,
};
use sea_orm_migration::sea_query::{ColumnDef, ForeignKeyAction as MigrationForeignKeyAction};
use sea_query::Expr;
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum RoutingDirection {
    #[sea_orm(string_value = "inbound")]
    Inbound,
    #[sea_orm(string_value = "outbound")]
    Outbound,
}

impl RoutingDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        }
    }
}

impl Default for RoutingDirection {
    fn default() -> Self {
        Self::Outbound
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum RoutingSelectionStrategy {
    #[sea_orm(string_value = "rr")]
    #[serde(alias = "rr", alias = "round_robin", alias = "round-robin")]
    RoundRobin,
    #[sea_orm(string_value = "weight")]
    #[serde(alias = "weight")]
    Weighted,
    #[sea_orm(string_value = "hash")]
    Hash,
}

impl RoutingSelectionStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoundRobin => "rr",
            Self::Weighted => "weight",
            Self::Hash => "hash",
        }
    }
}

impl Default for RoutingSelectionStrategy {
    fn default() -> Self {
        Self::RoundRobin
    }
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "rustpbx_routes")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub name: String,
    pub description: Option<String>,
    pub direction: RoutingDirection,
    pub priority: i32,
    pub is_active: bool,
    pub selection_strategy: RoutingSelectionStrategy,
    pub hash_key: Option<String>,
    pub source_trunk_id: Option<i64>,
    pub default_trunk_id: Option<i64>,
    pub source_pattern: Option<String>,
    pub destination_pattern: Option<String>,
    pub header_filters: Option<Json>,
    pub rewrite_rules: Option<Json>,
    pub target_trunks: Option<Json>,
    pub owner: Option<String>,
    pub notes: Option<Json>,
    pub metadata: Option<Json>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub last_deployed_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::sip_trunk::Entity",
        from = "Column::SourceTrunkId",
        to = "super::sip_trunk::Column::Id",
        on_delete = "SetNull",
        on_update = "Cascade"
    )]
    SourceTrunk,
    #[sea_orm(
        belongs_to = "super::sip_trunk::Entity",
        from = "Column::DefaultTrunkId",
        to = "super::sip_trunk::Column::Id",
        on_delete = "SetNull",
        on_update = "Cascade"
    )]
    DefaultTrunk,
}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use super::RoutingSelectionStrategy;

    #[test]
    fn selection_strategy_accepts_aliases() {
        let rr: RoutingSelectionStrategy = serde_json::from_str("\"rr\"").unwrap();
        assert!(matches!(rr, RoutingSelectionStrategy::RoundRobin));

        let rr_alt: RoutingSelectionStrategy = serde_json::from_str("\"round_robin\"").unwrap();
        assert!(matches!(rr_alt, RoutingSelectionStrategy::RoundRobin));

        let weight: RoutingSelectionStrategy = serde_json::from_str("\"weight\"").unwrap();
        assert!(matches!(weight, RoutingSelectionStrategy::Weighted));
    }

    #[test]
    fn selection_strategy_serializes_with_canonical_names() {
        let serialized = serde_json::to_string(&RoutingSelectionStrategy::RoundRobin).unwrap();
        assert_eq!(serialized, "\"roundrobin\"");

        let serialized_weight = serde_json::to_string(&RoutingSelectionStrategy::Weighted).unwrap();
        assert_eq!(serialized_weight, "\"weighted\"");
    }
}

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
                    .col(
                        ColumnDef::new(Column::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(Column::Name).char_len(160))
                    .col(text_null(Column::Description))
                    .col(
                        string(Column::Direction)
                            .char_len(32)
                            .default(RoutingDirection::default().as_str()),
                    )
                    .col(integer(Column::Priority).not_null().default(100))
                    .col(boolean(Column::IsActive).default(true))
                    .col(
                        string(Column::SelectionStrategy)
                            .char_len(32)
                            .default(RoutingSelectionStrategy::default().as_str()),
                    )
                    .col(string_null(Column::HashKey).char_len(120))
                    .col(ColumnDef::new(Column::SourceTrunkId).big_integer().null())
                    .col(ColumnDef::new(Column::DefaultTrunkId).big_integer().null())
                    .col(string_null(Column::SourcePattern).char_len(160))
                    .col(string_null(Column::DestinationPattern).char_len(160))
                    .col(json_null(Column::HeaderFilters))
                    .col(json_null(Column::RewriteRules))
                    .col(json_null(Column::TargetTrunks))
                    .col(string_null(Column::Owner).char_len(120))
                    .col(json_null(Column::Notes))
                    .col(json_null(Column::Metadata))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .col(timestamp_null(Column::LastDeployedAt))
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_routes_source_trunk")
                            .from(Entity, Column::SourceTrunkId)
                            .to(super::sip_trunk::Entity, super::sip_trunk::Column::Id)
                            .on_delete(MigrationForeignKeyAction::SetNull)
                            .on_update(MigrationForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_routes_default_trunk")
                            .from(Entity, Column::DefaultTrunkId)
                            .to(super::sip_trunk::Entity, super::sip_trunk::Column::Id)
                            .on_delete(MigrationForeignKeyAction::SetNull)
                            .on_update(MigrationForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_routes_name")
                    .table(Entity)
                    .col(Column::Name)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_routes_direction")
                    .table(Entity)
                    .col(Column::Direction)
                    .col(Column::IsActive)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_routes_priority")
                    .table(Entity)
                    .col(Column::Priority)
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
