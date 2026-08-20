//! Shared helpers for call-recording artifacts: segment naming, local
//! archival layout, upload-failure markers, and signaling JSONL paths.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::utils::sanitize_id;

/// How local recordings are archived under `[recording].path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RecordingSubdir {
    #[default]
    Daily,
    Hourly,
}

impl RecordingSubdir {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("hourly") => Self::Hourly,
            _ => Self::Daily,
        }
    }

    /// Directory relative to the recording root for `at`.
    pub fn relative_dir(&self, at: DateTime<Utc>) -> String {
        match self {
            Self::Daily => at.format("%Y%m%d").to_string(),
            Self::Hourly => at.format("%Y%m%d/%H").to_string(),
        }
    }
}

/// One completed recording segment (full-call or mid-call slice).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingSegment {
    pub path: String,
    pub size: u64,
    pub segment_type: String,
    pub segment_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    pub duration_secs: f64,
}

/// In-flight recording bookkeeping owned by `SipSession`.
#[derive(Debug, Clone)]
pub struct ActiveRecording {
    pub path: String,
    pub segment_type: String,
    pub segment_id: String,
    pub started_at: DateTime<Utc>,
    /// When true, `RecordingComplete` is delivered to the running CallApp
    /// (voicemail / IVR `torecord`). Mid-call `record_start` segments set
    /// this to false so a later `record_stop` does not hijack the IVR flow.
    pub notify_app: bool,
}

/// Build `{session_id}_{YYYYMMDDHHMMSS}_{type}_{id}.wav` under `root`.
pub fn segment_wav_path(
    root: &str,
    session_id: &str,
    segment_type: &str,
    segment_id: &str,
    at: DateTime<Utc>,
) -> PathBuf {
    let safe_session = sanitize_id(session_id);
    let safe_type = sanitize_component(segment_type, "seg");
    let safe_id = sanitize_component(segment_id, "id");
    let name = format!(
        "{}_{}_{}_{}.wav",
        safe_session,
        at.format("%Y%m%d%H%M%S"),
        safe_type,
        safe_id
    );
    PathBuf::from(root).join(name)
}

/// Signaling sidecar path for a call: `{root}/{session_id}_{call_id}.jsonl`
/// when session ≠ call, else `{root}/{session_id}.jsonl`.
pub fn signaling_jsonl_path(root: &str, session_id: &str, call_id: &str) -> PathBuf {
    let safe_session = sanitize_id(session_id);
    let safe_call = sanitize_id(call_id);
    let name = if safe_session == safe_call || safe_call.is_empty() {
        format!("{}.jsonl", safe_session)
    } else {
        format!("{}_{}.jsonl", safe_session, safe_call)
    };
    PathBuf::from(root).join(name)
}

/// Archive destination for `type=local`: `{root}/{daily|hourly}/{filename}`.
pub fn local_archive_path(
    root: &str,
    source: &Path,
    subdir: RecordingSubdir,
    at: DateTime<Utc>,
) -> PathBuf {
    let file_name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "recording.wav".to_string());
    PathBuf::from(root)
        .join(subdir.relative_dir(at))
        .join(file_name)
}

/// Marker file beside a failed upload: `.upload_failed.{filename}`.
pub fn upload_failed_marker_path(source: &Path) -> PathBuf {
    let file_name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "recording".to_string());
    source
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".upload_failed.{}", file_name))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadFailedMarker {
    pub time: String,
    pub address: String,
    pub duration_ms: u64,
    pub error: String,
}

pub async fn write_upload_failed_marker(
    source: &Path,
    address: &str,
    duration_ms: u64,
    error: &str,
) -> std::io::Result<()> {
    let marker = UploadFailedMarker {
        time: Utc::now().to_rfc3339(),
        address: address.to_string(),
        duration_ms,
        error: error.to_string(),
    };
    let path = upload_failed_marker_path(source);
    let body = serde_json::to_vec_pretty(&marker).unwrap_or_default();
    tokio::fs::write(path, body).await
}

fn sanitize_component(raw: &str, fallback: &str) -> String {
    let cleaned = sanitize_id(raw);
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn segment_path_uses_session_type_id() {
        let at = Utc.with_ymd_and_hms(2026, 8, 20, 1, 2, 3).unwrap();
        let path = segment_wav_path("/rec", "sess-1", "ivr", "a1b2", at);
        assert_eq!(
            path,
            PathBuf::from("/rec/sess-1_20260820010203_ivr_a1b2.wav")
        );
    }

    #[test]
    fn signaling_path_collapses_when_ids_match() {
        let path = signaling_jsonl_path("/rec", "abc", "abc");
        assert_eq!(path, PathBuf::from("/rec/abc.jsonl"));
        let path = signaling_jsonl_path("/rec", "root", "leg");
        assert_eq!(path, PathBuf::from("/rec/root_leg.jsonl"));
    }

    #[test]
    fn local_archive_daily_and_hourly() {
        let at = Utc.with_ymd_and_hms(2026, 8, 20, 15, 0, 0).unwrap();
        let src = Path::new("/tmp/foo.wav");
        assert_eq!(
            local_archive_path("/rec", src, RecordingSubdir::Daily, at),
            PathBuf::from("/rec/20260820/foo.wav")
        );
        assert_eq!(
            local_archive_path("/rec", src, RecordingSubdir::Hourly, at),
            PathBuf::from("/rec/20260820/15/foo.wav")
        );
    }

    #[test]
    fn upload_failed_marker_name() {
        let path = upload_failed_marker_path(Path::new("/rec/a.wav"));
        assert_eq!(path, PathBuf::from("/rec/.upload_failed.a.wav"));
    }

    #[tokio::test]
    async fn write_upload_failed_marker_json_fields() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.wav");
        std::fs::write(&src, b"x").unwrap();
        write_upload_failed_marker(&src, "s3://bucket/key", 42, "timeout")
            .await
            .unwrap();
        let marker = upload_failed_marker_path(&src);
        let body: UploadFailedMarker =
            serde_json::from_str(&std::fs::read_to_string(marker).unwrap()).unwrap();
        assert_eq!(body.address, "s3://bucket/key");
        assert_eq!(body.duration_ms, 42);
        assert_eq!(body.error, "timeout");
        assert!(!body.time.is_empty());
    }

    #[test]
    fn recording_subdir_parse_defaults_to_daily() {
        assert_eq!(RecordingSubdir::parse(None), RecordingSubdir::Daily);
        assert_eq!(RecordingSubdir::parse(Some("")), RecordingSubdir::Daily);
        assert_eq!(
            RecordingSubdir::parse(Some("hourly")),
            RecordingSubdir::Hourly
        );
        assert_eq!(
            RecordingSubdir::parse(Some("DAILY")),
            RecordingSubdir::Daily
        );
    }
}
