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
use serde_json::{Map as JsonMap, Number as JsonNumber, Value, json};

use crate::callrecord::{CallRecord, CallRecordExtras, CallRecordHook};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecordPersistArgs {
    pub direction: String,
    pub status: String,
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
    pub transcript_status: Option<String>,
    pub transcript_language: Option<String>,
    pub tags: Option<Value>,
    pub analytics: Option<Value>,
    pub metadata: Option<Value>,
}

impl Default for CallRecordPersistArgs {
    fn default() -> Self {
        Self {
            direction: "unknown".to_string(),
            status: "failed".to_string(),
            from_number: None,
            to_number: None,
            caller_name: None,
            agent_name: None,
            queue: None,
            department_id: None,
            extension_id: None,
            sip_trunk_id: None,
            route_id: None,
            sip_gateway: None,
            recording_url: None,
            recording_duration_secs: None,
            has_transcript: false,
            transcript_status: None,
            transcript_language: None,
            tags: None,
            analytics: None,
            metadata: None,
        }
    }
}

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
    // Ensure call_id is sanitized before database persistence
    record.call_id = crate::utils::sanitize_id(&record.call_id);

    // Sanitize leg IDs in sip_leg_roles for database signaling column
    if !record.sip_leg_roles.is_empty() {
        let old_roles = std::mem::take(&mut record.sip_leg_roles);
        for (id, role) in old_roles {
            record
                .sip_leg_roles
                .insert(crate::utils::sanitize_id(&id), role);
        }
    }

    let args = match record.extensions.get::<CallRecordPersistArgs>().cloned() {
        Some(args) => args,
        None => {
            return Ok(());
        }
    };

    let direction = args.direction.trim().to_ascii_lowercase();
    let status = args.status.trim().to_ascii_lowercase();
    let from_number = args.from_number.clone();
    let to_number = args.to_number.clone();
    let caller_name = args.caller_name.clone();
    let agent_name = args.agent_name.clone();
    let queue = args.queue.clone();
    let department_id = args.department_id;
    let extension_id = args.extension_id;
    let sip_trunk_id = args.sip_trunk_id;
    let route_id = args.route_id;
    let sip_gateway = args.sip_gateway.clone();
    let recording_url = args
        .recording_url
        .clone()
        .or_else(|| record.recorder.first().map(|media| media.path.clone()));
    let recording_duration_secs = args.recording_duration_secs;
    let tags = args.tags.clone();
    let analytics = args.analytics.clone();
    let metadata_value = merge_metadata(record, args.metadata.clone());
    let signaling_value = build_signaling_payload(record);
    let duration_secs = (record.end_time - record.start_time).num_seconds().max(0) as i32;

    let caller_uri = normalize_endpoint_uri(&record.caller);
    let callee_uri = normalize_endpoint_uri(&record.callee);

    // Update record with billing info so subsequent hooks can see it
    record.extensions.insert(args.clone());

    let transcript_status = args
        .transcript_status
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
        caller_uri: Set(caller_uri.clone()),
        callee_uri: Set(callee_uri.clone()),
        recording_url: Set(recording_url.clone()),
        recording_duration_secs: Set(recording_duration_secs),
        has_transcript: Set(args.has_transcript),
        transcript_status: Set(transcript_status),
        transcript_language: Set(args.transcript_language.clone()),
        tags: Set(tags.clone()),
        analytics: Set(analytics.clone()),
        metadata: Set(metadata_value.clone()),
        signaling: Set(signaling_value.clone()),
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
                    Column::CallerUri,
                    Column::CalleeUri,
                    Column::RecordingUrl,
                    Column::RecordingDurationSecs,
                    Column::HasTranscript,
                    Column::TranscriptStatus,
                    Column::TranscriptLanguage,
                    Column::Tags,
                    Column::Analytics,
                    Column::Metadata,
                    Column::Signaling,
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

