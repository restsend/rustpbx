use crate::addons::transcript::models::{
    SenseVoiceCliChannel, StoredTranscript, StoredTranscriptAnalysis, StoredTranscriptSegment,
    TranscriptRequest, TranscriptSettingsUpdate,
};
use crate::callrecord::storage::CdrStorage;
use crate::console::ConsoleState;
use crate::console::handlers::call_record::{CdrData, load_cdr_data, select_recording_path};
use crate::console::handlers::utils::{
    build_sensevoice_transcribe_command, command_exists, model_file_path,
};
use crate::console::middleware::AuthRequired;
use crate::models::call_record::{
    ActiveModel as CallRecordActiveModel, Entity as CallRecordEntity, Model as CallRecordModel,
};
use anyhow::{Context as AnyhowContext, Result as AnyResult};
use axum::{
    Json,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};
use tokio::time::timeout;
use toml_edit::{DocumentMut, Item, Table, value};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct TranscriptConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub models_path: Option<String>,
    #[serde(default)]
    pub hf_endpoint: Option<String>,
    #[serde(default)]
    pub samplerate: Option<u32>,
    #[serde(default)]
    pub default_language: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

pub async fn get_call_record_transcript(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let record = match CallRecordEntity::find_by_id(pk).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!(
                id = pk,
                "failed to load call record for transcript: {}", err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let cdr_data = load_cdr_data(&state, &record).await;
    match load_stored_transcript(&record, cdr_data.as_ref()).await {
        Ok(Some(stored)) => {
            let payload = build_transcript_payload_value(&record, Some(&stored));
            Json(json!({
                "status": record.transcript_status,
                "transcript": payload,
            }))
            .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Transcript not available" })),
        )
            .into_response(),
        Err(err) => {
            warn!(call_id = %record.call_id, "failed to load stored transcript: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to load transcript" })),
            )
                .into_response()
        }
    }
}

