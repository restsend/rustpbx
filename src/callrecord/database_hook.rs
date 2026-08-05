use sea_orm::{
    DatabaseConnection, EntityTrait, Set,
};

use crate::callrecord::{CallRecord, CallRecordHook};

pub struct DatabaseHook {
    pub db: DatabaseConnection,
}

#[async_trait::async_trait]
impl CallRecordHook for DatabaseHook {
    async fn on_record_completed(&self, record: &mut CallRecord) -> anyhow::Result<()> {
        persist_call_record(&self.db, record).await
    }
}

/// Null out a FK id when the referenced row no longer exists (e.g. an
/// auto-created trunk / extension that was never persisted, or was deleted
/// before the CDR landed). Without this, the INSERT fails with a SQLite
/// `FOREIGN KEY constraint failed` (code 787) and the call record is lost.
///
/// If the referent table cannot be queried (DB hiccup / table missing), keep
/// the id — the FK would not be enforceable anyway, and we must not lose the
/// call record because of a verification query.
macro_rules! fk_id_or_none {
    ($db:expr, $entity:path, $id:expr) => {{
        let result: Result<Option<i64>, sea_orm::DbErr> = match $id {
            Some(id) if id > 0 => {
                match <$entity>::find_by_id(id).one($db).await {
                    Ok(Some(_)) => Ok(Some(id)),
                    Ok(None) => Ok(None),
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            "call record FK referent lookup failed; keeping id {id}"
                        );
                        Ok(Some(id))
                    }
                }
            }
            _ => Ok(None),
        };
        result
    }};
}

pub async fn persist_call_record(
    db: &DatabaseConnection,
    record: &CallRecord,
) -> anyhow::Result<()> {
    use rustpbx_models::call_record::{ActiveModel, Column, Entity};

    let details = &record.details;

    let direction = details.direction.trim().to_ascii_lowercase();
    let status = details.status.trim().to_ascii_lowercase();
    let from_number = details.from_number.clone();
    let to_number = details.to_number.clone();
    let caller_name = details.caller_name.clone();
    let agent_name = details.agent_name.clone();
    let queue = details.queue.clone();
    // Validate FK referents before insert so stale in-memory ids cannot fail
    // the write (see `fk_id_or_none`).
    let department_id =
        fk_id_or_none!(db, rustpbx_models::department::Entity, details.department_id)?;
    let extension_id =
        fk_id_or_none!(db, rustpbx_models::extension::Entity, details.extension_id)?;
    let sip_trunk_id =
        fk_id_or_none!(db, rustpbx_models::sip_trunk::Entity, details.sip_trunk_id)?;
    let route_id = fk_id_or_none!(db, rustpbx_models::routing::Entity, details.route_id)?;
    let sip_gateway = details.sip_gateway.clone();

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
    let duration_secs =
        std::cmp::Ord::max((record.end_time - record.start_time).num_seconds(), 0) as i32;

    let caller_uri = rustpbx_models::call_record::normalize_endpoint_uri(&record.caller);
    let callee_uri = rustpbx_models::call_record::normalize_endpoint_uri(&record.callee);

    let transcript_status_str = transcript_status
        .clone()
        .unwrap_or_else(|| "none".to_string());

    let leg_timeline_json = if record.leg_timeline.is_empty() {
        None
    } else {
        serde_json::to_value(&record.leg_timeline).ok()
    };

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
        outbound_sip_trunk_id: Set(details.outbound_sip_trunk_id),
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
        leg_timeline: Set(leg_timeline_json),
        metadata: Set({
            let mut m = details.metadata.clone().unwrap_or_default();
            if !record.sip_leg_roles.is_empty() {
                let json = serde_json::to_string(&record.sip_leg_roles).unwrap_or_default();
                m.insert("sip_leg_roles".to_string(), json);
            }
            serde_json::to_value(&m).ok()
        }),
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
                    Column::OutboundSipTrunkId,
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
                    Column::LegTimeline,
                    Column::Metadata,
                    Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(db)
        .await?;

    Ok(())
}
