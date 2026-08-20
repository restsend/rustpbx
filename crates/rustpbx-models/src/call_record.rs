use sea_orm::Set;
use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, integer, integer_null, json_null, string_len, string_len_null, text_null,
    timestamp_with_time_zone as timestamp, timestamp_with_time_zone_null as timestamp_null,
};
use sea_orm_migration::sea_query::{ColumnDef, ForeignKeyAction as MigrationForeignKeyAction};
use sea_query::Expr;
use serde::{Deserialize, Serialize};

/// Update the recording URL and duration for a call record after it has been
/// inserted. This is used by the async upload hook which runs in the background
/// so it does not block the call flow.
pub async fn update_recording_url(
    db: &DatabaseConnection,
    call_id: &str,
    recording_url: &str,
    duration_secs: i32,
) -> anyhow::Result<()> {
    use sea_orm::EntityTrait;
    let record = Entity::find()
        .filter(Column::CallId.eq(call_id))
        .one(db)
        .await?;
    if let Some(record) = record {
        let mut active: ActiveModel = record.into();
        active.recording_url = Set(Some(recording_url.to_string()));
        active.recording_duration_secs = Set(Some(duration_secs));
        active.updated_at = Set(chrono::Utc::now());
        Entity::update(active).exec(db).await?;
    }
    Ok(())
}