pub async fn trigger_call_record_transcript(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<TranscriptRequest>,
) -> Response {
    let db = state.db();
    let mut record = match CallRecordEntity::find_by_id(pk).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!(
                id = pk,
                "failed to load call record for transcription: {}", err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let TranscriptRequest { force, .. } = payload;

    let app_state = match state.app_state() {
        Some(app) => app,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "message": "Application state unavailable" })),
            )
                .into_response();
        }
    };

    let transcript_cfg = get_merged_config(&app_state).await;

    let cdr_data = load_cdr_data(&state, &record).await;
    let recording_path = match select_recording_path(&record, cdr_data.as_ref()) {
        Some(path) => path,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Recording file not found for this call" })),
            )
                .into_response();
        }
    };

    if record.has_transcript && !force {
        if let Ok(Some(stored)) = load_stored_transcript(&record, cdr_data.as_ref()).await {
            let payload = build_transcript_payload_value(&record, Some(&stored));
            return Json(json!({
                "status": record.transcript_status,
                "transcript": payload,
            }))
            .into_response();
        }
    }

    if record.transcript_status.eq_ignore_ascii_case("processing") && !force {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "message": "Transcription already in progress" })),
        )
            .into_response();
    }

    let command = transcript_cfg
        .command
        .as_deref()
        .map(str::trim)
        .filter(|cmd| !cmd.is_empty())
        .unwrap_or("sensevoice-cli")
        .to_string();

    if !command_exists(&command) {
        return (
            StatusCode::FAILED_DEPENDENCY,
            Json(json!({
                "message": format!(
                    "sensevoice-cli is not available (looked for '{}'). Install via `cargo install sensevoice-cli` or configure proxy.transcript.command.",
                    command
                )
            })),
        )
            .into_response();
    }

    let models_path = match resolve_models_path(&transcript_cfg) {
        Some(path) => path,
        None => {
            return (
                StatusCode::FAILED_DEPENDENCY,
                Json(json!({
                    "message": "SenseVoice model path is not configured. Set MODEL_PATH or proxy.transcript.models_path and download the model before transcribing.",
                })),
            )
                .into_response();
        }
    };

    let model_file = model_file_path(&models_path);
    if tokio::fs::metadata(&model_file).await.is_err() {
        return (
            StatusCode::FAILED_DEPENDENCY,
            Json(json!({
                "message": format!(
                    "SenseVoice model not found at {}. Please download the model manually to this path.",
                    model_file.display()
                )
            })),
        )
            .into_response();
    }

    let storage = cdr_data.as_ref().and_then(|cdr| cdr.storage.clone());

    let transcript_storage_path = if let Some(cdr_ref) = cdr_data.as_ref() {
        let mut path = PathBuf::from(&cdr_ref.cdr_path);
        path.set_extension("transcript.json");
        path.to_string_lossy().into_owned()
    } else {
        let mut path = PathBuf::from(&recording_path);
        path.set_extension("transcript.json");
        path.to_string_lossy().into_owned()
    };

    let local_transcript_path = match storage.as_ref() {
        Some(storage_ref) => storage_ref
            .local_full_path(&transcript_storage_path)
            .unwrap_or_else(|| {
                let mut temp_path = env::temp_dir();
                temp_path.push(format!(
                    "rustpbx-{}-{}.transcript.json",
                    record.call_id,
                    Uuid::new_v4()
                ));
                temp_path
            }),
        None => PathBuf::from(&transcript_storage_path),
    };

    if let Some(parent) = local_transcript_path.parent() {
        if let Err(err) = tokio::fs::create_dir_all(parent).await {
            warn!(dir = %parent.display(), "failed to ensure transcript directory: {}", err);
        }
    }

    if tokio::fs::metadata(&local_transcript_path).await.is_ok() {
        if let Err(err) = tokio::fs::remove_file(&local_transcript_path).await {
            warn!(
                file = %local_transcript_path.display(),
                "failed to remove existing transcript before transcription: {}",
                err
            );
        }
    }

    let transcript_output_path = local_transcript_path.to_string_lossy().into_owned();

    let now = Utc::now();
    if let Err(err) = (CallRecordActiveModel {
        id: Set(record.id),
        transcript_status: Set("processing".to_string()),
        updated_at: Set(now),
        ..Default::default()
    })
    .update(db)
    .await
    {
        warn!(call_id = %record.call_id, "failed to update transcript status: {}", err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "message": "Failed to update call record" })),
        )
            .into_response();
    }
    record.transcript_status = "processing".to_string();
    record.updated_at = now;
    let mut cmd: tokio::process::Command = build_sensevoice_transcribe_command(
        &command,
        &recording_path,
        Some(models_path.as_str()),
        Some(transcript_output_path.as_str()),
    );
    let command_args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let start_instant = Instant::now();
    let output_result = if let Some(timeout_secs) = transcript_cfg.timeout_secs {
        match timeout(StdDuration::from_secs(timeout_secs), cmd.output()).await {
            Ok(result) => result,
            Err(_) => {
                warn!(call_id = %record.call_id, timeout_secs, "sensevoice-cli timed out");
                let _ = (CallRecordActiveModel {
                    id: Set(record.id),
                    transcript_status: Set("failed".to_string()),
                    updated_at: Set(Utc::now()),
                    ..Default::default()
                })
                .update(db)
                .await;
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(json!({
                        "message": format!(
                            "sensevoice-cli timed out after {} seconds",
                            timeout_secs
                        )
                    })),
                )
                    .into_response();
            }
        }
    } else {
        cmd.output().await
    };

    let output = match output_result {
        Ok(output) => output,
        Err(err) => {
            warn!(call_id = %record.call_id, "sensevoice-cli failed to execute: {}", err);
            let _ = (CallRecordActiveModel {
                id: Set(record.id),
                transcript_status: Set("failed".to_string()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            })
            .update(db)
            .await;
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "message": format!("Failed to execute sensevoice-cli: {}", err) })),
            )
                .into_response();
        }
    };

    if !output.status.success() {
        let stderr_full = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stderr_preview = stderr_full.chars().take(160).collect::<String>();
        let mut client_message = "sensevoice-cli transcription failed".to_string();
        let mut status_code = StatusCode::BAD_GATEWAY;

        if stderr_full.to_ascii_lowercase().contains("unsupported")
            && stderr_full.to_ascii_lowercase().contains("codec")
        {
            client_message = "SenseVoice CLI cannot process this recording codec. Convert the audio to PCM (16 kHz) or enable a supported codec before retrying.".to_string();
            status_code = StatusCode::UNPROCESSABLE_ENTITY;
        }
        warn!(
            call_id = %record.call_id,
            %command,
            args = %command_args.join(" "),
            code = output.status.code(),
            stderr = %stderr_preview,
            "sensevoice-cli exited with failure"
        );
        let _ = (CallRecordActiveModel {
            id: Set(record.id),
            transcript_status: Set("failed".to_string()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        })
        .update(db)
        .await;
        return (status_code, Json(json!({ "message": client_message }))).into_response();
    }

    let elapsed_secs = start_instant.elapsed().as_secs_f64();

    info!(
        call_id = %record.call_id,
        %command,
        args = %command_args.join(" "),
        elapsed_secs,
        "sensevoice-cli transcription completed successfully"
    );

    let transcript_bytes = match tokio::fs::read(&local_transcript_path).await {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!(
                call_id = %record.call_id,
                file = %local_transcript_path.display(),
                "sensevoice-cli succeeded but transcript file missing: {}",
                err
            );
            let _ = (CallRecordActiveModel {
                id: Set(record.id),
                transcript_status: Set("failed".to_string()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            })
            .update(db)
            .await;
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "message": "SenseVoice CLI did not produce transcript data" })),
            )
                .into_response();
        }
    };

    let cli_output: Vec<SenseVoiceCliChannel> = match serde_json::from_slice(&transcript_bytes) {
        Ok(parsed) => parsed,
        Err(err) => {
            let preview = String::from_utf8_lossy(&transcript_bytes)
                .chars()
                .take(160)
                .collect::<String>();
            warn!(
                call_id = %record.call_id,
                file = %local_transcript_path.display(),
                preview = %preview,
                "failed to parse sensevoice-cli output: {}",
                err
            );
            let _ = (CallRecordActiveModel {
                id: Set(record.id),
                transcript_status: Set("failed".to_string()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            })
            .update(db)
            .await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to parse transcript data" })),
            )
                .into_response();
        }
    };

    // Convert CLI output to StoredTranscript
    let mut segments = Vec::new();
    let mut full_text = String::new();
    let mut total_word_count = 0;

    for channel in cli_output {
        for segment in channel.segments {
            let text = segment.text.trim();
            if !text.is_empty() {
                if !full_text.is_empty() {
                    full_text.push(' ');
                }
                full_text.push_str(text);
                total_word_count += text.split_whitespace().count();
            }
            segments.push(StoredTranscriptSegment {
                idx: None,
                text: text.to_string(),
                start: segment.start_sec,
                end: segment.end_sec,
                channel: channel.channel,
            });
        }
    }

    let stored_transcript = StoredTranscript {
        version: 1,
        source: "sensevoice-cli".to_string(),
        generated_at: Utc::now(),
        language: None,
        duration_secs: Some(elapsed_secs),
        sample_rate: None,
        segments,
        text: full_text,
        analysis: Some(StoredTranscriptAnalysis {
            elapsed: Some(elapsed_secs),
            rtf: None, // Calculate if needed
            word_count: total_word_count,
            asr_model: Some("sensevoice-small".to_string()),
        }),
    };

    // Save transcript
    if let Some(storage_ref) = storage.as_ref() {
        let json_content = serde_json::to_string_pretty(&stored_transcript).unwrap();
        if let Err(e) = storage_ref
            .write_bytes(&transcript_storage_path, json_content.as_bytes())
            .await
        {
            warn!("Failed to upload transcript to storage: {}", e);
        }
    } else {
        let json_content = serde_json::to_string_pretty(&stored_transcript).unwrap();
        if let Err(e) = tokio::fs::write(&local_transcript_path, json_content).await {
            warn!("Failed to save transcript to disk: {}", e);
        }
    }

    // Update record
    let _ = (CallRecordActiveModel {
        id: Set(record.id),
        has_transcript: Set(true),
        transcript_status: Set("completed".to_string()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    })
    .update(db)
    .await;

    let payload = build_transcript_payload_value(&record, Some(&stored_transcript));
    Json(json!({
        "status": "completed",
        "transcript": payload,
    }))
    .into_response()
}

