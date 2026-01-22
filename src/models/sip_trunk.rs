use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, double_null, integer_null, json_null, string, string_null, text_null, timestamp,
    timestamp_null,
};
use sea_orm_migration::sea_query::ColumnDef;
use sea_query::Expr;
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum SipTrunkStatus {
    #[sea_orm(string_value = "healthy")]
    Healthy,
    #[sea_orm(string_value = "warning")]
    Warning,
    #[sea_orm(string_value = "standby")]
    Standby,
    #[sea_orm(string_value = "offline")]
    Offline,
}

impl SipTrunkStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Warning => "warning",
            Self::Standby => "standby",
            Self::Offline => "offline",
        }
    }
}

impl Default for SipTrunkStatus {
    fn default() -> Self {
        Self::Healthy
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum SipTrunkDirection {
    #[sea_orm(string_value = "inbound")]
    Inbound,
    #[sea_orm(string_value = "outbound")]
    Outbound,
    #[sea_orm(string_value = "bidirectional")]
    Bidirectional,
}

impl SipTrunkDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
            Self::Bidirectional => "bidirectional",
        }
    }
}

impl Default for SipTrunkDirection {
    fn default() -> Self {
        Self::Bidirectional
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum SipTransport {
    #[sea_orm(string_value = "udp")]
    Udp,
    #[sea_orm(string_value = "tcp")]
    Tcp,
    #[sea_orm(string_value = "tls")]
    Tls,
}

impl SipTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
            Self::Tls => "tls",
        }
    }
}

impl Default for SipTransport {
    fn default() -> Self {
        Self::Udp
    }
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, Default)]
#[sea_orm(table_name = "rustpbx_sip_trunks")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub name: String,
    pub display_name: Option<String>,
    pub carrier: Option<String>,
    pub description: Option<String>,
    pub status: SipTrunkStatus,
    pub direction: SipTrunkDirection,
    pub sip_server: Option<String>,
    pub sip_transport: SipTransport,
    pub outbound_proxy: Option<String>,
    pub auth_username: Option<String>,
    pub auth_password: Option<String>,
    pub default_route_label: Option<String>,
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
    pub incoming_from_user_prefix: Option<String>,
    pub incoming_to_user_prefix: Option<String>,
    pub is_active: bool,
    pub metadata: Option<Json>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub last_health_check_at: Option<DateTimeUtc>,
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
                    .col(
                        ColumnDef::new(Column::Id)
                            .big_integer()
                            .primary_key()
                            .auto_increment(),
                    )
                    .col(string(Column::Name).char_len(120))
                    .col(string_null(Column::DisplayName).char_len(160))
                    .col(string_null(Column::Carrier).char_len(160))
                    .col(text_null(Column::Description))
                    .col(
                        string(Column::Status)
                            .char_len(32)
                            .default(SipTrunkStatus::default().as_str()),
                    )
                    .col(
                        string(Column::Direction)
                            .char_len(32)
                            .default(SipTrunkDirection::default().as_str()),
                    )
                    .col(string_null(Column::SipServer).char_len(160))
                    .col(
                        string(Column::SipTransport)
                            .char_len(16)
                            .default(SipTransport::default().as_str()),
                    )
                    .col(string_null(Column::OutboundProxy).char_len(160))
                    .col(string_null(Column::AuthUsername).char_len(160))
                    .col(string_null(Column::AuthPassword).char_len(160))
                    .col(string_null(Column::DefaultRouteLabel).char_len(160))
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
                    .col(string_null(Column::IncomingFromUserPrefix).char_len(160))
                    .col(string_null(Column::IncomingToUserPrefix).char_len(160))
                    .col(boolean(Column::IsActive).default(true))
                    .col(json_null(Column::Metadata))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .col(timestamp_null(Column::LastHealthCheckAt))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
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
                    .if_not_exists()
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
                    .if_not_exists()
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
