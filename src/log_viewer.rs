//! Shared log-viewer helpers.
//!
//! Single implementation of "read the tail / follow window of the configured
//! log file" used by both the console settings endpoints
//! (`/settings/logs/...`, superuser-facing) and the AMI cluster log endpoints
//! (`/cluster/logs/...`, peer-facing so other nodes can proxy a node's logs
//! for the console cluster log viewer).

use serde_json::{Value as JsonValue, json};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};

pub const LOG_DEFAULT_LIMIT: usize = 200;
pub const LOG_MAX_LIMIT: usize = 5000;

pub struct FollowReadResult {
    pub lines: Vec<String>,
    pub next_position: u64,
    pub reset: bool,
    pub truncated: bool,
}

pub fn normalize_log_limit(limit: Option<usize>) -> usize {
    match limit {
        Some(value) => value.clamp(1, LOG_MAX_LIMIT),
        None => LOG_DEFAULT_LIMIT,
    }
}

/// Resolve the configured `log_file` (trimmed, non-empty) from a config.
pub fn log_file_path_from_config(config: &crate::config::Config) -> Option<String> {
    config
        .log_file
        .as_ref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn read_recent_log_lines(path: &str, limit: usize) -> io::Result<(Vec<String>, u64, bool)> {
    let file = File::open(path)?;
    let next_position = file.metadata()?.len();
    let reader = BufReader::new(file);
    let mut lines = VecDeque::new();
    let mut truncated = false;

    for line in reader.lines() {
        let line = line?;
        if lines.len() == limit {
            lines.pop_front();
            truncated = true;
        }
        lines.push_back(line);
    }

    Ok((lines.into_iter().collect(), next_position, truncated))
}

pub fn read_follow_log_lines(path: &str, position: u64, limit: usize) -> io::Result<FollowReadResult> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();

    if position > file_len {
        let (lines, next_position, truncated) = read_recent_log_lines(path, limit)?;
        return Ok(FollowReadResult {
            lines,
            next_position,
            reset: true,
            truncated,
        });
    }

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(position))?;

    let mut lines = Vec::new();
    let mut raw = String::new();
    let mut truncated = false;

    while lines.len() < limit {
        raw.clear();
        let read = reader.read_line(&mut raw)?;
        if read == 0 {
            break;
        }
        lines.push(raw.trim_end_matches(&['\n', '\r'][..]).to_string());
    }

    if lines.len() == limit {
        let mut extra = String::new();
        let extra_read = reader.read_line(&mut extra)?;
        if extra_read > 0 {
            truncated = true;
            let rewind = i64::try_from(extra_read).unwrap_or(i64::MAX);
            reader.seek(SeekFrom::Current(-rewind))?;
        }
    }

    let next_position = reader.stream_position()?;

    Ok(FollowReadResult {
        lines,
        next_position,
        reset: false,
        truncated,
    })
}

/// Build the JSON payload for a "recent logs" request. `Ok(payload)` covers
/// the ok / not-configured / not-found cases; `Err(message)` reports hard
/// I/O failures so callers can map them to their own error responses.
pub fn recent_log_payload(path: Option<&str>, limit: usize) -> Result<JsonValue, String> {
    let Some(path) = path else {
        return Ok(json!({
            "status": "ok",
            "available": false,
            "exists": false,
            "path": JsonValue::Null,
            "lines": [],
            "next_position": 0u64,
            "reset": false,
            "truncated": false,
            "message": "Log file is not configured. Set settings -> platform -> log_file first.",
        }));
    };

    match read_recent_log_lines(path, limit) {
        Ok((lines, next_position, truncated)) => Ok(json!({
            "status": "ok",
            "available": true,
            "exists": true,
            "path": path,
            "lines": lines,
            "next_position": next_position,
            "reset": false,
            "truncated": truncated,
            "message": JsonValue::Null,
        })),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(json!({
            "status": "ok",
            "available": true,
            "exists": false,
            "path": path,
            "lines": [],
            "next_position": 0u64,
            "reset": false,
            "truncated": false,
            "message": "Log file does not exist yet.",
        })),
        Err(err) => Err(format!("Failed to read log file: {err}")),
    }
}