async fn load_stored_transcript(
    record: &CallRecordModel,
    cdr: Option<&CdrData>,
) -> AnyResult<Option<StoredTranscript>> {
    let mut candidates: Vec<String> = Vec::new();

    if let Some(cdr_data) = cdr {
        let mut fallback = PathBuf::from(&cdr_data.cdr_path);
        fallback.set_extension("transcript.json");
        candidates.push(fallback.to_string_lossy().into_owned());
    }

    candidates.retain(|candidate| !candidate.trim().is_empty());
    candidates.dedup();

    let storage = cdr.and_then(|data| data.storage.as_ref());

    for candidate in candidates {
        let path = candidate.trim();
        match read_transcript_file(storage, path).await {
            Ok(mut transcript) => {
                if transcript.source.trim().is_empty() {
                    transcript.source = "sensevoice-cli".to_string();
                }
                return Ok(Some(transcript));
            }
            Err(err) => {
                warn!(call_id = %record.call_id, file = %path, "failed to read transcript file: {}", err);
            }
        }
    }

    Ok(None)
}

fn resolve_models_path(cfg: &TranscriptConfig) -> Option<String> {
    if let Ok(env_path) = std::env::var("MODEL_PATH") {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    cfg.models_path
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

async fn read_transcript_file(
    storage: Option<&CdrStorage>,
    path: &str,
) -> AnyResult<StoredTranscript> {
    if let Some(storage_ref) = storage {
        if let Ok(content) = storage_ref.read_to_string(path).await {
            let transcript: StoredTranscript = serde_json::from_str(&content)
                .with_context(|| format!("parse transcript file {}", path))?;
            return Ok(transcript);
        }
    }

    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read transcript file {}", path))?;
    let transcript: StoredTranscript = serde_json::from_str(&content)
        .with_context(|| format!("parse transcript file {}", path))?;
    Ok(transcript)
}

// fn transcript_file_from_cdr(record: &CallRecord) -> Option<String> {
//     record
//         .extensions
//         .get::<crate::callrecord::CallRecordExtras>()
//         .and_then(|extras| extras.0.get("transcript_file"))
//         .and_then(|value| value.as_str())
//         .map(|value| value.trim().to_string())
//         .filter(|value| !value.is_empty())
// }

// fn transcript_file_from_metadata(metadata: &Option<Value>) -> Option<String> {
//     metadata
//         .as_ref()
//         .and_then(|m| m.get("transcript_file"))
//         .and_then(|v| v.as_str())
//         .map(|v| v.trim().to_string())
//         .filter(|v| !v.is_empty())
// }

fn build_transcript_payload_value(
    record: &CallRecordModel,
    transcript: Option<&StoredTranscript>,
) -> Value {
    if let Some(data) = transcript {
        json!({
            "available": true,
            "status": record.transcript_status,
            "language": data.language,
            "generated_at": data.generated_at.to_rfc3339(),
            "segments": data.segments,
            "content": data.text,
            "analysis": data.analysis,
        })
    } else {
        json!({
            "available": false,
            "status": record.transcript_status,
        })
    }
}

pub async fn get_settings(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
) -> Response {
    let app_state = match state.app_state() {
        Some(s) => s,
        None => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "App state not available").into_response();
        }
    };

    let cfg = get_merged_config(&app_state).await;

    let model_ready = if let Some(path) = &cfg.models_path {
        let model_file = model_file_path(path);
        tokio::fs::metadata(&model_file)
            .await
            .map(|meta| meta.is_file())
            .unwrap_or(false)
    } else {
        false
    };

    let ctx = json!({
        "config": cfg,
        "model_ready": model_ready,
        "current_user": user,
        "base_path": state.base_path(),
        "nav_active": "Call Transcription",
    });
    state.render("settings.html", ctx)
}