fn build_signaling_payload(record: &CallRecord) -> Option<Value> {
    let mut legs = Vec::new();

    if !record.sip_leg_roles.is_empty() {
        for (sip_call_id, role) in record.sip_leg_roles.iter() {
            let leg_record = match role.as_str() {
                "primary" => Some(record),
                _ => None,
            };

            let payload = match leg_record {
                Some(inner) => call_leg_payload(role, inner, Some(sip_call_id)),
                None => minimal_leg_payload(role, sip_call_id),
            };

            legs.push(payload);
        }
    }

    if legs.is_empty() {
        legs.push(call_leg_payload("primary", record, None));
    }

    if legs.is_empty() {
        None
    } else {
        Some(json!({
            "is_b2bua": false,
            "legs": legs,
        }))
    }
}

fn call_leg_payload(role: &str, record: &CallRecord, sip_call_id: Option<&String>) -> Value {
    let call_id = sip_call_id
        .cloned()
        .unwrap_or_else(|| record.call_id.clone());

    json!({
        "role": role,
        "call_id": call_id,
        "session_id": record.call_id.clone(),
        "caller": record.caller.clone(),
        "callee": record.callee.clone(),
        "status_code": record.status_code,
        "hangup_reason": record
            .hangup_reason
            .as_ref()
            .map(|reason| reason.to_string()),
        "start_time": record.start_time.clone(),
        "ring_time": record.ring_time.clone(),
        "answer_time": record.answer_time.clone(),
        "end_time": record.end_time.clone(),
    })
}

fn minimal_leg_payload(role: &str, sip_call_id: &str) -> Value {
    json!({
        "role": role,
        "call_id": sip_call_id,
    })
}

fn merge_metadata(record: &CallRecord, extra_metadata: Option<Value>) -> Option<Value> {
    let mut map = JsonMap::new();
    map.insert(
        "status_code".to_string(),
        Value::Number(JsonNumber::from(record.status_code)),
    );
    if let Some(ring_time) = record.ring_time {
        map.insert(
            "ring_time".to_string(),
            Value::String(ring_time.to_rfc3339()),
        );
    }
    if let Some(answer_time) = record.answer_time {
        map.insert(
            "answer_time".to_string(),
            Value::String(answer_time.to_rfc3339()),
        );
    }
    if let Some(reason) = record
        .hangup_reason
        .as_ref()
        .map(|reason| reason.to_string())
    {
        map.insert("hangup_reason".to_string(), Value::String(reason));
    }
    if !record.hangup_messages.is_empty() {
        if let Ok(value) = serde_json::to_value(&record.hangup_messages) {
            map.insert("hangup_messages".to_string(), value);
        }
    }

    if let Some(extras) = record.extensions.get::<CallRecordExtras>() {
        for (key, value) in &extras.0 {
            map.insert(key.clone(), value.clone());
        }
    }

    if let Some(Value::Object(values)) = extra_metadata {
        for (key, value) in values {
            map.insert(key, value);
        }
    } else if let Some(value) = extra_metadata {
        map.insert("extra_metadata".to_string(), value);
    }

    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
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
    pub caller_uri: Option<String>,
    pub callee_uri: Option<String>,
    pub recording_url: Option<String>,
    pub recording_duration_secs: Option<i32>,
    pub has_transcript: bool,
    pub transcript_status: String,
    pub transcript_language: Option<String>,
    pub tags: Option<Json>,
    pub analytics: Option<Json>,
    pub metadata: Option<Json>,
    pub signaling: Option<Json>,
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
                    .col(json_null(Column::Analytics))
                    .col(json_null(Column::Metadata))
                    .col(json_null(Column::Signaling))
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
        CallRecord {
            call_id: self.call_id,
            start_time: self.started_at,
            ring_time: None,   // No ring_time in Model
            answer_time: None, // No answer_time in Model
            end_time: self.ended_at.unwrap_or(self.started_at),
            caller: self.from_number.unwrap_or_default(),
            callee: self.to_number.unwrap_or_default(),
            status_code: 0,                                  // No status_code in Model
            hangup_reason: None,                             // No hangup_reason in Model
            hangup_messages: Vec::new(),                     // No hangup_messages in Model
            recorder: Vec::new(),                            // No recorder list in Model
            sip_leg_roles: std::collections::HashMap::new(), // No sip_leg_roles in Model
            extensions: http::Extensions::new(),
        }
    }
}