/// Build the JSON payload for a polling "follow logs" request. Same
/// contract as [`recent_log_payload`].
pub fn follow_log_payload(
    path: Option<&str>,
    position: u64,
    limit: usize,
) -> Result<JsonValue, String> {
    let Some(path) = path else {
        return Ok(json!({
            "status": "ok",
            "available": false,
            "exists": false,
            "path": JsonValue::Null,
            "lines": [],
            "next_position": 0u64,
            "reset": false,
            "truncated": false,
            "message": "Log file is not configured. Set settings -> platform -> log_file first.",
        }));
    };

    match read_follow_log_lines(path, position, limit) {
        Ok(result) => Ok(json!({
            "status": "ok",
            "available": true,
            "exists": true,
            "path": path,
            "lines": result.lines,
            "next_position": result.next_position,
            "reset": result.reset,
            "truncated": result.truncated,
            "message": JsonValue::Null,
        })),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(json!({
            "status": "ok",
            "available": true,
            "exists": false,
            "path": path,
            "lines": [],
            "next_position": 0u64,
            "reset": position > 0,
            "truncated": false,
            "message": "Log file does not exist yet.",
        })),
        Err(err) => Err(format!("Failed to follow log file: {err}")),
    }
}

/// Build one SSE stream frame for the log follow stream. Frames keep the
/// payload shape understood by the console JS (`status`/`path`/`lines`/
/// `next_position`/`reset`/`truncated`); the cursor should advance to
/// `next_position` when present, otherwise stay unchanged (error frames
/// carry no `next_position`, not-found frames reset it to 0).
pub fn follow_log_stream_frame(path: &str, position: u64, limit: usize) -> JsonValue {
    match read_follow_log_lines(path, position, limit) {
        Ok(result) => json!({
            "status": "ok",
            "path": path,
            "lines": result.lines,
            "next_position": result.next_position,
            "reset": result.reset,
            "truncated": result.truncated,
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => json!({
            "status": "ok",
            "path": path,
            "lines": [],
            "next_position": 0u64,
            "reset": true,
            "truncated": false,
            "exists": false,
            "message": "Log file does not exist yet.",
        }),
        Err(err) => json!({
            "status": "error",
            "message": format!("Failed to follow log file: {err}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn read_recent_log_lines_limits_tail() {
        let mut file = NamedTempFile::new().expect("tempfile");
        writeln!(file, "line-1").expect("write line 1");
        writeln!(file, "line-2").expect("write line 2");
        writeln!(file, "line-3").expect("write line 3");

        let path = file.path().to_string_lossy().to_string();
        let (lines, next_position, truncated) =
            read_recent_log_lines(&path, 2).expect("read recent logs");

        assert_eq!(lines, vec!["line-2".to_string(), "line-3".to_string()]);
        assert!(next_position > 0);
        assert!(truncated);
    }

    #[test]
    fn follow_logs_resets_on_rotation() {
        let mut file = NamedTempFile::new().expect("tempfile");
        writeln!(file, "new-1").expect("write new-1");
        writeln!(file, "new-2").expect("write new-2");

        let path = file.path().to_string_lossy().to_string();
        let result = read_follow_log_lines(&path, 10_000, 200).expect("follow logs");

        assert!(result.reset);
        assert_eq!(result.lines, vec!["new-1".to_string(), "new-2".to_string()]);
        assert!(result.next_position > 0);
    }

    #[test]
    fn follow_logs_keeps_position_when_truncated() {
        let mut file = NamedTempFile::new().expect("tempfile");
        writeln!(file, "l1").expect("write l1");
        writeln!(file, "l2").expect("write l2");
        writeln!(file, "l3").expect("write l3");

        let path = file.path().to_string_lossy().to_string();
        let first = read_follow_log_lines(&path, 0, 2).expect("first follow");
        assert_eq!(first.lines, vec!["l1".to_string(), "l2".to_string()]);
        assert!(first.truncated);

        let second = read_follow_log_lines(&path, first.next_position, 2).expect("second follow");
        assert_eq!(second.lines, vec!["l3".to_string()]);
        assert!(!second.reset);
    }

    #[test]
    fn recent_log_payload_reports_unconfigured() {
        let payload = recent_log_payload(None, 200).expect("payload");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["available"], false);
        assert_eq!(payload["exists"], false);
        assert!(payload["lines"].as_array().unwrap().is_empty());
    }

    #[test]
    fn follow_log_payload_reports_missing_file() {
        let payload = follow_log_payload(Some("/nonexistent/rustpbx-log-test.log"), 5, 100)
            .expect("payload");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["exists"], false);
        assert_eq!(payload["reset"], true);
    }

    #[test]
    fn follow_log_stream_frame_error_has_no_next_position() {
        let frame = follow_log_stream_frame("/nonexistent/rustpbx-log-test.log", 0, 100);
        // not-found frames reset the cursor to 0
        assert_eq!(frame["next_position"], 0u64);
        assert_eq!(frame["reset"], true);
    }
}