pub async fn update_settings(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<TranscriptSettingsUpdate>,
) -> Response {
    let config_path = match get_config_path(&state) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let mut doc = match load_document(&config_path) {
        Ok(doc) => doc,
        Err(resp) => return resp,
    };

    // Ensure [proxy] table exists
    if !doc.contains_key("proxy") {
        doc["proxy"] = Item::Table(Table::new());
    }
    let proxy = &mut doc["proxy"];
    let proxy_table = match proxy.as_table_mut() {
        Some(t) => t,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "proxy is not a table"})),
            )
                .into_response();
        }
    };

    // Ensure [proxy.transcript] table exists
    if !proxy_table.contains_key("transcript") {
        proxy_table["transcript"] = Item::Table(Table::new());
    }
    let transcript = &mut proxy_table["transcript"];
    let transcript_table = match transcript.as_table_mut() {
        Some(t) => t,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "proxy.transcript is not a table"})),
            )
                .into_response();
        }
    };

    if let Some(v) = payload.command {
        transcript_table["command"] = value(v);
    }
    if let Some(v) = payload.models_path {
        transcript_table["models_path"] = value(v);
    }
    if let Some(v) = payload.default_language {
        transcript_table["default_language"] = value(v);
    }
    if let Some(v) = payload.timeout_secs {
        transcript_table["timeout_secs"] = value(v as i64);
    }
    if let Some(v) = payload.hf_endpoint {
        transcript_table["hf_endpoint"] = value(v);
    }

    if let Err(resp) = persist_document(&config_path, doc.to_string()) {
        return resp;
    }

    Json(json!({ "message": "Settings saved" })).into_response()
}

