use sea_orm::Set;
use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, integer, integer_null, json_null, string, string_null, text_null, timestamp,
    timestamp_null,
};
use sea_orm_migration::sea_query::{ColumnDef, ForeignKeyAction as MigrationForeignKeyAction};
use sea_query::Expr;
use serde::{Deserialize, Serialize};

use crate::callrecord::{CallRecord, CallRecordHook};

// CallRecordPersistArgs removed

pub struct DatabaseHook {
    pub db: DatabaseConnection,
}

#[async_trait::async_trait]
impl CallRecordHook for DatabaseHook {
    async fn on_record_completed(&self, record: &mut CallRecord) -> anyhow::Result<()> {
        persist_call_record(&self.db, record).await
    }
}

#[allow(unused_variables)]
pub async fn persist_call_record(
    db: &DatabaseConnection,
    record: &mut CallRecord,
) -> anyhow::Result<()> {
    let details = &record.details;

    let direction = details.direction.trim().to_ascii_lowercase();
    let status = details.status.trim().to_ascii_lowercase();
    let from_number = details.from_number.clone();
    let to_number = details.to_number.clone();
    let caller_name = details.caller_name.clone();
    let agent_name = details.agent_name.clone();
    let queue = details.queue.clone();
    let department_id = details.department_id;
    let extension_id = details.extension_id;
    let sip_trunk_id = details.sip_trunk_id;
    let route_id = details.route_id;
    let sip_gateway = details.sip_gateway.clone();

    // Derived from trace info if not explicit (but here we just use what's in details.rewrite)
    // Note: older PersistArgs had these as Option<String>, but Rewrite struct has them as String.
    // If they are empty strings, we might want to store None or Some("")?
    // Let's assume "" is valid or check if empty.
    let rewrite_original_from = if !details.rewrite.caller_original.is_empty() {
        Some(details.rewrite.caller_original.clone())
    } else {
        None
    };
    let rewrite_original_to = if !details.rewrite.callee_original.is_empty() {
        Some(details.rewrite.callee_original.clone())
    } else {
        None
    };

    let recording_url = details
        .recording_url
        .clone()
        .or_else(|| record.recorder.first().map(|media| media.path.clone()));
    let recording_duration_secs = details.recording_duration_secs;
    let has_transcript = details.has_transcript;
    let transcript_status = details.transcript_status.clone();
    let transcript_language = details.transcript_language.clone();
    let tags = details.tags.clone();
    let duration_secs = (record.end_time - record.start_time).num_seconds().max(0) as i32;

    let caller_uri = normalize_endpoint_uri(&record.caller);
    let callee_uri = normalize_endpoint_uri(&record.callee);

    let transcript_status_str = transcript_status
        .clone()
        .unwrap_or_else(|| "none".to_string());

    let active = ActiveModel {
        call_id: Set(record.call_id.clone()),
        display_id: Set(None),
        direction: Set(direction.clone()),
        status: Set(status.clone()),
        started_at: Set(record.start_time),
        ended_at: Set(Some(record.end_time)),
        duration_secs: Set(duration_secs),
        from_number: Set(from_number.clone()),
        to_number: Set(to_number.clone()),
        caller_name: Set(caller_name.clone()),
        agent_name: Set(agent_name.clone()),
        queue: Set(queue.clone()),
        department_id: Set(department_id),
        extension_id: Set(extension_id),
        sip_trunk_id: Set(sip_trunk_id),
        route_id: Set(route_id),
        sip_gateway: Set(sip_gateway.clone()),
        rewrite_original_from: Set(rewrite_original_from),
        rewrite_original_to: Set(rewrite_original_to),
        caller_uri: Set(caller_uri.clone()),
        callee_uri: Set(callee_uri.clone()),
        recording_url: Set(recording_url.clone()),
        recording_duration_secs: Set(recording_duration_secs),
        has_transcript: Set(has_transcript),
        transcript_status: Set(transcript_status_str),
        transcript_language: Set(transcript_language.clone()),
        tags: Set(tags.clone()),
        created_at: Set(record.start_time),
        updated_at: Set(record.end_time),
        archived_at: Set(None),
        ..Default::default()
    };

    Entity::insert(active)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(Column::CallId)
                .update_columns([
                    Column::DisplayId,
                    Column::Direction,
                    Column::Status,
                    Column::StartedAt,
                    Column::EndedAt,
                    Column::DurationSecs,
                    Column::FromNumber,
                    Column::ToNumber,
                    Column::CallerName,
                    Column::AgentName,
                    Column::Queue,
                    Column::DepartmentId,
                    Column::ExtensionId,
                    Column::SipTrunkId,
                    Column::RouteId,
                    Column::SipGateway,
                    Column::RewriteOriginalFrom,
                    Column::RewriteOriginalTo,
                    Column::CallerUri,
                    Column::CalleeUri,
                    Column::RecordingUrl,
                    Column::RecordingDurationSecs,
                    Column::HasTranscript,
                    Column::TranscriptStatus,
                    Column::TranscriptLanguage,
                    Column::Tags,
                    Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(db)
        .await?;

    Ok(())
}

pub fn extract_sip_username(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(uri) = rsip::Uri::try_from(trimmed) {
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

fn normalize_endpoint_uri(value: &str) -> Option<String> {
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
                    .col(string(Column::CallId).char_len(120))
                    .col(string_null(Column::DisplayId).char_len(120))
                    .col(string(Column::Direction).char_len(16))
                    .col(string(Column::Status).char_len(32))
                    .col(timestamp(Column::StartedAt).default(Expr::current_timestamp()))
                    .col(timestamp_null(Column::EndedAt))
                    .col(integer(Column::DurationSecs).not_null().default(0))
                    .col(string_null(Column::FromNumber).char_len(64))
                    .col(string_null(Column::ToNumber).char_len(64))
                    .col(string_null(Column::CallerName).char_len(160))
                    .col(string_null(Column::AgentName).char_len(160))
                    .col(string_null(Column::Queue).char_len(120))
                    .col(ColumnDef::new(Column::DepartmentId).big_integer().null())
                    .col(ColumnDef::new(Column::ExtensionId).big_integer().null())
                    .col(ColumnDef::new(Column::SipTrunkId).big_integer().null())
                    .col(ColumnDef::new(Column::RouteId).big_integer().null())
                    .col(string_null(Column::SipGateway).char_len(160))
                    .col(string_null(Column::RewriteOriginalFrom).char_len(64))
                    .col(string_null(Column::RewriteOriginalTo).char_len(64))
                    .col(text_null(Column::CallerUri))
                    .col(text_null(Column::CalleeUri))
                    .col(string_null(Column::RecordingUrl).char_len(255))
                    .col(integer_null(Column::RecordingDurationSecs))
                    .col(boolean(Column::HasTranscript).default(false))
                    .col(
                        string(Column::TranscriptStatus)
                            .char_len(32)
                            .default("pending"),
                    )
                    .col(string_null(Column::TranscriptLanguage).char_len(16))
                    .col(json_null(Column::Tags))
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
                    .if_not_exists()
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
                    .if_not_exists()
                    .name("idx_rustpbx_call_records_status")
                    .table(Entity)
                    .col(Column::Status)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
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

impl Into<CallRecord> for Model {
    fn into(self) -> CallRecord {
        let details = crate::callrecord::CallDetails {
            direction: self.direction,
            status: self.status,
            from_number: self.from_number.clone(),
            to_number: self.to_number.clone(),
            caller_name: self.caller_name,
            agent_name: self.agent_name,
            queue: self.queue,
            department_id: self.department_id,
            extension_id: self.extension_id,
            sip_trunk_id: self.sip_trunk_id,
            route_id: self.route_id,
            sip_gateway: self.sip_gateway,
            recording_url: self.recording_url,
            recording_duration_secs: self.recording_duration_secs,
            has_transcript: self.has_transcript,
            transcript_status: Some(self.transcript_status),
            transcript_language: self.transcript_language,
            tags: self.tags,
            rewrite: crate::callrecord::CallRecordRewrite {
                caller_original: self.rewrite_original_from.unwrap_or_default(),
                caller_final: String::new(),
                callee_original: self.rewrite_original_to.unwrap_or_default(),
                callee_final: String::new(),
                contact: None,
                destination: None,
            },
            last_error: None,
        };

        CallRecord {
            call_id: self.call_id,
            start_time: self.started_at,
            ring_time: None,   // No ring_time in Model
            answer_time: None, // No answer_time in Model
            end_time: self.ended_at.unwrap_or(self.started_at),
            caller: self
                .caller_uri
                .unwrap_or_else(|| self.from_number.unwrap_or_default()),
            callee: self
                .callee_uri
                .unwrap_or_else(|| self.to_number.unwrap_or_default()),
            status_code: 0,                                  // No status_code in Model
            hangup_reason: None,                             // No hangup_reason in Model
            hangup_messages: Vec::new(),                     // No hangup_messages in Model
            recorder: Vec::new(),                            // No recorder list in Model
            sip_leg_roles: std::collections::HashMap::new(), // No sip_leg_roles in Model
            details,
            extensions: http::Extensions::new(),
        }
    }
}