pub fn extract_sip_username(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(uri) = rsipstack::sip::Uri::try_from(trimmed) {
        if let Some(user) = uri.user() {
            let value = user.to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }

        if trimmed.starts_with("sip:") || trimmed.starts_with("sips:") {
            let without_scheme = trimmed
                .split_once(':')
                .map(|(_, rest)| rest)
                .unwrap_or(trimmed);
            let candidate = without_scheme.split('@').next().unwrap_or_default().trim();
            if !candidate.is_empty() {
                return Some(candidate.to_string());
            }
        }
    }

    let mut candidate = trimmed.split('@').next().unwrap_or(trimmed).trim();
    if let Some(stripped) = candidate.strip_prefix("tel:") {
        candidate = stripped;
    }
    if let Some(stripped) = candidate.strip_prefix("sip:") {
        candidate = stripped;
    }
    if let Some(stripped) = candidate.strip_prefix("sips:") {
        candidate = stripped;
    }
    if candidate.is_empty() {
        None
    } else {
        Some(candidate.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_sip_username_full_uri() {
        assert_eq!(
            extract_sip_username("sip:alice@192.168.1.1:5060"),
            Some("alice".to_string())
        );
        assert_eq!(
            extract_sip_username("sips:bob@example.com"),
            Some("bob".to_string())
        );
        assert_eq!(
            extract_sip_username("sip:+8613800138000@10.0.0.1"),
            Some("+8613800138000".to_string())
        );
    }

    #[test]
    fn extract_sip_username_plain_number() {
        assert_eq!(extract_sip_username("1001"), Some("1001".to_string()));
        assert_eq!(
            extract_sip_username("+8613800138000"),
            Some("+8613800138000".to_string())
        );
    }

    #[test]
    fn extract_sip_username_tel_uri() {
        assert_eq!(extract_sip_username("tel:12345"), Some("12345".to_string()));
    }

    #[test]
    fn extract_sip_username_empty_and_whitespace() {
        assert_eq!(extract_sip_username(""), None);
        assert_eq!(extract_sip_username("   "), None);
    }

    #[test]
    fn extract_sip_username_with_whitespace_trim() {
        assert_eq!(
            extract_sip_username("  sip:alice@example.com  "),
            Some("alice".to_string())
        );
    }
}

pub fn normalize_endpoint_uri(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "rustpbx_call_records")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub call_id: String,
    /// Root session id of the whole logical call — first INVITE's Call-ID,
    /// inherited by every child leg (queue dispatch, REFER transfer,
    /// consultation). Indexed (non-unique) for call-tree lookup.
    pub session_id: Option<String>,
    pub display_id: Option<String>,
    pub direction: String,
    pub status: String,
    pub started_at: DateTimeUtc,
    pub ended_at: Option<DateTimeUtc>,
    pub duration_secs: i32,
    pub from_number: Option<String>,
    pub to_number: Option<String>,
    pub caller_name: Option<String>,
    pub agent_name: Option<String>,
    pub queue: Option<String>,
    pub department_id: Option<i64>,
    pub extension_id: Option<i64>,
    pub sip_trunk_id: Option<i64>,
    pub outbound_sip_trunk_id: Option<i64>,
    pub route_id: Option<i64>,
    pub sip_gateway: Option<String>,
    pub rewrite_original_from: Option<String>,
    pub rewrite_original_to: Option<String>,
    pub caller_uri: Option<String>,
    pub callee_uri: Option<String>,
    pub recording_url: Option<String>,
    pub recording_duration_secs: Option<i32>,
    pub has_transcript: bool,
    pub transcript_status: String,
    pub transcript_language: Option<String>,
    pub tags: Option<Json>,
    pub leg_timeline: Option<Json>,
    pub metadata: Option<Json>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub archived_at: Option<DateTimeUtc>,
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
                    .col(
                        ColumnDef::new(Column::Id)
                            .big_integer()
                            .primary_key()
                            .auto_increment(),
                    )
                    .col(string_len(Column::CallId, 120))
                    .col(string_len_null(Column::SessionId, 120))
                    .col(string_len_null(Column::DisplayId, 120))
                    .col(string_len(Column::Direction, 16))
                    .col(string_len(Column::Status, 32))
                    .col(timestamp(Column::StartedAt).default(Expr::current_timestamp()))
                    .col(timestamp_null(Column::EndedAt))
                    .col(integer(Column::DurationSecs).not_null().default(0))
                    .col(string_len_null(Column::FromNumber, 64))
                    .col(string_len_null(Column::ToNumber, 64))
                    .col(string_len_null(Column::CallerName, 160))
                    .col(string_len_null(Column::AgentName, 160))
                    .col(string_len_null(Column::Queue, 120))
                    .col(ColumnDef::new(Column::DepartmentId).big_integer().null())
                    .col(ColumnDef::new(Column::ExtensionId).big_integer().null())
                    .col(ColumnDef::new(Column::SipTrunkId).big_integer().null())
                    .col(ColumnDef::new(Column::RouteId).big_integer().null())
                    .col(string_len_null(Column::SipGateway, 160))
                    .col(string_len_null(Column::RewriteOriginalFrom, 128))
                    .col(string_len_null(Column::RewriteOriginalTo, 128))
                    .col(text_null(Column::CallerUri))
                    .col(text_null(Column::CalleeUri))
                    .col(string_len_null(Column::RecordingUrl, 255))
                    .col(integer_null(Column::RecordingDurationSecs))
                    .col(boolean(Column::HasTranscript).default(false))
                    .col(string_len(Column::TranscriptStatus, 32).default("pending"))
                    .col(string_len_null(Column::TranscriptLanguage, 16))
                    .col(json_null(Column::Tags))
                    .col(json_null(Column::LegTimeline))
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

        if !manager
            .has_index("rustpbx_call_records", "idx_rustpbx_call_records_call_id")
            .await?
        {
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
        }

        if !manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_session_id",
            )
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_session_id")
                        .table(Entity)
                        .col(Column::SessionId)
                        .to_owned(),
                )
                .await?;
        }

        if !manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_started_at",
            )
            .await?
        {
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
        }

        if !manager
            .has_index("rustpbx_call_records", "idx_rustpbx_call_records_status")
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_status")
                        .table(Entity)
                        .col(Column::Status)
                        .to_owned(),
                )
                .await?;
        }

        if !manager
            .has_index("rustpbx_call_records", "idx_rustpbx_call_records_extension")
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_extension")
                        .table(Entity)
                        .col(Column::ExtensionId)
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