async fn get_merged_config(app_state: &crate::app::AppState) -> TranscriptConfig {
    // Try to read from file first to get latest changes
    if let Some(path) = &app_state.config_path {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            if let Ok(config) = toml::from_str::<Value>(&content) {
                if let Some(proxy) = config.get("proxy") {
                    if let Some(transcript_val) = proxy.get("transcript") {
                        if let Ok(mut transcript) =
                            serde_json::from_value::<TranscriptConfig>(transcript_val.clone())
                        {
                            if transcript.models_path.is_none() {
                                transcript.models_path = Some("./config/models".to_string());
                            }
                            return transcript;
                        }
                    }
                }
            }
        }
    }

    // Fallback to default if not found in config
    let mut cfg = TranscriptConfig::default();
    if cfg.models_path.is_none() {
        cfg.models_path = Some("./config/models".to_string());
    }
    cfg
}

fn get_config_path(state: &ConsoleState) -> Result<String, Response> {
    let Some(app_state) = state.app_state() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"message": "Application state is unavailable."})),
        )
            .into_response());
    };
    let Some(path) = app_state.config_path.clone() else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Configuration file path is unknown."})),
        )
            .into_response());
    };
    Ok(path)
}

fn load_document(path: &str) -> Result<DocumentMut, Response> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": format!("Failed to read config file: {}", e)})),
            )
                .into_response());
        }
    };
    match content.parse::<DocumentMut>() {
        Ok(doc) => Ok(doc),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": format!("Failed to parse config file: {}", e)})),
        )
            .into_response()),
    }
}

fn persist_document(path: &str, content: String) -> Result<(), Response> {
    match std::fs::write(path, content) {
        Ok(_) => Ok(()),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": format!("Failed to write config file: {}", e)})),
        )
            .into_response()),
    }
}
